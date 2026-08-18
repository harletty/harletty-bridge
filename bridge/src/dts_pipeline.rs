// SPDX-License-Identifier: Apache-2.0
//
// DTS (DCA) raw transport pipeline. Demuxes the `[core][exss]` byte stream and
// routes each frame to either the DTS-HD MA lossless decoder (5.1/7.1, when an
// EXSS substream follows the core) or the plain DTS core decoder (5.1). Every
// decoded core channel is emitted as a bed channel, placed at its canonical
// speaker by the renderer. XLL-X's standard four-source profile is mapped to
// the four 7.1.4 height positions after undoing its -3 dB contribution to the
// backward-compatible 7.1 bed. Alternate decoded profiles use their current
// experimental presentations automatically.

use abi_stable::std_types::{RString, RVec};
use bridge_api::RPushResult;
use bridge_api::{
    RChannelLabel, RDecodedFrame, REvent, RMetadataFrame, RNameUpdate, RObjectChannel,
};
use dca::{
    CorePcmFrame, HdError, HdFrame, XPresentation, exss_has_xll, exss_substream_size, parse_header,
};
use serde::Deserialize;

use crate::bridge::AtmosBridge;
use crate::frame_builders::float_to_pcm_i32;
use crate::labels::{dca_bed_channel_to_r, dca_spatial_channel_to_r};
use crate::metadata::declare_object_channels;

const CORE_SYNC: [u8; 4] = 0x7FFE_8001u32.to_be_bytes();
const SUBSTREAM_SYNC: [u8; 4] = 0x6458_2025u32.to_be_bytes();
// Exact Q15 -3 dB coefficient used by the DTS downmix table. The regular 7.1
// presentation contains this contribution from each corresponding height feed.
#[cfg(test)]
const XLL_X_HEIGHT_DOWNMIX_GAIN: f32 = 23_170.0 / 32_768.0;

// Research-only D0 partial-unfold coefficients. The source-to-speaker
// associations are repeatable across the two measured programmes, but these
// gains are conservative presentation choices, not decoded metadata or a proven
// profile-exact matrix. In particular, the partial subtraction deliberately
// preserves some programme material authored in both the lower and top feeds.
#[cfg(test)]
const D0_TFC_CENTER_DOWNMIX_GAIN: f32 = 0.5;
#[cfg(test)]
const ALTERNATE_CORNER_HEIGHT_PARTIAL_UNFOLD_GAIN: f32 = 1.0 / std::f32::consts::SQRT_2;

// Research-only D1 compatibility-fold estimates, measured on strict,
// well-conditioned full-programme matrices in the D1 corpus. These are
// intentionally isolated from the lossless decoder. They are not claimed to be
// a normative/profile-exact matrix yet.

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
}
// The D0/D1 channel identities and the D3 inferred object positions moved to
// `dca::spatial`, which documents their (research-only) provenance. Both this
// pipeline and the offline ADM exporter read them from there.
const D3_OBJECT_SOURCE_INDICES: [usize; 8] = [0, 1, 2, 3, 4, 5, 6, 7];

enum FoldSources<'a> {
    None,
    One(&'a [f32], f32),
    Two(&'a [f32], f32, &'a [f32], f32),
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
                            &mut bridge.dts_height_locked,
                            &bridge.dts_fold_config,
                            bridge.total_samples,
                            &mut bridge.declared_object_channels,
                        ) {
                            bridge.dts_objects_active = emitted_objects;
                            result.frames.push(frame);
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

/// ABI labels for a presentation's extension feeds, in feed order.
///
/// The feed->position tables themselves live in `dca::spatial` so the offline
/// ADM exporter reads the same ones; this only projects them onto the ABI.
fn spatial_labels(presentation: XPresentation) -> impl Iterator<Item = RChannelLabel> {
    presentation
        .channels()
        .iter()
        .copied()
        .map(dca_spatial_channel_to_r)
}

/// Static position of D3 object feed `source`. Research-only defaults; see
/// `dca::spatial` for their provenance.
fn d3_object_position(source: usize) -> [f64; 3] {
    XPresentation::ObjectsD3
        .object_positions()
        .and_then(|positions| positions.get(source).copied())
        .unwrap_or([0.0; 3])
}

/// DCA speaker index -> renderer channel label, for the DTS-HD bed.
fn speaker_to_label(spkr: usize) -> RChannelLabel {
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

/// Build a frame from decoded DTS-HD per-speaker PCM. The normal path remains
/// fixed-channel only. Decoded alternate profiles use the current experimental
/// presentation: inferred fixed channels for D0/D1 and unchanged named objects
/// at inferred static positions for D3.
///
/// `height_locked` latches once a valid XLL-X quartet has been emitted: from
/// then on a frame with a missing/invalid quartet keeps the 12-channel shape
/// (composite bed + silent heights) instead of collapsing to 8 channels, so
/// the host never renegotiates mid-stream and no content is lost (the folded
/// height contribution stays in the bed for that frame).
fn build_hd_frame_with_extensions(
    hd: &HdFrame,
    height_locked: &mut bool,
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

    // The XLL-X channel set carries four full-coded waveforms but no
    // one-to-one speaker mask. Treat its stable stereo pairs as front-height
    // L/R followed by rear-height L/R. Keep the mapping isolated here so it
    // can be replaced without touching the lossless decoder if later metadata
    // proves otherwise.
    let height_samples: Option<&[Vec<f32>; 4]> =
        hd.x_samples
            .as_slice()
            .try_into()
            .ok()
            .filter(|channels: &&[Vec<f32>; 4]| {
                channels.iter().all(|channel| channel.len() == sample_count)
            });
    if height_samples.is_none() && hd.x_present && *height_locked {
        let err = hd.x_decode_error.unwrap_or("no quartet");
        log::warn!("dts: XLL-X quartet unavailable ({err}); emitting silent heights");
    }

    let emit_heights = height_samples.is_some() || *height_locked;
    let height_count = if emit_heights {
        XPresentation::Height.feed_count()
    } else {
        0
    };
    let alternate_samples = hd
        .x_imax
        .then_some(hd.x_samples.as_slice())
        .filter(|channels| matches!(channels.len(), 5 | 6 | 8))
        .filter(|channels| channels.iter().all(|channel| channel.len() == sample_count));
    let d0_fixed_samples = alternate_samples
        .filter(|channels| channels.len() == 5)
        .map(|channels| {
            [
                &channels[0][..],
                &channels[1][..],
                &channels[2][..],
                &channels[3][..],
                &channels[4][..],
            ]
        });
    let d1_fixed_samples = alternate_samples
        .filter(|channels| channels.len() == 6)
        .map(|channels| {
            [
                &channels[0][..],
                &channels[1][..],
                &channels[2][..],
                &channels[3][..],
                &channels[4][..],
                &channels[5][..],
            ]
        });
    let object_source_indices: &[usize] = match alternate_samples.map(<[Vec<f32>]>::len) {
        Some(8) => &D3_OBJECT_SOURCE_INDICES,
        _ => &[],
    };
    let d0_fixed_count = d0_fixed_samples.map_or(0, |_| XPresentation::FixedD0.feed_count());
    let d1_fixed_count = d1_fixed_samples.map_or(0, |_| XPresentation::FixedD1.feed_count());
    let object_count = object_source_indices.len();
    let channel_count =
        active.len() + height_count + d0_fixed_count + d1_fixed_count + object_count;

    // Per-channel source and coefficient for the unfold, hoisted out of the
    // sample loop (no per-sample speaker/profile branching). The D0 choices
    // below are conservative research settings, not a normative matrix.
    let fold_sources: Vec<FoldSources<'_>> = active
        .iter()
        .map(|&spkr| {
            if let Some(heights) = height_samples {
                let height_idx = match spkr {
                    1 => 0usize,
                    2 => 1,
                    7 => 2,
                    8 => 3,
                    _ => return FoldSources::None,
                };
                return FoldSources::One(
                    heights[height_idx].as_slice(),
                    match spkr {
                        1 => fold_config.gain("L", "TFL"),
                        2 => fold_config.gain("R", "TFR"),
                        7 => fold_config.gain("Lb", "TBL"),
                        8 => fold_config.gain("Rb", "TBR"),
                        _ => 0.0,
                    },
                );
            }
            if let Some(fixed) = d0_fixed_samples {
                return match spkr {
                    0 => FoldSources::One(fixed[0], fold_config.gain("C", "X0")),
                    1 => FoldSources::One(fixed[1], fold_config.gain("L", "X1")),
                    2 => FoldSources::One(fixed[2], fold_config.gain("R", "X2")),
                    7 => FoldSources::One(fixed[3], fold_config.gain("Lb", "X3")),
                    8 => FoldSources::One(fixed[4], fold_config.gain("Rb", "X4")),
                    _ => FoldSources::None,
                };
            }
            if let Some(fixed) = d1_fixed_samples {
                return match spkr {
                    1 => FoldSources::Two(
                        fixed[0],
                        fold_config.gain("L", "X0"),
                        fixed[2],
                        fold_config.gain("L", "X2"),
                    ),
                    2 => FoldSources::Two(
                        fixed[1],
                        fold_config.gain("R", "X1"),
                        fixed[3],
                        fold_config.gain("R", "X3"),
                    ),
                    3 => FoldSources::One(fixed[2], fold_config.gain("Ls", "X2")),
                    4 => FoldSources::One(fixed[3], fold_config.gain("Rs", "X3")),
                    7 => FoldSources::One(fixed[4], fold_config.gain("Lb", "X4")),
                    8 => FoldSources::One(fixed[5], fold_config.gain("Rb", "X5")),
                    _ => FoldSources::None,
                };
            }
            FoldSources::None
        })
        .collect();

    let mut pcm: RVec<i32> = RVec::with_capacity(sample_count * channel_count);
    for s in 0..sample_count {
        for (channel, fold) in bed.iter().zip(fold_sources.iter()) {
            let sample = match *fold {
                FoldSources::None => channel[s],
                FoldSources::One(source, gain) => channel[s] - source[s] * gain,
                FoldSources::Two(first, first_gain, second, second_gain) => {
                    channel[s] - first[s] * first_gain - second[s] * second_gain
                }
            };
            pcm.push(float_to_pcm_i32(sample));
        }
        if let Some(height_samples) = height_samples {
            for channel in height_samples.iter() {
                pcm.push(float_to_pcm_i32(channel[s]));
            }
        } else {
            for _ in 0..height_count {
                pcm.push(0);
            }
        }
        if let Some(fixed) = d0_fixed_samples {
            for channel in fixed {
                pcm.push(float_to_pcm_i32(channel[s]));
            }
        }
        if let Some(fixed) = d1_fixed_samples {
            for channel in fixed {
                pcm.push(float_to_pcm_i32(channel[s]));
            }
        }
        if let Some(alternate) = alternate_samples {
            for &source in object_source_indices {
                pcm.push(float_to_pcm_i32(alternate[source][s]));
            }
        }
    }

    let mut channel_labels: RVec<RChannelLabel> = RVec::with_capacity(channel_count);
    for &spkr in &active {
        channel_labels.push(speaker_to_label(spkr));
    }
    channel_labels.extend(spatial_labels(XPresentation::Height).take(height_count));
    channel_labels.extend(spatial_labels(XPresentation::FixedD0).take(d0_fixed_count));
    channel_labels.extend(spatial_labels(XPresentation::FixedD1).take(d1_fixed_count));
    channel_labels.extend(std::iter::repeat_n(RChannelLabel::Object, object_count));

    if height_samples.is_some() {
        *height_locked = true;
    }

    let metadata = build_d3_metadata(
        object_source_indices,
        active.len() + height_count + d0_fixed_count + d1_fixed_count,
        sample_pos,
        hd.sample_rate,
        sample_count,
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
        object_count > 0,
    ))
}

fn build_d3_metadata(
    object_source_indices: &[usize],
    first_object_channel: usize,
    sample_pos: u64,
    sample_rate: u32,
    sample_count: usize,
    declared_object_channels: &mut Option<RVec<RObjectChannel>>,
) -> RVec<RMetadataFrame> {
    const HEARTBEAT_HZ: u64 = 2;

    let object_count = object_source_indices.len();
    if object_count == 0 {
        return RVec::new();
    }

    let declaration_unchanged = declared_object_channels.as_deref().is_some_and(|channels| {
        channels.len() == object_count
            && channels.iter().zip(object_source_indices).enumerate().all(
                |(index, (channel, source))| {
                    channel.id == (10 + source) as u32
                        && channel.channel == (first_object_channel + index) as u32
                },
            )
    });
    // Keep static presentations alive in monitoring clients whose stream-idle
    // timeout is shorter than one second, while staying far below the former
    // per-audio-frame metadata rate that could overwhelm Studio.
    let heartbeat_period = (sample_rate as u64) / HEARTBEAT_HZ;
    let heartbeat_due = heartbeat_period > 0 && sample_pos % heartbeat_period < sample_count as u64;
    if declaration_unchanged && !heartbeat_due {
        return RVec::new();
    }
    let object_channels = if declaration_unchanged {
        RVec::new()
    } else {
        log::warn!(
            "dts: EXPERIMENTAL D3 presentation declaring X0..X{} at inferred positions; mapping is not source metadata",
            object_count - 1,
        );
        let current: RVec<RObjectChannel> = object_source_indices
            .iter()
            .enumerate()
            .map(|(index, &source)| RObjectChannel {
                id: (10 + source) as u32,
                channel: (first_object_channel + index) as u32,
            })
            .collect();
        declare_object_channels(declared_object_channels, current)
    };

    let events: RVec<REvent> = object_source_indices
        .iter()
        .map(|&source| REvent {
            id: (10 + source) as u32,
            sample_pos,
            has_pos: true,
            pos: d3_object_position(source),
            gain_db: 0,
            size: [0.0; 3],
            ramp_duration: 0,
        })
        .collect();
    let name_updates = if declaration_unchanged {
        RVec::new()
    } else {
        object_source_indices
            .iter()
            .map(|&source| RNameUpdate {
                id: (10 + source) as u32,
                name: RString::from(format!("X{source}")),
            })
            .collect()
    };
    let mut metadata = RVec::with_capacity(1);
    metadata.push(RMetadataFrame {
        events,
        object_channels,
        channel_gains: RVec::new(),
        name_updates,
        sample_pos,
        ramp_duration: 0,
    });
    metadata
}

#[cfg(test)]
fn build_hd_frame(hd: &HdFrame, height_locked: &mut bool) -> Option<RDecodedFrame> {
    build_hd_frame_with_extensions(hd, height_locked, &DtsFoldConfig::default(), 0, &mut None)
        .map(|(frame, _)| frame)
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

    const SAMPLE_COUNT: usize = 2;

    fn hd_frame(samples: Vec<Option<Vec<f32>>>, x_samples: Vec<Vec<f32>>) -> HdFrame {
        HdFrame {
            sample_rate: 48_000,
            output_mask: 0,
            samples,
            x_present: !x_samples.is_empty(),
            x_imax: false,
            x_payload: Vec::new(),
            x_payload_offset: 0,
            x_samples,
            x_pcm_bit_res: 24,
            x_bits_consumed: 0,
            x_decode_error: None,
            x_header_tail_bits: 0,
            xll_frame_segments: 1,
            xll_segment_samples: SAMPLE_COUNT,
            xll_segment_size_bits: 0,
            xll_band_crc_present: 0,
            xll_scalable_lsbs: false,
            exss_descriptor_tail: Vec::new(),
            exss_descriptor_tail_bits: 0,
            x_descriptor_offset: None,
            x_descriptor_size: None,
            x_descriptor_navigation_used: false,
        }
    }

    fn assert_pcm_close(actual: i32, expected: f32) {
        let expected = float_to_pcm_i32(expected);
        assert!(
            // YAML dB values are converted back to linear amplitude; allow
            // the bounded float32/PCM quantisation difference.
            actual.abs_diff(expected) <= 64,
            "actual={actual}, expected={expected}"
        );
    }

    #[test]
    fn xll_x_heights_are_removed_from_the_compatible_bed() {
        let heights = [
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
        let mut samples: Vec<Option<Vec<f32>>> = (0..9).map(|_| None).collect();
        samples[0] = Some(vec![0.01, -0.01]);
        samples[1] = Some(
            dry[0]
                .iter()
                .zip(&heights[0])
                .map(|(&bed, &height)| bed + height * XLL_X_HEIGHT_DOWNMIX_GAIN)
                .collect(),
        );
        samples[2] = Some(
            dry[1]
                .iter()
                .zip(&heights[1])
                .map(|(&bed, &height)| bed + height * XLL_X_HEIGHT_DOWNMIX_GAIN)
                .collect(),
        );
        samples[3] = Some(vec![0.02, -0.02]);
        samples[4] = Some(vec![0.03, -0.03]);
        samples[5] = Some(vec![0.04, -0.04]);
        samples[7] = Some(
            dry[2]
                .iter()
                .zip(&heights[2])
                .map(|(&bed, &height)| bed + height * XLL_X_HEIGHT_DOWNMIX_GAIN)
                .collect(),
        );
        samples[8] = Some(
            dry[3]
                .iter()
                .zip(&heights[3])
                .map(|(&bed, &height)| bed + height * XLL_X_HEIGHT_DOWNMIX_GAIN)
                .collect(),
        );

        let hd = hd_frame(samples, heights.into());
        let mut locked = false;
        let frame = build_hd_frame(&hd, &mut locked).expect("valid frame");
        assert!(locked, "a valid quartet must latch the height lock");

        assert_eq!(frame.sample_count, SAMPLE_COUNT as u32);
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
    fn invalid_xll_x_quartet_keeps_the_compatible_bed_unchanged() {
        let composite_left = vec![0.25, -0.25];
        let mut samples: Vec<Option<Vec<f32>>> = (0..9).map(|_| None).collect();
        samples[0] = Some(vec![0.0; SAMPLE_COUNT]);
        samples[1] = Some(composite_left.clone());
        samples[2] = Some(vec![0.0; SAMPLE_COUNT]);
        samples[3] = Some(vec![0.0; SAMPLE_COUNT]);
        samples[4] = Some(vec![0.0; SAMPLE_COUNT]);
        samples[5] = Some(vec![0.0; SAMPLE_COUNT]);
        samples[7] = Some(vec![0.0; SAMPLE_COUNT]);
        samples[8] = Some(vec![0.0; SAMPLE_COUNT]);
        let hd = hd_frame(samples, vec![vec![0.5; SAMPLE_COUNT]; 3]);

        // Not locked yet: an invalid quartet keeps the plain 8-channel bed.
        let mut locked = false;
        let frame = build_hd_frame(&hd, &mut locked).expect("valid frame");
        assert!(!locked, "an invalid quartet must not latch the lock");

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
    fn locked_stream_keeps_shape_with_silent_heights_on_quartet_dropout() {
        let composite_left = vec![0.25, -0.25];
        let mut samples: Vec<Option<Vec<f32>>> = (0..9).map(|_| None).collect();
        for idx in [0usize, 2, 3, 4, 5, 7, 8] {
            samples[idx] = Some(vec![0.0; SAMPLE_COUNT]);
        }
        samples[1] = Some(composite_left.clone());
        // Invalid quartet (3 channels) after a previous frame locked heights.
        let hd = hd_frame(samples, vec![vec![0.5; SAMPLE_COUNT]; 3]);

        let mut locked = true;
        let frame = build_hd_frame(&hd, &mut locked).expect("valid frame");

        // Stable 12-channel shape: composite bed (no unfold without a
        // quartet) + silent height channels — the host never renegotiates.
        assert_eq!(frame.channel_count, 12);
        assert!(frame.metadata.is_empty());
        for sample in 0..SAMPLE_COUNT {
            let row = &frame.pcm[sample * 12..(sample + 1) * 12];
            assert_eq!(row[1], float_to_pcm_i32(composite_left[sample]));
            assert!(row[8..12].iter().all(|&s| s == 0));
        }
    }

    #[test]
    fn d3_emits_named_objects_without_unfolding_the_bed() {
        let mut samples: Vec<Option<Vec<f32>>> = (0..9).map(|_| None).collect();
        samples[0] = Some(vec![0.10, -0.10]);
        samples[1] = Some(vec![0.20, -0.20]);
        samples[2] = Some(vec![0.30, -0.30]);
        let extension: Vec<Vec<f32>> = (0..8)
            .map(|index| vec![0.01 * (index + 1) as f32; SAMPLE_COUNT])
            .collect();
        let mut hd = hd_frame(samples, extension);
        hd.x_present = false;
        hd.x_imax = true;

        let mut locked = false;
        let mut declared = None;
        let (frame, emitted) = build_hd_frame_with_extensions(
            &hd,
            &mut locked,
            &DtsFoldConfig::default(),
            48_000,
            &mut declared,
        )
        .expect("valid D3 frame");

        assert!(emitted);
        assert!(!locked);
        assert_eq!(frame.channel_count, 11);
        assert_eq!(
            &frame.channel_labels[..3],
            &[RChannelLabel::C, RChannelLabel::L, RChannelLabel::R]
        );
        assert!(
            frame.channel_labels[3..]
                .iter()
                .all(|&label| label == RChannelLabel::Object)
        );
        for sample in 0..SAMPLE_COUNT {
            let row = &frame.pcm[sample * 11..(sample + 1) * 11];
            for bed in 0..3 {
                assert_pcm_close(row[bed], hd.samples[bed].as_ref().unwrap()[sample]);
            }
            for source in 0..8 {
                assert_pcm_close(row[3 + source], hd.x_samples[source][sample]);
            }
        }

        assert_eq!(frame.metadata.len(), 1);
        let metadata = &frame.metadata[0];
        assert_eq!(metadata.sample_pos, 48_000);
        assert_eq!(metadata.events.len(), 8);
        assert_eq!(metadata.object_channels.len(), 8);
        assert_eq!(metadata.name_updates.len(), 8);
        for source in 0..8 {
            assert_eq!(metadata.object_channels[source].id, (10 + source) as u32);
            assert_eq!(
                metadata.object_channels[source].channel,
                (3 + source) as u32
            );
            assert_eq!(metadata.name_updates[source].id, (10 + source) as u32);
            assert_eq!(
                metadata.name_updates[source].name.as_str(),
                format!("X{source}")
            );
            assert_eq!(metadata.events[source].id, (10 + source) as u32);
            assert_eq!(metadata.events[source].pos, d3_object_position(source));
        }
    }

    #[test]
    fn d0_emits_fixed_7_1_5_with_partial_unfold() {
        let dry = [
            vec![0.10, -0.10],
            vec![0.20, -0.20],
            vec![0.30, -0.30],
            vec![0.40, -0.40],
            vec![0.50, -0.50],
        ];
        let extension: Vec<Vec<f32>> = (0..5)
            .map(|index| vec![0.01 * (index + 1) as f32; SAMPLE_COUNT])
            .collect();
        let mut samples: Vec<Option<Vec<f32>>> = (0..9).map(|_| None).collect();
        samples[0] = Some(
            dry[0]
                .iter()
                .zip(&extension[0])
                .map(|(&bed, &top)| bed + top * D0_TFC_CENTER_DOWNMIX_GAIN)
                .collect(),
        );
        samples[1] = Some(
            dry[1]
                .iter()
                .zip(&extension[1])
                .map(|(&bed, &top)| bed + top * ALTERNATE_CORNER_HEIGHT_PARTIAL_UNFOLD_GAIN)
                .collect(),
        );
        samples[2] = Some(
            dry[2]
                .iter()
                .zip(&extension[2])
                .map(|(&bed, &top)| bed + top * ALTERNATE_CORNER_HEIGHT_PARTIAL_UNFOLD_GAIN)
                .collect(),
        );
        samples[3] = Some(vec![0.60, -0.60]);
        samples[4] = Some(vec![0.70, -0.70]);
        samples[5] = Some(vec![0.80, -0.80]);
        samples[7] = Some(
            dry[3]
                .iter()
                .zip(&extension[3])
                .map(|(&bed, &top)| bed + top * ALTERNATE_CORNER_HEIGHT_PARTIAL_UNFOLD_GAIN)
                .collect(),
        );
        samples[8] = Some(
            dry[4]
                .iter()
                .zip(&extension[4])
                .map(|(&bed, &top)| bed + top * ALTERNATE_CORNER_HEIGHT_PARTIAL_UNFOLD_GAIN)
                .collect(),
        );
        let mut hd = hd_frame(samples, extension);
        hd.x_present = false;
        hd.x_imax = true;

        let mut locked = false;
        let mut declared = None;
        let (frame, emitted) = build_hd_frame_with_extensions(
            &hd,
            &mut locked,
            &DtsFoldConfig::default(),
            100,
            &mut declared,
        )
        .expect("valid D0 frame");

        assert!(
            !emitted,
            "fixed channels must not set the object stream fact"
        );
        assert!(
            !locked,
            "alternate fixed channels must not latch standard heights"
        );
        assert!(declared.is_none());
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
            assert_pcm_close(row[4], hd.samples[4].as_ref().unwrap()[sample]);
            assert_pcm_close(row[5], hd.samples[5].as_ref().unwrap()[sample]);
            assert_pcm_close(row[6], dry[3][sample]);
            assert_pcm_close(row[7], dry[4][sample]);
            for source in 0..5 {
                assert_pcm_close(row[8 + source], hd.x_samples[source][sample]);
            }
        }
    }

    #[test]
    fn d1_emits_fixed_wides_and_partial_unfold() {
        let left_wide = vec![0.20, -0.10];
        let right_wide = vec![-0.15, 0.25];
        let dry_l = vec![0.03, -0.04];
        let dry_r = vec![-0.05, 0.06];
        let dry_ls = vec![0.07, -0.08];
        let dry_rs = vec![-0.09, 0.10];
        let mut samples: Vec<Option<Vec<f32>>> = (0..9).map(|_| None).collect();
        samples[0] = Some(vec![0.0; SAMPLE_COUNT]);
        samples[1] = Some(
            dry_l
                .iter()
                .zip(&left_wide)
                .map(|(&dry, &wide)| dry + wide * 0.707107)
                .collect(),
        );
        samples[2] = Some(
            dry_r
                .iter()
                .zip(&right_wide)
                .map(|(&dry, &wide)| dry + wide * 0.707107)
                .collect(),
        );
        samples[3] = Some(
            dry_ls
                .iter()
                .zip(&left_wide)
                .map(|(&dry, &wide)| dry + wide * 0.707107)
                .collect(),
        );
        samples[4] = Some(
            dry_rs
                .iter()
                .zip(&right_wide)
                .map(|(&dry, &wide)| dry + wide * 0.707107)
                .collect(),
        );
        let extension = vec![
            vec![0.01; SAMPLE_COUNT],
            vec![0.02; SAMPLE_COUNT],
            left_wide.clone(),
            right_wide.clone(),
            vec![0.05; SAMPLE_COUNT],
            vec![0.06; SAMPLE_COUNT],
        ];
        let mut hd = hd_frame(samples, extension);
        hd.x_present = false;
        hd.x_imax = true;

        let mut locked = false;
        let mut declared = None;
        let (frame, emitted) = build_hd_frame_with_extensions(
            &hd,
            &mut locked,
            &DtsFoldConfig::default(),
            100,
            &mut declared,
        )
        .expect("valid D1 frame");

        assert!(!emitted, "D1 sources are fixed channels");
        assert_eq!(frame.channel_count, 11);
        assert_eq!(
            frame.channel_labels.as_slice(),
            &[
                RChannelLabel::C,
                RChannelLabel::L,
                RChannelLabel::R,
                RChannelLabel::Ls,
                RChannelLabel::Rs,
                RChannelLabel::Tfl,
                RChannelLabel::Tfr,
                RChannelLabel::Lw,
                RChannelLabel::Rw,
                RChannelLabel::Tbl,
                RChannelLabel::Tbr,
            ]
        );
        for sample in 0..SAMPLE_COUNT {
            let row = &frame.pcm[sample * 11..(sample + 1) * 11];
            assert_pcm_close(
                row[1],
                dry_l[sample]
                    - hd.x_samples[0][sample] * ALTERNATE_CORNER_HEIGHT_PARTIAL_UNFOLD_GAIN,
            );
            assert_pcm_close(
                row[2],
                dry_r[sample]
                    - hd.x_samples[1][sample] * ALTERNATE_CORNER_HEIGHT_PARTIAL_UNFOLD_GAIN,
            );
            assert_pcm_close(row[3], dry_ls[sample]);
            assert_pcm_close(row[4], dry_rs[sample]);
            for (slot, source) in (0usize..6).enumerate() {
                assert_pcm_close(row[5 + slot], hd.x_samples[source][sample]);
            }
        }
        assert!(frame.metadata.is_empty());
    }

    #[test]
    fn mismatched_bed_channel_length_drops_the_frame() {
        let mut samples: Vec<Option<Vec<f32>>> = (0..9).map(|_| None).collect();
        samples[0] = Some(vec![0.0; SAMPLE_COUNT]);
        samples[1] = Some(vec![0.0; SAMPLE_COUNT + 1]); // corrupt length
        let hd = hd_frame(samples, Vec::new());

        let mut locked = false;
        assert!(build_hd_frame(&hd, &mut locked).is_none());
    }
}
