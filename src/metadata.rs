use abi_stable::std_types::RVec;
use bridge_api::RMetadataFrame;
use eac3::OamdPayload;
#[cfg(feature = "bridge-perf")]
use std::time::Instant;
use truehd::structs::oamd::{ObjectAudioMetadataPayload, SpeakerLabels};

use crate::bridge::AtmosBridge;
use crate::labels::speaker_to_id;
use crate::logging::bridge_diag_log;
use crate::perf::PerfStats;

/// Build an [`RMetadataFrame`] from an OAMD payload parsed from E-AC3.
pub(crate) fn build_eac3_metadata_frame(
    oamd: &OamdPayload,
    evo_base: u64,
    frame_sample_pos: u64,
    bed_indices: &[usize],
    object_channel_count: usize,
    bridge: &mut AtmosBridge,
) -> RMetadataFrame {
    let events = extract_eac3_events(oamd, evo_base, bed_indices, object_channel_count);
    let bed_indices: RVec<usize> = bed_indices.iter().copied().collect();

    let mut name_updates = RVec::new();
    for &speaker_id in bed_indices.iter() {
        let id = speaker_id as u32;
        let key = ObjectNameKey::Bed(speaker_id as u8);
        if name_key_changed(&mut bridge.object_name_keys_by_id, id, &key) {
            name_updates.push(bridge_api::RNameUpdate {
                id,
                name: object_name_from_key(&key).into(),
            });
        }
    }

    let dynamic_objects = oamd
        .object_count
        .saturating_sub(oamd.bed_or_isf_objects)
        .min(object_channel_count);
    for dynamic_idx in 0..dynamic_objects {
        let id = (10 + dynamic_idx) as u32;
        let key = ObjectNameKey::Dynamic(id as usize);
        if name_key_changed(&mut bridge.object_name_keys_by_id, id, &key) {
            name_updates.push(bridge_api::RNameUpdate {
                id,
                name: object_name_from_key(&key).into(),
            });
        }
    }

    RMetadataFrame {
        events,
        bed_indices,
        name_updates,
        sample_pos: frame_sample_pos,
        ramp_duration: 0,
    }
}

/// Convert an E-AC3 OAMD payload into the bridge event list.
///
/// Mirrors TrueHD's structure: bed events first (one per PCM bed channel,
/// `has_pos=false`, `id=speaker_id`), then dynamic events (`id=10+dynamic_idx`).
/// Studio displays slots ordered by `events[]` index, so the bed events keep
/// the dynamic slots aligned with their position metadata.
fn extract_eac3_events(
    oamd: &OamdPayload,
    base_sample_pos: u64,
    bed_indices: &[usize],
    object_channel_count: usize,
) -> RVec<bridge_api::REvent> {
    let dynamic_objects = oamd
        .object_count
        .saturating_sub(oamd.bed_or_isf_objects)
        .min(object_channel_count);
    let mut events: RVec<bridge_api::REvent> =
        RVec::with_capacity(bed_indices.len() + dynamic_objects);

    for &speaker_id in bed_indices {
        events.push(bridge_api::REvent {
            id: speaker_id as u32,
            sample_pos: base_sample_pos,
            has_pos: false,
            pos: [0.0; 3],
            gain_db: 0,
            size: [0.0, 0.0, 0.0],
            ramp_duration: 0,
        });
    }

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
    fn eac3_metadata_names_lfe_bed() {
        let mut bridge = AtmosBridge::new(false);

        let meta = build_eac3_metadata_frame(&empty_oamd_payload(), 0, 0, &[3], 0, &mut bridge);

        assert_eq!(meta.name_updates.len(), 1);
        assert_eq!(meta.name_updates[0].id, 3);
        assert_eq!(meta.name_updates[0].name.as_str(), "LFE");
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

        let events = extract_eac3_events(&payload, 0, &[], 1);
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ObjectNameKey {
    Bed(u8),
    Isf(usize),
    Dynamic(usize),
}

/// Build an [`RMetadataFrame`] from a parsed OAMD payload.
///
/// - `evo_base` = `total_samples + evo_sample_offset` (used for event sample_pos).
/// - `frame_sample_pos` = `total_samples` (used for OSC timing, no evo offset).
pub(crate) fn object_name_from_key(key: &ObjectNameKey) -> String {
    match key {
        ObjectNameKey::Bed(idx) => SpeakerLabels::from_u8(*idx)
            .map(|l| format!("{:?}", l))
            .unwrap_or_else(|| format!("Bed_{}", idx)),
        ObjectNameKey::Isf(i) => format!("ISF_{}", i),
        ObjectNameKey::Dynamic(i) => format!("Obj_{}", i),
    }
}

#[inline]
pub(crate) fn name_key_changed(
    cache: &mut Vec<Option<ObjectNameKey>>,
    id: u32,
    key: &ObjectNameKey,
) -> bool {
    let idx = id as usize;
    if idx >= cache.len() {
        cache.resize(idx + 1, None);
        cache[idx] = Some(key.clone());
        return true;
    }
    match &cache[idx] {
        Some(prev) if prev == key => false,
        _ => {
            cache[idx] = Some(key.clone());
            true
        }
    }
}

pub(crate) fn build_metadata_frame_from_oamd(
    oamd: &ObjectAudioMetadataPayload,
    evo_base: u64,
    frame_sample_pos: u64,
    name_key_cache: &mut Vec<Option<ObjectNameKey>>,
    _perf: &mut PerfStats,
) -> RMetadataFrame {
    #[cfg(feature = "bridge-perf")]
    let perf = _perf;
    #[cfg(feature = "bridge-perf")]
    let events_started = Instant::now();
    let events = extract_events(oamd, evo_base);
    #[cfg(feature = "bridge-perf")]
    perf.record_build_metadata_events(events_started.elapsed());

    #[cfg(feature = "bridge-perf")]
    let bed_indices_started = Instant::now();
    let bed_index_vec: Vec<usize> = oamd
        .program_assignment
        .bed_assignment
        .first()
        .map(|bed| bed.to_index_vec())
        .unwrap_or_default();
    let bed_indices: RVec<usize> = bed_index_vec.iter().map(|&i| speaker_to_id(i)).collect();
    #[cfg(feature = "bridge-perf")]
    perf.record_build_metadata_bed_indices(bed_indices_started.elapsed());

    #[cfg(feature = "bridge-perf")]
    let name_updates_started = Instant::now();
    let mut name_updates = RVec::new();
    let num_isf_objects = oamd.program_assignment.num_isf_objects;
    let num_dynamic_objects = oamd.program_assignment.num_dynamic_objects;
    for (idx, event) in events.iter().enumerate() {
        let id = event.id;
        let key = object_name_key_for_index(
            idx,
            id,
            &bed_index_vec,
            num_isf_objects,
            num_dynamic_objects,
        );
        if name_key_changed(name_key_cache, id, &key) {
            name_updates.push(bridge_api::RNameUpdate {
                id,
                name: object_name_from_key(&key).into(),
            });
        }
    }
    #[cfg(feature = "bridge-perf")]
    perf.record_build_metadata_name_updates(name_updates_started.elapsed());

    let ramp_duration = oamd
        .object_element
        .as_ref()
        .and_then(|e| e.md_update_info.block_update_info.first())
        .map(|b| b.ramp_duration as u32)
        .unwrap_or(0);

    RMetadataFrame {
        events,
        bed_indices,
        name_updates,
        sample_pos: frame_sample_pos,
        ramp_duration,
    }
}

#[inline]
fn object_name_key_for_index(
    object_index: usize,
    object_id: u32,
    bed_index_vec: &[usize],
    num_isf_objects: usize,
    num_dynamic_objects: usize,
) -> ObjectNameKey {
    let bed_count = bed_index_vec.len();
    if object_index < bed_count {
        return ObjectNameKey::Bed(bed_index_vec[object_index] as u8);
    }
    let isf_start = bed_count;
    let isf_end = isf_start + num_isf_objects;
    if object_index < isf_end {
        return ObjectNameKey::Isf(object_index - isf_start);
    }
    let dyn_start = isf_end;
    let dyn_end = dyn_start + num_dynamic_objects;
    if object_index < dyn_end {
        return ObjectNameKey::Dynamic(object_id as usize);
    }
    ObjectNameKey::Dynamic(object_id as usize)
}

/// Extract spatial events from a TrueHD OAMD frame.
fn extract_events(
    oamd: &ObjectAudioMetadataPayload,
    base_sample_pos: u64,
) -> RVec<bridge_api::REvent> {
    let object_count = oamd.object_count;
    let Some(object_element) = &oamd.object_element else {
        return RVec::new();
    };

    if object_element.md_update_info.num_obj_info_blocks != 1 {
        log::warn!(
            "atmos-bridge: unsupported OAMD with num_obj_info_blocks={} (expected 1); skipping metadata frame",
            object_element.md_update_info.num_obj_info_blocks
        );
        return RVec::new();
    }
    if oamd.program_assignment.bed_assignment.len() != 1 {
        log::warn!(
            "atmos-bridge: unsupported OAMD with bed_assignment_count={} (expected 1); skipping metadata frame",
            oamd.program_assignment.bed_assignment.len()
        );
        return RVec::new();
    }
    if oamd.program_assignment.num_isf_objects != 0 {
        log::warn!(
            "atmos-bridge: unsupported OAMD with num_isf_objects={} (expected 0); skipping metadata frame",
            oamd.program_assignment.num_isf_objects
        );
        return RVec::new();
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

        let id = if object_data.b_object_in_bed_or_isf {
            let Some(&bed_idx) = bed_index_vec.get(i) else {
                bed_index_oob += 1;
                continue;
            };
            speaker_to_id(bed_idx) as u32
        } else {
            (i + 10 - bed_index_vec.len()) as u32
        };
        let (has_pos, pos, size) = if !object_data.b_object_in_bed_or_isf {
            let render = &object_data.object_render_info;
            match pos_vec.get(i).and_then(|raw_blocks| raw_blocks.first()) {
                Some(raw) if raw.len() >= 3 => (true, [raw[0], raw[1], raw[2]], render.object_size),
                Some(_) => (false, [0.0; 3], [0.0; 3]),
                None => {
                    missing_damf_pos += 1;
                    (false, [0.0; 3], [0.0; 3])
                }
            }
        } else {
            (false, [0.0; 3], [0.0; 3])
        };

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

    events
}
