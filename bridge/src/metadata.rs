use abi_stable::std_types::RVec;
use bridge_api::RMetadataFrame;
use eac3::OamdPayload;
#[cfg(feature = "bridge-perf")]
use std::time::Instant;
use truehd::structs::oamd::ObjectAudioMetadataPayload;

use crate::bridge::AtmosBridge;
use crate::logging::bridge_diag_log;

/// Sparse-emit an object↔channel declaration: return it only when it differs
/// from the cached one (or after a cache clear, i.e. pipeline reset).
pub(crate) fn declare_object_channels(
    cache: &mut Option<RVec<bridge_api::RObjectChannel>>,
    current: RVec<bridge_api::RObjectChannel>,
) -> RVec<bridge_api::RObjectChannel> {
    if cache.as_deref() == Some(current.as_slice()) {
        RVec::new()
    } else {
        *cache = Some(current.clone());
        current
    }
}

/// Build an [`RMetadataFrame`] from an OAMD payload parsed from E-AC3.
///
/// Fixed channels are described by the frame's channel labels; here we emit
/// the dynamic-object events plus the sparse object↔channel declaration
/// (objects sit after the `num_bed_channels` fixed channels, in PCM order).
/// Names are not emitted: the engine derives fixed-channel names from labels
/// and falls back to `Obj_<id>` for unnamed objects, which matches the
/// legacy generated names exactly.
pub(crate) fn build_eac3_metadata_frame(
    oamd: &OamdPayload,
    evo_base: u64,
    frame_sample_pos: u64,
    num_bed_channels: usize,
    object_channel_count: usize,
    bridge: &mut AtmosBridge,
) -> RMetadataFrame {
    let events = extract_eac3_events(oamd, evo_base, object_channel_count);

    let dynamic_objects = oamd
        .object_count
        .saturating_sub(oamd.bed_or_isf_objects)
        .min(object_channel_count);
    let current: RVec<bridge_api::RObjectChannel> = (0..dynamic_objects)
        .map(|k| bridge_api::RObjectChannel {
            id: (10 + k) as u32,
            channel: (num_bed_channels + k) as u32,
        })
        .collect();
    let object_channels = declare_object_channels(&mut bridge.declared_object_channels, current);

    RMetadataFrame {
        events,
        object_channels,
        channel_gains: RVec::new(),
        name_updates: RVec::new(),
        sample_pos: frame_sample_pos,
        ramp_duration: 0,
    }
}

/// Convert an E-AC3 OAMD payload into the dynamic-object event list
/// (`id = 10 + dynamic_idx`, matching the object↔channel declaration).
fn extract_eac3_events(
    oamd: &OamdPayload,
    base_sample_pos: u64,
    object_channel_count: usize,
) -> RVec<bridge_api::REvent> {
    let dynamic_objects = oamd
        .object_count
        .saturating_sub(oamd.bed_or_isf_objects)
        .min(object_channel_count);
    let mut events: RVec<bridge_api::REvent> = RVec::with_capacity(dynamic_objects);

    for element in &oamd.elements {
        let eac3::OamdElementKind::Object(ref obj_element) = element.kind else {
            continue;
        };

        for obj_idx in 0..obj_element.object_blocks.len() {
            let Some(blocks) = obj_element.object_blocks.get(obj_idx) else {
                continue;
            };
            let Some(block) = blocks.first() else {
                continue;
            };

            if obj_idx < oamd.bed_or_isf_objects {
                continue;
            }
            let dynamic_idx = obj_idx - oamd.bed_or_isf_objects;
            if dynamic_idx >= object_channel_count {
                continue;
            }
            let id = (10 + dynamic_idx) as u32;
            let has_pos = block.valid_position;
            let pos: [f64; 3] = if has_pos {
                match block.position.as_ref() {
                    Some(p) if !block.differential_position => [
                        ((p.x as f64).clamp(0.0, 1.0) - 0.5) * 2.0,
                        (0.5 - (p.y as f64).clamp(0.0, 1.0)) * 2.0,
                        (p.z as f64).clamp(-1.0, 1.0),
                    ],
                    Some(p) => [
                        (p.x as f64).clamp(-1.0, 1.0),
                        (-(p.y as f64)).clamp(-1.0, 1.0),
                        (p.z as f64).clamp(-1.0, 1.0),
                    ],
                    None => [0.0; 3],
                }
            } else {
                [0.0; 3]
            };
            let size: [f64; 3] = block
                .size
                .map(|s| [s[0] as f64, s[1] as f64, s[2] as f64])
                .unwrap_or([0.0, 0.0, 0.0]);

            let sample_offset = obj_element
                .block_updates
                .first()
                .map(|u| u.offset as u64)
                .unwrap_or(0);
            let ramp_duration = obj_element
                .block_updates
                .first()
                .map(|u| u.ramp_duration as u32)
                .unwrap_or(0);

            if size != [0.0, 0.0, 0.0] {
                bridge_diag_log(
                    log::Level::Info,
                    &format!(
                        "[harletty][object-size] non-zero detected obj_idx={} dyn_idx={} id={} sample_pos={} sample_offset={} has_pos={} size={:?} ramp={}",
                        obj_idx,
                        dynamic_idx,
                        id,
                        base_sample_pos + sample_offset,
                        sample_offset,
                        has_pos,
                        size,
                        ramp_duration
                    ),
                );
            }
            if block.distance.is_some()
                || block.screen_factor.is_some()
                || block.depth_factor.is_some()
                || block.anchor != eac3::ObjectAnchor::Room
            {
                bridge_diag_log(
                    log::Level::Info,
                    &format!(
                        "[harletty][oamd] visual attrs obj_idx={} dyn_idx={} id={} sample_pos={} sample_offset={} anchor={:?} distance={:?} screen_factor={:?} depth_factor={:?} size={:?}",
                        obj_idx,
                        dynamic_idx,
                        id,
                        base_sample_pos + sample_offset,
                        sample_offset,
                        block.anchor,
                        block.distance,
                        block.screen_factor,
                        block.depth_factor,
                        size
                    ),
                );
            }

            events.push(bridge_api::REvent {
                id,
                sample_pos: base_sample_pos + sample_offset,
                has_pos,
                pos,
                gain_db: block.gain.unwrap_or(0.0) as i8,
                size,
                ramp_duration,
            });
        }
    }

    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::AtmosBridge;

    fn empty_oamd_payload() -> OamdPayload {
        OamdPayload {
            version: 0,
            object_count: 1,
            alternate_object_present: false,
            element_count: 0,
            beds: 1,
            bed_instances: 1,
            bed_or_isf_objects: 1,
            dynamic_objects: 0,
            isf_in_use: false,
            isf_index: None,
            bed_assignment: vec![vec![eac3::BedChannel::LowFrequencyEffects]],
            elements: Vec::new(),
        }
    }

    #[test]
    fn eac3_metadata_declares_objects_sparsely() {
        let mut bridge = AtmosBridge::new(false);
        let mut payload = empty_oamd_payload();
        payload.object_count = 3;
        payload.dynamic_objects = 2;

        // Two dynamic objects after one fixed (LFE) channel: ids 10/11 on
        // channels 1/2, declared on the first frame only.
        let meta = build_eac3_metadata_frame(&payload, 0, 0, 1, 2, &mut bridge);
        let decl: Vec<(u32, u32)> = meta
            .object_channels
            .iter()
            .map(|oc| (oc.id, oc.channel))
            .collect();
        assert_eq!(decl, vec![(10, 1), (11, 2)]);
        assert!(meta.name_updates.is_empty());

        // Unchanged declaration → sparse (empty) re-emission.
        let again = build_eac3_metadata_frame(&payload, 0, 0, 1, 2, &mut bridge);
        assert!(again.object_channels.is_empty());
    }

    /// Anisotropic OAMD size [w, d, h] is preserved end-to-end without any
    /// L2-norm or single-axis collapse.
    #[test]
    fn eac3_extract_events_preserves_anisotropic_size() {
        use eac3::{
            OamdBlockUpdate, OamdElement, OamdElementKind, OamdObjectBlock, OamdObjectElement,
            ObjectAnchor, Vec3,
        };

        let block = OamdObjectBlock {
            inactive: false,
            basic_info_status: 0,
            basic_info_blocks: None,
            render_info_status: 0,
            render_info_blocks: None,
            anchor: ObjectAnchor::Room,
            gain: Some(0.0),
            priority: None,
            valid_position: true,
            differential_position: false,
            position: Some(Vec3 {
                x: 0.5,
                y: 0.5,
                z: 0.5,
            }),
            distance: None,
            // Anisotropic: width=0.2, depth=0.5, height=0.9 — must survive.
            size: Some([0.2, 0.5, 0.9]),
            screen_factor: None,
            depth_factor: None,
            additional_data_bytes: 0,
        };
        let payload = OamdPayload {
            version: 0,
            object_count: 1,
            alternate_object_present: false,
            element_count: 1,
            beds: 0,
            bed_instances: 0,
            bed_or_isf_objects: 0,
            dynamic_objects: 1,
            isf_in_use: false,
            isf_index: None,
            bed_assignment: Vec::new(),
            elements: vec![OamdElement {
                element_index: 1,
                byte_length: 0,
                kind: OamdElementKind::Object(OamdObjectElement {
                    sample_offset: 0,
                    block_updates: vec![OamdBlockUpdate {
                        offset: 0,
                        ramp_duration: 0,
                    }],
                    object_blocks: vec![vec![block]],
                }),
            }],
        };

        let events = extract_eac3_events(&payload, 0, 1);
        let dynamic = events.iter().find(|e| e.has_pos).expect("dynamic event");
        let expected: [f64; 3] = [0.2_f32 as f64, 0.5_f32 as f64, 0.9_f32 as f64];
        for axis in 0..3 {
            assert!(
                (dynamic.size[axis] - expected[axis]).abs() < 1e-6,
                "axis {axis}: got {} expected {}",
                dynamic.size[axis],
                expected[axis]
            );
        }
    }
}

/// Build an [`RMetadataFrame`] from a TrueHD OAMD payload.
///
/// Fixed channels are described by the frame's channel labels; the metadata
/// carries the dynamic-object events, the sparse object↔channel declaration
/// and the OAMD bed-gain automation. Names are not emitted: the engine
/// derives fixed-channel names from labels and falls back to `Obj_<id>`,
/// matching the legacy generated names.
pub(crate) fn build_metadata_frame_from_oamd(
    oamd: &ObjectAudioMetadataPayload,
    evo_base: u64,
    frame_sample_pos: u64,
    declared_object_channels: &mut Option<RVec<bridge_api::RObjectChannel>>,
    #[cfg(feature = "bridge-perf")] perf: &mut crate::perf::PerfStats,
) -> RMetadataFrame {
    #[cfg(feature = "bridge-perf")]
    let events_started = Instant::now();
    let extracted = extract_events(oamd, evo_base);
    #[cfg(feature = "bridge-perf")]
    perf.record_build_metadata_events(events_started.elapsed());

    let current: RVec<bridge_api::RObjectChannel> = extracted
        .objects
        .iter()
        .map(|&(id, channel)| bridge_api::RObjectChannel {
            id,
            channel: channel as u32,
        })
        .collect();
    let object_channels = declare_object_channels(declared_object_channels, current);

    let ramp_duration = oamd
        .object_element
        .as_ref()
        .and_then(|e| e.md_update_info.block_update_info.first())
        .map(|b| b.ramp_duration as u32)
        .unwrap_or(0);

    RMetadataFrame {
        events: extracted.events,
        object_channels,
        channel_gains: extracted.channel_gains,
        name_updates: RVec::new(),
        sample_pos: frame_sample_pos,
        ramp_duration,
    }
}

/// Extract spatial events from a TrueHD OAMD frame.
struct ExtractedTruehdMetadata {
    /// Dynamic-object events (ids match `objects`).
    events: RVec<bridge_api::REvent>,
    /// OAMD gain automation for the bed members, by PCM channel.
    channel_gains: RVec<bridge_api::RChannelGain>,
    /// Dynamic-object declaration: (id, PCM channel).
    objects: Vec<(u32, usize)>,
}

impl ExtractedTruehdMetadata {
    fn empty() -> Self {
        Self {
            events: RVec::new(),
            channel_gains: RVec::new(),
            objects: Vec::new(),
        }
    }
}

fn extract_events(
    oamd: &ObjectAudioMetadataPayload,
    base_sample_pos: u64,
) -> ExtractedTruehdMetadata {
    let object_count = oamd.object_count;
    let Some(object_element) = &oamd.object_element else {
        return ExtractedTruehdMetadata::empty();
    };

    if object_element.md_update_info.num_obj_info_blocks != 1 {
        log::warn!(
            "atmos-bridge: unsupported OAMD with num_obj_info_blocks={} (expected 1); skipping metadata frame",
            object_element.md_update_info.num_obj_info_blocks
        );
        return ExtractedTruehdMetadata::empty();
    }
    if oamd.program_assignment.bed_assignment.len() != 1 {
        log::warn!(
            "atmos-bridge: unsupported OAMD with bed_assignment_count={} (expected 1); skipping metadata frame",
            oamd.program_assignment.bed_assignment.len()
        );
        return ExtractedTruehdMetadata::empty();
    }
    if oamd.program_assignment.num_isf_objects != 0 {
        log::warn!(
            "atmos-bridge: unsupported OAMD with num_isf_objects={} (expected 0); skipping metadata frame",
            oamd.program_assignment.num_isf_objects
        );
        return ExtractedTruehdMetadata::empty();
    }

    let sample_offset = object_element.md_update_info.sample_offset as u64;
    let ramp_duration = object_element.md_update_info.block_update_info[0].ramp_duration as u32;
    let sample_pos = base_sample_pos + sample_offset;

    let pos_vec = oamd.get_damf_pos();
    let bed_index_vec = oamd
        .program_assignment
        .bed_assignment
        .first()
        .map(|b| b.to_index_vec())
        .unwrap_or_default();

    let mut events: RVec<bridge_api::REvent> = RVec::with_capacity(object_count);
    let mut channel_gains: RVec<bridge_api::RChannelGain> = RVec::new();
    let mut objects: Vec<(u32, usize)> = Vec::new();
    let mut missing_object_data = 0usize;
    let mut empty_object_blocks = 0usize;
    let mut bed_index_oob = 0usize;
    let mut missing_damf_pos = 0usize;

    for i in 0..object_count {
        let Some(object_blocks) = object_element.object_data.get(i) else {
            missing_object_data += 1;
            continue;
        };
        let Some(object_data) = object_blocks.first() else {
            empty_object_blocks += 1;
            continue;
        };

        if object_data.b_object_in_bed_or_isf {
            // Bed member: its channel is fixed (described by the frame's
            // labels); OAMD only contributes live gain automation.
            if bed_index_vec.get(i).is_none() {
                bed_index_oob += 1;
                continue;
            }
            channel_gains.push(bridge_api::RChannelGain {
                channel: i as u32,
                gain_db: object_data.object_basic_info.object_gain,
            });
            continue;
        }

        let id = (i + 10 - bed_index_vec.len()) as u32;
        let render = &object_data.object_render_info;
        let (has_pos, pos, size) =
            match pos_vec.get(i).and_then(|raw_blocks| raw_blocks.first()) {
                Some(raw) if raw.len() >= 3 => (true, [raw[0], raw[1], raw[2]], render.object_size),
                Some(_) => (false, [0.0; 3], [0.0; 3]),
                None => {
                    missing_damf_pos += 1;
                    (false, [0.0; 3], [0.0; 3])
                }
            };

        objects.push((id, i));
        events.push(bridge_api::REvent {
            id,
            sample_pos,
            has_pos,
            pos,
            gain_db: object_data.object_basic_info.object_gain,
            size,
            ramp_duration,
        });
    }

    if missing_object_data > 0 {
        log::warn!(
            "atmos-bridge: missing object_data for {} object(s) (object_count={}); skipped",
            missing_object_data,
            object_count
        );
    }
    if empty_object_blocks > 0 {
        log::warn!(
            "atmos-bridge: empty object_data blocks for {} object(s); skipped",
            empty_object_blocks
        );
    }
    if bed_index_oob > 0 {
        log::warn!(
            "atmos-bridge: bed index out-of-range for {} object(s) (bed_index_len={}); skipped",
            bed_index_oob,
            bed_index_vec.len()
        );
    }
    if missing_damf_pos > 0 {
        log::warn!(
            "atmos-bridge: missing DAMF position for {} object(s); positions omitted",
            missing_damf_pos
        );
    }

    ExtractedTruehdMetadata {
        events,
        channel_gains,
        objects,
    }
}
