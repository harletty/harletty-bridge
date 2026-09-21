use abi_stable::std_types::{RSlice, RStr, RString, RVec};
use bridge_api::{
    FormatBridge, RChannelPose, RCoordinateFormat, RInputTransport, RPushResult,
    RVbapCartesianDefaults, RVbapTableMode,
};
use eac3::{CorePcmFrame, Extractor as Eac3RawExtractor, ObjectPcmDecoder, PcmDecoder};
#[cfg(feature = "bridge-perf")]
use std::env;
#[cfg(feature = "bridge-perf")]
use std::time::Instant;
use truehd::process::{MAX_PRESENTATIONS, decode::Decoder, extract::Extractor, parse::Parser};

use crate::ac3_native::NativeAc3Decoder;
use crate::auro_pipeline::DtsAuroState;
use crate::dts_pipeline::{DtsFoldConfig, DtsXState};
use crate::eac3_pipeline::{
    build_legacy_ac3_core_failure_silence, diagnose_eac3_frame, eac3_frame_can_carry_dependents,
    eac3_frame_carries_joc, eac3_frame_carries_self_contained_joc, is_dependent_eac3_frame,
    is_legacy_ac3_frame, is_temporary_eac3_silence_frame, process_eac3_frame,
};
use crate::eac3_spdif::Eac3SpdifStream;
use crate::frame_builders::PcmStats;
use crate::logging::bridge_diag_log;
use crate::mat::MatStream;
use crate::perf::PerfStats;
use crate::truehd_pipeline::{configure_parser, process_extractor_input};

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

/// Codec carried by a [`RInputTransport::Raw`] packet, which (unlike the IEC
/// 61937 transport) has no `data_type` to disambiguate TrueHD from E-AC3.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RawCodec {
    TrueHd,
    Eac3,
    Dts,
}

/// Which DTS carrier the frames come in: what the demux found after the
/// core. Named the way FFmpeg names the profiles, so a host's track
/// information reads the same on either decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum DtsProfile {
    /// A core frame alone (DTS, DTS-ES, DTS 96/24).
    #[default]
    Core,
    /// An extension substream without a lossless asset — DTS-HD High
    /// Resolution Audio and the other lossy extensions, of which only the
    /// core is decoded.
    Hd,
    /// An extension substream with a lossless asset: DTS-HD Master Audio.
    Ma,
}

/// Best-effort codec detection on a raw access unit, used when the host did not
/// declare the codec via `configure("input_codec", …)`. Checks the most
/// specific pattern first: the TrueHD major-sync word `0xF8726FBA` at offset 4,
/// then the E-AC3/AC-3 sync word `0x0B77` at offset 0 (incl. byte-swapped).
fn sniff_raw_codec(data: &[u8]) -> Option<RawCodec> {
    if data.len() >= 8 && data[4] == 0xF8 && data[5] == 0x72 && data[6] == 0x6F && data[7] == 0xBA {
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

/// Decoder state for every supported format, owned by one bridge instance.
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
/// Boxing keeps `AtmosBridge` itself pointer-sized per field, so the largest
/// transient is a single `Decoder` in a leaf frame. The cost is one pointer
/// hop per pipeline entry — never per sample — so hot loops are unaffected.
/// `tests::atmos_bridge_stack_footprint_stays_small` guards the invariant.
/// A presentation being assembled: the core, and the dependents that have
/// attached themselves to it so far.
///
/// An independent may be followed by up to eight dependents, and the JOC
/// payload rides in the last of them, so the group is collected rather than
/// resolved on the first arrival - taking the core away from the second
/// dependent would orphan it and lose exactly the payload that matters.
pub(crate) struct PendingEac3Presentation {
    pub(crate) core: PendingEac3Core,
    pub(crate) dependents: Vec<Vec<u8>>,
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
    /// Boxed to keep `AtmosBridge` pointer-sized per field, which
    /// `tests::atmos_bridge_stack_footprint_stays_small` holds it to.
    Independent(Box<eac3::PcmPushResult>),
}

pub(crate) struct AtmosBridge {
    // ── TrueHD pipeline ──────────────────────────────────────────────
    pub(crate) mat_stream: MatStream,
    pub(crate) extractor: Extractor,
    pub(crate) parser: Box<Parser>,
    pub(crate) decoder: Box<Decoder>,
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
    pub(crate) dts_decoder: Box<dca::PcmDecoder>,
    /// DTS-HD Master Audio lossless (5.1/7.1) decoder.
    pub(crate) dts_hd_decoder: Box<dca::HdDecoder>,
    pub(crate) dts_frame_count: u64,
    /// DTS:X extension state: latched presentation shape, last readable
    /// metadata, announced object positions. Cleared with the pipeline.
    pub(crate) dts_x: DtsXState,
    /// True when the most recent `push_packet` used the DTS path.
    pub(crate) dts_active: bool,
    pub(crate) dts_fold_config: DtsFoldConfig,
    /// Live stream fact: the latest DTS frame emitted object channels. Set
    /// from what the frame presented rather than from its profile, so every
    /// object-bearing presentation reaches it by the same route.
    pub(crate) dts_objects_active: bool,
    /// The carrier of the latest DTS frame, for the source label.
    pub(crate) dts_profile: DtsProfile,
    /// Auro-3D detection and unfolding over the lossless DTS-HD output.
    /// Holds the first frames back until the carrier question is settled.
    pub(crate) dts_auro: DtsAuroState,
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
    /// Last object↔channel declaration emitted (sparse re-emission on change
    /// and after reset). Shared by the TrueHD and E-AC3 metadata paths.
    pub(crate) declared_object_channels:
        Option<abi_stable::std_types::RVec<bridge_api::RObjectChannel>>,
    /// Fixed-channel labels of the active TrueHD spatial presentation
    /// (bed labels from the OAMD bed assignment, then `Object` fillers).
    pub(crate) truehd_spatial_labels:
        Option<abi_stable::std_types::RVec<bridge_api::RChannelLabel>>,
    pub(crate) perf: PerfStats,
}

impl AtmosBridge {
    pub(crate) fn new(strict: bool) -> Self {
        // Default to presentation 3 (full Atmos/JOC); overridable via configure().
        let presentation = 3u8;

        // Boxed as they are built, never held by value: see `AtmosBridge`.
        let mut parser = Box::new(Parser::default());
        let mut decoder = Box::new(Decoder::default());

        let fail_level = if strict {
            log::Level::Warn
        } else {
            log::Level::Error
        };
        decoder.set_fail_level(fail_level);
        configure_parser(&mut parser, fail_level, presentation);

        let eac3_log_level = if strict {
            log::Level::Warn
        } else {
            log::Level::Error
        };
        let mut eac3_pcm = Box::new(PcmDecoder::new());
        eac3_pcm.set_debug_log_level(eac3_log_level);
        let mut eac3_dependent_pcm = Box::new(PcmDecoder::new());
        eac3_dependent_pcm.set_debug_log_level(eac3_log_level);
        let mut eac3_obj = Box::new(ObjectPcmDecoder::new());
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
            eac3_dependent_pcm_decoder: eac3_dependent_pcm,
            eac3_object_decoder: eac3_obj,
            ac3_decoder: Box::new(NativeAc3Decoder::default()),
            pending_eac3_core: None,
            eac3_frame_count: 0,
            eac3_total_samples: 0,
            eac3_active: false,
            forced_raw_codec: None,
            raw_codec: None,
            eac3_diag_stats: Eac3DiagStats::default(),
            dts_buf: Vec::new(),
            dts_decoder: Box::new(dca::PcmDecoder::new()),
            dts_hd_decoder: Box::new(dca::HdDecoder::new()),
            dts_frame_count: 0,
            dts_x: DtsXState::default(),
            dts_active: false,
            dts_fold_config: DtsFoldConfig::from_env(),
            dts_objects_active: false,
            dts_profile: DtsProfile::default(),
            dts_auro: DtsAuroState::default(),
            presentation,
            strict,
            total_samples: 0,
            current_dialogue_level: None,
            current_substream_info: None,
            current_extended_substream_info: None,
            recovering_until_major_sync: false,
            drc_mode: DrcMode::Off,
            frame_count: 0,
            declared_object_channels: None,
            truehd_spatial_labels: None,
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

        // DTS reset.
        self.dts_buf.clear();
        self.dts_decoder.reset();
        self.dts_hd_decoder.reset();
        self.dts_frame_count = 0;
        self.dts_x = DtsXState::default();
        self.dts_active = false;
        self.dts_objects_active = false;
        self.dts_profile = DtsProfile::default();
        self.dts_auro.reset();
        // Re-sniff after reset, but keep any host-declared codec.
        self.raw_codec = None;

        // Re-apply configuration to new parser/decoder instances.
        let fail_level = if self.strict {
            log::Level::Warn
        } else {
            log::Level::Error
        };
        self.decoder.set_fail_level(fail_level);
        self.eac3_pcm_decoder.set_debug_log_level(fail_level);
        self.eac3_object_decoder.set_debug_log_level(fail_level);
        configure_parser(&mut self.parser, fail_level, self.presentation);
        self.declared_object_channels = None;
        self.truehd_spatial_labels = None;
        self.recovering_until_major_sync = false;
    }

    /// Resolve the pending presentation, applying the pipeline's failure
    /// policy: strict mode surfaces the error and resets, as every other decode
    /// failure here does.
    fn finish_presentation(&mut self, result: &mut RPushResult) -> Result<(), ()> {
        match self.resolve_pending_presentation(result) {
            Ok(()) => Ok(()),
            Err(msg) => {
                log::warn!("{msg}");
                self.reset_pipeline();
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
    fn resolve_pending_presentation(&mut self, result: &mut RPushResult) -> Result<(), String> {
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
            .filter(|dependent| eac3_frame_carries_joc(dependent));
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
                        .push(crate::eac3_pipeline::build_buffered_core_frame(self, &push));
                }
            }
            return Ok(());
        }

        self.eac3_diag_stats.dependent_pair_attempts += 1;
        let core_pcm = match core {
            PendingEac3Core::LegacyAc3 { core, .. } => core,
            PendingEac3Core::Independent(push) => push.pcm,
        };
        match crate::eac3_pipeline::resolve_eac3_presentation(self, core_pcm, &dependents) {
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
        // A dependent substream belongs to the access unit it immediately
        // follows, so any other kind of unit ends the group in hand.
        if is_dependent_eac3_frame(frame) {
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
                return self.finish_presentation(result);
            }
            pending.dependents.push(frame.to_vec());
            // The JOC payload rides in the last dependent, so one that carries
            // it ends the group with no need to wait for the next access unit.
            if eac3_frame_carries_joc(frame) {
                return self.finish_presentation(result);
            }
            return Ok(());
        }

        if let Err(()) = self.finish_presentation(result) {
            return Err(());
        }

        let decode_result = if is_legacy_ac3_frame(frame) {
            match self.ac3_decoder.decode_frame(frame) {
                Ok(core) => {
                    diagnose_eac3_frame(self, frame);
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
                    diagnose_eac3_frame(self, frame);
                    self.eac3_diag_stats.ac3_core_decode_failures += 1;
                    self.eac3_diag_stats.last_ac3_core_decode_error = Some(err.clone());
                    bridge_diag_log(
                        log::Level::Warn,
                        &format!(
                            "ac3_core_decode_failed index={} error={}",
                            self.eac3_frame_count, err
                        ),
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
        } else if eac3_frame_can_carry_dependents(frame)
            && !eac3_frame_carries_self_contained_joc(frame)
        {
            // A plain independent core might be the first half of a group, and
            // nothing in it says whether a dependent follows. Hold it until the
            // next access unit answers that. A converted-AC-3 frame is excluded:
            // ETSI allows it no dependents, so buffering it would add latency
            // for a partner that cannot arrive. An independent whose own JOC
            // payload declares a downmix wider than the frame carries is the
            // opposite case - its extension pair, the back channels or the top
            // front ones, is in a dependent, and emitting the frame alone would
            // reconstruct from a bed two channels short of its own header - so
            // that one is held too.
            match self.eac3_pcm_decoder.push_access_unit(frame) {
                Ok(push) => {
                    diagnose_eac3_frame(self, frame);
                    crate::eac3_pipeline::note_eac3_dialogue_level(self, &push.info);
                    self.pending_eac3_core = Some(PendingEac3Presentation {
                        core: PendingEac3Core::Independent(Box::new(push)),
                        dependents: Vec::new(),
                    });
                    return Ok(());
                }
                Err(err) => Err(format!("E-AC3 core decode error: {err}")),
            }
        } else {
            // A JOC payload the frame's own channels satisfy is a complete
            // presentation - the reconstruction takes the core it arrived with
            // and wants no dependent - so it is emitted straight away rather
            // than buffered. That keeps the common 5.1-core Atmos stream free of
            // the access unit of latency buffering would add.
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
                result.error_message = msg.as_str().into();
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

                // ── DTS (data types 0x0B/0x0C/0x0D/0x11) ──────────────
                if crate::dts_spdif::accepts_data_type(data_type) {
                    self.eac3_active = false;
                    self.dts_active = true;
                    let payload = crate::dts_spdif::normalise_payload(data.as_slice());
                    // Types I/II/III carry the frame directly; type IV wraps it
                    // in a start code plus a length, and pads the burst past it.
                    // Fed whole, the pipeline finds no sync word and decodes
                    // nothing at all.
                    let payload = if data_type == crate::dts_spdif::DTSHD_DATA_TYPE {
                        crate::dts_spdif::unwrap_hd_payload(&payload).unwrap_or(&payload)
                    } else {
                        &payload
                    };
                    self.dts_buf.extend_from_slice(payload);
                    crate::dts_pipeline::drain_dts(self, &mut result);
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

    fn has_objects(&self) -> bool {
        if self.dts_active {
            if self.dts_objects_active {
                return true;
            }
            // DTS core, and the presentations whose feeds are all fixed - the
            // standard height quartet and D0's five - are labeled fixed
            // channels. Whether those are placed directly or virtualized
            // remains the renderer's channel-mode decision. A presentation
            // that declares objects labels those feeds as object channels
            // instead: D1, D3 and D4 over the height quartet, and the
            // object-only variant for the single one it carries alone.
            return false;
        }
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
                log::debug!(
                    "atmos-bridge: input_codec set to {:?}",
                    self.forced_raw_codec
                );
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

    fn fixed_channel_poses(&self) -> RVec<RChannelPose> {
        // Two formats here state an angle for their channels: an unfolded
        // Auro-3D carrier declares its whole layout from Auro's setup table,
        // and DTS declares its lower layer from the ETSI loudspeaker table.
        // Dolby's bed is defined in its room cube, not by angles, and
        // declares nothing: the renderer's room model is its model.
        if self.dts_active {
            if self.dts_auro.is_unfolding() {
                self.dts_auro.declared_poses()
            } else {
                crate::labels::dts_declared_poses()
            }
        } else {
            RVec::new()
        }
    }

    fn source_family(&self) -> RString {
        // The renderer's placement policy is chosen per family
        // (`renderer::placement`): Dolby's codecs share the room-cube bed,
        // DTS its ITU angles, and an unfolded Auro-3D carrier is its own
        // family with its own default (a sphere).
        RString::from(if self.dts_active {
            if self.dts_auro.is_unfolding() {
                "auro"
            } else {
                "dts"
            }
        } else {
            "dolby"
        })
    }

    fn source_label(&self) -> RString {
        // What the host's track information calls the stream: the carrier
        // the demux found, then the spatial layer actually decoded over it.
        // Nothing until a frame decoded — before that the codec path is a
        // guess, and the host has its own.
        if !self.is_ready() {
            return RString::new();
        }
        let mut label = String::with_capacity(40);
        if self.dts_active {
            label.push_str(match self.dts_profile {
                DtsProfile::Core => "DTS",
                DtsProfile::Hd => "DTS-HD HRA",
                DtsProfile::Ma => "DTS-HD MA",
            });
            if let Some(layout) = self.dts_auro.unfolded_layout() {
                label.push_str(" + Auro-3D");
                if let Some(name) = layout.auro_name() {
                    label.push(' ');
                    label.push_str(name);
                }
            } else if let Some(presentation) = self.dts_x.locked {
                label.push_str(" + DTS:X ");
                label.push_str(presentation.layout_label());
            }
        } else if self.eac3_active {
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
        RString::from(label)
    }

    fn vbap_cartesian_defaults(&self) -> RVbapCartesianDefaults {
        // Balanced default grid size for runtime cartesian VBAP table
        // generation. The axis sizes mirror the OAMD position quantisation
        // (x, y on 6 bits / 62, z magnitude on 4 bits / 15).
        RVbapCartesianDefaults {
            x_size: 62,
            y_size: 62,
            z_size: 15,
            // The OAMD position decode carries z in [-1, 1] — the bitstream
            // has an explicit sign bit for below-floor objects — so the
            // renderer must not clamp z at the panner. Grids without
            // negative-z cells (the default) clamp such requests onto the
            // z = 0 plane, which is the pre-existing behaviour; realtime and
            // polar evaluation render them at their true position.
            allow_negative_z: true,
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
    use bridge_api::RChannelLabel;
    use std::io::Read;

    fn read_prefix(path: &str, bytes: u64) -> Option<Vec<u8>> {
        let mut input = std::fs::File::open(path).ok()?;
        let mut prefix = Vec::with_capacity(bytes as usize);
        input.by_ref().take(bytes).read_to_end(&mut prefix).ok()?;
        Some(prefix)
    }

    fn corpus_path(variable: &str) -> Option<String> {
        let path = std::env::var(variable).ok()?;
        std::path::Path::new(&path).is_file().then_some(path)
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
        let mut bridge = AtmosBridge::new(false);
        let mut result = RPushResult {
            frames: RVec::new(),
            error_message: RString::new(),
            did_reset: false,
        };
        let mut temporary_silence_pushed = false;

        bridge
            .process_eac3_access_unit(&frame, &mut result, &mut temporary_silence_pushed)
            .expect("a failed core is not a pipeline error");

        assert!(result.error_message.is_empty(), "{}", result.error_message);
        assert!(!result.did_reset);
        assert_eq!(bridge.eac3_diag_stats.ac3_core_decode_failures, 1);
        assert_eq!(bridge.eac3_diag_stats.legacy_ac3_frames, 1);
        assert_eq!(bridge.eac3_total_samples, 1536);
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
        assert_eq!(
            bridge.resolve_raw_codec(&[0x12, 0x34, 0x56, 0x78]),
            RawCodec::Eac3
        );
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
        assert_eq!(
            bridge.resolve_raw_codec(&[0x12, 0x34, 0x56, 0x78]),
            RawCodec::TrueHd
        );
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

    /// The family follows the codec path the last packet took: Dolby until
    /// a DTS packet, DTS until an Auro carrier is confirmed. Before any
    /// packet the bridge is a Dolby bridge, which is also what an older host
    /// that never asks would assume.
    #[test]
    fn source_family_follows_the_active_codec() {
        let mut bridge = AtmosBridge::new(false);
        assert_eq!(bridge.source_family().as_str(), "dolby");
        assert!(
            bridge.fixed_channel_poses().is_empty(),
            "Dolby declares no angles"
        );
        bridge.dts_active = true;
        assert_eq!(bridge.source_family().as_str(), "dts");
        let poses = bridge.fixed_channel_poses();
        assert!(
            poses
                .iter()
                .any(|p| p.label == bridge_api::RChannelLabel::Ls && p.azimuth_deg == -110.0),
            "DTS declares its ETSI angles"
        );
        bridge.dts_active = false;
        bridge.eac3_active = true;
        assert_eq!(bridge.source_family().as_str(), "dolby");
    }

    /// The label names the carrier the demux found and the spatial layer
    /// decoded over it, once a frame decoded; nothing before.
    #[test]
    fn source_label_names_the_carrier_and_its_spatial_layer() {
        let mut bridge = AtmosBridge::new(false);
        assert_eq!(bridge.source_label().as_str(), "", "nothing before a frame");

        bridge.dts_active = true;
        bridge.dts_frame_count = 1;
        assert_eq!(bridge.source_label().as_str(), "DTS");
        bridge.dts_profile = DtsProfile::Hd;
        assert_eq!(bridge.source_label().as_str(), "DTS-HD HRA");
        bridge.dts_profile = DtsProfile::Ma;
        assert_eq!(bridge.source_label().as_str(), "DTS-HD MA");
        bridge.dts_x.locked = Some(dca::XPresentation::Height);
        assert_eq!(bridge.source_label().as_str(), "DTS-HD MA + DTS:X 7.1.4");
        bridge.dts_x.locked = Some(dca::XPresentation::ObjectsD3);
        assert_eq!(bridge.source_label().as_str(), "DTS-HD MA + DTS:X 7.1.4+4");

        bridge.dts_active = false;
        bridge.dts_frame_count = 0;
        bridge.eac3_active = true;
        bridge.eac3_frame_count = 1;
        bridge.eac3_diag_stats.total_frames = 1;
        bridge.eac3_diag_stats.legacy_ac3_frames = 1;
        assert_eq!(bridge.source_label().as_str(), "Dolby Digital");
        bridge.eac3_diag_stats.total_frames = 2;
        assert_eq!(bridge.source_label().as_str(), "Dolby Digital Plus");
        bridge.eac3_diag_stats.joc_frames = 1;
        assert_eq!(
            bridge.source_label().as_str(),
            "Dolby Digital Plus + Dolby Atmos"
        );

        bridge.eac3_active = false;
        bridge.eac3_frame_count = 0;
        bridge.frame_count = 1;
        assert_eq!(bridge.source_label().as_str(), "Dolby TrueHD");
        bridge.truehd_spatial_labels = Some(RVec::new());
        assert_eq!(bridge.source_label().as_str(), "Dolby TrueHD + Dolby Atmos");
    }

    // End-to-end: feed a raw DTS core stream through the FormatBridge and check
    // it emits 5.1 bed frames with the expected channel labels. Skips when the
    // (uncommitted) corpus is absent.
    #[test]
    fn dts_hd_auro_carrier_unfolds_into_its_layout() {
        let Some(dts) = corpus_path("HARLETTY_AURO_DTS_CORPUS") else {
            eprintln!("skipping: HARLETTY_AURO_DTS_CORPUS is not set to a readable file");
            return;
        };
        let bytes = std::fs::read(dts).unwrap();
        let mut bridge = AtmosBridge::new(false);
        let mut frames = Vec::new();
        for chunk in bytes.chunks(16 * 1024) {
            let result = bridge.push_packet(RSlice::from_slice(chunk), RInputTransport::Raw, 0);
            assert!(result.error_message.is_empty(), "{}", result.error_message);
            frames.extend(result.frames.into_iter());
        }
        assert!(
            bridge.dts_auro.is_unfolding(),
            "the carrier was not confirmed"
        );
        assert!(!bridge.has_objects(), "Auro is fixed channels, not objects");
        assert_eq!(bridge.source_family().as_str(), "auro");
        assert!(
            bridge
                .source_label()
                .as_str()
                .starts_with("DTS-HD MA + Auro-3D"),
            "label {}",
            bridge.source_label()
        );
        // Every frame that came out is the unfolded layout: the carrier was
        // held back until the verdict, never emitted as 7.1.
        let channels = frames[0].channel_count;
        assert!(
            channels > 8,
            "expected more than the carrier's channels, got {channels}"
        );
        assert!(frames.iter().all(|f| f.channel_count == channels));
        let labels = &frames[0].channel_labels;
        assert!(
            labels.contains(&bridge_api::RChannelLabel::Lh)
                && labels.contains(&bridge_api::RChannelLabel::Rhs),
            "the height layer is the height tier, not the top one: {labels:?}"
        );
        // The bridge declares where Auro puts every one of them.
        let poses = bridge.fixed_channel_poses();
        let lhs = poses
            .iter()
            .find(|p| p.label == bridge_api::RChannelLabel::Lhs)
            .expect("Lhs is declared");
        assert_eq!((lhs.azimuth_deg, lhs.elevation_deg), (-110.0, 30.0));
        assert!(
            poses.iter().all(|p| labels.contains(&p.label)),
            "declared poses name only channels of the frame"
        );
        // Output is one block behind input, and no more.
        let emitted: u64 = frames.iter().map(|f| u64::from(f.sample_count)).sum();
        assert!(
            bridge.total_samples - emitted <= 4096,
            "{} held back",
            bridge.total_samples - emitted
        );
        eprintln!(
            "{} frames, {} channels, {emitted}/{} samples out",
            frames.len(),
            channels,
            bridge.total_samples
        );
    }

    #[test]
    fn dts_raw_transport_emits_bed_frames() {
        let Some(dts) = corpus_path("HARLETTY_DTS_CORE_CORPUS") else {
            eprintln!("skipping: HARLETTY_DTS_CORE_CORPUS is not set to a readable file");
            return;
        };
        let bytes = std::fs::read(dts).unwrap();
        let mut bridge = AtmosBridge::new(false);
        let result = bridge.push_packet(RSlice::from_slice(&bytes), RInputTransport::Raw, 0);
        assert!(result.error_message.is_empty(), "{}", result.error_message);
        assert!(!result.frames.is_empty(), "no frames decoded");
        // DTS core is plain channel-based audio, not objects: it reports
        // non-spatial and lets the host's channel-render mode place/virtualise
        // the bed (same as AC-3).
        assert!(!bridge.has_objects());
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
    fn dtshd_raw_transport_emits_labeled_7_1_4_channels() {
        let Some(dump) = corpus_path("HARLETTY_DTSX_STANDARD_CORPUS") else {
            eprintln!("skipping: HARLETTY_DTSX_STANDARD_CORPUS is not set to a readable file");
            return;
        };
        // Feed ~2 MB — enough for many frames past the silent intro.
        let bytes = std::fs::read(dump).unwrap();
        let chunk = &bytes[..bytes.len().min(2_000_000)];
        let mut bridge = AtmosBridge::new(false);
        bridge.configure("input_codec".into(), "dts".into());
        let result = bridge.push_packet(RSlice::from_slice(chunk), RInputTransport::Raw, 0);
        assert!(result.error_message.is_empty(), "{}", result.error_message);
        assert!(!result.frames.is_empty(), "no HD frames decoded");
        // A DTS:X fixed 7.1.4 presentation is twelve labeled fixed channels —
        // no dynamic objects, no fabricated metadata: the renderer decides
        // placement (docs/channel-object-contract.md).
        assert!(!bridge.has_objects());

        // Once the XLL-X quartet locks, frames carry the fixed 7.1.4 shape.
        let f = result
            .frames
            .iter()
            .find(|f| f.channel_count == 12)
            .expect("expected a 12-channel 7.1.4 frame");
        assert_eq!(f.sampling_frequency, 48_000);
        assert!(
            f.metadata.is_empty(),
            "fixed presentation must carry no metadata"
        );
        use bridge_api::RChannelLabel::*;
        let labels: Vec<_> = f.channel_labels.iter().copied().collect();
        // Active speakers ascending (C,L,R,Ls,Rs,LFE,Lsr,Rsr) + the height quartet.
        assert_eq!(
            labels,
            vec![C, L, R, Ls, Rs, LFE, Lb, Rb, Tfl, Tfr, Tbl, Tbr]
        );
    }

    #[test]
    fn alternate_profiles_emit_automatic_presentations() {
        let Some(d0_path) = corpus_path("HARLETTY_D0_CORPUS") else {
            eprintln!("skipping: HARLETTY_D0_CORPUS is not set to a readable file");
            return;
        };
        let Some(bytes) = read_prefix(&d0_path, 2_000_000) else {
            eprintln!("skipping: D0 corpus could not be read");
            return;
        };
        let mut bridge = AtmosBridge::new(false);
        assert!(bridge.configure("input_codec".into(), "dts".into()));
        let result = bridge.push_packet(RSlice::from_slice(&bytes), RInputTransport::Raw, 0);
        assert!(result.error_message.is_empty(), "{}", result.error_message);
        assert!(
            !bridge.has_objects(),
            "fixed D0 presentation must not set the object stream fact"
        );
        use bridge_api::RChannelLabel::*;
        let frame = result
            .frames
            .iter()
            .find(|frame| {
                [Tfc, Tfl, Tfr, Tbl, Tbr]
                    .iter()
                    .all(|label| frame.channel_labels.contains(label))
            })
            .expect("no experimental fixed D0 declaration");
        assert!(frame.metadata.is_empty());
        assert!(
            !frame.channel_labels.contains(&Object),
            "fixed D0 presentation must not fabricate objects"
        );

        let Some(d1_path) = corpus_path("HARLETTY_D1_CORPUS") else {
            eprintln!("skipping: HARLETTY_D1_CORPUS is not set to a readable file");
            return;
        };
        let Some(bytes) = read_prefix(&d1_path, 2_000_000) else {
            eprintln!("skipping: D1 corpus could not be read");
            return;
        };
        let mut bridge = AtmosBridge::new(false);
        assert!(bridge.configure("input_codec".into(), "dts".into()));
        let result = bridge.push_packet(RSlice::from_slice(&bytes), RInputTransport::Raw, 0);
        assert!(result.error_message.is_empty(), "{}", result.error_message);
        assert!(bridge.has_objects(), "D1 declares its two objects");

        let frame = result
            .frames
            .iter()
            .find(|frame| {
                [Tfl, Tfr, Tbl, Tbr]
                    .iter()
                    .all(|label| frame.channel_labels.contains(label))
                    && frame
                        .channel_labels
                        .iter()
                        .filter(|&&label| label == Object)
                        .count()
                        == 2
            })
            .expect("no D1 presentation with four heights and two objects");
        assert_eq!(frame.channel_count, 8 + 6);
        assert!(
            !frame.channel_labels.contains(&Lw),
            "D1 carries no wide channels; its first two feeds are objects"
        );

        let Some(d3_path) = corpus_path("HARLETTY_D3_CORPUS") else {
            eprintln!("skipping: HARLETTY_D3_CORPUS is not set to a readable file");
            return;
        };
        let Some(bytes) = read_prefix(&d3_path, 2_000_000) else {
            eprintln!("skipping D3: alternate-extension corpus not present: {d3_path}");
            return;
        };
        let mut bridge = AtmosBridge::new(false);
        assert!(bridge.configure("input_codec".into(), "dts".into()));
        let result = bridge.push_packet(RSlice::from_slice(&bytes), RInputTransport::Raw, 0);
        assert!(result.error_message.is_empty(), "{}", result.error_message);
        assert!(bridge.has_objects(), "D3 must expose object channels");

        let frame = result
            .frames
            .iter()
            .find(|frame| {
                frame
                    .channel_labels
                    .iter()
                    .filter(|&&label| label == Object)
                    .count()
                    == 4
                    && frame
                        .metadata
                        .iter()
                        .any(|metadata| metadata.name_updates.len() == 4)
            })
            .expect("no D3 object declaration");
        assert_eq!(frame.channel_count, 16);
        assert!(
            [Tfl, Tfr, Tbl, Tbr]
                .iter()
                .all(|label| frame.channel_labels.contains(label)),
            "the last four D3 feeds are the fixed heights"
        );
        let metadata = frame
            .metadata
            .iter()
            .find(|metadata| metadata.name_updates.len() == 4)
            .expect("no D3 name declaration");
        for source in 0..4 {
            assert_eq!(
                metadata.name_updates[source].name.as_str(),
                format!("X{source}")
            );
        }
        assert_eq!(
            metadata.events.len(),
            4,
            "every object announces a position"
        );
        assert!(metadata.events.iter().all(|event| event.has_pos));
    }
}

#[cfg(test)]
mod dts_object_only_tests {
    use super::*;
    use abi_stable::std_types::RSlice;
    use bridge_api::RInputTransport;

    /// The object-only variant on a 5.1 bed presents six labeled bed channels
    /// plus one object channel, with a position on the object.
    #[test]
    fn object_only_stream_presents_5_1_plus_one_object() {
        let Some(path) = std::env::var("HARLETTY_ALT_51_CORPUS")
            .ok()
            .filter(|p| std::path::Path::new(p).is_file())
        else {
            eprintln!("skipping: HARLETTY_ALT_51_CORPUS is not set to a readable file");
            return;
        };
        let bytes = std::fs::read(&path).expect("read corpus");
        let bytes = &bytes[..bytes.len().min(4_000_000)];
        let mut bridge = AtmosBridge::new(false);
        assert!(bridge.configure("input_codec".into(), "dts".into()));
        let result = bridge.push_packet(RSlice::from_slice(bytes), RInputTransport::Raw, 0);
        assert!(result.error_message.is_empty(), "{}", result.error_message);
        assert!(
            bridge.has_objects(),
            "the object-only variant declares its object"
        );
        use bridge_api::RChannelLabel::*;
        let frame = result
            .frames
            .iter()
            .find(|frame| frame.metadata.iter().any(|m| m.name_updates.len() == 1))
            .expect("no object declaration");
        assert_eq!(frame.channel_count, 7);
        assert_eq!(
            frame.channel_labels.as_slice(),
            &[C, L, R, Ls, Rs, LFE, Object]
        );
        let metadata = frame
            .metadata
            .iter()
            .find(|m| m.name_updates.len() == 1)
            .unwrap();
        assert_eq!(metadata.events.len(), 1);
        assert!(metadata.events[0].has_pos && metadata.events[0].pos[2] > 0.0);
        assert!(result.frames.len() > 100, "corpus was not exercised");
    }
}

#[cfg(test)]
mod dts_object_motion_tests {
    use super::*;
    use abi_stable::std_types::RSlice;
    use bridge_api::RInputTransport;

    /// A D3 stream with moving objects must produce position events whose
    /// coordinates change over time: the bridge follows the per-frame
    /// metadata rather than freezing the first position.
    #[test]
    fn d3_object_positions_follow_the_stream() {
        let Some(path) = std::env::var("HARLETTY_D3_MOTION_CORPUS")
            .ok()
            .filter(|p| std::path::Path::new(p).is_file())
        else {
            eprintln!("skipping: HARLETTY_D3_MOTION_CORPUS is not set to a readable file");
            return;
        };
        let bytes = std::fs::read(&path).expect("read corpus");
        let bytes = &bytes[..bytes.len().min(8_000_000)];
        let mut bridge = AtmosBridge::new(false);
        assert!(bridge.configure("input_codec".into(), "dts".into()));
        let mut positions: std::collections::BTreeMap<u32, Vec<[i64; 3]>> = Default::default();
        let mut frames = 0usize;
        for chunk in bytes.chunks(256 * 1024) {
            let result = bridge.push_packet(RSlice::from_slice(chunk), RInputTransport::Raw, 0);
            assert!(result.error_message.is_empty(), "{}", result.error_message);
            for frame in result.frames.iter() {
                frames += 1;
                for metadata in frame.metadata.iter() {
                    for event in metadata.events.iter() {
                        let quantised =
                            [event.pos[0], event.pos[1], event.pos[2]].map(|v| (v * 1000.0) as i64);
                        let list = positions.entry(event.id).or_default();
                        if list.last() != Some(&quantised) {
                            list.push(quantised);
                        }
                    }
                }
            }
        }
        assert!(frames > 100, "corpus was not exercised ({frames} frames)");
        assert!(bridge.has_objects());
        let moving = positions.values().filter(|list| list.len() > 3).count();
        eprintln!(
            "frames={frames} objects={} distinct positions per object={:?}",
            positions.len(),
            positions.values().map(Vec::len).collect::<Vec<_>>()
        );
        assert!(
            moving >= 2,
            "at least two objects must change position: {positions:?}"
        );
    }
}

#[cfg(test)]
mod stack_footprint_tests {
    use super::*;

    /// Hosts may create the bridge on a small-stack thread — mpv's macOS
    /// playback thread is a bare `pthread_create`, so 512 KiB. Keeping the
    /// struct pointer-sized per decoder is what stops `new()` from copying
    /// hundreds of KiB across the three frames between the constructor and the
    /// heap. Box any decoder state added here; do not inline it.
    #[test]
    fn atmos_bridge_stack_footprint_stays_small() {
        const BUDGET: usize = 4 * 1024;
        let actual = std::mem::size_of::<AtmosBridge>();
        assert!(
            actual <= BUDGET,
            "AtmosBridge grew to {actual} bytes (budget {BUDGET}); \
             box the newly inlined decoder state instead"
        );
    }

    /// End-to-end guard for the same invariant: build a bridge on a thread
    /// sized like mpv's macOS playback thread. A by-value decoder field would
    /// overflow the guard page here exactly as it did in the field report
    /// (`SIGBUS` in `AtmosBridge::new`, Omniphony issue #205).
    #[test]
    fn new_fits_on_a_macos_sized_playback_thread() {
        const MACOS_PLAYBACK_STACK: usize = 512 * 1024;
        std::thread::Builder::new()
            .stack_size(MACOS_PLAYBACK_STACK)
            .spawn(|| {
                let bridge = AtmosBridge::new(false);
                assert_eq!(bridge.presentation, 3);
            })
            .expect("spawn small-stack thread")
            .join()
            .expect("bridge construction overflowed a 512 KiB stack");
    }
}
