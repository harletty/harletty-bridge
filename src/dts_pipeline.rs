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
use bridge_api::{RChannelLabel, RDecodedFrame, REvent, RMetadataFrame, RNameUpdate};
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
const XLL_X_HEIGHT_POSITIONS: [[f64; 3]; 4] = [
    [-1.0, 1.0, 1.0],
    [1.0, 1.0, 1.0],
    [-1.0, -1.0, 1.0],
    [1.0, -1.0, 1.0],
];
const XLL_X_HEIGHT_NAMES: [&str; 4] = ["TFL", "TFR", "TBL", "TBR"];
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
                let base = bridge.total_samples;
                match bridge
                    .dts_hd_decoder
                    .decode(&rest[..fs], &rest[fs..fs + es])
                {
                    Ok(hd) => {
                        let n = hd_samples(&hd);
                        let frame = build_hd_frame(&hd, base, !bridge.dts_spatial_names_emitted);
                        if !frame.metadata.is_empty() {
                            bridge.dts_spatial_active = true;
                            bridge.dts_spatial_names_emitted = true;
                        }
                        result.frames.push(frame);
                        bridge.total_samples += n as u64;
                        bridge.dts_frame_count += 1;
                    }
                    Err(HdError::Pending) => {} // PBR buffering; no frame this packet
                    Err(e) => {
                        let msg = format!("dts_hd_decode_error={e:?}");
                        log::warn!("{msg}");
                        let _ = base;
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

/// DCA speaker index -> renderer bed ID. IDs below 10 are reserved for direct
/// bed speakers; dynamic/object channels begin at ID 10.
fn speaker_to_bed_id(spkr: usize) -> Option<usize> {
    match spkr {
        0 => Some(2),     // C
        1 => Some(0),     // L
        2 => Some(1),     // R
        3 => Some(4),     // Ls
        4 => Some(5),     // Rs
        5 => Some(3),     // LFE
        6 | 7 => Some(6), // Cs or Lsr
        8 => Some(7),     // Rsr
        _ => None,
    }
}

/// Describe the provisional XLL-X height quartet as fixed spatial objects.
///
/// PCM ordering remains `[beds..., TFL, TFR, TBL, TBR]`. The renderer maps
/// object IDs 10+ to channels immediately following the declared bed, so the
/// four events below address the four XLL-X waveforms without copying audio.
fn build_xll_x_metadata(
    active: &[usize],
    sample_pos: u64,
    emit_names: bool,
) -> Option<RMetadataFrame> {
    let mut bed_indices: RVec<usize> = RVec::with_capacity(active.len());
    for &spkr in active {
        let id = speaker_to_bed_id(spkr)?;
        // A 6.1 layout can contain both Cs and Lsr, which share the renderer's
        // legacy bed ID. Do not claim an ambiguous spatial layout.
        if bed_indices.contains(&id) {
            return None;
        }
        bed_indices.push(id);
    }

    let mut events: RVec<REvent> = RVec::with_capacity(active.len() + 4);
    for &id in bed_indices.iter() {
        events.push(REvent {
            id: id as u32,
            sample_pos,
            has_pos: false,
            pos: [0.0; 3],
            gain_db: 0,
            size: [0.0; 3],
            ramp_duration: 0,
        });
    }
    for (idx, &pos) in XLL_X_HEIGHT_POSITIONS.iter().enumerate() {
        events.push(REvent {
            id: 10 + idx as u32,
            sample_pos,
            has_pos: true,
            pos,
            gain_db: 0,
            size: [0.0; 3],
            ramp_duration: 0,
        });
    }

    let name_updates: RVec<RNameUpdate> = if emit_names {
        XLL_X_HEIGHT_NAMES
            .iter()
            .enumerate()
            .map(|(idx, &name)| RNameUpdate {
                id: 10 + idx as u32,
                name: name.into(),
            })
            .collect()
    } else {
        RVec::new()
    };

    Some(RMetadataFrame {
        events,
        bed_indices,
        name_updates,
        sample_pos,
        ramp_duration: 0,
    })
}

#[inline]
fn undo_xll_x_height_downmix(spkr: usize, bed: f32, height: &[f32; 4]) -> f32 {
    let embedded_height = match spkr {
        1 => height[0], // L  <- TFL
        2 => height[1], // R  <- TFR
        7 => height[2], // Lb <- TBL
        8 => height[3], // Rb <- TBR
        _ => return bed,
    };
    bed - embedded_height * XLL_X_HEIGHT_DOWNMIX_GAIN
}

/// Build a bed frame from a decoded DTS-HD frame (per-speaker f32).
fn build_hd_frame(hd: &HdFrame, sample_pos: u64, emit_names: bool) -> RDecodedFrame {
    // Active speakers in ascending index order = stable channel order.
    let active: Vec<usize> = (0..hd.samples.len())
        .filter(|&s| hd.samples[s].is_some())
        .collect();
    let sample_count = hd_samples(hd);
    // The XLL-X channel set carries four full-coded waveforms but no one-to-one
    // speaker mask. Treat its stable stereo pairs as front-height L/R followed
    // by rear-height L/R. Keep the mapping isolated here so it can be replaced
    // without touching the lossless decoder if later metadata proves otherwise.
    let height_samples: Option<&[Vec<f32>; 4]> =
        hd.x_samples
            .as_slice()
            .try_into()
            .ok()
            .filter(|channels: &&[Vec<f32>; 4]| {
                channels.iter().all(|channel| channel.len() == sample_count)
            });
    let height_count = height_samples.map_or(0, |_| XLL_X_HEIGHT_LABELS.len());
    let channel_count = active.len() + height_count;

    let mut pcm: RVec<i32> = RVec::with_capacity(sample_count * channel_count);
    if let Some(height_samples) = height_samples {
        for s in 0..sample_count {
            let height = [
                height_samples[0][s],
                height_samples[1][s],
                height_samples[2][s],
                height_samples[3][s],
            ];
            for &spkr in &active {
                let bed = hd.samples[spkr].as_ref().unwrap()[s];
                pcm.push(float_to_pcm_i32(undo_xll_x_height_downmix(
                    spkr, bed, &height,
                )));
            }
            for sample in height {
                pcm.push(float_to_pcm_i32(sample));
            }
        }
    } else {
        for s in 0..sample_count {
            for &spkr in &active {
                pcm.push(float_to_pcm_i32(hd.samples[spkr].as_ref().unwrap()[s]));
            }
        }
    }
    let mut channel_labels: RVec<RChannelLabel> = RVec::with_capacity(channel_count);
    for &spkr in &active {
        channel_labels.push(speaker_to_label(spkr));
    }
    channel_labels.extend(XLL_X_HEIGHT_LABELS[..height_count].iter().copied());

    let metadata = if height_samples.is_some() {
        build_xll_x_metadata(&active, sample_pos, emit_names)
            .into_iter()
            .collect()
    } else {
        RVec::new()
    };

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
    }
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
        let frame = build_hd_frame(&hd, 123, true);

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
        assert_eq!(frame.metadata.len(), 1);

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

        let frame = build_hd_frame(&hd, 0, true);

        assert_eq!(frame.channel_count, 8);
        assert!(frame.metadata.is_empty());
        for sample in 0..SAMPLE_COUNT {
            assert_eq!(
                frame.pcm[sample * 8 + 1],
                float_to_pcm_i32(composite_left[sample])
            );
        }
    }
}
