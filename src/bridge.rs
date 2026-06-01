use abi_stable::std_types::{RSlice, RStr, RString, RVec};
use bridge_api::{
    FormatBridge, RCoordinateFormat, RInputTransport, RPushResult, RVbapCartesianDefaults,
    RVbapTableMode,
};
use eac3::{CorePcmFrame, Extractor as Eac3RawExtractor, ObjectPcmDecoder, PcmDecoder};
use std::collections::VecDeque;
#[cfg(feature = "bridge-perf")]
use std::env;
#[cfg(feature = "bridge-perf")]
use std::time::Instant;
use truehd::process::{MAX_PRESENTATIONS, decode::Decoder, extract::Extractor, parse::Parser};

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

/// Codec carried by a [`RInputTransport::Raw`] packet, which (unlike the IEC
/// 61937 transport) has no `data_type` to disambiguate TrueHD from E-AC3.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RawCodec {
    TrueHd,
    Eac3,
    Dts,
}

/// Best-effort codec detection on a raw access unit, used when the host did not
/// declare the codec via `configure("input_codec", …)`. Checks the most
/// specific pattern first: the TrueHD major-sync word `0xF8726FBA` at offset 4,
/// then the E-AC3/AC-3 sync word `0x0B77` at offset 0 (incl. byte-swapped).
fn sniff_raw_codec(data: &[u8]) -> Option<RawCodec> {
    if data.len() >= 8
        && data[4] == 0xF8
        && data[5] == 0x72
        && data[6] == 0x6F
        && data[7] == 0xBA
    {
        return Some(RawCodec::TrueHd);
    }
    // DTS core (0x7FFE8001) or extension substream (0x64582025) at offset 0.
    if data.len() >= 4 {
        let w = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        if w == 0x7FFE_8001 || w == 0x6458_2025 {
            return Some(RawCodec::Dts);
        }
    }
    if data.len() >= 2
        && ((data[0] == 0x0B && data[1] == 0x77) || (data[0] == 0x77 && data[1] == 0x0B))
    {
        return Some(RawCodec::Eac3);
    }
    None
}

pub(crate) struct AtmosBridge {
    // ── TrueHD pipeline ──────────────────────────────────────────────
    pub(crate) mat_stream: MatStream,
    pub(crate) extractor: Extractor,
    pub(crate) parser: Parser,
    pub(crate) decoder: Decoder,
    // ── E-AC3 pipeline ───────────────────────────────────────────────
    pub(crate) eac3_spdif: Eac3SpdifStream,
    /// Raw E-AC3 syncframe extractor (used by the `Raw` transport, e.g. mpv).
    pub(crate) eac3_raw_extractor: Eac3RawExtractor,
    pub(crate) eac3_pcm_decoder: PcmDecoder,
    pub(crate) eac3_object_decoder: ObjectPcmDecoder,
    pub(crate) ac3_decoder: NativeAc3Decoder,
    pub(crate) pending_ac3_cores: VecDeque<CorePcmFrame>,
    pub(crate) pending_dependent_frames: VecDeque<Vec<u8>>,
    pub(crate) eac3_frame_count: u64,
    pub(crate) eac3_total_samples: u64,
    /// True when the most recent `push_packet` used the E-AC3 path.
    pub(crate) eac3_active: bool,
    /// Codec forced by the host for the `Raw` transport via
    /// `configure("input_codec", …)`. Persists across pipeline resets.
    pub(crate) forced_raw_codec: Option<RawCodec>,
    /// Codec locked for the current raw session (forced or sniffed). Cleared on
    /// reset so a re-sniff happens after a seek / stream change.
    pub(crate) raw_codec: Option<RawCodec>,
    pub(crate) eac3_diag_stats: Eac3DiagStats,
    // ── DTS (DCA) pipeline ───────────────────────────────────────────
    /// Raw byte buffer for demuxing `[core][exss]` DTS-HD frames.
    pub(crate) dts_buf: Vec<u8>,
    /// Plain DTS core (5.1) decoder.
    pub(crate) dts_decoder: dca::PcmDecoder,
    /// DTS-HD Master Audio lossless (5.1/7.1) decoder.
    pub(crate) dts_hd_decoder: dca::HdDecoder,
    pub(crate) dts_frame_count: u64,
    /// True when the most recent `push_packet` used the DTS path.
    pub(crate) dts_active: bool,
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
            eac3_raw_extractor: Eac3RawExtractor::default(),
            eac3_pcm_decoder: eac3_pcm,
            eac3_object_decoder: eac3_obj,
            ac3_decoder: NativeAc3Decoder::default(),
            pending_ac3_cores: VecDeque::new(),
            pending_dependent_frames: VecDeque::new(),
            eac3_frame_count: 0,
            eac3_total_samples: 0,
            eac3_active: false,
            forced_raw_codec: None,
            raw_codec: None,
            eac3_diag_stats: Eac3DiagStats::default(),
            dts_buf: Vec::new(),
            dts_decoder: dca::PcmDecoder::new(),
            dts_hd_decoder: dca::HdDecoder::new(),
            dts_frame_count: 0,
            dts_active: false,
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
        self.eac3_raw_extractor = Eac3RawExtractor::default();
        self.eac3_pcm_decoder.reset();
        self.eac3_object_decoder.reset();
        self.ac3_decoder.reset();
        self.pending_ac3_cores.clear();
        self.pending_dependent_frames.clear();
        self.eac3_frame_count = 0;
        self.eac3_active = false;

        // DTS reset.
        self.dts_buf.clear();
        self.dts_decoder.reset();
        self.dts_hd_decoder.reset();
        self.dts_frame_count = 0;
        self.dts_active = false;
        // Re-sniff after reset, but keep any host-declared codec.
        self.raw_codec = None;

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
        // Both queues must have a frame before we commit to popping either —
        // otherwise an AC-3 core popped here without a partner is silently
        // dropped when `?` short-circuits the function. On streams that
        // deliver `[AC-3 core, E-AC-3 dep]` per packet (DD+ JOC with a
        // backward-compat AC-3 core), the first try_pair after the core was
        // queued ALWAYS hits the empty dep queue, losing the core. The next
        // try_pair after the dep is queued then finds no core to pair with.
        // Net effect: pair_attempts stays at 0 forever and is_spatial never
        // flips to true.
        if self.pending_ac3_cores.is_empty() || self.pending_dependent_frames.is_empty() {
            return None;
        }
        let core = self.pending_ac3_cores.pop_front().unwrap();
        let dependent = self.pending_dependent_frames.pop_front().unwrap();
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

    /// Resolve the codec for a `Raw` packet. A host-declared codec
    /// (`configure("input_codec", …)`) wins; otherwise the first recognisable
    /// sync word locks the session. An unrecognised first packet falls back to
    /// TrueHD for that packet without locking, so a later syncful packet can
    /// still pin the codec.
    fn resolve_raw_codec(&mut self, data: &[u8]) -> RawCodec {
        if let Some(c) = self.raw_codec {
            return c;
        }
        if let Some(c) = self.forced_raw_codec {
            self.raw_codec = Some(c);
            return c;
        }
        if let Some(c) = sniff_raw_codec(data) {
            self.raw_codec = Some(c);
            return c;
        }
        RawCodec::TrueHd
    }

    /// Process one extracted E-AC3 access unit, shared by the IEC 61937 and raw
    /// transports. Returns `Err(())` on a fatal decode error, in which case the
    /// pipeline has been reset and `result` already carries the error — the
    /// caller must stop draining and return.
    fn process_eac3_access_unit(
        &mut self,
        frame: &[u8],
        result: &mut RPushResult,
        temporary_silence_pushed: &mut bool,
    ) -> Result<(), ()> {
        self.eac3_frame_count += 1;
        if is_legacy_ac3_frame(frame) {
            match self.ac3_decoder.decode_frame(frame) {
                Ok(core) => {
                    diagnose_eac3_frame(self, frame);
                    self.eac3_diag_stats.ac3_core_decoded += 1;
                    self.pending_ac3_cores.push_back(core);
                    if let Some(decoded_frame) = self.try_decode_pending_eac3_pair() {
                        result.frames.push(decoded_frame);
                    }
                    return Ok(());
                }
                Err(err) => {
                    self.eac3_diag_stats.ac3_core_decode_failures += 1;
                    self.eac3_diag_stats.last_ac3_core_decode_error = Some(err.clone());
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

        let decode_result = if is_dependent_eac3_frame(frame) {
            self.pending_dependent_frames.push_back(frame.to_vec());
            match self.try_decode_pending_eac3_pair() {
                Some(decoded_frame) => Ok(decoded_frame),
                None => return Ok(()),
            }
        } else {
            process_eac3_frame(self, frame)
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
                    return Ok(());
                }
                if is_temporary_eac3_silence_frame(&decoded_frame) {
                    if *temporary_silence_pushed {
                        return Ok(());
                    }
                    *temporary_silence_pushed = true;
                }
                result.frames.push(decoded_frame);
                Ok(())
            }
            Err(msg) => {
                log::warn!("{msg}");
                self.reset_pipeline();
                result.did_reset = true;
                result.error_message = msg.into();
                Err(())
            }
        }
    }

    /// Drain all complete E-AC3 access units currently buffered in the raw
    /// extractor, rendering each through [`Self::process_eac3_access_unit`].
    fn drain_eac3_raw(&mut self, result: &mut RPushResult) {
        let mut temporary_silence_pushed = false;
        loop {
            match self.eac3_raw_extractor.next_frame() {
                Ok(Some(frame)) => {
                    if self
                        .process_eac3_access_unit(
                            frame.as_bytes(),
                            result,
                            &mut temporary_silence_pushed,
                        )
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    let msg = format!("eac3_raw_extract_error={err:?}");
                    bridge_diag_log(log::Level::Warn, &msg);
                    log::warn!("{msg}");
                    self.reset_pipeline();
                    result.did_reset = true;
                    result.error_message = msg.into();
                    return;
                }
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
                // One-shot diagnostic: log the first raw packet's first 64 bytes so we
                // can correlate what the host (e.g. mpv-omniphony's ad_orender) feeds
                // us against what the SPDIF path receives. Triggered only until the
                // first frame is successfully decoded; cleared on reset so post-seek
                // packets log again.
                match self.resolve_raw_codec(data.as_slice()) {
                    RawCodec::Eac3 => {
                        self.eac3_active = true;
                        self.dts_active = false;
                        self.eac3_raw_extractor.push_bytes(data.as_slice());
                        self.drain_eac3_raw(&mut result);
                    }
                    RawCodec::TrueHd => {
                        self.eac3_active = false;
                        self.dts_active = false;
                        process_extractor_input(self, data.as_slice(), &mut result);
                    }
                    RawCodec::Dts => {
                        self.eac3_active = false;
                        self.dts_active = true;
                        self.dts_buf.extend_from_slice(data.as_slice());
                        crate::dts_pipeline::drain_dts(self, &mut result);
                    }
                }
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
                                if self
                                    .process_eac3_access_unit(
                                        &frame,
                                        &mut result,
                                        &mut temporary_silence_pushed,
                                    )
                                    .is_err()
                                {
                                    return result;
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
        self.frame_count > 0 || self.eac3_frame_count > 0 || self.dts_frame_count > 0
    }

    fn is_spatial(&self) -> bool {
        if self.dts_active {
            // DTS core is rendered as a fixed 5.1/7.1 bed, placed at the
            // canonical speaker positions — spatial by layout.
            return true;
        }
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
            "input_codec" => {
                self.forced_raw_codec = match value.as_str() {
                    "eac3" | "ec3" | "e-ac3" | "ac3" => Some(RawCodec::Eac3),
                    "truehd" | "mlp" => Some(RawCodec::TrueHd),
                    "dts" | "dca" | "dtsx" | "dts:x" | "dts-hd" | "dtshd" => Some(RawCodec::Dts),
                    "auto" | "" => None,
                    s => {
                        log::warn!("atmos-bridge: unknown input_codec {s:?}");
                        return false;
                    }
                };
                // Force re-resolution against the new codec on the next packet.
                self.raw_codec = None;
                log::debug!("atmos-bridge: input_codec set to {:?}", self.forced_raw_codec);
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
            RString::from("standard/line"),
            RString::from("heavy/RF"),
        ]
        .into()
    }

    fn set_drc_mode(&mut self, mode: RStr<'_>) -> bool {
        let new_mode = match mode.as_str() {
            "Off" => DrcMode::Off,
            "Standard" | "Line" | "standard/line" => DrcMode::Standard,
            "Heavy" | "RF" | "heavy/RF" => DrcMode::Heavy,
            _ => {
                bridge_diag_log(
                    log::Level::Warn,
                    &format!("[harletty][drc] unknown drc_mode {:?}", mode.as_str()),
                );
                return false;
            }
        };
        bridge_diag_log(
            log::Level::Info,
            &format!(
                "[harletty][drc] set_drc_mode {:?} -> {:?}",
                self.drc_mode, new_mode
            ),
        );
        self.drc_mode = new_mode;
        true
    }
}

#[cfg(test)]
mod raw_transport_tests {
    use super::*;

    #[test]
    fn sniff_detects_eac3_syncword() {
        assert_eq!(
            sniff_raw_codec(&[0x0B, 0x77, 0x00, 0x00]),
            Some(RawCodec::Eac3)
        );
        // Byte-swapped 16-bit order is still E-AC3.
        assert_eq!(
            sniff_raw_codec(&[0x77, 0x0B, 0x00, 0x00]),
            Some(RawCodec::Eac3)
        );
    }

    #[test]
    fn sniff_detects_truehd_major_sync() {
        let buf = [0x00, 0x00, 0x00, 0x00, 0xF8, 0x72, 0x6F, 0xBA];
        assert_eq!(sniff_raw_codec(&buf), Some(RawCodec::TrueHd));
    }

    #[test]
    fn sniff_unknown_is_none() {
        assert_eq!(sniff_raw_codec(&[0x12, 0x34, 0x56, 0x78]), None);
        assert_eq!(sniff_raw_codec(&[0x0B]), None); // too short
    }

    #[test]
    fn resolve_prefers_forced_codec_and_locks() {
        let mut bridge = AtmosBridge::new(false);
        bridge.forced_raw_codec = Some(RawCodec::Eac3);
        // Unrecognisable bytes, but the host-declared codec wins.
        assert_eq!(bridge.resolve_raw_codec(&[0x12, 0x34, 0x56, 0x78]), RawCodec::Eac3);
        assert_eq!(bridge.raw_codec, Some(RawCodec::Eac3));
    }

    #[test]
    fn resolve_sniffs_when_unforced() {
        let mut eac3 = AtmosBridge::new(false);
        assert_eq!(eac3.resolve_raw_codec(&[0x0B, 0x77, 0, 0]), RawCodec::Eac3);
        assert_eq!(eac3.raw_codec, Some(RawCodec::Eac3));

        let mut thd = AtmosBridge::new(false);
        let buf = [0, 0, 0, 0, 0xF8, 0x72, 0x6F, 0xBA];
        assert_eq!(thd.resolve_raw_codec(&buf), RawCodec::TrueHd);
        assert_eq!(thd.raw_codec, Some(RawCodec::TrueHd));
    }

    #[test]
    fn resolve_unknown_first_packet_defaults_truehd_without_locking() {
        let mut bridge = AtmosBridge::new(false);
        // No recognisable sync → treat as TrueHD for this packet but do NOT
        // lock, so a later syncful packet can still pin the codec.
        assert_eq!(bridge.resolve_raw_codec(&[0x12, 0x34, 0x56, 0x78]), RawCodec::TrueHd);
        assert_eq!(bridge.raw_codec, None);
    }

    #[test]
    fn sniff_detects_dts_syncwords() {
        // Core syncword 0x7FFE8001.
        assert_eq!(
            sniff_raw_codec(&[0x7F, 0xFE, 0x80, 0x01]),
            Some(RawCodec::Dts)
        );
        // Extension substream syncword 0x64582025.
        assert_eq!(
            sniff_raw_codec(&[0x64, 0x58, 0x20, 0x25]),
            Some(RawCodec::Dts)
        );
    }

    #[test]
    fn configure_input_codec_accepts_dts() {
        let mut bridge = AtmosBridge::new(false);
        assert!(bridge.configure("input_codec".into(), "dts".into()));
        assert_eq!(bridge.forced_raw_codec, Some(RawCodec::Dts));
    }

    // End-to-end: feed a raw DTS core stream through the FormatBridge and check
    // it emits 5.1 bed frames with the expected channel labels. Skips when the
    // (uncommitted) corpus is absent.
    #[test]
    fn dts_raw_transport_emits_bed_frames() {
        const DTS: &str = "/home/user/dev/spatial-renderer/dumps/dts51_core.dts";
        if !std::path::Path::new(DTS).exists() {
            eprintln!("skipping: DTS corpus not present");
            return;
        }
        let bytes = std::fs::read(DTS).unwrap();
        let mut bridge = AtmosBridge::new(false);
        let result = bridge.push_packet(RSlice::from_slice(&bytes), RInputTransport::Raw, 0);
        assert!(result.error_message.is_empty(), "{}", result.error_message);
        assert!(!result.frames.is_empty(), "no frames decoded");
        assert!(bridge.is_spatial());
        assert!(bridge.is_ready());

        let f = &result.frames[0];
        assert_eq!(f.channel_count, 6, "expected 5.1 bed");
        assert_eq!(f.sampling_frequency, 48_000);
        // DCA primary order for 3F2R is C,L,R,Ls,Rs then LFE.
        use bridge_api::RChannelLabel::*;
        let labels: Vec<_> = f.channel_labels.iter().copied().collect();
        assert_eq!(labels, vec![C, L, R, Ls, Rs, LFE]);
    }

    // End-to-end DTS-HD MA: feed the raw 7.1 dump and check it emits 8-channel
    // lossless bed frames. Skips when the (uncommitted) dump is absent.
    #[test]
    fn dtshd_raw_transport_emits_7_1_bed() {
        const DUMP: &str = "/home/user/dev/spatial-renderer/dumps/Ex.Machina.2014.dtsx.eng.dts";
        if !std::path::Path::new(DUMP).exists() {
            eprintln!("skipping: 7.1 dump not present");
            return;
        }
        // Feed ~2 MB — enough for many frames past the silent intro.
        let bytes = std::fs::read(DUMP).unwrap();
        let chunk = &bytes[..bytes.len().min(2_000_000)];
        let mut bridge = AtmosBridge::new(false);
        bridge.configure("input_codec".into(), "dts".into());
        let result = bridge.push_packet(RSlice::from_slice(chunk), RInputTransport::Raw, 0);
        assert!(result.error_message.is_empty(), "{}", result.error_message);
        assert!(!result.frames.is_empty(), "no HD frames decoded");
        assert!(bridge.is_spatial());

        // Find a fully-populated 7.1 frame (some early frames may be 5.1 before
        // the surround-back channels carry signal, but the bed is 8ch once XLL
        // emits the full hierarchy).
        let f = result
            .frames
            .iter()
            .find(|f| f.channel_count == 8)
            .expect("expected an 8-channel 7.1 frame");
        assert_eq!(f.sampling_frequency, 48_000);
        use bridge_api::RChannelLabel::*;
        let labels: Vec<_> = f.channel_labels.iter().copied().collect();
        // Active speakers ascending: C,L,R,Ls,Rs,LFE,Lsr,Rsr.
        assert_eq!(labels, vec![C, L, R, Ls, Rs, LFE, Lb, Rb]);
    }
}
