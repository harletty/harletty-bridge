//! The Dolby codec family: TrueHD (raw, and MAT over IEC 61937) and
//! E-AC-3 / AC-3 with their JOC objects (raw and IEC 61937).
//!
//! [`DolbyPipeline`] holds the whole family's state, decodes what a packet
//! completes, and answers the host's questions about the stream. Resetting
//! the whole bridge is the router's job: a path says when it is needed
//! ([`AfterPush`]).

use abi_stable::std_types::{RString, RVec};
use bridge_api::RPushResult;
use eac3::{CorePcmFrame, Extractor as Eac3RawExtractor, FrameType, ObjectPcmDecoder, PcmDecoder};
#[cfg(feature = "bridge-perf")]
use std::env;
#[cfg(feature = "bridge-perf")]
use std::time::Instant;
use truehd::process::decode::DecodedAccessUnit;
use truehd::process::{MAX_PRESENTATIONS, decode::Decoder, extract::Extractor, parse::Parser};
use truehd::structs::access_unit::AccessUnit;

use crate::ac3_native::NativeAc3Decoder;
use crate::eac3_pipeline::{
    DecodedDependent, Eac3IndependentOutcome, PendingEac3Dependent,
    build_legacy_ac3_core_failure_silence, decode_eac3_independent, decode_eac3_inspected,
    diagnose_eac3_frame, eac3_frame_carries_joc, inspect_eac3_frame, is_legacy_ac3_frame,
    is_temporary_eac3_silence_frame,
};
use crate::eac3_spdif::Eac3SpdifStream;
use crate::frame_builders::validate_frame_shape;
use crate::logging::{bridge_diag_log, bridge_log};
use crate::mat::MatStream;
use crate::metadata::OamdWarnings;
use crate::perf::PerfStats;
use crate::shared::{AfterPush, SharedState};
use crate::truehd_pipeline::{configure_parser, process_extractor_input, required_presentations};

/// The source family every Dolby stream reports
/// (`FormatBridge::source_family`).
pub const FAMILY_DOLBY: &str = "dolby";

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
    /// Non-JOC AC-3-core + dependent pairs emitted as plain channel beds.
    pub(crate) dependent_pair_channel_beds: u64,
    /// Dependents whose channels could not be overlaid onto the core.
    pub(crate) dependent_merge_failures: u64,
    /// Presentations whose JOC configuration declares a downmix the merged bed
    /// is not: a 5-channel configuration reached with dependents overlaid.
    pub(crate) joc_downmix_config_mismatch: u64,
    /// Standalone AC-3 cores (plain AC-3, no dependent) emitted as 5.1 beds.
    pub(crate) standalone_ac3_core_beds: u64,
    pub(crate) short_packet_silence_frames: u64,
    /// Dependent frames evicted because the pending queue hit its bound
    /// (their AC-3 cores kept failing to decode).
    pub(crate) dependent_frames_dropped: u64,
    pub(crate) last_ac3_core_decode_error: Option<String>,
    pub(crate) last_dependent_pair_error: Option<String>,
}

/// Upper bound on buffered dependent access units awaiting an AC-3 core
/// partner. In a healthy stream the queue never holds more than one entry;
/// it only grows while cores fail to decode, so keep a small window and drop
/// the oldest beyond it.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum DrcMode {
    #[default]
    Off,
    Standard,
    Heavy,
}

/// A presentation being assembled: the core, and the dependents that have
/// attached themselves to it so far.
///
/// An independent may be followed by up to eight dependents, and the JOC
/// payload rides in the last of them, so the group is collected rather than
/// resolved on the first arrival - taking the core away from the second
/// dependent would orphan it and lose exactly the payload that matters.
pub(crate) struct PendingEac3Presentation {
    pub(crate) core: PendingEac3Core,
    pub(crate) dependents: Vec<PendingEac3Dependent>,
}

/// ETSI TS 102 366 E.1.3.1.2 allows at most eight dependent substreams behind
/// one independent. More than that is a malformed group, not a longer one.
const MAX_EAC3_DEPENDENTS: usize = 8;

/// A presentation's core, held until its dependent arrives or the next
/// non-dependent access unit ends the presentation without one.
pub(crate) enum PendingEac3Core {
    /// A legacy AC-3 core. AC-3 carries no JOC, so this is only ever the
    /// channel half of a pair. The access unit is kept so a standalone flush
    /// can recover its DRC and dialnorm.
    LegacyAc3 {
        core: CorePcmFrame,
        access_unit: Vec<u8>,
    },
    /// An independent E-AC-3 frame that carried no JOC payload of its own.
    ///
    /// Boxed to keep `DolbyPipeline` pointer-sized per field, which
    /// `tests::atmos_bridge_stack_footprint_stays_small` holds it to.
    Independent(Box<eac3::PcmPushResult>),
}

/// Decoder state for the Dolby codecs: TrueHD (raw, and MAT over IEC 61937)
/// and E-AC-3 / AC-3 with their JOC objects (raw and IEC 61937).
///
/// Every decoder field is boxed on purpose. Their inline state is large (the
/// TrueHD `Decoder` alone is ~126 KiB, the whole set ~210 KiB), and a host may
/// well create the bridge on a thread with a small stack: mpv's macOS playback
/// thread is a plain `pthread_create` with no attributes, so 512 KiB. Held by
/// value, the struct is copied two to three times on the way to the heap
/// (`new()`'s locals → the return temporary → `RBox::new` inside
/// `FormatBridge_TO::from_value`) and overflows that stack before the first
/// packet is ever pushed.
///
/// Boxing keeps this struct, and the bridge holding it, pointer-sized per field, so the largest
/// transient is a single `Decoder` in a leaf frame. The cost is one pointer
/// hop per pipeline entry — never per sample — so hot loops are unaffected.
/// `tests::atmos_bridge_stack_footprint_stays_small` guards the invariant.
pub struct DolbyPipeline {
    // ── TrueHD pipeline ──────────────────────────────────────────────
    pub(crate) mat_stream: MatStream,
    pub(crate) extractor: Extractor,
    pub(crate) parser: Box<Parser>,
    /// The access unit every frame is parsed into. Kept from one to the next: a
    /// new one allocates the blocks of each substream and the sample rows of
    /// each block, to free them as soon as the frame is decoded.
    pub(crate) truehd_access_unit: Box<AccessUnit>,
    pub(crate) decoder: Box<Decoder>,
    /// The access unit the decoder writes its samples into. Kept from one to the
    /// next: a new one is 10 KiB to build and copy out for every 1/1200 s of
    /// audio, most of it rows the access unit does not use.
    pub(crate) truehd_decoded: Box<DecodedAccessUnit>,
    // ── E-AC3 pipeline ───────────────────────────────────────────────
    pub(crate) eac3_spdif: Eac3SpdifStream,
    /// Raw E-AC3 syncframe extractor (used by the `Raw` transport, e.g. mpv).
    pub(crate) eac3_raw_extractor: Eac3RawExtractor,
    pub(crate) eac3_pcm_decoder: Box<PcmDecoder>,
    /// Separate PCM decoder for the dependent substream of a non-JOC 7.1
    /// channel-extension pair (kept apart from the core decoder so their
    /// per-stream state never interferes).
    pub(crate) eac3_dependent_pcm_decoder: Box<PcmDecoder>,
    pub(crate) eac3_object_decoder: Box<ObjectPcmDecoder>,
    /// Whether the last independent E-AC-3 frame carried JOC its own channels
    /// satisfy, so the next one is decoded by the object decoder first. Only
    /// a guess at the decoder to try: `decode_eac3_independent` confirms it on
    /// every frame.
    pub(crate) eac3_expect_self_contained_joc: bool,
    pub(crate) ac3_decoder: Box<NativeAc3Decoder>,
    /// The core of a presentation whose dependent has not arrived yet.
    ///
    /// A dependent substream belongs to the independent it immediately follows,
    /// so at most one core is ever outstanding: the next non-dependent access
    /// unit ends the presentation whether a dependent came or not. Holding it
    /// as an `Option` rather than a queue is what makes a dependent that
    /// arrives with nothing in front of it an orphan to drop, instead of one to
    /// park until some later, unrelated core turns up.
    pub(crate) pending_eac3_core: Option<PendingEac3Presentation>,
    pub(crate) eac3_frame_count: u64,
    pub(crate) eac3_total_samples: u64,
    /// True when the most recent Dolby packet took the E-AC-3 path rather
    /// than TrueHD's.
    pub(crate) eac3_active: bool,
    pub(crate) eac3_diag_stats: Eac3DiagStats,
    // ── Shared by TrueHD and E-AC-3 ──────────────────────────────────
    pub(crate) presentation: u8,
    /// Current dialogue level from the last major sync.
    pub(crate) current_dialogue_level: Option<i8>,
    /// Substream info tracking for change detection (TrueHD only).
    pub(crate) current_substream_info: Option<u8>,
    pub(crate) current_extended_substream_info: Option<u8>,
    pub(crate) recovering_until_major_sync: bool,
    /// The DRC mode changed which presentations the TrueHD parser is to be asked
    /// for, and it has not been asked yet: it is at the next major sync.
    pub(crate) truehd_presentations_stale: bool,
    pub(crate) drc_mode: DrcMode,
    pub(crate) frame_count: u64,
    /// Fixed-channel labels of the active TrueHD spatial presentation
    /// (bed labels from the OAMD bed assignment, then `Object` fillers).
    pub(crate) truehd_spatial_labels:
        Option<abi_stable::std_types::RVec<bridge_api::RChannelLabel>>,
    /// TrueHD object metadata the bridge could not use, since the last reset.
    pub(crate) truehd_oamd_warnings: OamdWarnings,
    pub(crate) perf: PerfStats,
}

impl DolbyPipeline {
    pub fn new(shared: &SharedState) -> Self {
        // Default to presentation 3 (full Atmos/JOC); overridable via configure().
        let presentation = 3u8;

        // Boxed as they are built, never held by value: see `DolbyPipeline`.
        let mut parser = Box::new(Parser::default());
        let mut decoder = Box::new(Decoder::default());

        let fail_level = shared.fail_level();
        decoder.set_fail_level(fail_level);
        configure_parser(&mut parser, fail_level, presentation, DrcMode::default());

        let eac3_log_level = fail_level;
        let mut eac3_pcm = Box::new(PcmDecoder::new());
        eac3_pcm.set_debug_log_level(eac3_log_level);
        let mut eac3_dependent_pcm = Box::new(PcmDecoder::new());
        eac3_dependent_pcm.set_debug_log_level(eac3_log_level);
        let mut eac3_obj = Box::new(ObjectPcmDecoder::new());
        eac3_obj.set_debug_log_level(eac3_log_level);
        #[allow(unused_mut)]
        let mut dolby = Self {
            mat_stream: MatStream::default(),
            extractor: Extractor::default(),
            parser,
            truehd_access_unit: Box::default(),
            decoder,
            truehd_decoded: Box::default(),
            eac3_spdif: Eac3SpdifStream::default(),
            eac3_raw_extractor: Eac3RawExtractor::default(),
            eac3_pcm_decoder: eac3_pcm,
            eac3_dependent_pcm_decoder: eac3_dependent_pcm,
            eac3_object_decoder: eac3_obj,
            eac3_expect_self_contained_joc: false,
            ac3_decoder: Box::new(NativeAc3Decoder::default()),
            pending_eac3_core: None,
            eac3_frame_count: 0,
            eac3_total_samples: 0,
            eac3_active: false,
            eac3_diag_stats: Eac3DiagStats::default(),
            presentation,
            current_dialogue_level: None,
            current_substream_info: None,
            current_extended_substream_info: None,
            recovering_until_major_sync: false,
            truehd_presentations_stale: false,
            drc_mode: DrcMode::Off,
            frame_count: 0,
            truehd_spatial_labels: None,
            truehd_oamd_warnings: OamdWarnings::default(),
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
            dolby.perf.configure(enabled, interval);
        }

        dolby
    }

    /// Forget the stream (seek, sync loss), then re-apply the configuration
    /// to the fresh parser and decoders. The running counters stay.
    pub fn reset(&mut self, shared: &SharedState) {
        // TrueHD reset.
        self.mat_stream.reset();
        self.extractor = Extractor::default();
        // Assign through the boxes: the fresh state lands in the existing
        // allocations instead of being copied around the stack.
        *self.parser = Parser::default();
        *self.decoder = Decoder::default();

        // E-AC3 reset.
        self.eac3_spdif.reset();
        self.eac3_raw_extractor = Eac3RawExtractor::default();
        self.eac3_pcm_decoder.reset();
        self.eac3_dependent_pcm_decoder.reset();
        self.eac3_object_decoder.reset();
        self.ac3_decoder.reset();
        self.pending_eac3_core = None;
        self.eac3_frame_count = 0;
        self.eac3_active = false;

        // Re-apply configuration to new parser/decoder instances.
        let fail_level = shared.fail_level();
        self.decoder.set_fail_level(fail_level);
        self.eac3_pcm_decoder.set_debug_log_level(fail_level);
        self.eac3_object_decoder.set_debug_log_level(fail_level);
        configure_parser(
            &mut self.parser,
            fail_level,
            self.presentation,
            self.drc_mode,
        );
        self.truehd_presentations_stale = false;
        self.truehd_spatial_labels = None;
        self.truehd_oamd_warnings = OamdWarnings::default();
        self.recovering_until_major_sync = false;
    }

    /// Raw transport, TrueHD: access units as they come in the elementary
    /// stream.
    pub fn push_raw_truehd(
        &mut self,
        shared: &mut SharedState,
        data: &[u8],
        result: &mut RPushResult,
    ) -> AfterPush {
        self.eac3_active = false;
        process_extractor_input(self, shared, data, result)
    }

    /// Raw transport, E-AC-3 / AC-3: syncframes as they come in the
    /// elementary stream.
    pub fn push_raw_eac3(
        &mut self,
        shared: &mut SharedState,
        data: &[u8],
        result: &mut RPushResult,
    ) -> AfterPush {
        self.eac3_active = true;
        self.eac3_raw_extractor.push_bytes(data);
        self.drain_eac3_raw(shared, result)
    }

    /// The IEC 61937 data types this family decodes: TrueHD in MAT (0x16)
    /// and E-AC-3 (0x15).
    pub fn accepts_data_type(data_type: u8) -> bool {
        MatStream::accepts_data_type(data_type) || Eac3SpdifStream::accepts_data_type(data_type)
    }

    /// IEC 61937 transport: one burst payload of one of the data types
    /// [`Self::accepts_data_type`] takes.
    pub fn push_iec61937(
        &mut self,
        shared: &mut SharedState,
        data: &[u8],
        data_type: u8,
        result: &mut RPushResult,
    ) -> AfterPush {
        // ── TrueHD (data type 0x16) ───────────────────────────
        if MatStream::accepts_data_type(data_type) {
            self.eac3_active = false;

            #[cfg(feature = "bridge-perf")]
            let mat_started = Instant::now();
            #[cfg(feature = "bridge-perf")]
            self.perf.note_mat_packet(data.len());
            self.mat_stream.push_payload(data);
            let mut after = AfterPush::Continue;
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
                        after = process_extractor_input(self, shared, &chunk, result);
                        if after == AfterPush::ResetPipeline {
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
                        bridge_diag_log(log::Level::Warn, &msg);
                        result.did_reset = true;
                        if shared.strict {
                            result.error_message = msg.into();
                        }
                        return AfterPush::ResetPipeline;
                    }
                }
            }
            #[cfg(feature = "bridge-perf")]
            self.perf.record_mat(mat_started.elapsed());
            return after;
        }

        // ── E-AC3 (data type 0x15) ────────────────────────────
        self.eac3_active = true;
        self.eac3_spdif.push_payload(data);
        let mut temporary_silence_pushed = false;
        loop {
            match self.eac3_spdif.next_frame() {
                Ok(Some(frame)) => {
                    if self
                        .process_eac3_access_unit(
                            shared,
                            &frame,
                            result,
                            &mut temporary_silence_pushed,
                        )
                        .is_err()
                    {
                        return AfterPush::ResetPipeline;
                    }
                }
                Ok(None) => {
                    break;
                }
                Err(msg) => {
                    bridge_log!(log::Level::Warn, "eac3_error={msg}");
                    result.did_reset = true;
                    result.error_message = msg.into();
                    return AfterPush::ResetPipeline;
                }
            }
        }
        AfterPush::Continue
    }

    /// Count a raw-transport packet in the `bridge-perf` statistics, whichever
    /// family it goes to: they are the bridge's only timing profile.
    #[cfg(feature = "bridge-perf")]
    pub fn note_raw_packet(&mut self, len: usize) {
        self.perf.note_raw_packet(len);
    }

    /// The stream moved to another family: no Dolby codec is the current
    /// one until a Dolby packet says which.
    pub fn leave(&mut self) {
        self.eac3_active = false;
    }

    pub fn is_ready(&self) -> bool {
        self.frame_count > 0 || self.eac3_frame_count > 0
    }

    pub fn has_objects(&self) -> bool {
        if self.eac3_active {
            // E-AC3/AC-3 is spatial only when it actually carries JOC object
            // payloads (Atmos). `frames_seen` counts every decoded frame, so it
            // is true for plain AC-3 / E-AC3 multichannel too — gating on it
            // would wrongly mark non-object streams as spatial, and host mode
            // could then never hand them back to the native decoder (it would
            // render silence instead). `joc_frames` only increments on frames
            // with a JOC payload, so it is the correct "has real objects" probe.
            self.eac3_diag_stats.joc_frames > 0
        } else {
            // Presentations 0–(MAX-2) are pure downmixes; the top presentation carries objects.
            self.presentation >= (MAX_PRESENTATIONS as u8) - 1
        }
    }

    /// The host's configuration keys this family answers: `presentation`
    /// (TrueHD), and the `bridge-perf` keys. `None` for any other key.
    pub fn configure(&mut self, key: &str, value: &str) -> Option<bool> {
        Some(match key {
            "presentation" => {
                let p = match value {
                    "best" => (MAX_PRESENTATIONS as u8) - 1,
                    s => match s.parse::<u8>() {
                        Ok(p) if p < MAX_PRESENTATIONS as u8 => p,
                        Ok(p) => {
                            bridge_log!(
                                log::Level::Warn,
                                "atmos-bridge: presentation {p} out of range (0–{})",
                                MAX_PRESENTATIONS - 1
                            );
                            return Some(false);
                        }
                        Err(_) => {
                            bridge_log!(
                                log::Level::Warn,
                                "atmos-bridge: cannot parse presentation value {:?}",
                                s
                            );
                            return Some(false);
                        }
                    },
                };
                self.presentation = p;
                self.parser
                    .set_required_presentations(&required_presentations(p, self.drc_mode));
                self.truehd_presentations_stale = false;
                bridge_log!(log::Level::Debug, "atmos-bridge: presentation set to {p}");
                true
            }
            #[cfg(feature = "bridge-perf")]
            "perf_profile" => {
                let enabled = matches!(value, "1" | "true" | "on" | "yes");
                let report_every = self.perf.configure_profile(enabled);
                eprintln!(
                    "harletty-bridge perf profiling {} (report_every_frames={})",
                    if enabled { "enabled" } else { "disabled" },
                    report_every
                );
                true
            }
            #[cfg(feature = "bridge-perf")]
            "perf_report_every" => match value.parse::<u64>() {
                Ok(interval) if interval > 0 => {
                    self.perf.configure_report_every(interval);
                    eprintln!(
                        "harletty-bridge perf reporting interval set to {} frames",
                        interval
                    );
                    true
                }
                _ => {
                    bridge_log!(
                        log::Level::Warn,
                        "atmos-bridge: invalid perf_report_every value {:?}",
                        value
                    );
                    false
                }
            },
            #[cfg(not(feature = "bridge-perf"))]
            "perf_profile" | "perf_report_every" => false,
            _ => return None,
        })
    }

    pub fn supported_drc_modes() -> RVec<RString> {
        vec![
            RString::from("Off"),
            RString::from("standard/line"),
            RString::from("heavy/RF"),
        ]
        .into()
    }

    pub fn set_drc_mode(&mut self, mode: &str) -> bool {
        let new_mode = match mode {
            "Off" => DrcMode::Off,
            "Standard" | "Line" | "standard/line" => DrcMode::Standard,
            "Heavy" | "RF" | "heavy/RF" => DrcMode::Heavy,
            _ => {
                bridge_diag_log(
                    log::Level::Warn,
                    &format!("[harletty][drc] unknown drc_mode {:?}", mode),
                );
                return false;
            }
        };
        bridge_log!(
            log::Level::Info,
            "[harletty][drc] set_drc_mode {:?} -> {:?}",
            self.drc_mode,
            new_mode
        );
        if required_presentations(self.presentation, new_mode)
            != required_presentations(self.presentation, self.drc_mode)
        {
            self.truehd_presentations_stale = true;
        }
        self.drc_mode = new_mode;
        true
    }

    /// The carrier, then the spatial layer decoded over it.
    pub fn source_label(&self, label: &mut String) {
        if self.eac3_active {
            let stats = &self.eac3_diag_stats;
            label.push_str(if stats.total_frames > stats.legacy_ac3_frames {
                "Dolby Digital Plus"
            } else {
                "Dolby Digital"
            });
            if stats.joc_frames > 0 {
                label.push_str(" + Dolby Atmos");
            }
        } else {
            label.push_str("Dolby TrueHD");
            if self.truehd_spatial_labels.is_some() {
                label.push_str(" + Dolby Atmos");
            }
        }
    }

    /// Resolve the pending presentation, applying the pipeline's failure
    /// policy: strict mode surfaces the error and resets, as every other decode
    /// failure here does.
    fn finish_presentation(
        &mut self,
        shared: &mut SharedState,
        result: &mut RPushResult,
    ) -> Result<(), ()> {
        match self.resolve_pending_presentation(shared, result) {
            Ok(()) => Ok(()),
            Err(msg) => {
                bridge_diag_log(log::Level::Warn, &msg);
                result.did_reset = true;
                result.error_message = msg.as_str().into();
                Err(())
            }
        }
    }

    /// Resolve the presentation in hand into exactly one emitted frame.
    ///
    /// Any non-dependent access unit ends the group, because a dependent
    /// belongs to the unit it immediately follows. The dependents are merged
    /// onto the core in bitstream order, and the last of them decides what
    /// comes out: a JOC payload there means objects reconstructed from the
    /// merged bed, and its absence means the bed itself.
    ///
    /// Returns the decode error rather than swallowing it, so strict mode can
    /// reset the pipeline; the caller decides whether to fall back.
    fn resolve_pending_presentation(
        &mut self,
        shared: &mut SharedState,
        result: &mut RPushResult,
    ) -> Result<(), String> {
        let Some(pending) = self.pending_eac3_core.take() else {
            return Ok(());
        };
        let PendingEac3Presentation { core, dependents } = pending;

        // A presentation with no JOC anywhere in it is a gap in object
        // carriage, and the object decoder cannot see one from the inside: its
        // sequence counter only advances on frames that carry a payload, so the
        // next JOC frame would otherwise interpolate away from a matrix
        // belonging to whatever played before the gap.
        let joc_dependent = dependents
            .last()
            .filter(|dependent| eac3_frame_carries_joc(&dependent.info));
        if joc_dependent.is_none() {
            self.eac3_object_decoder.note_non_joc_presentation();
        }

        // Nothing attached: the core stands on its own.
        if dependents.is_empty() {
            match core {
                PendingEac3Core::LegacyAc3 { core, access_unit } => {
                    result
                        .frames
                        .push(crate::eac3_pipeline::build_standalone_ac3_core_frame(
                            self,
                            &core,
                            &access_unit,
                        ));
                }
                PendingEac3Core::Independent(push) => {
                    result
                        .frames
                        .push(crate::eac3_pipeline::build_buffered_core_frame(
                            self, shared, &push,
                        ));
                }
            }
            return Ok(());
        }

        self.eac3_diag_stats.dependent_pair_attempts += 1;
        let core_pcm = match core {
            PendingEac3Core::LegacyAc3 { core, .. } => core,
            PendingEac3Core::Independent(push) => push.pcm,
        };
        match crate::eac3_pipeline::resolve_eac3_presentation(self, shared, core_pcm, &dependents) {
            Ok(frame) => {
                result.frames.push(frame);
                Ok(())
            }
            Err(err) => {
                self.eac3_diag_stats.dependent_pair_failures += 1;
                self.eac3_diag_stats.last_dependent_pair_error = Some(err.clone());
                Err(err)
            }
        }
    }

    /// Process one extracted E-AC3 access unit, shared by the IEC 61937 and raw
    /// transports. Returns `Err(())` on a fatal decode error, in which case the
    /// pipeline has been reset and `result` already carries the error — the
    /// caller must stop draining and return.
    pub(crate) fn process_eac3_access_unit(
        &mut self,
        shared: &mut SharedState,
        frame: &[u8],
        result: &mut RPushResult,
        temporary_silence_pushed: &mut bool,
    ) -> Result<(), ()> {
        self.eac3_frame_count += 1;
        // A dependent substream belongs to the access unit it immediately
        // follows, so any other kind of unit ends the group in hand. Only a
        // dependent is looked into here. One the presentation in hand can
        // take is decoded at once and held with its channels and what the
        // decode found - an inspection used to come first, and the merge then
        // walked the same blocks again. One the decode rejects, or that has
        // no presentation to join, is inspected as before: it is held (or
        // dropped) on what the inspection found, and one whose inspection
        // fails is handled as the frame of unknown type it then is.
        let header = eac3::parse_header(frame).ok();
        let is_legacy = is_legacy_ac3_frame(frame);
        let is_dependent =
            header.is_some_and(|header| header.stream_type == eac3::StreamType::Dependent);
        if is_dependent
            && !is_legacy
            && self
                .pending_eac3_core
                .as_ref()
                .is_some_and(|pending| pending.dependents.len() < MAX_EAC3_DEPENDENTS)
        {
            match self.eac3_dependent_pcm_decoder.push_access_unit(frame) {
                Ok(push) if push.info.frame_type == FrameType::Dependent => {
                    return self.push_eac3_dependent(
                        shared,
                        frame,
                        push.info,
                        DecodedDependent::Channels(push.pcm),
                        result,
                    );
                }
                _ => {}
            }
        }
        let inspection = if is_legacy || is_dependent {
            match inspect_eac3_frame(frame) {
                Ok(info) if info.frame_type == FrameType::Dependent => {
                    // Either the decode above rejected it, or it has no
                    // presentation to join and is dropped before its
                    // channels are asked for.
                    return self.push_eac3_dependent(
                        shared,
                        frame,
                        info,
                        DecodedDependent::Failed,
                        result,
                    );
                }
                inspection => Some(inspection),
            }
        } else {
            None
        };

        if let Err(()) = self.finish_presentation(shared, result) {
            return Err(());
        }

        let decode_result = if is_legacy {
            let inspection = inspection.unwrap_or_else(|| inspect_eac3_frame(frame));
            match self.ac3_decoder.decode_frame(frame) {
                Ok(core) => {
                    diagnose_eac3_frame(self, frame, &inspection);
                    self.eac3_diag_stats.ac3_core_decoded += 1;
                    self.pending_eac3_core = Some(PendingEac3Presentation {
                        core: PendingEac3Core::LegacyAc3 {
                            core,
                            access_unit: frame.to_vec(),
                        },
                        dependents: Vec::new(),
                    });
                    return Ok(());
                }
                Err(err) => {
                    diagnose_eac3_frame(self, frame, &inspection);
                    self.eac3_diag_stats.ac3_core_decode_failures += 1;
                    self.eac3_diag_stats.last_ac3_core_decode_error = Some(err.clone());
                    bridge_log!(
                        log::Level::Warn,
                        "ac3_core_decode_failed index={} error={}",
                        self.eac3_frame_count,
                        err
                    );
                    // Stand in one frame of silence for the core and stop here.
                    // This used to fall through to the E-AC-3 decoders, which
                    // can only reject an AC-3 syncframe ("not-eac3") and then
                    // substituted silence labelled in WAV order (L R C LFE Ls
                    // Rs) — a different channel list from the decoded frames
                    // (fullband order, LFE last), so the renderer replanned
                    // its bed on every dropped frame and Studio reordered its
                    // virtual speakers. The silence now carries the labels
                    // the core decoder would have produced for this header.
                    build_legacy_ac3_core_failure_silence(self, frame)
                        .ok_or_else(|| format!("AC-3 core decode error: {err}"))
                }
            }
        } else {
            // A plain independent core might be the first half of a group, and
            // nothing in it says whether a dependent follows: it is held until
            // the next access unit answers that. A converted-AC-3 frame cannot
            // carry dependents (ETSI allows it none), and an independent whose
            // JOC payload its own channels satisfy is a complete presentation:
            // both are emitted at once, which keeps the common 5.1-core Atmos
            // stream free of the access unit of latency buffering would add.
            // An independent whose JOC declares a wider downmix than it carries
            // is held like a plain core, so its dependents reach the bed.
            let outcome = match &inspection {
                Some(inspection) => decode_eac3_inspected(self, shared, frame, inspection),
                None => {
                    let can_carry_dependents = header
                        .is_some_and(|header| header.stream_type == eac3::StreamType::Independent);
                    decode_eac3_independent(self, shared, frame, can_carry_dependents)
                }
            };
            match outcome {
                Eac3IndependentOutcome::Hold(push) => {
                    self.pending_eac3_core = Some(PendingEac3Presentation {
                        core: PendingEac3Core::Independent(Box::new(push)),
                        dependents: Vec::new(),
                    });
                    return Ok(());
                }
                Eac3IndependentOutcome::Emit(decoded) => decoded,
            }
        };

        match decode_result {
            Ok(decoded_frame) => {
                if let Err(reason) = validate_frame_shape(&decoded_frame) {
                    bridge_log!(
                        log::Level::Warn,
                        "eac3_frame_rejected index={} reason={} sr={} samples={} ch={} pcm_len={}",
                        self.eac3_frame_count,
                        reason,
                        decoded_frame.sampling_frequency,
                        decoded_frame.sample_count,
                        decoded_frame.channel_count,
                        decoded_frame.pcm.len()
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
                bridge_diag_log(log::Level::Warn, &msg);
                result.did_reset = true;
                result.error_message = msg.as_str().into();
                Err(())
            }
        }
    }

    /// Attach a dependent access unit to the presentation in hand, resolving
    /// the presentation once its group is complete.
    fn push_eac3_dependent(
        &mut self,
        shared: &mut SharedState,
        frame: &[u8],
        info: eac3::AccessUnitInfo,
        decoded: DecodedDependent,
        result: &mut RPushResult,
    ) -> Result<(), ()> {
        let Some(pending) = self.pending_eac3_core.as_mut() else {
            // Nothing in front of it: this dependent belongs to nothing. It
            // used to be parked for some later, unrelated core to claim,
            // which put one programme's extension channels on another's bed.
            self.eac3_diag_stats.dependent_frames_dropped += 1;
            bridge_diag_log(
                log::Level::Warn,
                "eac3_orphan_dependent no core precedes this dependent access unit",
            );
            return Ok(());
        };
        if pending.dependents.len() >= MAX_EAC3_DEPENDENTS {
            // More than the eight ETSI allows behind one independent: the
            // group is malformed rather than longer, so resolve what is
            // valid and drop the excess instead of growing without bound.
            self.eac3_diag_stats.dependent_frames_dropped += 1;
            bridge_diag_log(
                log::Level::Warn,
                "eac3_dependent_group_overflow more than eight dependents behind one independent",
            );
            return self.finish_presentation(shared, result);
        }
        let carries_joc = eac3_frame_carries_joc(&info);
        pending.dependents.push(PendingEac3Dependent {
            access_unit: frame.to_vec(),
            info,
            decoded,
        });
        // The JOC payload rides in the last dependent, so one that carries
        // it ends the group with no need to wait for the next access unit.
        if carries_joc {
            return self.finish_presentation(shared, result);
        }
        Ok(())
    }

    /// Drain all complete E-AC3 access units currently buffered in the raw
    /// extractor, rendering each through [`Self::process_eac3_access_unit`].
    fn drain_eac3_raw(&mut self, shared: &mut SharedState, result: &mut RPushResult) -> AfterPush {
        let mut temporary_silence_pushed = false;
        loop {
            match self.eac3_raw_extractor.next_frame() {
                Ok(Some(frame)) => {
                    if self
                        .process_eac3_access_unit(
                            shared,
                            frame.as_bytes(),
                            result,
                            &mut temporary_silence_pushed,
                        )
                        .is_err()
                    {
                        return AfterPush::ResetPipeline;
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    let msg = format!("eac3_raw_extract_error={err:?}");
                    bridge_diag_log(log::Level::Warn, &msg);
                    result.did_reset = true;
                    result.error_message = msg.into();
                    return AfterPush::ResetPipeline;
                }
            }
        }
        AfterPush::Continue
    }
}

/// The Dolby paths as the bridge drives them, for this crate's tests: the
/// same `dolby` and `shared` fields, the raw codec sniffed and locked the way
/// the bridge does for a Dolby stream, and the whole pipeline reset when a
/// path asks.
#[cfg(test)]
pub(crate) mod test_bridge {
    use super::*;
    use abi_stable::std_types::{RSlice, RStr};
    use bridge_api::RInputTransport;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum RawCodec {
        TrueHd,
        Eac3,
    }

    pub(crate) struct TestBridge {
        pub(crate) dolby: DolbyPipeline,
        pub(crate) shared: SharedState,
        forced: Option<RawCodec>,
        locked: Option<RawCodec>,
    }

    impl TestBridge {
        pub(crate) fn new(strict: bool) -> Self {
            let shared = SharedState::new(strict);
            Self {
                dolby: DolbyPipeline::new(&shared),
                shared,
                forced: None,
                locked: None,
            }
        }

        fn raw_codec(&mut self, data: &[u8]) -> RawCodec {
            if let Some(codec) = self.locked.or(self.forced) {
                self.locked = Some(codec);
                return codec;
            }
            if data.len() >= 8 && data[4..8] == [0xF8, 0x72, 0x6F, 0xBA] {
                self.locked = Some(RawCodec::TrueHd);
                return RawCodec::TrueHd;
            }
            if data.len() >= 2 && (data[..2] == [0x0B, 0x77] || data[..2] == [0x77, 0x0B]) {
                self.locked = Some(RawCodec::Eac3);
                return RawCodec::Eac3;
            }
            RawCodec::TrueHd
        }

        pub(crate) fn push_packet(
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
            let data = data.as_slice();
            let after = match transport {
                RInputTransport::Raw => match self.raw_codec(data) {
                    RawCodec::TrueHd => {
                        self.dolby
                            .push_raw_truehd(&mut self.shared, data, &mut result)
                    }
                    RawCodec::Eac3 => self
                        .dolby
                        .push_raw_eac3(&mut self.shared, data, &mut result),
                },
                RInputTransport::Iec61937 => {
                    assert!(DolbyPipeline::accepts_data_type(data_type));
                    self.dolby
                        .push_iec61937(&mut self.shared, data, data_type, &mut result)
                }
            };
            if after == AfterPush::ResetPipeline {
                self.dolby.reset(&self.shared);
                self.shared.declared_object_channels = None;
                self.locked = None;
            }
            result
        }

        pub(crate) fn configure(&mut self, key: RStr<'_>, value: RStr<'_>) -> bool {
            if key.as_str() == "input_codec" {
                self.forced = match value.as_str() {
                    "truehd" => Some(RawCodec::TrueHd),
                    "eac3" => Some(RawCodec::Eac3),
                    other => panic!("the test bridge takes no input_codec {other:?}"),
                };
                self.locked = None;
                return true;
            }
            self.dolby
                .configure(key.as_str(), value.as_str())
                .unwrap_or(false)
        }

        pub(crate) fn set_drc_mode(&mut self, mode: RStr<'_>) -> bool {
            self.dolby.set_drc_mode(mode.as_str())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_bridge::TestBridge;
    use super::*;
    use crate::dolby::DrcMode;
    use abi_stable::std_types::RSlice;
    use bridge_api::{RChannelLabel, RInputTransport};

    /// The label names the carrier and the spatial layer decoded over it.
    #[test]
    fn source_label_names_the_carrier_and_its_spatial_layer() {
        let mut dolby = DolbyPipeline::new(&SharedState::new(false));
        let label = |dolby: &DolbyPipeline| {
            let mut label = String::new();
            dolby.source_label(&mut label);
            label
        };
        dolby.eac3_active = true;
        dolby.eac3_diag_stats.total_frames = 1;
        dolby.eac3_diag_stats.legacy_ac3_frames = 1;
        assert_eq!(label(&dolby), "Dolby Digital");
        dolby.eac3_diag_stats.total_frames = 2;
        assert_eq!(label(&dolby), "Dolby Digital Plus");
        dolby.eac3_diag_stats.joc_frames = 1;
        assert_eq!(label(&dolby), "Dolby Digital Plus + Dolby Atmos");

        dolby.eac3_active = false;
        assert_eq!(label(&dolby), "Dolby TrueHD");
        dolby.truehd_spatial_labels = Some(RVec::new());
        assert_eq!(label(&dolby), "Dolby TrueHD + Dolby Atmos");
    }

    #[test]
    fn failed_legacy_ac3_core_becomes_silence_in_decoder_channel_order() {
        // 44.1 kHz frmsizecod=29 header (1672-byte frame) handed over two
        // bytes short: the core decoder rejects it, and the access unit must
        // still advance the stream by one frame of silence whose channel list
        // is the one decoded frames carry (fullband order, then LFE) — not a
        // second, differently ordered list that would make the renderer
        // replan the bed twice around every dropped frame.
        let mut frame = vec![0u8; 1670];
        frame[..7].copy_from_slice(&[0x0B, 0x77, 0x00, 0x00, 0x5D, 0x40, 0xE1]);
        let mut bridge = TestBridge::new(false);
        let mut result = RPushResult {
            frames: RVec::new(),
            error_message: RString::new(),
            did_reset: false,
        };
        let mut temporary_silence_pushed = false;

        bridge
            .dolby
            .process_eac3_access_unit(
                &mut bridge.shared,
                &frame,
                &mut result,
                &mut temporary_silence_pushed,
            )
            .expect("a failed core is not a pipeline error");

        assert!(result.error_message.is_empty(), "{}", result.error_message);
        assert!(!result.did_reset);
        assert_eq!(bridge.dolby.eac3_diag_stats.ac3_core_decode_failures, 1);
        assert_eq!(bridge.dolby.eac3_diag_stats.legacy_ac3_frames, 1);
        assert_eq!(bridge.dolby.eac3_total_samples, 1536);
        assert_eq!(result.frames.len(), 1);
        let silence = &result.frames[0];
        assert_eq!(silence.sampling_frequency, 44_100);
        assert_eq!(silence.sample_count, 1536);
        assert_eq!(silence.channel_count, 6);
        assert_eq!(
            silence.channel_labels.as_slice(),
            &[
                RChannelLabel::L,
                RChannelLabel::C,
                RChannelLabel::R,
                RChannelLabel::Ls,
                RChannelLabel::Rs,
                RChannelLabel::LFE,
            ]
        );
        assert_eq!(silence.pcm.len(), 1536 * 6);
        assert!(silence.pcm.iter().all(|sample| *sample == 0));
        assert!(silence.metadata.is_empty());
    }

    /// The presentations asked of the TrueHD parser follow the DRC mode, and a
    /// mode set in mid-stream reaches the parser at the next major sync, where
    /// every substream can be taken up, with the frames a bridge in that mode
    /// from the start hands out.
    #[test]
    fn truehd_presentations_follow_the_drc_mode_at_a_major_sync() {
        use crate::truehd_pipeline::required_presentations;

        // The DRC log asks for every presentation whatever the mode.
        if crate::logging::drc_diag_log_enabled() {
            return;
        }

        assert_eq!(
            required_presentations(2, DrcMode::Off),
            [false, false, true, false]
        );
        assert_eq!(
            required_presentations(2, DrcMode::Standard),
            [false, false, true, false]
        );
        assert_eq!(
            required_presentations(2, DrcMode::Heavy),
            [true, true, true, false]
        );

        // One major sync and one access unit after it, four times over.
        let unit = truehd::process::EXAMPLE_DATA;
        let push = |bridge: &mut TestBridge, copies: usize| {
            let bytes = unit.repeat(copies);
            let result = bridge.push_packet(RSlice::from_slice(&bytes), RInputTransport::Raw, 0);

            assert!(result.error_message.is_empty(), "{}", result.error_message);
            assert!(!result.did_reset);
            result
                .frames
                .into_iter()
                .map(|f| (f.pcm.to_vec(), f.drc_gain.to_bits(), f.drc_ramp_duration))
                .collect::<Vec<_>>()
        };

        let mut heavy = TestBridge::new(false);
        heavy.configure("input_codec".into(), "truehd".into());
        assert!(heavy.set_drc_mode("heavy/RF".into()));
        let mut expected = push(&mut heavy, 1);
        expected.extend(push(&mut heavy, 3));
        assert!(!heavy.dolby.truehd_presentations_stale);

        let mut bridge = TestBridge::new(false);
        bridge.configure("input_codec".into(), "truehd".into());
        let mut frames = push(&mut bridge, 1);
        assert!(!bridge.dolby.truehd_presentations_stale);

        // Nothing is asked of the parser between two major syncs.
        assert!(bridge.set_drc_mode("standard/line".into()));
        assert!(!bridge.dolby.truehd_presentations_stale);
        assert!(bridge.set_drc_mode("heavy/RF".into()));
        assert!(bridge.dolby.truehd_presentations_stale);

        frames.extend(push(&mut bridge, 3));
        assert!(!bridge.dolby.truehd_presentations_stale);

        assert!(frames.len() >= 6, "{} frames", frames.len());
        assert_eq!(frames.len(), expected.len());
        for (i, (frame, expected)) in frames.iter().zip(&expected).enumerate() {
            assert_eq!(frame.0, expected.0, "samples of frame {i}");
        }
        // From the major sync after the mode was set, the gains too.
        assert_eq!(frames[4..], expected[4..]);
    }
}
