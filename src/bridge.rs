use abi_stable::std_types::{RSlice, RStr, RString, RVec};
use bridge_api::{
    FormatBridge, RCoordinateFormat, RInputTransport, RPushResult, RVbapCartesianDefaults,
    RVbapTableMode,
};
use eac3::{CorePcmFrame, ObjectPcmDecoder, PcmDecoder};
use std::collections::VecDeque;
#[cfg(feature = "bridge-perf")]
use std::env;
#[cfg(feature = "bridge-perf")]
use std::time::Instant;
use truehd::process::{decode::Decoder, extract::Extractor, parse::Parser, MAX_PRESENTATIONS};

use crate::ac3_native::NativeAc3Decoder;
use crate::eac3_pipeline::{
    diagnose_eac3_frame, is_dependent_eac3_frame, is_legacy_ac3_frame,
    is_temporary_eac3_silence_frame, process_eac3_dependent_frame_with_core, process_eac3_frame,
};
use crate::eac3_spdif::Eac3SpdifStream;
use crate::frame_builders::PcmStats;
use crate::logging::bridge_diag_log;
use crate::mat::MatStream;
use crate::metadata::ObjectNameKey;
use crate::perf::PerfStats;
use crate::truehd_pipeline::process_extractor_input;

#[derive(Debug, Default)]
pub(crate) struct Eac3DiagStats {
    pub(crate) total_frames: u64,
    pub(crate) legacy_ac3_frames: u64,
    pub(crate) independent_frames: u64,
    pub(crate) dependent_frames: u64,
    pub(crate) ac3_convert_frames: u64,
    pub(crate) joc_frames: u64,
    pub(crate) oamd_frames: u64,
    pub(crate) ac3_core_decoded: u64,
    pub(crate) ac3_core_decode_failures: u64,
    pub(crate) dependent_pair_attempts: u64,
    pub(crate) dependent_pair_no_object: u64,
    pub(crate) dependent_pair_failures: u64,
    pub(crate) paired_object_frames: u64,
    pub(crate) short_packet_silence_frames: u64,
    pub(crate) last_ac3_core_decode_error: Option<String>,
    pub(crate) last_dependent_pair_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum DrcMode {
    #[default]
    Off,
    Standard,
    Heavy,
}

pub(crate) struct AtmosBridge {
    // ── TrueHD pipeline ──────────────────────────────────────────────
    pub(crate) mat_stream: MatStream,
    pub(crate) extractor: Extractor,
    pub(crate) parser: Parser,
    pub(crate) decoder: Decoder,
    // ── E-AC3 pipeline ───────────────────────────────────────────────
    pub(crate) eac3_spdif: Eac3SpdifStream,
    pub(crate) eac3_pcm_decoder: PcmDecoder,
    pub(crate) eac3_object_decoder: ObjectPcmDecoder,
    pub(crate) ac3_decoder: NativeAc3Decoder,
    pub(crate) pending_ac3_cores: VecDeque<CorePcmFrame>,
    pub(crate) pending_dependent_frames: VecDeque<Vec<u8>>,
    pub(crate) eac3_frame_count: u64,
    pub(crate) eac3_total_samples: u64,
    /// True when the most recent `push_packet` used the E-AC3 path.
    pub(crate) eac3_active: bool,
    pub(crate) eac3_diag_stats: Eac3DiagStats,
    // ── Shared ───────────────────────────────────────────────────────
    pub(crate) presentation: u8,
    pub(crate) strict: bool,
    /// Running total of decoded samples (used for metadata timestamping).
    pub(crate) total_samples: u64,
    /// Current dialogue level from the last major sync.
    pub(crate) current_dialogue_level: Option<i8>,
    /// Substream info tracking for change detection (TrueHD only).
    pub(crate) current_substream_info: Option<u8>,
    pub(crate) current_extended_substream_info: Option<u8>,
    pub(crate) recovering_until_major_sync: bool,
    pub(crate) drc_mode: DrcMode,
    pub(crate) frame_count: u64,
    pub(crate) object_name_keys_by_id: Vec<Option<ObjectNameKey>>,
    pub(crate) perf: PerfStats,
}

impl AtmosBridge {
    pub(crate) fn new(strict: bool) -> Self {
        // Default to presentation 3 (full Atmos/JOC); overridable via configure().
        let presentation = 3u8;

        let mut parser = Parser::default();
        let mut decoder = Decoder::default();

        let fail_level = if strict {
            log::Level::Warn
        } else {
            log::Level::Error
        };
        parser.set_fail_level(fail_level);
        decoder.set_fail_level(fail_level);

        // Require all presentations up to and including the requested one.
        let mut required_presentations = [false; MAX_PRESENTATIONS];
        required_presentations[..=presentation as usize]
            .iter_mut()
            .for_each(|p| *p = true);
        parser.set_required_presentations(&required_presentations);

        let eac3_log_level = if strict {
            log::Level::Warn
        } else {
            log::Level::Error
        };
        let mut eac3_pcm = PcmDecoder::new();
        eac3_pcm.set_debug_log_level(eac3_log_level);
        let mut eac3_obj = ObjectPcmDecoder::new();
        eac3_obj.set_debug_log_level(eac3_log_level);

        #[allow(unused_mut)]
        let mut bridge = Self {
            mat_stream: MatStream::default(),
            extractor: Extractor::default(),
            parser,
            decoder,
            eac3_spdif: Eac3SpdifStream::default(),
            eac3_pcm_decoder: eac3_pcm,
            eac3_object_decoder: eac3_obj,
            ac3_decoder: NativeAc3Decoder::default(),
            pending_ac3_cores: VecDeque::new(),
            pending_dependent_frames: VecDeque::new(),
            eac3_frame_count: 0,
            eac3_total_samples: 0,
            eac3_active: false,
            eac3_diag_stats: Eac3DiagStats::default(),
            presentation,
            strict,
            total_samples: 0,
            current_dialogue_level: None,
            current_substream_info: None,
            current_extended_substream_info: None,
            recovering_until_major_sync: false,
            drc_mode: DrcMode::Off,
            frame_count: 0,
            object_name_keys_by_id: Vec::new(),
            perf: PerfStats::default(),
        };

        #[cfg(feature = "bridge-perf")]
        {
            let enabled = env::var("TRUEHD_BRIDGE_PERF_PROFILE")
                .ok()
                .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "on" | "yes"));
            let interval = env::var("TRUEHD_BRIDGE_PERF_REPORT_EVERY")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|&v| v > 0)
                .unwrap_or(120);
            bridge.perf.configure(enabled, interval);
        }

        bridge
    }

    pub(crate) fn reset_pipeline(&mut self) {
        // TrueHD reset.
        self.mat_stream.reset();
        self.extractor = Extractor::default();
        self.parser = Parser::default();
        self.decoder = Decoder::default();

        // E-AC3 reset.
        self.eac3_spdif.reset();
        self.eac3_pcm_decoder.reset();
        self.eac3_object_decoder.reset();
        self.ac3_decoder.reset();
        self.pending_ac3_cores.clear();
        self.pending_dependent_frames.clear();
        self.eac3_frame_count = 0;
        self.eac3_active = false;

        // Re-apply configuration to new parser/decoder instances.
        let fail_level = if self.strict {
            log::Level::Warn
        } else {
            log::Level::Error
        };
        self.parser.set_fail_level(fail_level);
        self.decoder.set_fail_level(fail_level);
        self.eac3_pcm_decoder.set_debug_log_level(fail_level);
        self.eac3_object_decoder.set_debug_log_level(fail_level);
        let mut required_presentations = [false; MAX_PRESENTATIONS];
        required_presentations[..=self.presentation as usize]
            .iter_mut()
            .for_each(|p| *p = true);
        self.parser
            .set_required_presentations(&required_presentations);
        self.object_name_keys_by_id.clear();
        self.recovering_until_major_sync = false;
    }

    fn try_decode_pending_eac3_pair(&mut self) -> Option<bridge_api::RDecodedFrame> {
        let core = self.pending_ac3_cores.pop_front()?;
        let dependent = self.pending_dependent_frames.pop_front()?;
        self.eac3_diag_stats.dependent_pair_attempts += 1;
        match process_eac3_dependent_frame_with_core(self, &dependent, core) {
            Ok(Some(decoded_frame)) => Some(decoded_frame),
            Ok(None) => {
                self.eac3_diag_stats.dependent_pair_no_object += 1;
                self.eac3_diag_stats.last_dependent_pair_error =
                    Some("no_object_payload".to_string());
                None
            }
            Err(err) => {
                self.eac3_diag_stats.dependent_pair_failures += 1;
                self.eac3_diag_stats.last_dependent_pair_error = Some(err.clone());
                bridge_diag_log(
                    log::Level::Warn,
                    &format!("eac3_dependent_pair_failed error={err}"),
                );
                None
            }
        }
    }
}

impl FormatBridge for AtmosBridge {
    fn push_packet(
        &mut self,
        data: RSlice<'_, u8>,
        transport: RInputTransport,
        data_type: u8,
    ) -> RPushResult {
        let mut result = RPushResult {
            frames: RVec::new(),
            error_message: RString::new(),
            did_reset: false,
        };

        match transport {
            RInputTransport::Raw => {
                #[cfg(feature = "bridge-perf")]
                self.perf.note_raw_packet(data.len());
                process_extractor_input(self, data.as_slice(), &mut result);
                result
            }
            RInputTransport::Iec61937 => {
                // ── TrueHD (data type 0x16) ───────────────────────────
                if MatStream::accepts_data_type(data_type) {
                    self.eac3_active = false;

                    #[cfg(feature = "bridge-perf")]
                    let mat_started = Instant::now();
                    #[cfg(feature = "bridge-perf")]
                    self.perf.note_mat_packet(data.len());
                    self.mat_stream.push_payload(data.as_slice());
                    loop {
                        #[cfg(feature = "bridge-perf")]
                        let chunk_extract_started = Instant::now();
                        match self.mat_stream.next_chunk() {
                            Ok(Some(chunk)) => {
                                #[cfg(feature = "bridge-perf")]
                                {
                                    self.perf
                                        .record_mat_chunk_extract(chunk_extract_started.elapsed());
                                    self.perf.note_mat_chunk(chunk.len());
                                }
                                process_extractor_input(self, &chunk, &mut result);
                                if result.did_reset {
                                    break;
                                }
                            }
                            Ok(None) => {
                                #[cfg(feature = "bridge-perf")]
                                self.perf
                                    .record_mat_chunk_extract(chunk_extract_started.elapsed());
                                break;
                            }
                            Err(msg) => {
                                #[cfg(feature = "bridge-perf")]
                                self.perf
                                    .record_mat_chunk_extract(chunk_extract_started.elapsed());
                                log::warn!("{msg}");
                                self.reset_pipeline();
                                result.did_reset = true;
                                if self.strict {
                                    result.error_message = msg.into();
                                }
                                return result;
                            }
                        }
                    }
                    #[cfg(feature = "bridge-perf")]
                    self.perf.record_mat(mat_started.elapsed());
                    return result;
                }

                // ── E-AC3 (data type 0x15) ────────────────────────────
                if Eac3SpdifStream::accepts_data_type(data_type) {
                    self.eac3_active = true;

                    self.eac3_spdif.push_payload(data.as_slice());
                    let mut temporary_silence_pushed = false;
                    loop {
                        match self.eac3_spdif.next_frame() {
                            Ok(Some(frame)) => {
                                self.eac3_frame_count += 1;
                                if is_legacy_ac3_frame(&frame) {
                                    match self.ac3_decoder.decode_frame(&frame) {
                                        Ok(core) => {
                                            diagnose_eac3_frame(self, &frame);
                                            self.eac3_diag_stats.ac3_core_decoded += 1;
                                            self.pending_ac3_cores.push_back(core);
                                            if let Some(decoded_frame) =
                                                self.try_decode_pending_eac3_pair()
                                            {
                                                result.frames.push(decoded_frame);
                                            }
                                            continue;
                                        }
                                        Err(err) => {
                                            self.eac3_diag_stats.ac3_core_decode_failures += 1;
                                            self.eac3_diag_stats.last_ac3_core_decode_error =
                                                Some(err.clone());
                                            bridge_diag_log(
                                                log::Level::Warn,
                                                &format!(
                                                    "ac3_core_decode_failed index={} error={}",
                                                    self.eac3_frame_count, err
                                                ),
                                            );
                                        }
                                    }
                                }

                                let decode_result = if is_dependent_eac3_frame(&frame) {
                                    self.pending_dependent_frames.push_back(frame.clone());
                                    if let Some(decoded_frame) = self.try_decode_pending_eac3_pair()
                                    {
                                        Ok(decoded_frame)
                                    } else {
                                        continue;
                                    }
                                } else {
                                    process_eac3_frame(self, &frame)
                                };

                                match decode_result {
                                    Ok(decoded_frame) => {
                                        if let Err(reason) = PcmStats::from_frame(&decoded_frame) {
                                            bridge_diag_log(
                                                    log::Level::Warn,
                                                    &format!(
                                                        "eac3_frame_rejected index={} reason={} sr={} samples={} ch={} pcm_len={}",
                                                        self.eac3_frame_count,
                                                        reason,
                                                        decoded_frame.sampling_frequency,
                                                        decoded_frame.sample_count,
                                                        decoded_frame.channel_count,
                                                        decoded_frame.pcm.len()
                                                    ),
                                                );
                                            continue;
                                        }
                                        if is_temporary_eac3_silence_frame(&decoded_frame) {
                                            if temporary_silence_pushed {
                                                continue;
                                            }
                                            temporary_silence_pushed = true;
                                        }
                                        result.frames.push(decoded_frame);
                                    }
                                    Err(msg) => {
                                        log::warn!("{msg}");
                                        self.reset_pipeline();
                                        result.did_reset = true;
                                        result.error_message = msg.into();
                                        return result;
                                    }
                                }
                            }
                            Ok(None) => {
                                break;
                            }
                            Err(msg) => {
                                bridge_diag_log(log::Level::Warn, &format!("eac3_error={msg}"));
                                log::warn!("{msg}");
                                self.reset_pipeline();
                                result.did_reset = true;
                                result.error_message = msg.into();
                                return result;
                            }
                        }
                    }
                    return result;
                }

                // Unsupported data type.
                let msg =
                    format!("Unsupported IEC 61937 data type for this bridge: 0x{data_type:02X}");
                log::warn!("{msg}");
                if self.strict {
                    result.error_message = msg.into();
                    self.reset_pipeline();
                    result.did_reset = true;
                }
                result
            }
        }
    }

    fn reset(&mut self) {
        log::info!("Bridge reset requested");
        self.reset_pipeline();
        // Note: total_samples is NOT reset — it tracks the global position for
        // continuous-mode timestamping. The handler manages segment offsets.
    }

    fn is_ready(&self) -> bool {
        self.frame_count > 0 || self.eac3_frame_count > 0
    }

    fn is_spatial(&self) -> bool {
        if self.eac3_active {
            // E-AC3/JOC is spatial when we've decoded object channels.
            // The object decoder count acts as a proxy.
            self.eac3_object_decoder.frames_seen() > 0
        } else {
            // Presentations 0–(MAX-2) are pure downmixes; the top presentation carries objects.
            self.presentation >= (MAX_PRESENTATIONS as u8) - 1
        }
    }

    fn configure(&mut self, key: RStr<'_>, value: RStr<'_>) -> bool {
        match key.as_str() {
            "presentation" => {
                let p = match value.as_str() {
                    "best" => (MAX_PRESENTATIONS as u8) - 1,
                    s => match s.parse::<u8>() {
                        Ok(p) if p < MAX_PRESENTATIONS as u8 => p,
                        Ok(p) => {
                            log::warn!(
                                "atmos-bridge: presentation {p} out of range (0–{})",
                                MAX_PRESENTATIONS - 1
                            );
                            return false;
                        }
                        Err(_) => {
                            log::warn!("atmos-bridge: cannot parse presentation value {:?}", s);
                            return false;
                        }
                    },
                };
                self.presentation = p;
                let mut required_presentations = [false; MAX_PRESENTATIONS];
                required_presentations[..=p as usize]
                    .iter_mut()
                    .for_each(|v| *v = true);
                self.parser
                    .set_required_presentations(&required_presentations);
                log::debug!("atmos-bridge: presentation set to {p}");
                true
            }
            #[cfg(feature = "bridge-perf")]
            "perf_profile" => {
                let enabled = matches!(value.as_str(), "1" | "true" | "on" | "yes");
                let report_every = self.perf.configure_profile(enabled);
                eprintln!(
                    "harletty-bridge perf profiling {} (report_every_frames={})",
                    if enabled { "enabled" } else { "disabled" },
                    report_every
                );
                true
            }
            #[cfg(feature = "bridge-perf")]
            "perf_report_every" => match value.as_str().parse::<u64>() {
                Ok(interval) if interval > 0 => {
                    self.perf.configure_report_every(interval);
                    eprintln!(
                        "harletty-bridge perf reporting interval set to {} frames",
                        interval
                    );
                    true
                }
                _ => {
                    log::warn!(
                        "atmos-bridge: invalid perf_report_every value {:?}",
                        value.as_str()
                    );
                    false
                }
            },
            #[cfg(not(feature = "bridge-perf"))]
            "perf_profile" | "perf_report_every" => false,
            _ => {
                log::debug!("atmos-bridge: unknown configuration key {:?}", key.as_str());
                false
            }
        }
    }

    fn coordinate_format(&self) -> RCoordinateFormat {
        RCoordinateFormat::Cartesian
    }

    fn vbap_cartesian_defaults(&self) -> RVbapCartesianDefaults {
        // Balanced default grid size for runtime cartesian VBAP table generation.
        RVbapCartesianDefaults {
            x_size: 62,
            y_size: 62,
            z_size: 15,
            allow_negative_z: false,
        }
    }

    fn preferred_vbap_table_mode(&self) -> RVbapTableMode {
        RVbapTableMode::Cartesian
    }

    fn supported_drc_modes(&self) -> RVec<RString> {
        vec![
            RString::from("Off"),
            RString::from("Standard"),
            RString::from("Heavy"),
        ]
        .into()
    }

    fn set_drc_mode(&mut self, mode: RStr<'_>) -> bool {
        let new_mode = match mode.as_str() {
            "Off" => DrcMode::Off,
            "Standard" => DrcMode::Standard,
            "Heavy" => DrcMode::Heavy,
            _ => {
                eprintln!("[harletty][drc] unknown drc_mode {:?}", mode.as_str());
                return false;
            }
        };
        eprintln!(
            "[harletty][drc] set_drc_mode {:?} -> {:?}",
            self.drc_mode, new_mode
        );
        self.drc_mode = new_mode;
        true
    }
}
