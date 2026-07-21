// SPDX-License-Identifier: Apache-2.0
//
// DTS (DCA) raw transport pipeline. Demuxes the `[core][exss]` byte stream and
// routes each frame to either the DTS-HD MA lossless decoder (5.1/7.1, when an
// EXSS substream follows the core) or the plain DTS core decoder (5.1). Every
// decoded core channel is emitted as a bed channel, placed at its canonical
// speaker by the renderer. XLL-X's four additional waveforms are mapped as
// fixed objects at the four 7.1.4 height positions after undoing their -3 dB
// contribution to the backward-compatible 7.1 bed.

use abi_stable::std_types::{RString, RVec};
use bridge_api::RPushResult;
use bridge_api::{RChannelLabel, RDecodedFrame};
use dca::{exss_has_xll, exss_substream_size, parse_header, CorePcmFrame, HdError, HdFrame};

use crate::bridge::AtmosBridge;
use crate::frame_builders::float_to_pcm_i32;
use crate::labels::dca_bed_channel_to_r;

const CORE_SYNC: [u8; 4] = 0x7FFE_8001u32.to_be_bytes();
const SUBSTREAM_SYNC: [u8; 4] = 0x6458_2025u32.to_be_bytes();
const XLL_X_HEIGHT_LABELS: [RChannelLabel; 4] = [
    RChannelLabel::Tfl,
    RChannelLabel::Tfr,
    RChannelLabel::Tbl,
    RChannelLabel::Tbr,
];
// Exact Q15 -3 dB coefficient used by the DTS downmix table. The regular 7.1
// presentation contains this contribution from each corresponding height feed.
const XLL_X_HEIGHT_DOWNMIX_GAIN: f32 = 23_170.0 / 32_768.0;

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
                        if let Some(frame) = build_hd_frame(&hd, &mut bridge.dts_height_locked) {
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
    hd.samples
        .iter()
        .find_map(|o| o.as_ref().map(|v| v.len()))
        .unwrap_or(0)
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


/// Build a labeled-channel frame from a decoded DTS-HD frame (per-speaker
/// f32). Fixed channels only — a DTS:X fixed 7.1.4 presentation is twelve
/// labeled channels, never fabricated metadata
/// (`docs/channel-object-contract.md`).
///
/// `height_locked` latches once a valid XLL-X quartet has been emitted: from
/// then on a frame with a missing/invalid quartet keeps the 12-channel shape
/// (composite bed + silent heights) instead of collapsing to 8 channels, so
/// the host never renegotiates mid-stream and no content is lost (the folded
/// height contribution stays in the bed for that frame).
fn build_hd_frame(hd: &HdFrame, height_locked: &mut bool) -> Option<RDecodedFrame> {
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
        XLL_X_HEIGHT_LABELS.len()
    } else {
        0
    };
    let channel_count = active.len() + height_count;

    // Per-channel embedded-height source for the unfold, hoisted out of the
    // sample loop (no per-sample match / Option deref): DCA speakers
    // 1=L, 2=R, 7=Lsr, 8=Rsr carry the -3 dB fold of TFL/TFR/TBL/TBR.
    let fold_source: Vec<Option<&[f32]>> = active
        .iter()
        .map(|&spkr| {
            let height_idx = match spkr {
                1 => 0usize,
                2 => 1,
                7 => 2,
                8 => 3,
                _ => return None,
            };
            height_samples.map(|h| h[height_idx].as_slice())
        })
        .collect();

    let mut pcm: RVec<i32> = RVec::with_capacity(sample_count * channel_count);
    for s in 0..sample_count {
        for (channel, fold) in bed.iter().zip(fold_source.iter()) {
            let sample = match fold {
                Some(height) => channel[s] - height[s] * XLL_X_HEIGHT_DOWNMIX_GAIN,
                None => channel[s],
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
    }

    let mut channel_labels: RVec<RChannelLabel> = RVec::with_capacity(channel_count);
    for &spkr in &active {
        channel_labels.push(speaker_to_label(spkr));
    }
    channel_labels.extend(XLL_X_HEIGHT_LABELS[..height_count].iter().copied());

    if height_samples.is_some() {
        *height_locked = true;
    }

    Some(RDecodedFrame {
        sampling_frequency: hd.sample_rate,
        sample_count: sample_count as u32,
        channel_count: channel_count as u32,
        pcm,
        channel_labels,
        metadata: RVec::new(),
        drc_gain: 1.0,
        drc_ramp_duration: 0,
        dialogue_level: None.into(),
        is_new_segment: false,
    })
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
            actual.abs_diff(expected) <= 1,
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
    fn mismatched_bed_channel_length_drops_the_frame() {
        let mut samples: Vec<Option<Vec<f32>>> = (0..9).map(|_| None).collect();
        samples[0] = Some(vec![0.0; SAMPLE_COUNT]);
        samples[1] = Some(vec![0.0; SAMPLE_COUNT + 1]); // corrupt length
        let hd = hd_frame(samples, Vec::new());

        let mut locked = false;
        assert!(build_hd_frame(&hd, &mut locked).is_none());
    }
}
