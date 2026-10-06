// SPDX-License-Identifier: Apache-2.0
//
// DTS (DCA) raw transport pipeline. Demuxes the `[core][exss]` byte stream and
// routes each frame to either the DTS-HD MA lossless decoder (5.1/7.1, when an
// EXSS substream follows the core) or the plain DTS core decoder (5.1). Every
// decoded core channel is emitted as a bed channel, placed at its canonical
// speaker by the renderer.
//
// DTS:X extension waveforms are presented the way the stream describes them
// (`dca::XPresentation` + `dca::XMetadata`): fixed heights as labeled
// channels, objects as object channels with transmitted positions. Every
// waveform was also mixed into the backward-compatible bed by the encoder, at
// gains the stream's private metadata states; that contribution is removed
// from the bed before the waveform is emitted at its own position, so nothing
// plays twice. A waveform whose fold is not stated (an object record without
// reference rows) was panned into the bed by the encoder from its position:
// that render is recomputed (`dca::FoldRenderer`) and subtracted the same way.
// Whatever is still unknown (render off, or a layout the renderer cannot
// cover) has its fold estimated from the audio (`dca::FoldEstimator`); with
// both off it stays in the bed and its own channel is silent.

use abi_stable::std_types::{RString, RVec};
use bridge_api::RPushResult;
use bridge_api::{
    RChannelLabel, RChannelPose, RDecodedFrame, REvent, RMetadataFrame, RNameUpdate, RObjectChannel,
};
use dca::{
    CorePcmFrame, ExssKind, FoldEstimator, FoldPlan, FoldRenderer, HdError, HdFrame, MAX_SOURCES,
    SourceRole, SphericalPosition, XMetadata, XPresentation, exss_kind, exss_substream_size,
    parse_header,
};
use serde::Deserialize;

use crate::auro_pipeline::DtsAuroState;
use crate::frame_builders::float_to_pcm_i32;
use crate::labels::{dca_bed_channel_to_r, dca_spatial_channel_to_r};
use crate::logging::{RepeatCounter, bridge_diag_log, bridge_log};
use crate::shared::{AfterPush, SharedState};
use bridge_common::objects::declare_object_channels;

const CORE_SYNC: [u8; 4] = 0x7FFE_8001u32.to_be_bytes();
const SUBSTREAM_SYNC: [u8; 4] = 0x6458_2025u32.to_be_bytes();
/// Object ids start past the legacy bed-id range.
const OBJECT_ID_BASE: u32 = 10;
/// Re-emit unchanged object positions at this rate so monitoring clients
/// whose stream-idle timeout is shorter than a second keep the objects
/// alive, while staying far below the per-audio-frame rate that could
/// overwhelm Studio.
const HEARTBEAT_HZ: u64 = 2;

/// Fallback fold for a standard-profile frame whose type-2 matrix could not
/// be read: the four height feeds at one configured gain. Every stream in
/// the corpus states code 55 (Q15 23170/32768, -3.01 dB), which is also the
/// bundled default; a local `dts-fold.yaml` overrides it.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct FoldRoute {
    pub target: String,
    pub gain_db: f32,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct FoldSource {
    pub source: String,
    #[serde(default)]
    pub routes: Vec<FoldRoute>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct DtsFoldConfig {
    #[serde(default)]
    pub sources: Vec<FoldSource>,
    /// Recompute the fold of an object the stream says the encoder rendered
    /// into the bed from its position (no reference rows), and subtract it.
    #[serde(default = "default_true")]
    pub render_unstated: bool,
    /// Estimate the fold of a waveform the stream does not state one for,
    /// from the bed's own audio, so the waveform plays at its position and
    /// leaves the bed. Off, such a waveform stays in the bed and is muted.
    #[serde(default = "default_true")]
    pub estimate_unknown: bool,
}

fn default_true() -> bool {
    true
}

impl Default for DtsFoldConfig {
    fn default() -> Self {
        serde_yaml_ng::from_str(include_str!("../dts-fold.yaml"))
            .expect("valid bundled dts-fold.yaml")
    }
}

impl DtsFoldConfig {
    pub(crate) fn from_env() -> Self {
        let path = std::path::Path::new("dts-fold.yaml");
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_yaml_ng::from_str(&text).ok())
            .unwrap_or_else(|| Self::default())
    }

    pub(crate) fn gain(&self, target: &str, source: &str) -> f32 {
        self.sources
            .iter()
            .find(|entry| entry.source.eq_ignore_ascii_case(source))
            .and_then(|entry| {
                entry
                    .routes
                    .iter()
                    .find(|route| route.target.eq_ignore_ascii_case(target))
            })
            .map_or(0.0, |route| 10.0_f32.powf(route.gain_db / 20.0))
    }

    /// The standard-profile fallback plan: TFL→L, TFR→R, TBL→Lb, TBR→Rb.
    fn standard_fallback(&self) -> FoldPlan {
        FoldPlan::standard_heights(self.gain("L", "TFL"))
    }
}

/// Per-stream DTS:X extension state. Cleared with the pipeline.
#[derive(Default)]
pub(crate) struct DtsXState {
    /// Presentation latched from the first frame with usable extension
    /// waveforms. A later frame whose waveforms are missing or invalid keeps
    /// this channel shape (composite bed + silent extension channels) instead
    /// of renegotiating, and no content is lost: the folded contribution stays
    /// in the bed for that frame.
    pub(crate) locked: Option<XPresentation>,
    /// The most recent metadata that parsed. A frame whose own metadata does
    /// not parse reuses it, so a transient failure neither clicks nor changes
    /// which waveforms play.
    pub(crate) last_metadata: Option<XMetadata>,
    /// Position last announced per object feed, for sparse events.
    emitted_positions: [Option<SphericalPosition>; MAX_SOURCES],
    /// Recomputed folds of the objects rendered into the bed from their
    /// position.
    renderer: FoldRenderer,
    /// Whether the recomputed folds have been announced for this stream.
    render_noted: bool,
    /// Running fold estimates for the waveforms whose fold is not stated.
    estimator: FoldEstimator,
    /// Whether the estimation has been announced for this stream.
    estimation_noted: bool,
    parse_failures: RepeatCounter,
    feed_dropouts: RepeatCounter,
    bed_extension_dropouts: RepeatCounter,
    /// The decoded frame, kept from packet to packet so the decoder refills
    /// its buffers instead of allocating new ones.
    frame: HdFrame,
    /// The same for a frame decoded from the core alone.
    core_frame: CorePcmFrame,
}

impl DtsXState {
    fn note_parse_failure(&mut self, error: dca::XMetadataError) {
        if let Some(failures) = self.parse_failures.note() {
            bridge_log!(
                log::Level::Warn,
                "dts: extension metadata unreadable ({error:?}, {failures} frames so far); {}",
                if self.last_metadata.is_some() {
                    "reusing the last readable frame"
                } else {
                    "keeping the bed as authored and muting the extension channels"
                }
            );
        }
    }

    /// A lossy carrier's XXCH channels did not decode this frame: the bed
    /// is the core alone for it.
    fn note_bed_extension_dropout(&mut self, reason: &str) {
        if let Some(dropouts) = self.bed_extension_dropouts.note() {
            bridge_log!(
                log::Level::Warn,
                "dts: XXCH channels unavailable ({reason}, {dropouts} frames so far); playing the core bed"
            );
        }
    }

    fn note_feed_dropout(&mut self, reason: &str) {
        if let Some(dropouts) = self.feed_dropouts.note() {
            bridge_log!(
                log::Level::Warn,
                "dts: extension waveforms unavailable ({reason}, {dropouts} frames so far); emitting silent extension channels"
            );
        }
    }
}

/// The source families this path reports (`FormatBridge::source_family`):
/// DTS, or Auro-3D once a lossless carrier is confirmed and unfolded.
pub const FAMILY_DTS: &str = "dts";
pub const FAMILY_AURO: &str = "auro";

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

/// Everything the DTS path keeps from one packet to the next: the demux
/// buffer, the decoders, the DTS:X and Auro-3D stages, and the live stream
/// facts the host asks about between packets.
///
/// The decoders are boxed: a host may create the bridge on a thread with a
/// small stack (see the bridge's `AtmosBridge`).
pub struct DtsPipeline {
    /// Raw byte buffer for demuxing `[core][exss]` DTS-HD frames.
    pub(crate) buf: Vec<u8>,
    /// Plain DTS core (5.1) decoder.
    pub(crate) decoder: Box<dca::PcmDecoder>,
    /// DTS-HD Master Audio lossless (5.1/7.1) decoder.
    pub(crate) hd_decoder: Box<dca::HdDecoder>,
    pub(crate) frame_count: u64,
    /// DTS:X extension state: latched presentation shape, last readable
    /// metadata, announced object positions. Cleared with the pipeline.
    pub(crate) x: DtsXState,
    /// Live stream fact: the latest DTS frame's surround pair is the
    /// carrier's side-surround pair (Lss/Rss, ±90°) rather than its surround
    /// pair (Ls/Rs, ±110°). Both play through `Ls`/`Rs`; this picks the angle
    /// the pair is declared at (`fixed_channel_poses`).
    pub(crate) surrounds_on_side: bool,
    pub(crate) fold_config: DtsFoldConfig,
    /// Live stream fact: the latest DTS frame emitted object channels. Set
    /// from what the frame presented rather than from its profile, so every
    /// object-bearing presentation reaches it by the same route.
    pub(crate) objects_active: bool,
    /// The carrier of the latest DTS frame, for the source label.
    pub(crate) profile: DtsProfile,
    /// Auro-3D detection and unfolding over the lossless DTS-HD output.
    /// Holds the first frames back until the carrier question is settled.
    pub(crate) auro: DtsAuroState,
}

impl DtsPipeline {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            decoder: Box::new(dca::PcmDecoder::new()),
            hd_decoder: Box::new(dca::HdDecoder::new()),
            frame_count: 0,
            x: DtsXState::default(),
            surrounds_on_side: false,
            fold_config: DtsFoldConfig::from_env(),
            objects_active: false,
            profile: DtsProfile::default(),
            auro: DtsAuroState::default(),
        }
    }

    /// Forget the stream (seek, sync loss): the fold configuration stays.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.decoder.reset();
        self.hd_decoder.reset();
        self.frame_count = 0;
        self.x = DtsXState::default();
        self.surrounds_on_side = false;
        self.objects_active = false;
        self.profile = DtsProfile::default();
        self.auro.reset();
    }

    /// Raw transport: frames as they come in the elementary stream.
    pub fn push_raw(
        &mut self,
        shared: &mut SharedState,
        data: &[u8],
        result: &mut RPushResult,
    ) -> AfterPush {
        self.buf.extend_from_slice(data);
        drain_dts(self, shared, result)
    }

    /// IEC 61937 transport: one burst payload of one of the DTS data types
    /// (`dts_spdif::accepts_data_type`).
    pub fn push_iec61937(
        &mut self,
        shared: &mut SharedState,
        data: &[u8],
        data_type: u8,
        result: &mut RPushResult,
    ) -> AfterPush {
        let payload = crate::dts_spdif::normalise_payload(data);
        // Types I/II/III carry the frame directly; type IV wraps it
        // in a start code plus a length, and pads the burst past it.
        // Fed whole, the pipeline finds no sync word and decodes
        // nothing at all.
        let payload = if data_type == crate::dts_spdif::DTSHD_DATA_TYPE {
            crate::dts_spdif::unwrap_hd_payload(&payload).unwrap_or(&payload)
        } else {
            &payload
        };
        self.push_raw(shared, payload, result)
    }

    pub fn is_ready(&self) -> bool {
        self.frame_count > 0
    }

    pub fn has_objects(&self) -> bool {
        // DTS core, and the presentations whose feeds are all fixed - the
        // standard height quartet and D0's five - are labeled fixed
        // channels. Whether those are placed directly or virtualized
        // remains the renderer's channel-mode decision. A presentation
        // that declares objects labels those feeds as object channels
        // instead: D1, D3 and D4 over the height quartet, and the
        // object-only variant for the single one it carries alone.
        self.objects_active
    }

    pub fn source_family(&self) -> &'static str {
        if self.auro.is_unfolding() {
            FAMILY_AURO
        } else {
            FAMILY_DTS
        }
    }

    /// The carrier the demux found, then the spatial layer decoded over it.
    pub fn source_label(&self, label: &mut String) {
        label.push_str(match self.profile {
            DtsProfile::Core => "DTS",
            DtsProfile::Hd => "DTS-HD HRA",
            DtsProfile::Ma => "DTS-HD MA",
        });
        if let Some(layout) = self.auro.unfolded_layout() {
            label.push_str(" + Auro-3D");
            if let Some(name) = layout.auro_name() {
                label.push(' ');
                label.push_str(name);
            }
        } else if let Some(presentation) = self.x.locked {
            label.push_str(" + DTS:X ");
            label.push_str(presentation.layout_label());
        }
    }

    /// An unfolded Auro-3D carrier declares its whole layout from Auro's
    /// setup table; DTS declares its lower layer from the ETSI loudspeaker
    /// table.
    pub fn fixed_channel_poses(&self) -> RVec<RChannelPose> {
        if self.auro.is_unfolding() {
            self.auro.declared_poses()
        } else {
            crate::labels::dts_declared_poses(self.surrounds_on_side)
        }
    }
}

/// Demux and decode all complete DTS frames buffered in `dts.buf`.
fn drain_dts(
    dts: &mut DtsPipeline,
    shared: &mut SharedState,
    result: &mut RPushResult,
) -> AfterPush {
    let mut consumed = 0usize;
    loop {
        let rest = &dts.buf[consumed..];
        // Locate the next core syncword.
        let Some(sync_off) = find(rest, &CORE_SYNC) else {
            // Keep only a possible partial trailing syncword.
            consumed += rest.len().saturating_sub(3);
            break;
        };
        consumed += sync_off;
        let rest = &dts.buf[consumed..];

        let info = match parse_header(rest) {
            Ok(i) => i,
            Err(dca::HeaderParseError::InsufficientData) => break,
            Err(_) => {
                consumed += 4; // resync past this candidate
                continue;
            }
        };
        let fs = info.frame_size;
        // Need the core frame plus 4 bytes to check for a trailing EXSS.
        if rest.len() < fs + 4 {
            break;
        }
        let is_hd = rest[fs..fs + 4] == SUBSTREAM_SYNC;

        if is_hd {
            let Some(es) = exss_substream_size(&rest[fs..]) else {
                break; // EXSS not fully buffered yet
            };
            if rest.len() < fs + es {
                break;
            }
            let kind = exss_kind(&rest[fs..fs + es]);
            if kind != ExssKind::Core {
                dts.profile = if kind == ExssKind::Lossless {
                    DtsProfile::Ma
                } else {
                    DtsProfile::Hd
                };
                let mut hd = std::mem::take(&mut dts.x.frame);
                match dts
                    .hd_decoder
                    .decode_into(&rest[..fs], &rest[fs..fs + es], &mut hd)
                {
                    Ok(()) => {
                        let n = hd_samples(&hd);
                        dts.surrounds_on_side = hd.surrounds_on_side();
                        if let Some(kind) = hd.xxch_decode_error {
                            dts.x.note_bed_extension_dropout(kind);
                        }
                        if let Some((frame, emitted_objects)) = build_hd_frame_with_extensions(
                            &hd,
                            &mut dts.x,
                            &dts.fold_config,
                            shared.total_samples,
                            &mut shared.declared_object_channels,
                        ) {
                            dts.objects_active = emitted_objects;
                            if kind == ExssKind::Lossless {
                                // The Auro side channel lives in the low bits
                                // of the lossless integers, read off the
                                // decoder's integer tap. The stage decides
                                // whether the frame goes out as it is or
                                // unfolded.
                                let active: Vec<usize> = (0..hd.samples.len())
                                    .filter(|&s| hd.samples[s].is_some())
                                    .collect();
                                dts.auro.route(
                                    frame,
                                    &active,
                                    dts.hd_decoder.lossless_samples(),
                                    &mut result.frames,
                                );
                            } else {
                                // A lossy carrier has no side channel to read.
                                dts.auro.not_a_carrier(&mut result.frames);
                                result.frames.push(frame);
                            }
                        }
                        shared.total_samples += n as u64;
                        dts.frame_count += 1;
                    }
                    Err(HdError::Pending) => {} // PBR buffering; no frame this packet
                    Err(e) => {
                        let msg = format!("dts_hd_decode_error={e:?}");
                        bridge_diag_log(log::Level::Warn, &msg);
                        if shared.strict {
                            result.error_message = RString::from(msg);
                            result.did_reset = true;
                            return AfterPush::ResetPipeline;
                        }
                    }
                }
                dts.x.frame = hd;
            } else {
                // Nothing the HD decoder reads beyond the core: an XBR-only
                // DTS-HD HRA layers high-frequency detail on top of an
                // ordinary DTS core, which is not decoded, so render the
                // core (5.1) and drop it instead of failing the whole track.
                dts.profile = DtsProfile::Hd;
                let pcm = &mut dts.x.core_frame;
                match dts.decoder.decode_into(&rest[..fs], pcm) {
                    Ok(_) => {
                        dts.objects_active = false;
                        // A core names its surrounds Ls/Rs only.
                        dts.surrounds_on_side = false;
                        dts.auro.not_a_carrier(&mut result.frames);
                        result.frames.push(build_core_frame(pcm));
                        shared.total_samples += pcm.samples_per_channel() as u64;
                        dts.frame_count += 1;
                    }
                    Err(err) => {
                        let msg = format!("dts_decode_error={err}");
                        bridge_diag_log(log::Level::Warn, &msg);
                        if shared.strict {
                            result.error_message = RString::from(msg);
                            result.did_reset = true;
                            return AfterPush::ResetPipeline;
                        }
                    }
                }
            }
            consumed += fs + es;
        } else {
            dts.profile = DtsProfile::Core;
            let pcm = &mut dts.x.core_frame;
            match dts.decoder.decode_into(&rest[..fs], pcm) {
                Ok(_) => {
                    let frame = build_core_frame(pcm);
                    dts.objects_active = false;
                    dts.surrounds_on_side = false;
                    dts.auro.not_a_carrier(&mut result.frames);
                    shared.total_samples += pcm.samples_per_channel() as u64;
                    dts.frame_count += 1;
                    result.frames.push(frame);
                }
                Err(err) => {
                    let msg = format!("dts_decode_error={err}");
                    bridge_diag_log(log::Level::Warn, &msg);
                    if shared.strict {
                        result.error_message = RString::from(msg);
                        result.did_reset = true;
                        return AfterPush::ResetPipeline;
                    }
                }
            }
            consumed += fs;
        }
    }

    if consumed > 0 {
        dts.buf.drain(..consumed.min(dts.buf.len()));
    }
    AfterPush::Continue
}

fn find(data: &[u8], needle: &[u8; 4]) -> Option<usize> {
    data.windows(4).position(|w| w == needle)
}

fn hd_samples(hd: &HdFrame) -> usize {
    hd.bed_sample_count()
}

/// DCA speaker index -> renderer channel label, for the DTS-HD bed.
pub(crate) fn speaker_to_label(spkr: usize) -> RChannelLabel {
    match spkr {
        0 => RChannelLabel::C,
        1 => RChannelLabel::L,
        2 => RChannelLabel::R,
        3 => RChannelLabel::Ls,
        4 => RChannelLabel::Rs,
        5 => RChannelLabel::LFE,
        6 => RChannelLabel::Cb, // Cs (rear center)
        7 => RChannelLabel::Lb, // Lsr (rear surround left)
        8 => RChannelLabel::Rb, // Rsr (rear surround right)
        _ => RChannelLabel::Unknown,
    }
}

/// The fold plan for this frame: its own metadata when readable, otherwise
/// the last readable metadata, otherwise the profile's fallback.
fn resolve_plan(
    hd: &HdFrame,
    presentation: XPresentation,
    state: &mut DtsXState,
    fold_config: &DtsFoldConfig,
) -> FoldPlan {
    match XMetadata::parse(&hd.x_payload, presentation.feed_count()) {
        Ok(metadata) => {
            state.last_metadata = Some(metadata);
            FoldPlan::from_metadata(&metadata)
        }
        Err(error) => {
            state.note_parse_failure(error);
            match (&state.last_metadata, presentation) {
                (Some(metadata), _) => FoldPlan::from_metadata(metadata),
                (None, XPresentation::Height) => fold_config.standard_fallback(),
                (None, _) => FoldPlan::all_unknown(presentation.feed_count()),
            }
        }
    }
}

/// Build a frame from decoded DTS-HD per-speaker PCM: the bed with every
/// stated extension contribution removed, then the fixed extension channels,
/// then the object channels. Returns the frame and whether it carries object
/// channels (the live `has_objects` fact).
fn build_hd_frame_with_extensions(
    hd: &HdFrame,
    state: &mut DtsXState,
    fold_config: &DtsFoldConfig,
    sample_pos: u64,
    declared_object_channels: &mut Option<RVec<RObjectChannel>>,
) -> Option<(RDecodedFrame, bool)> {
    // Active speakers in ascending index order = stable channel order.
    let active: Vec<usize> = (0..hd.samples.len())
        .filter(|&s| hd.samples[s].is_some())
        .collect();
    let sample_count = hd_samples(hd);

    // Validate every bed channel length before any indexing: a decoder bug or
    // malformed stream must degrade to a dropped frame, never a panic.
    let mut bed: Vec<&[f32]> = Vec::with_capacity(active.len());
    for &spkr in &active {
        let channel = hd.samples[spkr].as_ref().expect("active speaker");
        if channel.len() != sample_count {
            bridge_log!(
                log::Level::Warn,
                "dts: bed channel {spkr} length {} != {sample_count}; dropping frame",
                channel.len()
            );
            return None;
        }
        bed.push(channel.as_slice());
    }

    // A presentation is detected only when every extension waveform is
    // present at the bed length; otherwise the latched one keeps the shape.
    let detected = XPresentation::detect(hd);
    let presentation = match (detected, state.locked) {
        (Some(detected), _) => {
            if state.locked.is_some_and(|locked| locked != detected) {
                bridge_log!(
                    log::Level::Warn,
                    "dts: extension presentation changed {:?} -> {detected:?}; channel shape changes",
                    state.locked
                );
                state.estimator.reset();
                state.renderer.reset();
            }
            state.locked = Some(detected);
            detected
        }
        (None, Some(locked)) => {
            if hd.x_present || hd.x_imax {
                state.note_feed_dropout(hd.x_decode_error.unwrap_or("no usable waveform set"));
            }
            locked
        }
        (None, None) => return Some((bed_only_frame(hd, &active, &bed, sample_count), false)),
    };
    let feeds: &[Vec<f32>] = if detected.is_some() {
        hd.x_samples.as_slice()
    } else {
        &[]
    };
    let mut plan = if detected.is_some() {
        resolve_plan(hd, presentation, state, fold_config)
    } else {
        FoldPlan::all_unknown(presentation.feed_count())
    };
    if detected.is_some() && fold_config.render_unstated && plan.has_unknown() {
        if let Some(metadata) = state.last_metadata {
            state.renderer.apply(&mut plan, &metadata, sample_count);
            if !state.render_noted && metadata.sources().count() > 0 {
                let rendered = (0..metadata.source_count())
                    .filter(|&feed| metadata.fold_is_rendered(feed))
                    .count();
                if rendered > 0 {
                    state.render_noted = true;
                    bridge_log!(
                        log::Level::Info,
                        "dts: {presentation:?} carries {rendered} object(s) rendered into the bed from their position; recomputing that fold"
                    );
                }
            }
        }
    }
    if detected.is_some() && fold_config.estimate_unknown && plan.has_unknown() {
        if !state.estimation_noted {
            state.estimation_noted = true;
            bridge_log!(
                log::Level::Info,
                "dts: {:?} carries waveform(s) without a stated bed fold; estimating their fold from the bed",
                presentation
            );
        }
        state.estimator.refine(&mut plan, &hd.samples, feeds);
    }
    let metadata = detected.and(state.last_metadata);

    let fixed_feeds = presentation.fixed_feeds();
    let object_feeds = presentation.object_feeds();
    let channel_count = active.len() + presentation.feed_count();

    // Channel by channel: each bed channel with its stated feeds removed,
    // then the feeds, silent when their fold is not stated.
    let width = bed.len() + fixed_feeds.len() + object_feeds.len();
    let mut pcm = vec![0i32; sample_count * width];
    let mut cleaned = vec![0.0f32; sample_count];
    for (column, (channel, &spkr)) in bed.iter().zip(&active).enumerate() {
        plan.clean_channel(spkr, channel, feeds, &mut cleaned);
        write_column(&mut pcm, column, width, &cleaned);
    }
    for (column, feed) in (bed.len()..).zip(fixed_feeds.clone().chain(object_feeds.clone())) {
        if let Some(waveform) = feeds.get(feed).filter(|_| plan.source_is_known(feed)) {
            write_column(&mut pcm, column, width, &waveform[..sample_count]);
        }
    }
    let pcm = RVec::from(pcm);

    let mut channel_labels: RVec<RChannelLabel> = RVec::with_capacity(channel_count);
    for &spkr in &active {
        channel_labels.push(speaker_to_label(spkr));
    }
    channel_labels.extend(
        presentation
            .fixed_channels()
            .iter()
            .map(|&channel| dca_spatial_channel_to_r(channel)),
    );
    channel_labels.extend(std::iter::repeat_n(
        RChannelLabel::Object,
        object_feeds.len(),
    ));

    let metadata = build_object_metadata(
        presentation,
        metadata.as_ref(),
        active.len() + fixed_feeds.len(),
        sample_pos,
        hd.sample_rate,
        sample_count,
        state,
        declared_object_channels,
    );

    Some((
        RDecodedFrame {
            sampling_frequency: hd.sample_rate,
            sample_count: sample_count as u32,
            channel_count: channel_count as u32,
            pcm,
            channel_labels,
            metadata,
            drc_gain: 1.0,
            drc_ramp_duration: 0,
            dialogue_level: None.into(),
            is_new_segment: false,
        },
        !object_feeds.is_empty(),
    ))
}

/// Convert one channel into column `column` of interleaved PCM `width`
/// channels wide.
fn write_column(pcm: &mut [i32], column: usize, width: usize, samples: &[f32]) {
    for (out, &sample) in pcm.iter_mut().skip(column).step_by(width).zip(samples) {
        *out = float_to_pcm_i32(sample);
    }
}

/// A frame with no extension presentation: the lossless bed as decoded.
fn bed_only_frame(
    hd: &HdFrame,
    active: &[usize],
    bed: &[&[f32]],
    sample_count: usize,
) -> RDecodedFrame {
    let mut pcm = vec![0i32; sample_count * bed.len()];
    for (column, channel) in bed.iter().enumerate() {
        write_column(&mut pcm, column, bed.len(), &channel[..sample_count]);
    }
    let pcm = RVec::from(pcm);
    let channel_labels: RVec<RChannelLabel> =
        active.iter().map(|&spkr| speaker_to_label(spkr)).collect();
    RDecodedFrame {
        sampling_frequency: hd.sample_rate,
        sample_count: sample_count as u32,
        channel_count: active.len() as u32,
        pcm,
        channel_labels,
        metadata: RVec::new(),
        drc_gain: 1.0,
        drc_ramp_duration: 0,
        dialogue_level: None.into(),
        is_new_segment: false,
    }
}

/// Object channel declaration and position events for a presentation with
/// objects. Declarations are sparse (re-emitted on change); positions are
/// emitted when they change, when the declaration changes, and on the
/// heartbeat. A fixed presentation emits nothing.
#[allow(clippy::too_many_arguments)]
fn build_object_metadata(
    presentation: XPresentation,
    metadata: Option<&XMetadata>,
    first_object_channel: usize,
    sample_pos: u64,
    sample_rate: u32,
    sample_count: usize,
    state: &mut DtsXState,
    declared_object_channels: &mut Option<RVec<RObjectChannel>>,
) -> RVec<RMetadataFrame> {
    let object_feeds = presentation.object_feeds();
    if object_feeds.is_empty() {
        return RVec::new();
    }

    let current: RVec<RObjectChannel> = object_feeds
        .clone()
        .enumerate()
        .map(|(index, feed)| RObjectChannel {
            id: OBJECT_ID_BASE + feed as u32,
            channel: (first_object_channel + index) as u32,
        })
        .collect();
    let declaration_unchanged = declared_object_channels.as_deref() == Some(current.as_slice());
    let heartbeat_period = u64::from(sample_rate) / HEARTBEAT_HZ;
    let heartbeat_due = heartbeat_period > 0 && sample_pos % heartbeat_period < sample_count as u64;

    // The engine broadcasts the objects present in each metadata frame and
    // clears the others as stale, so a frame must carry every object or none:
    // emitting only the objects that moved makes the static ones flicker in
    // monitoring clients.
    let positions: Vec<(usize, SphericalPosition)> = object_feeds
        .clone()
        .filter_map(|feed| {
            match metadata
                .and_then(|metadata| metadata.source(feed))
                .map(|source| source.role)
            {
                Some(SourceRole::Object { position, .. }) => Some((feed, position)),
                _ => None,
            }
        })
        .collect();
    let moved = positions
        .iter()
        .any(|&(feed, position)| state.emitted_positions[feed] != Some(position));
    let mut events: RVec<REvent> = RVec::new();
    if moved || !declaration_unchanged || heartbeat_due {
        for &(feed, position) in &positions {
            state.emitted_positions[feed] = Some(position);
            events.push(REvent {
                id: OBJECT_ID_BASE + feed as u32,
                sample_pos,
                has_pos: true,
                pos: position.to_adm_cartesian(),
                gain_db: 0,
                size: [0.0; 3],
                ramp_duration: 0,
            });
        }
    }
    if declaration_unchanged && events.is_empty() {
        return RVec::new();
    }

    let (object_channels, name_updates) = if declaration_unchanged {
        (RVec::new(), RVec::new())
    } else {
        bridge_log!(
            log::Level::Info,
            "dts: {presentation:?} declares object channels for extension waveforms {}..{}",
            object_feeds.start,
            object_feeds.end
        );
        let names = object_feeds
            .clone()
            .map(|feed| RNameUpdate {
                id: OBJECT_ID_BASE + feed as u32,
                name: RString::from(format!("X{feed}")),
            })
            .collect();
        (
            declare_object_channels(declared_object_channels, current),
            names,
        )
    };

    let mut frames = RVec::with_capacity(1);
    frames.push(RMetadataFrame {
        events,
        object_channels,
        channel_gains: RVec::new(),
        name_updates,
        sample_pos,
        ramp_duration: 0,
    });
    frames
}

/// Build a bed frame from a plain DTS core PCM frame (DCA primary order + LFE).
fn build_core_frame(core: &CorePcmFrame) -> RDecodedFrame {
    let sample_count = core.samples_per_channel();
    let total_channel_count = core.total_channels();

    let mut pcm = vec![0i32; sample_count * total_channel_count];
    for (column, channel) in core
        .fullband_channels
        .iter()
        .chain(&core.lfe_channel)
        .enumerate()
    {
        write_column(
            &mut pcm,
            column,
            total_channel_count,
            &channel[..sample_count],
        );
    }
    let pcm = RVec::from(pcm);
    let mut channel_labels: RVec<RChannelLabel> = RVec::with_capacity(total_channel_count);
    for bed in &core.fullband_channel_order {
        channel_labels.push(dca_bed_channel_to_r(*bed));
    }
    if core.lfe_channel.is_some() {
        channel_labels.push(RChannelLabel::LFE);
    }

    RDecodedFrame {
        sampling_frequency: core.sample_rate,
        sample_count: sample_count as u32,
        channel_count: total_channel_count as u32,
        pcm,
        channel_labels,
        metadata: RVec::new(),
        drc_gain: 1.0,
        drc_ramp_duration: 0,
        dialogue_level: None.into(),
        is_new_segment: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The label names the carrier the demux found and the spatial layer
    /// decoded over it.
    #[test]
    fn source_label_names_the_carrier_and_its_spatial_layer() {
        let mut dts = DtsPipeline::new();
        let label = |dts: &DtsPipeline| {
            let mut label = String::new();
            dts.source_label(&mut label);
            label
        };
        assert_eq!(label(&dts), "DTS");
        dts.profile = DtsProfile::Hd;
        assert_eq!(label(&dts), "DTS-HD HRA");
        dts.profile = DtsProfile::Ma;
        assert_eq!(label(&dts), "DTS-HD MA");
        dts.x.locked = Some(dca::XPresentation::Height);
        assert_eq!(label(&dts), "DTS-HD MA + DTS:X 7.1.4");
        dts.x.locked = Some(dca::XPresentation::ObjectsD3);
        assert_eq!(label(&dts), "DTS-HD MA + DTS:X 7.1.4+4");
    }

    /// The surround pair is declared where the stream named it: ±110° for
    /// Ls/Rs, ±90° for a side pair (Lss/Rss).
    #[test]
    fn the_surround_pair_is_declared_where_the_stream_names_it() {
        let ls_azimuth = |dts: &DtsPipeline| {
            dts.fixed_channel_poses()
                .iter()
                .find(|p| p.label == RChannelLabel::Ls)
                .map(|p| p.azimuth_deg)
        };
        let mut dts = DtsPipeline::new();
        assert_eq!(dts.source_family(), FAMILY_DTS);
        assert_eq!(ls_azimuth(&dts), Some(-110.0));
        dts.surrounds_on_side = true;
        assert_eq!(ls_azimuth(&dts), Some(-90.0));
    }
    use dca::{BedFold, SourceMetadata, SpatialChannel, gain_code_linear};

    const SAMPLE_COUNT: usize = 2;
    const Q55: f32 = 23170.0 / 32768.0;

    fn hd_frame(samples: Vec<Option<Vec<f32>>>, x_samples: Vec<Vec<f32>>) -> HdFrame {
        HdFrame {
            sample_rate: 48_000,
            x_present: !x_samples.is_empty(),
            x_pcm_bit_res: 24,
            xll_frame_segments: 1,
            xll_segment_samples: SAMPLE_COUNT,
            samples,
            x_samples,
            ..HdFrame::default()
        }
    }

    fn assert_pcm_close(actual: i32, expected: f32) {
        let expected = float_to_pcm_i32(expected);
        assert!(
            // Allow the bounded float32/PCM quantisation difference.
            actual.abs_diff(expected) <= 64,
            "actual={actual}, expected={expected}"
        );
    }

    fn height(channel: SpatialChannel, column: usize, gain: f32) -> SourceMetadata {
        let mut columns = [0.0; 8];
        columns[column] = gain;
        SourceMetadata {
            role: SourceRole::Height(channel),
            fold: BedFold::Known(columns),
        }
    }

    fn object(azimuth: i16, elevation: i16, fold: BedFold) -> SourceMetadata {
        SourceMetadata {
            role: SourceRole::Object {
                position: SphericalPosition {
                    azimuth_half_degrees: azimuth,
                    elevation_half_degrees: elevation,
                    distance_64ths: 64,
                },
                centre_height_alternative: false,
            },
            fold,
        }
    }

    /// The four fixed heights folded into L, R, Lb, Rb at `gain`.
    fn heights(gain: f32) -> [SourceMetadata; 4] {
        [
            height(SpatialChannel::TopFrontLeft, 1, gain),
            height(SpatialChannel::TopFrontRight, 2, gain),
            height(SpatialChannel::TopBackLeft, 4, gain),
            height(SpatialChannel::TopBackRight, 5, gain),
        ]
    }

    fn state_with(metadata: Option<XMetadata>) -> DtsXState {
        DtsXState {
            last_metadata: metadata,
            ..DtsXState::default()
        }
    }

    fn build(hd: &HdFrame, state: &mut DtsXState) -> (RDecodedFrame, bool) {
        let mut declared = None;
        build_hd_frame_with_extensions(hd, state, &DtsFoldConfig::default(), 0, &mut declared)
            .expect("valid frame")
    }

    fn build_without_estimation(hd: &HdFrame, state: &mut DtsXState) -> (RDecodedFrame, bool) {
        let mut declared = None;
        let config = DtsFoldConfig {
            estimate_unknown: false,
            ..DtsFoldConfig::default()
        };
        build_hd_frame_with_extensions(hd, state, &config, 0, &mut declared).expect("valid frame")
    }

    /// Deterministic pseudo-noise in [-1, 1] (splitmix64).
    fn noise(seed: u64, length: usize) -> Vec<f32> {
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        (0..length)
            .map(|_| {
                state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
                let mut z = state;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                z ^= z >> 31;
                ((z >> 33) as f32 / (1u64 << 31) as f32) * 2.0 - 1.0
            })
            .collect()
    }

    fn folded(dry: &[f32], sources: &[(&[f32], f32)]) -> Vec<f32> {
        dry.iter()
            .enumerate()
            .map(|(s, &value)| {
                value
                    + sources
                        .iter()
                        .map(|(source, gain)| source[s] * gain)
                        .sum::<f32>()
            })
            .collect()
    }

    fn full_bed() -> Vec<Option<Vec<f32>>> {
        let mut samples: Vec<Option<Vec<f32>>> = (0..9).map(|_| None).collect();
        for idx in [0usize, 1, 2, 3, 4, 5, 7, 8] {
            samples[idx] = Some(vec![0.01 * idx as f32, -0.01 * idx as f32]);
        }
        samples
    }

    #[test]
    fn standard_heights_are_removed_from_the_bed_with_the_stated_gain() {
        let heights_pcm = [
            vec![0.40, -0.20],
            vec![-0.30, 0.10],
            vec![0.20, -0.40],
            vec![-0.10, 0.30],
        ];
        let dry = [
            vec![0.05, -0.06],
            vec![-0.07, 0.08],
            vec![0.09, -0.10],
            vec![-0.11, 0.12],
        ];
        // The stream states unity here, unlike the bundled -3 dB fallback.
        let mut samples = full_bed();
        samples[1] = Some(folded(&dry[0], &[(&heights_pcm[0], 1.0)]));
        samples[2] = Some(folded(&dry[1], &[(&heights_pcm[1], 1.0)]));
        samples[7] = Some(folded(&dry[2], &[(&heights_pcm[2], 1.0)]));
        samples[8] = Some(folded(&dry[3], &[(&heights_pcm[3], 1.0)]));
        let hd = hd_frame(samples, heights_pcm.to_vec());

        let mut state = state_with(XMetadata::from_sources(&heights(1.0)));
        let (frame, emitted) = build(&hd, &mut state);
        assert!(!emitted, "a fixed presentation carries no objects");
        assert_eq!(state.locked, Some(XPresentation::Height));
        assert_eq!(frame.channel_count, 12);
        assert_eq!(
            frame.channel_labels.as_slice(),
            &[
                RChannelLabel::C,
                RChannelLabel::L,
                RChannelLabel::R,
                RChannelLabel::Ls,
                RChannelLabel::Rs,
                RChannelLabel::LFE,
                RChannelLabel::Lb,
                RChannelLabel::Rb,
                RChannelLabel::Tfl,
                RChannelLabel::Tfr,
                RChannelLabel::Tbl,
                RChannelLabel::Tbr,
            ]
        );
        assert!(
            frame.metadata.is_empty(),
            "a fixed presentation must not fabricate metadata"
        );
        for sample in 0..SAMPLE_COUNT {
            let row = &frame.pcm[sample * 12..(sample + 1) * 12];
            assert_pcm_close(row[1], dry[0][sample]);
            assert_pcm_close(row[2], dry[1][sample]);
            assert_pcm_close(row[6], dry[2][sample]);
            assert_pcm_close(row[7], dry[3][sample]);
            for height in 0..4 {
                assert_pcm_close(row[8 + height], hd.x_samples[height][sample]);
            }
        }
    }

    #[test]
    fn unreadable_standard_metadata_falls_back_to_the_configured_gain() {
        let left_height = vec![0.40, -0.20];
        let dry = vec![0.05, -0.06];
        let mut samples = full_bed();
        samples[1] = Some(folded(&dry, &[(&left_height, Q55)]));
        let hd = hd_frame(samples, vec![left_height; 4]);

        let mut state = DtsXState::default();
        let (frame, _) = build(&hd, &mut state);
        assert_eq!(frame.channel_count, 12);
        assert_eq!(state.parse_failures.count(), 1);
        for sample in 0..SAMPLE_COUNT {
            assert_pcm_close(frame.pcm[sample * 12 + 1], dry[sample]);
        }
    }

    #[test]
    fn invalid_quartet_keeps_the_compatible_bed_unchanged() {
        let composite_left = vec![0.25, -0.25];
        let mut samples = full_bed();
        samples[1] = Some(composite_left.clone());
        let hd = hd_frame(samples, vec![vec![0.5; SAMPLE_COUNT]; 3]);

        // Not locked yet: an invalid quartet keeps the plain 8-channel bed.
        let mut state = DtsXState::default();
        let (frame, _) = build(&hd, &mut state);
        assert_eq!(state.locked, None, "an invalid quartet must not latch");
        assert_eq!(frame.channel_count, 8);
        assert!(frame.metadata.is_empty());
        for sample in 0..SAMPLE_COUNT {
            assert_eq!(
                frame.pcm[sample * 8 + 1],
                float_to_pcm_i32(composite_left[sample])
            );
        }
    }

    #[test]
    fn locked_stream_keeps_shape_with_silent_feeds_on_dropout() {
        let composite_left = vec![0.25, -0.25];
        let mut samples = full_bed();
        samples[1] = Some(composite_left.clone());
        // Invalid quartet (3 channels) after a previous frame locked heights.
        let hd = hd_frame(samples, vec![vec![0.5; SAMPLE_COUNT]; 3]);

        let mut state = state_with(XMetadata::from_sources(&heights(Q55)));
        state.locked = Some(XPresentation::Height);
        let (frame, _) = build(&hd, &mut state);

        // Stable 12-channel shape: composite bed (nothing to subtract without
        // a quartet) + silent height channels — the host never renegotiates.
        assert_eq!(frame.channel_count, 12);
        assert!(frame.metadata.is_empty());
        assert_eq!(state.feed_dropouts.count(), 1);
        for sample in 0..SAMPLE_COUNT {
            let row = &frame.pcm[sample * 12..(sample + 1) * 12];
            assert_eq!(row[1], float_to_pcm_i32(composite_left[sample]));
            assert!(row[8..12].iter().all(|&s| s == 0));
        }
    }

    #[test]
    fn d3_unfolds_objects_and_heights_and_positions_the_objects() {
        let extension: Vec<Vec<f32>> = (0..8)
            .map(|index| vec![0.01 * (index + 1) as f32, -0.02 * (index + 1) as f32])
            .collect();
        let code21 = gain_code_linear(21).unwrap();
        let dry_lb = vec![0.10, -0.10];
        let dry_l = vec![0.20, -0.20];
        let mut samples: Vec<Option<Vec<f32>>> = (0..9).map(|_| None).collect();
        samples[0] = Some(vec![0.30, -0.30]);
        // L carries TFL (feed 4) at -3 dB; Lb carries objects 0 (unity), 1
        // (code 21) and TBL (feed 6) at -3 dB.
        samples[1] = Some(folded(&dry_l, &[(&extension[4], Q55)]));
        samples[7] = Some(folded(
            &dry_lb,
            &[
                (&extension[0], 1.0),
                (&extension[1], code21),
                (&extension[6], Q55),
            ],
        ));
        let mut hd = hd_frame(samples, extension);
        hd.x_present = false;
        hd.x_imax = true;

        let mut columns0 = [0.0; 8];
        columns0[4] = 1.0;
        let mut columns1 = [0.0; 8];
        columns1[4] = code21;
        let sources = [
            object(-291, 57, BedFold::Known(columns0)),
            object(291, 54, BedFold::Known(columns1)),
            object(-300, 0, BedFold::Known([0.0; 8])),
            object(300, 0, BedFold::Known([0.0; 8])),
            heights(Q55)[0],
            heights(Q55)[1],
            heights(Q55)[2],
            heights(Q55)[3],
        ];
        let mut state = state_with(XMetadata::from_sources(&sources));
        let mut declared = None;
        let (frame, emitted) = build_hd_frame_with_extensions(
            &hd,
            &mut state,
            &DtsFoldConfig::default(),
            48_000,
            &mut declared,
        )
        .expect("valid D3 frame");

        assert!(emitted);
        assert_eq!(state.locked, Some(XPresentation::ObjectsD3));
        assert_eq!(frame.channel_count, 3 + 8);
        assert_eq!(
            frame.channel_labels.as_slice(),
            &[
                RChannelLabel::C,
                RChannelLabel::L,
                RChannelLabel::Lb,
                RChannelLabel::Tfl,
                RChannelLabel::Tfr,
                RChannelLabel::Tbl,
                RChannelLabel::Tbr,
                RChannelLabel::Object,
                RChannelLabel::Object,
                RChannelLabel::Object,
                RChannelLabel::Object,
            ]
        );
        for sample in 0..SAMPLE_COUNT {
            let row = &frame.pcm[sample * 11..(sample + 1) * 11];
            assert_pcm_close(row[0], 0.30 * if sample == 0 { 1.0 } else { -1.0 });
            assert_pcm_close(row[1], dry_l[sample]);
            assert_pcm_close(row[2], dry_lb[sample]);
            for (slot, feed) in (4..8).chain(0..4).enumerate() {
                assert_pcm_close(row[3 + slot], hd.x_samples[feed][sample]);
            }
        }

        assert_eq!(frame.metadata.len(), 1);
        let metadata = &frame.metadata[0];
        assert_eq!(metadata.sample_pos, 48_000);
        assert_eq!(metadata.events.len(), 4);
        assert_eq!(metadata.object_channels.len(), 4);
        assert_eq!(metadata.name_updates.len(), 4);
        for feed in 0..4 {
            assert_eq!(metadata.object_channels[feed].id, 10 + feed as u32);
            assert_eq!(metadata.object_channels[feed].channel, (7 + feed) as u32);
            assert_eq!(
                metadata.name_updates[feed].name.as_str(),
                format!("X{feed}")
            );
            let event = &metadata.events[feed];
            assert_eq!(event.id, 10 + feed as u32);
            assert!(event.has_pos);
            let SourceRole::Object { position, .. } = sources[feed].role else {
                panic!()
            };
            assert_eq!(event.pos, position.to_adm_cartesian());
        }
        // Rear-left object: negative x, negative y, raised.
        assert!(metadata.events[0].pos[0] < 0.0 && metadata.events[0].pos[1] < 0.0);
        assert!(metadata.events[0].pos[2] > 0.0);

        // One object moves: every object is announced again, so the engine's
        // per-frame object list keeps its size and nothing flickers.
        let mut moved = sources;
        moved[2] = object(-280, 20, BedFold::Known([0.0; 8]));
        state.last_metadata = XMetadata::from_sources(&moved);
        let mut moved_hd = hd_frame(hd.samples.clone(), hd.x_samples.clone());
        moved_hd.x_present = false;
        moved_hd.x_imax = true;
        let (frame_moved, _) = build_hd_frame_with_extensions(
            &moved_hd,
            &mut state,
            &DtsFoldConfig::default(),
            48_000 + 1024,
            &mut declared,
        )
        .expect("valid D3 frame");
        assert_eq!(frame_moved.metadata.len(), 1);
        assert_eq!(
            frame_moved.metadata[0].events.len(),
            4,
            "all objects, not only the mover"
        );
        assert!(
            frame_moved.metadata[0].object_channels.is_empty(),
            "declaration unchanged"
        );

        // Same positions again: the declaration is cached and no event is due
        // between heartbeats.
        let (again, _) = build_hd_frame_with_extensions(
            &hd,
            &mut state,
            &DtsFoldConfig::default(),
            48_000 + 512,
            &mut declared,
        )
        .expect("valid D3 frame");
        assert!(again.metadata.is_empty());
    }

    /// D0's single feed is the object its record declares, unfolded out of the
    /// bed and played at the position it states — here 25.5 degrees above the
    /// centre, with the centre-height speaker named only as an alternative.
    /// It used to be presented as a fixed top-front-centre channel whatever
    /// the record said, which two other streams under the same marker
    /// contradict.
    #[test]
    fn d0_plays_its_object_at_the_declared_position_and_unfolds_it() {
        let extension: Vec<Vec<f32>> = (0..5)
            .map(|index| vec![0.01 * (index + 1) as f32, -0.01 * (index + 1) as f32])
            .collect();
        let code58 = gain_code_linear(58).unwrap();
        let code46 = gain_code_linear(46).unwrap();
        let dry = [
            vec![0.10, -0.10],
            vec![0.20, -0.20],
            vec![0.30, -0.30],
            vec![0.40, -0.40],
            vec![0.50, -0.50],
        ];
        let mut samples = full_bed();
        samples[0] = Some(folded(&dry[0], &[(&extension[0], code58)]));
        samples[1] = Some(folded(
            &dry[1],
            &[(&extension[0], code46), (&extension[1], 1.0)],
        ));
        samples[2] = Some(folded(
            &dry[2],
            &[(&extension[0], code46), (&extension[2], 1.0)],
        ));
        samples[7] = Some(folded(&dry[3], &[(&extension[3], 1.0)]));
        samples[8] = Some(folded(&dry[4], &[(&extension[4], 1.0)]));
        let mut hd = hd_frame(samples, extension);
        hd.x_present = false;
        hd.x_imax = true;

        let mut columns = [0.0; 8];
        columns[0] = code58;
        columns[1] = code46;
        columns[2] = code46;
        let mut sources = vec![SourceMetadata {
            role: SourceRole::Object {
                position: SphericalPosition {
                    azimuth_half_degrees: 0,
                    elevation_half_degrees: 51,
                    distance_64ths: 64,
                },
                centre_height_alternative: true,
            },
            fold: BedFold::Known(columns),
        }];
        sources.extend(heights(1.0));
        let mut state = state_with(XMetadata::from_sources(&sources));
        let (frame, emitted) = build(&hd, &mut state);

        assert!(emitted, "the D0 object is announced to the engine");
        assert_eq!(state.locked, Some(XPresentation::ObjectD0));
        assert_eq!(frame.channel_count, 13);
        assert_eq!(
            frame.channel_labels.as_slice(),
            &[
                RChannelLabel::C,
                RChannelLabel::L,
                RChannelLabel::R,
                RChannelLabel::Ls,
                RChannelLabel::Rs,
                RChannelLabel::LFE,
                RChannelLabel::Lb,
                RChannelLabel::Rb,
                RChannelLabel::Tfl,
                RChannelLabel::Tfr,
                RChannelLabel::Tbl,
                RChannelLabel::Tbr,
                RChannelLabel::Object,
            ]
        );
        for sample in 0..SAMPLE_COUNT {
            let row = &frame.pcm[sample * 13..(sample + 1) * 13];
            for channel in 0..3 {
                assert_pcm_close(row[channel], dry[channel][sample]);
            }
            assert_pcm_close(row[3], hd.samples[3].as_ref().unwrap()[sample]);
            assert_pcm_close(row[6], dry[3][sample]);
            assert_pcm_close(row[7], dry[4][sample]);
            // The four heights, then the object last.
            for (slot, feed) in (1..5).chain(0..1).enumerate() {
                assert_pcm_close(row[8 + slot], hd.x_samples[feed][sample]);
            }
        }

        assert_eq!(frame.metadata.len(), 1);
        let metadata = &frame.metadata[0];
        assert_eq!(metadata.object_channels.len(), 1);
        assert_eq!(
            metadata.object_channels[0].channel, 12,
            "the object is the last channel"
        );
        assert_eq!(metadata.events.len(), 1);
        let event = &metadata.events[0];
        assert!(event.has_pos);
        let SourceRole::Object { position, .. } = sources[0].role else {
            panic!("feed 0 is the declared object")
        };
        assert_eq!(event.pos, position.to_adm_cartesian());
        assert!(
            event.pos[2] > 0.0 && event.pos[0].abs() < 1e-6,
            "above the centre, not off to a side"
        );
    }

    #[test]
    fn d1_objects_without_a_stated_fold_are_estimated_out_of_the_bed_and_played() {
        let n = 2048;
        let extension: Vec<Vec<f32>> = (0..6).map(|i| noise(100 + i, n)).collect();
        let dry: Vec<Vec<f32>> = (0..9)
            .map(|i| noise(200 + i, n).iter().map(|v| v * 0.3).collect())
            .collect();
        let mut samples: Vec<Option<Vec<f32>>> = (0..9).map(|_| None).collect();
        for idx in [0usize, 2, 3, 5, 7, 8] {
            samples[idx] = Some(dry[idx].clone());
        }
        // L holds object 0 at 0.92 plus TFL (feed 2) at the stated Q55; Rs
        // holds object 1 at 0.34.
        samples[1] = Some(folded(
            &dry[1],
            &[(&extension[0], 0.9152), (&extension[2], Q55)],
        ));
        samples[4] = Some(folded(&dry[4], &[(&extension[1], 0.34)]));
        let mut hd = hd_frame(samples, extension);
        hd.xll_segment_samples = n;
        hd.x_present = false;
        hd.x_imax = true;

        let mut sources = vec![
            object(-69, 24, BedFold::Unknown),
            object(69, 24, BedFold::Unknown),
        ];
        sources.extend(heights(Q55));
        let mut state = state_with(XMetadata::from_sources(&sources));
        let (frame, emitted) = build(&hd, &mut state);

        assert!(emitted);
        assert_eq!(frame.channel_count, 8 + 6);
        let mut l_residual = 0.0f64;
        let mut rs_residual = 0.0f64;
        for sample in 0..n {
            let row = &frame.pcm[sample * 14..(sample + 1) * 14];
            l_residual += f64::from(row[1] as f32 / 8_388_608.0 - dry[1][sample]).powi(2);
            rs_residual += f64::from(row[4] as f32 / 8_388_608.0 - dry[4][sample]).powi(2);
            // Both objects now play on their own channels.
            assert_pcm_close(row[12], hd.x_samples[0][sample]);
            assert_pcm_close(row[13], hd.x_samples[1][sample]);
        }
        // What is left in L and Rs is the dry content: the objects are gone to
        // within the fit's noise (-30 dB of the dry level at least).
        let dry_l: f64 = dry[1].iter().map(|v| f64::from(*v).powi(2)).sum();
        let dry_rs: f64 = dry[4].iter().map(|v| f64::from(*v).powi(2)).sum();
        assert!(
            l_residual / dry_l < 5e-3,
            "L residual {}",
            l_residual / dry_l
        );
        assert!(
            rs_residual / dry_rs < 5e-3,
            "Rs residual {}",
            rs_residual / dry_rs
        );
    }

    #[test]
    fn d1_objects_rendered_from_their_position_leave_the_bed_without_estimation() {
        let n = 1024;
        let extension: Vec<Vec<f32>> = (0..6).map(|i| noise(300 + i, n)).collect();
        let dry: Vec<Vec<f32>> = (0..9)
            .map(|i| noise(400 + i, n).iter().map(|v| v * 0.3).collect())
            .collect();
        // The encoder's render of the two mode-0 objects at (-+34.5, 12) on
        // 7.1, as measured in the corpus, by DCA speaker.
        let left = [
            (0usize, 0.1305f32),
            (1, 0.9152),
            (2, 0.1305),
            (3, 0.3430),
            (7, 0.1037),
        ];
        let mirror = |speaker: usize| match speaker {
            1 => 2,
            2 => 1,
            3 => 4,
            7 => 8,
            other => other,
        };
        let mut samples: Vec<Option<Vec<f32>>> = (0..9).map(|_| None).collect();
        for speaker in [0usize, 1, 2, 3, 4, 5, 7, 8] {
            let mut parts: Vec<(&[f32], f32)> = Vec::new();
            for &(s, g) in &left {
                if s == speaker {
                    parts.push((extension[0].as_slice(), g));
                }
                if mirror(s) == speaker {
                    parts.push((extension[1].as_slice(), g));
                }
            }
            samples[speaker] = Some(folded(&dry[speaker], &parts));
        }
        let mut hd = hd_frame(samples, extension);
        hd.xll_segment_samples = n;
        hd.x_present = false;
        hd.x_imax = true;

        let mut sources = vec![
            object(-69, 24, BedFold::Unknown),
            object(69, 24, BedFold::Unknown),
        ];
        sources.extend(heights(0.0));
        let mut metadata = XMetadata::from_sources(&sources).unwrap();
        metadata.mark_rendered(0, false);
        metadata.mark_rendered(1, false);
        let mut state = state_with(Some(metadata));
        let (frame, emitted) = build_without_estimation(&hd, &mut state);

        assert!(emitted);
        let width = frame.channel_count as usize;
        for (column, speaker) in [0usize, 1, 2, 3, 4, 5, 7, 8].into_iter().enumerate() {
            for sample in 0..n {
                let got = frame.pcm[sample * width + column] as f32 / 8_388_608.0;
                assert!(
                    (got - dry[speaker][sample]).abs() < 2e-4,
                    "speaker {speaker} sample {sample}: {got} vs {}",
                    dry[speaker][sample]
                );
            }
        }
        for sample in 0..n {
            let row = &frame.pcm[sample * width..(sample + 1) * width];
            assert_pcm_close(row[12], hd.x_samples[0][sample]);
            assert_pcm_close(row[13], hd.x_samples[1][sample]);
        }
    }

    #[test]
    fn d1_objects_without_a_stated_fold_stay_in_the_bed_and_are_muted_without_estimation() {
        let extension: Vec<Vec<f32>> = (0..6)
            .map(|index| vec![0.01 * (index + 1) as f32, -0.01 * (index + 1) as f32])
            .collect();
        let dry_l = vec![0.20, -0.20];
        let mut samples = full_bed();
        // L holds the mode-0 object at an unstated gain plus TFL (feed 2).
        samples[1] = Some(folded(
            &dry_l,
            &[(&extension[0], 0.9152), (&extension[2], Q55)],
        ));
        let mut hd = hd_frame(samples, extension);
        hd.x_present = false;
        hd.x_imax = true;

        let mut sources = vec![
            object(-69, 24, BedFold::Unknown),
            object(69, 24, BedFold::Unknown),
        ];
        sources.extend(heights(Q55));
        let mut state = state_with(XMetadata::from_sources(&sources));
        let (frame, emitted) = build_without_estimation(&hd, &mut state);

        assert!(emitted, "D1 declares object channels");
        assert_eq!(state.locked, Some(XPresentation::ObjectsD1));
        assert_eq!(frame.channel_count, 8 + 6);
        assert_eq!(
            &frame.channel_labels[8..],
            &[
                RChannelLabel::Tfl,
                RChannelLabel::Tfr,
                RChannelLabel::Tbl,
                RChannelLabel::Tbr,
                RChannelLabel::Object,
                RChannelLabel::Object,
            ]
        );
        for sample in 0..SAMPLE_COUNT {
            let row = &frame.pcm[sample * 14..(sample + 1) * 14];
            // Only the height leaves L; the object's share stays.
            assert_pcm_close(row[1], dry_l[sample] + hd.x_samples[0][sample] * 0.9152);
            for (slot, feed) in (2..6).enumerate() {
                assert_pcm_close(row[8 + slot], hd.x_samples[feed][sample]);
            }
            assert_eq!(row[12], 0, "an unfoldable object must not play twice");
            assert_eq!(row[13], 0);
        }
        assert_eq!(frame.metadata.len(), 1);
        assert_eq!(
            frame.metadata[0].events.len(),
            2,
            "positions are still announced"
        );
        assert_eq!(frame.metadata[0].object_channels.len(), 2);
    }

    #[test]
    fn three_component_d0_form_presents_the_bed_plus_three_objects_and_heights() {
        let component = vec![0.20, -0.10];
        let copy: Vec<f32> = component.iter().map(|s| s * 0.365).collect();
        let heights_pcm: Vec<Vec<f32>> =
            (0..4).map(|k| noise(7 + k as u64, SAMPLE_COUNT)).collect();
        let dry_c = vec![0.05, -0.05];
        let dry_l = vec![0.07, -0.02];
        let dry_r = vec![-0.03, 0.06];
        let mut samples = full_bed();
        samples[0] = Some(folded(&dry_c, &[(&component, 1.0)]));
        samples[1] = Some(folded(&dry_l, &[(&copy, 1.0), (&heights_pcm[0], Q55)]));
        samples[2] = Some(folded(&dry_r, &[(&copy, 1.0), (&heights_pcm[1], Q55)]));
        let mut extension = vec![component.clone(), copy.clone(), copy.clone()];
        extension.extend(heights_pcm.iter().cloned());
        let mut hd = hd_frame(samples, extension);
        hd.x_present = false;
        hd.x_imax = true;

        let unity = |column: usize| {
            let mut columns = [0.0; 8];
            columns[column] = 1.0;
            BedFold::Known(columns)
        };
        let sources = [
            object(0, 0, unity(0)),
            object(-60, 0, unity(1)),
            object(60, 0, unity(2)),
            heights(Q55)[0],
            heights(Q55)[1],
            heights(Q55)[2],
            heights(Q55)[3],
        ];
        let mut state = state_with(XMetadata::from_sources(&sources));
        let (frame, emitted) = build(&hd, &mut state);

        assert!(emitted, "the components are object channels");
        assert_eq!(state.locked, Some(XPresentation::ObjectsD0));
        assert_eq!(frame.channel_count, 8 + 4 + 3);
        assert_eq!(
            &frame.channel_labels.as_slice()[8..],
            &[
                RChannelLabel::Tfl,
                RChannelLabel::Tfr,
                RChannelLabel::Tbl,
                RChannelLabel::Tbr,
                RChannelLabel::Object,
                RChannelLabel::Object,
                RChannelLabel::Object,
            ]
        );
        for sample in 0..SAMPLE_COUNT {
            let row = &frame.pcm[sample * 15..(sample + 1) * 15];
            assert_pcm_close(row[0], dry_c[sample]);
            assert_pcm_close(row[1], dry_l[sample]);
            assert_pcm_close(row[2], dry_r[sample]);
            for (slot, feed) in (3..7).chain(0..3).enumerate() {
                assert_pcm_close(row[8 + slot], hd.x_samples[feed][sample]);
            }
        }
        assert_eq!(frame.metadata.len(), 1);
        let metadata = &frame.metadata[0];
        assert_eq!(metadata.object_channels.len(), 3);
        assert_eq!(metadata.events.len(), 3);
        assert_eq!(metadata.events[0].pos[2], 0.0, "ear level");
        assert!(
            metadata.events[1].pos[0] < 0.0 && metadata.events[2].pos[0] > 0.0,
            "the left and right components sit on their sides"
        );
    }

    #[test]
    fn object_only_variant_presents_a_5_1_bed_plus_one_object() {
        let object_pcm = vec![0.20, -0.10];
        let dry_c = vec![0.05, -0.05];
        let dry_l = vec![0.07, -0.02];
        let code58 = gain_code_linear(58).unwrap();
        let code46 = gain_code_linear(46).unwrap();
        let mut samples: Vec<Option<Vec<f32>>> = (0..9).map(|_| None).collect();
        samples[0] = Some(folded(&dry_c, &[(&object_pcm, code58)]));
        samples[1] = Some(folded(&dry_l, &[(&object_pcm, code46)]));
        samples[2] = Some(vec![0.03, -0.03]);
        samples[3] = Some(vec![0.04, -0.04]);
        samples[4] = Some(vec![0.06, -0.06]);
        samples[5] = Some(vec![0.08, -0.08]);
        let mut hd = hd_frame(samples, vec![object_pcm.clone()]);
        hd.x_present = false;
        hd.x_imax = true;

        let mut columns = [0.0; 8];
        columns[0] = code58;
        columns[1] = code46;
        columns[2] = code46;
        let source = object(0, 51, BedFold::Known(columns));
        let mut state = state_with(XMetadata::from_sources_on(
            &[source],
            dca::REFERENCE_MASK_5_1,
        ));
        let (frame, emitted) = build(&hd, &mut state);

        assert!(emitted, "the object is an object channel");
        assert_eq!(state.locked, Some(XPresentation::ObjectOnly));
        assert_eq!(frame.channel_count, 7);
        assert_eq!(
            frame.channel_labels.as_slice(),
            &[
                RChannelLabel::C,
                RChannelLabel::L,
                RChannelLabel::R,
                RChannelLabel::Ls,
                RChannelLabel::Rs,
                RChannelLabel::LFE,
                RChannelLabel::Object,
            ]
        );
        for sample in 0..SAMPLE_COUNT {
            let row = &frame.pcm[sample * 7..(sample + 1) * 7];
            assert_pcm_close(row[0], dry_c[sample]);
            assert_pcm_close(row[1], dry_l[sample]);
            assert_pcm_close(row[3], hd.samples[3].as_ref().unwrap()[sample]);
            assert_pcm_close(row[6], object_pcm[sample]);
        }
        assert_eq!(frame.metadata.len(), 1);
        let metadata = &frame.metadata[0];
        assert_eq!(metadata.object_channels.len(), 1);
        assert_eq!(metadata.object_channels[0].channel, 6);
        assert_eq!(metadata.events.len(), 1);
        assert!(metadata.events[0].pos[2] > 0.0, "the object is raised");
    }

    #[test]
    fn alternate_frame_without_any_readable_metadata_mutes_its_feeds() {
        let extension: Vec<Vec<f32>> = (0..8).map(|_| vec![0.1; SAMPLE_COUNT]).collect();
        let composite_left = vec![0.25, -0.25];
        let mut samples = full_bed();
        samples[1] = Some(composite_left.clone());
        let mut hd = hd_frame(samples, extension);
        hd.x_present = false;
        hd.x_imax = true;

        let mut state = DtsXState::default();
        let (frame, emitted) = build_without_estimation(&hd, &mut state);
        assert!(emitted);
        assert_eq!(frame.channel_count, 16);
        assert_eq!(state.parse_failures.count(), 1);
        for sample in 0..SAMPLE_COUNT {
            let row = &frame.pcm[sample * 16..(sample + 1) * 16];
            assert_eq!(row[1], float_to_pcm_i32(composite_left[sample]));
            assert!(row[8..16].iter().all(|&s| s == 0));
        }
        assert_eq!(frame.metadata.len(), 1, "channels are still declared");
        assert!(
            frame.metadata[0].events.is_empty(),
            "no positions to announce"
        );
    }

    #[test]
    fn mismatched_bed_channel_length_drops_the_frame() {
        let mut samples: Vec<Option<Vec<f32>>> = (0..9).map(|_| None).collect();
        samples[0] = Some(vec![0.0; SAMPLE_COUNT]);
        samples[1] = Some(vec![0.0; SAMPLE_COUNT + 1]); // corrupt length
        let hd = hd_frame(samples, Vec::new());
        let mut state = DtsXState::default();
        let mut declared = None;
        assert!(
            build_hd_frame_with_extensions(
                &hd,
                &mut state,
                &DtsFoldConfig::default(),
                0,
                &mut declared
            )
            .is_none()
        );
    }
}
