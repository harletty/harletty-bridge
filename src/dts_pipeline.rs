// SPDX-License-Identifier: Apache-2.0
//
// DTS (DCA) raw transport pipeline. Demuxes the `[core][exss]` byte stream and
// routes each frame to either the DTS-HD MA lossless decoder (5.1/7.1, when an
// EXSS substream follows the core) or the plain DTS core decoder (5.1). Every
// decoded channel is emitted as a bed channel, placed at its canonical speaker
// by the renderer (the layout is the "fixed object disposition").

use abi_stable::std_types::{RString, RVec};
use bridge_api::{RChannelLabel, RDecodedFrame};
use bridge_api::RPushResult;
use dca::{exss_has_xll, exss_substream_size, parse_header, CorePcmFrame, HdError, HdFrame};

use crate::bridge::AtmosBridge;
use crate::frame_builders::float_to_pcm_i32;
use crate::labels::dca_bed_channel_to_r;

const CORE_SYNC: [u8; 4] = 0x7FFE_8001u32.to_be_bytes();
const SUBSTREAM_SYNC: [u8; 4] = 0x6458_2025u32.to_be_bytes();

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
                match bridge.dts_hd_decoder.decode(&rest[..fs], &rest[fs..fs + es]) {
                    Ok(hd) => {
                        let n = hd_samples(&hd);
                        result.frames.push(build_hd_frame(&hd));
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
        6 => RChannelLabel::Cb,    // Cs (rear center)
        7 => RChannelLabel::Lb,    // Lsr (rear surround left)
        8 => RChannelLabel::Rb,    // Rsr (rear surround right)
        _ => RChannelLabel::Unknown,
    }
}

/// Build a bed frame from a decoded DTS-HD frame (per-speaker f32).
fn build_hd_frame(hd: &HdFrame) -> RDecodedFrame {
    // Active speakers in ascending index order = stable channel order.
    let active: Vec<usize> = (0..hd.samples.len())
        .filter(|&s| hd.samples[s].is_some())
        .collect();
    let sample_count = hd_samples(hd);
    let channel_count = active.len();

    let mut pcm: RVec<i32> = RVec::with_capacity(sample_count * channel_count);
    for s in 0..sample_count {
        for &spkr in &active {
            pcm.push(float_to_pcm_i32(hd.samples[spkr].as_ref().unwrap()[s]));
        }
    }
    let mut channel_labels: RVec<RChannelLabel> = RVec::with_capacity(channel_count);
    for &spkr in &active {
        channel_labels.push(speaker_to_label(spkr));
    }

    RDecodedFrame {
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
