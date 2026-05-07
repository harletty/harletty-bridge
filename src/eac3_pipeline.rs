use abi_stable::std_types::RVec;
use bridge_api::{RChannelLabel, RDecodedFrame, RMetadataFrame};
use eac3::{AccessUnitInfo, CorePcmFrame, OamdPayload, ObjectPcmPushResult, ParsedEmdfPayloadData};
#[cfg(feature = "bridge-perf")]
use std::time::Instant;

use crate::bridge::AtmosBridge;
use crate::frame_builders::float_to_pcm_i32;
use crate::labels::{bed_channel_to_r, eac3_core_bed_indices, eac3_object_output_bed_indices};
use crate::logging::dbg_log;
use crate::metadata::build_eac3_metadata_frame;

/// Process a raw E-AC3 access unit (one complete syncframe).
///
/// Attempts object-level decode first (JOC + OAMD), then falls back to
/// core PCM decode.  Converts the result into one [`RDecodedFrame`].
pub(crate) fn process_eac3_frame(
    bridge: &mut AtmosBridge,
    frame: &[u8],
) -> Result<RDecodedFrame, String> {
    // Write first frame to a file for external inspection.
    if bridge.eac3_frame_count == 1 {
        let _ = std::fs::write("/tmp/harletty-eac3-frame.bin", frame);
        dbg_log("eac3_first_frame_dumped_to_/tmp/harletty-eac3-frame.bin\n");
    }

    match bridge.eac3_object_decoder.push_access_unit(frame) {
        Ok(Some(result)) => {
            let sample_count = result.pcm.samples_per_channel();
            let frame_ms = sample_count as f64 / result.pcm.core.sample_rate.max(1) as f64 * 1000.0;
            dbg_log(&format!(
                "eac3_object_decode ok sr={} samples={} frame_ms={:.3} bed_ch={} object_ch={} active={} frames_seen={}\n",
                result.pcm.core.sample_rate,
                sample_count,
                frame_ms,
                result.pcm.core.total_channels(),
                result.pcm.object_channels.len(),
                result
                    .pcm
                    .object_active
                    .iter()
                    .filter(|active| **active)
                    .count(),
                result.frames_seen
            ));

            let base_sample_pos = bridge.eac3_total_samples;
            bridge.eac3_total_samples += sample_count as u64;
            let rf = build_eac3_frame_from_object(result, base_sample_pos, bridge);
            bridge.perf.maybe_report(bridge.eac3_frame_count);
            return Ok(rf);
        }
        Ok(None) => {
            dbg_log("eac3_object_decode no_joc_payload fallback=core\n");
        }
        Err(e) => {
            dbg_log(&format!("eac3_object_decode_error={e} fallback=core\n"));
        }
    }

    match bridge.eac3_pcm_decoder.push_access_unit(frame) {
        Ok(result) => {
            let info = result.info;
            let pcm = result.pcm;
            let decoded_samples = pcm.samples_per_channel();
            let frame_ms = decoded_samples as f64 / pcm.sample_rate.max(1) as f64 * 1000.0;
            dbg_log(&format!(
                "eac3_inspect_ok frame={}B blocks={} expected_samples={} body_offset={} block_start={}\n",
                info.frame_size,
                info.num_blocks,
                info.num_blocks as usize * 256,
                info.body_start_bit_offset,
                info.audio_frame.block_payload_start_bit_offset
            ));

            // Log the frame bytes near the end to diagnose footer.
            let tail_start = frame.len().saturating_sub(16);
            let tail: Vec<String> = frame[tail_start..]
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect();
            dbg_log(&format!("eac3_frame_tail_last16=[{}]\n", tail.join(" ")));

            dbg_log(&format!(
                "eac3_decode ok sr={} samples={} frame_ms={:.3} ch={} frames_seen={}\n",
                pcm.sample_rate,
                decoded_samples,
                frame_ms,
                pcm.total_channels(),
                result.frames_seen
            ));
            if decoded_samples != info.num_blocks as usize * 256 {
                dbg_log(&format!(
                    "eac3_decode_sample_count_mismatch decoded={} expected={} blocks={}\n",
                    decoded_samples,
                    info.num_blocks as usize * 256,
                    info.num_blocks
                ));
            }
            let sample_count = decoded_samples as u32;
            let base_sample_pos = bridge.eac3_total_samples;
            bridge.eac3_total_samples += sample_count as u64;
            let rf = build_eac3_frame_from_core(&pcm, &info, base_sample_pos, bridge);
            bridge.perf.maybe_report(bridge.eac3_frame_count);
            Ok(rf)
        }
        Err(e) => {
            dbg_log(&format!("eac3_decode_error={e}\n"));
            Err(format!("E-AC3 decode error: {e}"))
        }
    }
}

/// Build an [`RDecodedFrame`] from an E-AC3 object decode result.
fn build_eac3_frame_from_object(
    result: ObjectPcmPushResult,
    base_sample_pos: u64,
    bridge: &mut AtmosBridge,
) -> RDecodedFrame {
    let pcm_frame = &result.pcm;
    let core = &pcm_frame.core;
    let sample_count = pcm_frame.samples_per_channel();
    let sampling_frequency = core.sample_rate;
    let bed_channels = usize::from(core.lfe_channel.is_some());
    let object_channels = pcm_frame.object_channels.len();
    let total_channel_count = bed_channels + object_channels;

    let (pcm, channel_labels) = build_eac3_object_output_pcm_and_labels(pcm_frame);

    // Metadata from OAMD payloads.
    #[cfg(feature = "bridge-perf")]
    let metadata_started = Instant::now();
    let mut metadata: RVec<RMetadataFrame> = RVec::new();
    #[cfg(feature = "bridge-perf")]
    let mut metadata_events = 0usize;
    let bed_indices = eac3_object_output_bed_indices(core);
    for (oamd, sample_offset) in &pcm_frame.oamd_payloads {
        let evo_base = base_sample_pos + sample_offset.unwrap_or(0) as u64;
        let oamd_ref: &OamdPayload = oamd;
        let meta = build_eac3_metadata_frame(
            oamd_ref,
            evo_base,
            base_sample_pos,
            &bed_indices,
            object_channels,
            bridge,
        );
        #[cfg(feature = "bridge-perf")]
        {
            metadata_events += meta.events.len();
        }
        metadata.push(meta);
    }
    #[cfg(feature = "bridge-perf")]
    {
        let elapsed = metadata_started.elapsed();
        bridge.perf.record_build_metadata(elapsed);
        bridge
            .perf
            .note_built_frame(metadata.len(), metadata_events);
    }

    RDecodedFrame {
        sampling_frequency,
        sample_count: sample_count as u32,
        channel_count: total_channel_count as u32,
        pcm,
        channel_labels,
        metadata,
        dialogue_level: bridge.current_dialogue_level.into(),
        is_new_segment: false,
    }
}

fn build_eac3_object_output_pcm_and_labels(
    pcm_frame: &eac3::ObjectPcmFrame,
) -> (RVec<i32>, RVec<RChannelLabel>) {
    let core = &pcm_frame.core;
    let sample_count = pcm_frame.samples_per_channel();
    let bed_channels = usize::from(core.lfe_channel.is_some());
    let object_channels = pcm_frame.object_channels.len();
    let total_channel_count = bed_channels + object_channels;

    // Build interleaved PCM: LFE bed first, then dynamic-object channels.
    // JOC output is dynamic-only and does not carry LFE. The fullband core bed
    // is used as JOC input, so exposing it here would double-count the final mix.
    let pcm_capacity = sample_count * total_channel_count;
    let mut pcm: RVec<i32> = RVec::with_capacity(pcm_capacity);

    for s in 0..sample_count {
        if let Some(lfe) = &core.lfe_channel {
            pcm.push(float_to_pcm_i32(lfe[s]));
        }
        for obj_ch in &pcm_frame.object_channels {
            pcm.push(float_to_pcm_i32(obj_ch[s]));
        }
    }

    let mut channel_labels: RVec<RChannelLabel> = RVec::with_capacity(total_channel_count);
    if core.lfe_channel.is_some() {
        channel_labels.push(RChannelLabel::LFE);
    }
    for _ in 0..object_channels {
        channel_labels.push(RChannelLabel::Unknown);
    }

    (pcm, channel_labels)
}

/// Build an [`RDecodedFrame`] from a core-PCM-only E-AC3 decode result.
fn build_eac3_frame_from_core(
    core: &CorePcmFrame,
    info: &AccessUnitInfo,
    base_sample_pos: u64,
    bridge: &mut AtmosBridge,
) -> RDecodedFrame {
    let sampling_frequency = core.sample_rate;
    let sample_count = core.samples_per_channel();
    let total_channel_count = core.total_channels();

    // Build interleaved PCM.
    let pcm_capacity = sample_count * total_channel_count;
    let mut pcm: RVec<i32> = RVec::with_capacity(pcm_capacity);

    for s in 0..sample_count {
        for ch in &core.fullband_channels {
            pcm.push(float_to_pcm_i32(ch[s]));
        }
        if let Some(lfe) = &core.lfe_channel {
            pcm.push(float_to_pcm_i32(lfe[s]));
        }
    }

    // Channel labels.
    let mut channel_labels: RVec<RChannelLabel> = RVec::with_capacity(total_channel_count);
    for bed in &core.fullband_channel_order {
        channel_labels.push(bed_channel_to_r(*bed));
    }
    if core.lfe_channel.is_some() {
        channel_labels.push(RChannelLabel::LFE);
    }

    // Extract metadata from OAMD payloads in the access unit info.
    let mut metadata: RVec<RMetadataFrame> = RVec::new();
    let bed_indices = eac3_core_bed_indices(core);
    for payload in info.payloads() {
        if let ParsedEmdfPayloadData::Oamd(oamd) = &payload.parsed {
            let evo_base = base_sample_pos + payload.info.sample_offset.unwrap_or(0) as u64;
            let meta =
                build_eac3_metadata_frame(oamd, evo_base, base_sample_pos, &bed_indices, 0, bridge);
            metadata.push(meta);
        }
    }

    RDecodedFrame {
        sampling_frequency,
        sample_count: sample_count as u32,
        channel_count: total_channel_count as u32,
        pcm,
        channel_labels,
        metadata,
        dialogue_level: bridge.current_dialogue_level.into(),
        is_new_segment: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labels::eac3_object_output_bed_indices;
    use eac3::{BedChannel, CorePcmFrame, JocPayload, ObjectPcmFrame};

    fn object_pcm_frame(core: CorePcmFrame, object_channels: Vec<Vec<f32>>) -> ObjectPcmFrame {
        ObjectPcmFrame {
            core,
            object_active: vec![true; object_channels.len()],
            object_channels,
            joc: JocPayload {
                downmix_config: 0,
                channel_count: 0,
                object_count: 0,
                gain: 1.0,
                sequence_counter: 0,
                objects: Vec::new(),
            },
            oamd_payloads: Vec::new(),
        }
    }

    #[test]
    fn eac3_object_output_keeps_only_lfe_bed_before_dynamic_objects() {
        let core = CorePcmFrame {
            sample_rate: 48_000,
            fullband_channel_order: vec![
                BedChannel::FrontLeft,
                BedChannel::FrontRight,
                BedChannel::Center,
                BedChannel::SurroundLeft,
                BedChannel::SurroundRight,
            ],
            fullband_channels: vec![
                vec![0.11, 0.12],
                vec![0.21, 0.22],
                vec![0.31, 0.32],
                vec![0.41, 0.42],
                vec![0.51, 0.52],
            ],
            lfe_channel: Some(vec![0.61, 0.62]),
        };
        let frame = object_pcm_frame(core.clone(), vec![vec![0.71, 0.72], vec![0.81, 0.82]]);

        let (pcm, labels) = build_eac3_object_output_pcm_and_labels(&frame);

        assert_eq!(
            labels.as_slice(),
            &[
                RChannelLabel::LFE,
                RChannelLabel::Unknown,
                RChannelLabel::Unknown
            ]
        );
        assert_eq!(
            pcm.as_slice(),
            &[
                float_to_pcm_i32(0.61),
                float_to_pcm_i32(0.71),
                float_to_pcm_i32(0.81),
                float_to_pcm_i32(0.62),
                float_to_pcm_i32(0.72),
                float_to_pcm_i32(0.82),
            ]
        );
        assert_eq!(eac3_object_output_bed_indices(&core), vec![3]);
    }

    #[test]
    fn eac3_object_output_omits_bed_when_lfe_is_absent() {
        let core = CorePcmFrame {
            sample_rate: 48_000,
            fullband_channel_order: vec![BedChannel::FrontLeft, BedChannel::FrontRight],
            fullband_channels: vec![vec![0.11], vec![0.21]],
            lfe_channel: None,
        };
        let frame = object_pcm_frame(core.clone(), vec![vec![0.31], vec![0.41]]);

        let (pcm, labels) = build_eac3_object_output_pcm_and_labels(&frame);

        assert_eq!(
            labels.as_slice(),
            &[RChannelLabel::Unknown, RChannelLabel::Unknown]
        );
        assert_eq!(
            pcm.as_slice(),
            &[float_to_pcm_i32(0.31), float_to_pcm_i32(0.41)]
        );
        assert!(eac3_object_output_bed_indices(&core).is_empty());
    }
}
