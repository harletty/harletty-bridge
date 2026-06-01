// SPDX-License-Identifier: Apache-2.0
//
// DTS (DCA) raw transport pipeline. Drives the native `dca` core decoder and
// builds bed-only `RDecodedFrame`s: each decoded 5.1/7.1 channel is emitted as
// a bed channel and placed at its canonical speaker by the renderer (no object
// metadata — the "fixed object disposition" is the speaker layout itself).

use abi_stable::std_types::{RString, RVec};
use bridge_api::{RChannelLabel, RDecodedFrame, RPushResult};
use dca::CorePcmFrame;

use crate::bridge::AtmosBridge;
use crate::frame_builders::float_to_pcm_i32;
use crate::labels::dca_bed_channel_to_r;

/// Drain all complete DTS core frames buffered in the extractor.
pub(crate) fn drain_dts_raw(bridge: &mut AtmosBridge, result: &mut RPushResult) {
    loop {
        match bridge.dts_extractor.next_frame() {
            Ok(Some(frame)) => {
                match bridge.dts_decoder.push_access_unit(frame.as_bytes()) {
                    Ok(push) => {
                        let base = bridge.total_samples;
                        let decoded = build_dts_frame_from_core(&push.pcm, base);
                        bridge.total_samples += push.pcm.samples_per_channel() as u64;
                        bridge.dts_frame_count += 1;
                        result.frames.push(decoded);
                    }
                    Err(err) => {
                        // Skip a bad core frame rather than aborting the stream;
                        // the extractor will resync on the next syncword.
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
            Ok(None) => break,
            Err(err) => {
                let msg = format!("dts_extract_error={err:?}");
                log::warn!("{msg}");
                bridge.reset_pipeline();
                result.did_reset = true;
                result.error_message = RString::from(msg);
                return;
            }
        }
    }
}

/// Build a bed-only decoded frame from a decoded DTS core PCM frame.
fn build_dts_frame_from_core(core: &CorePcmFrame, _base_sample_pos: u64) -> RDecodedFrame {
    let sample_count = core.samples_per_channel();
    let total_channel_count = core.total_channels();

    // Interleave: fullband channels in DCA primary order, then LFE.
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
        metadata: RVec::new(), // bed-only; placement is via channel_labels
        drc_gain: 1.0,
        drc_ramp_duration: 0,
        dialogue_level: None.into(),
        is_new_segment: false,
    }
}
