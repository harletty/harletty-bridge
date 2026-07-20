// SPDX-License-Identifier: Apache-2.0
//
// DTS (DCA) raw transport pipeline. Demuxes the `[core][exss]` byte stream and
// routes each frame to either the DTS-HD MA lossless decoder (5.1/7.1, when an
// EXSS substream follows the core) or the plain DTS core decoder (5.1). Every
// decoded core channel is emitted as a bed channel, placed at its canonical
// speaker by the renderer. XLL-X's four additional waveforms are provisionally
// mapped as fixed objects at the four 7.1.4 height positions.

use abi_stable::std_types::{RString, RVec};
use bridge_api::RPushResult;
use bridge_api::{RChannelLabel, RDecodedFrame, REvent, RMetadataFrame, RNameUpdate};
use dca::{CorePcmFrame, HdError, HdFrame, exss_has_xll, exss_substream_size, parse_header};

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
    let height_samples = if hd.x_samples.len() == XLL_X_HEIGHT_LABELS.len()
        && hd
            .x_samples
            .iter()
            .all(|channel| channel.len() == sample_count)
    {
        hd.x_samples.as_slice()
    } else {
        &[]
    };
    let channel_count = active.len() + height_samples.len();

    let mut pcm: RVec<i32> = RVec::with_capacity(sample_count * channel_count);
    for s in 0..sample_count {
        for &spkr in &active {
            pcm.push(float_to_pcm_i32(hd.samples[spkr].as_ref().unwrap()[s]));
        }
        for channel in height_samples {
            pcm.push(float_to_pcm_i32(channel[s]));
        }
    }
    let mut channel_labels: RVec<RChannelLabel> = RVec::with_capacity(channel_count);
    for &spkr in &active {
        channel_labels.push(speaker_to_label(spkr));
    }
    channel_labels.extend(XLL_X_HEIGHT_LABELS[..height_samples.len()].iter().copied());

    let metadata = if height_samples.len() == XLL_X_HEIGHT_LABELS.len() {
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
