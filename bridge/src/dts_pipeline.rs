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
// reference rows) stays in the bed and its own channel is silent.

use abi_stable::std_types::{RString, RVec};
use bridge_api::RPushResult;
use bridge_api::{
    RChannelLabel, RDecodedFrame, REvent, RMetadataFrame, RNameUpdate, RObjectChannel,
};
use dca::{
    CorePcmFrame, FoldPlan, HdError, HdFrame, MAX_SOURCES, SourceRole, SphericalPosition,
    XMetadata, XPresentation, exss_has_xll, exss_substream_size, parse_header,
};
use serde::Deserialize;

use crate::bridge::AtmosBridge;
use crate::frame_builders::float_to_pcm_i32;
use crate::labels::{dca_bed_channel_to_r, dca_spatial_channel_to_r};
use crate::metadata::declare_object_channels;

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
    parse_failures: u64,
    feed_dropouts: u64,
}

impl DtsXState {
    fn note_parse_failure(&mut self, error: dca::XMetadataError) {
        self.parse_failures += 1;
        if self.parse_failures == 1 || self.parse_failures.is_power_of_two() {
            log::warn!(
                "dts: extension metadata unreadable ({error:?}, {} frames so far); {}",
                self.parse_failures,
                if self.last_metadata.is_some() {
                    "reusing the last readable frame"
                } else {
                    "keeping the bed as authored and muting the extension channels"
                }
            );
        }
    }

    fn note_feed_dropout(&mut self, reason: &str) {
        self.feed_dropouts += 1;
        if self.feed_dropouts == 1 || self.feed_dropouts.is_power_of_two() {
            log::warn!(
                "dts: extension waveforms unavailable ({reason}, {} frames so far); emitting silent extension channels",
                self.feed_dropouts
            );
        }
    }
}

/// Demux and decode all complete DTS frames buffered in `bridge.dts_buf`.
pub(crate) fn drain_dts(bridge: &mut AtmosBridge, result: &mut RPushResult) {
    let mut consumed = 0usize;
    loop {
        let rest = &bridge.dts_buf[consumed..];
        // Locate the next core syncword.
        let Some(sync_off) = find(rest, &CORE_SYNC) else {
            // Keep only a possible partial trailing syncword.
            consumed += rest.len().saturating_sub(3);
            break;
        };
        consumed += sync_off;
        let rest = &bridge.dts_buf[consumed..];

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
            if exss_has_xll(&rest[fs..fs + es]) {
                match bridge
                    .dts_hd_decoder
                    .decode(&rest[..fs], &rest[fs..fs + es])
                {
                    Ok(hd) => {
                        let n = hd_samples(&hd);
                        if let Some((frame, emitted_objects)) = build_hd_frame_with_extensions(
                            &hd,
                            &mut bridge.dts_x,
                            &bridge.dts_fold_config,
                            bridge.total_samples,
                            &mut bridge.declared_object_channels,
                        ) {
                            bridge.dts_objects_active = emitted_objects;
                            // The Auro side channel lives in the low bits of
                            // the lossless integers, read off the decoder's
                            // integer tap. The stage decides whether the
                            // frame goes out as it is or unfolded.
                            let active: Vec<usize> = (0..hd.samples.len())
                                .filter(|&s| hd.samples[s].is_some())
                                .collect();
                            bridge.dts_auro.route(
                                frame,
                                &active,
                                bridge.dts_hd_decoder.lossless_samples(),
                                &mut result.frames,
                            );
                        }
                        bridge.total_samples += n as u64;
                        bridge.dts_frame_count += 1;
                    }
                    Err(HdError::Pending) => {} // PBR buffering; no frame this packet
                    Err(e) => {
                        let msg = format!("dts_hd_decode_error={e:?}");
                        log::warn!("{msg}");
                        if bridge.strict {
                            result.error_message = RString::from(msg);
                            bridge.reset_pipeline();
                            result.did_reset = true;
                            return;
                        }
                    }
                }
            } else {
                // No XLL asset: DTS-HD HRA (and other lossy EXSS extensions) layer
                // only high-frequency detail on top of an ordinary DTS core. We do
                // not decode that extension, so render the core (5.1) and drop it,
                // instead of failing the whole track.
                match bridge.dts_decoder.push_access_unit(&rest[..fs]) {
                    Ok(push) => {
                        bridge.dts_objects_active = false;
                        bridge.dts_auro.not_a_carrier(&mut result.frames);
                        result.frames.push(build_core_frame(&push.pcm));
                        bridge.total_samples += push.pcm.samples_per_channel() as u64;
                        bridge.dts_frame_count += 1;
                    }
                    Err(err) => {
                        let msg = format!("dts_decode_error={err}");
                        log::warn!("{msg}");
                        if bridge.strict {
                            result.error_message = RString::from(msg);
                            bridge.reset_pipeline();
                            result.did_reset = true;
                            return;
                        }
                    }
                }
            }
            consumed += fs + es;
        } else {
            match bridge.dts_decoder.push_access_unit(&rest[..fs]) {
                Ok(push) => {
                    let frame = build_core_frame(&push.pcm);
                    bridge.dts_objects_active = false;
                    bridge.dts_auro.not_a_carrier(&mut result.frames);
                    bridge.total_samples += push.pcm.samples_per_channel() as u64;
                    bridge.dts_frame_count += 1;
                    result.frames.push(frame);
                }
                Err(err) => {
                    let msg = format!("dts_decode_error={err}");
                    log::warn!("{msg}");
                    if bridge.strict {
                        result.error_message = RString::from(msg);
                        bridge.reset_pipeline();
                        result.did_reset = true;
                        return;
                    }
                }
            }
            consumed += fs;
        }
    }

    if consumed > 0 {
        bridge.dts_buf.drain(..consumed.min(bridge.dts_buf.len()));
    }
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
            log::warn!(
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
                log::warn!(
                    "dts: extension presentation changed {:?} -> {detected:?}; channel shape changes",
                    state.locked
                );
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
    let plan = if detected.is_some() {
        resolve_plan(hd, presentation, state, fold_config)
    } else {
        FoldPlan::all_unknown(presentation.feed_count())
    };
    let metadata = detected.and(state.last_metadata);

    let fixed_feeds = presentation.fixed_feeds();
    let object_feeds = presentation.object_feeds();
    let channel_count = active.len() + presentation.feed_count();

    let mut pcm: RVec<i32> = RVec::with_capacity(sample_count * channel_count);
    for s in 0..sample_count {
        for (channel, &spkr) in bed.iter().zip(&active) {
            pcm.push(float_to_pcm_i32(plan.clean(spkr, channel[s], s, feeds)));
        }
        for feed in fixed_feeds.clone().chain(object_feeds.clone()) {
            let sample = match feeds.get(feed) {
                Some(waveform) if plan.source_is_known(feed) => waveform[s],
                _ => 0.0,
            };
            pcm.push(float_to_pcm_i32(sample));
        }
    }

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

/// A frame with no extension presentation: the lossless bed as decoded.
fn bed_only_frame(
    hd: &HdFrame,
    active: &[usize],
    bed: &[&[f32]],
    sample_count: usize,
) -> RDecodedFrame {
    let mut pcm: RVec<i32> = RVec::with_capacity(sample_count * active.len());
    for s in 0..sample_count {
        for channel in bed {
            pcm.push(float_to_pcm_i32(channel[s]));
        }
    }
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
        log::info!(
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

    let mut pcm: RVec<i32> = RVec::with_capacity(sample_count * total_channel_count);
    for s in 0..sample_count {
        for ch in &core.fullband_channels {
            pcm.push(float_to_pcm_i32(ch[s]));
        }
        if let Some(lfe) = &core.lfe_channel {
            pcm.push(float_to_pcm_i32(lfe[s]));
        }
    }
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
        assert_eq!(state.parse_failures, 1);
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
        assert_eq!(state.feed_dropouts, 1);
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

    #[test]
    fn d0_presents_its_object_as_a_fixed_channel_and_unfolds_it() {
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

        assert!(!emitted, "the D0 object is presented as a fixed channel");
        assert_eq!(state.locked, Some(XPresentation::FixedD0));
        assert!(frame.metadata.is_empty());
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
                RChannelLabel::Tfc,
                RChannelLabel::Tfl,
                RChannelLabel::Tfr,
                RChannelLabel::Tbl,
                RChannelLabel::Tbr,
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
            for source in 0..5 {
                assert_pcm_close(row[8 + source], hd.x_samples[source][sample]);
            }
        }
    }

    #[test]
    fn d1_objects_without_a_stated_fold_stay_in_the_bed_and_are_muted() {
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
        let (frame, emitted) = build(&hd, &mut state);

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
        let (frame, emitted) = build(&hd, &mut state);
        assert!(emitted);
        assert_eq!(frame.channel_count, 16);
        assert_eq!(state.parse_failures, 1);
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
