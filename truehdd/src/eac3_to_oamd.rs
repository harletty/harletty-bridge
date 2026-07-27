use eac3::{BedChannel, OamdElementKind, OamdObjectBlock, OamdObjectElement, OamdPayload};
use truehd::structs::oamd::{
    BedAssignment, BlockUpdateInfo, MDUpdateInfo, ObjectAudioMetadataPayload, ObjectBasicInfo,
    ObjectData, ObjectElement, ObjectInfoBlock, ObjectRenderInfo, ProgramAssignment,
    SpeakerLabels,
};

const GAIN_MINUS_INFINITY: i8 = -128;

fn bed_channel_to_speaker_index(channel: BedChannel) -> Option<usize> {
    let label = match channel {
        BedChannel::FrontLeft => SpeakerLabels::L,
        BedChannel::FrontRight => SpeakerLabels::R,
        BedChannel::Center => SpeakerLabels::C,
        BedChannel::LowFrequencyEffects => SpeakerLabels::LFE,
        BedChannel::SurroundLeft => SpeakerLabels::Lss,
        BedChannel::SurroundRight => SpeakerLabels::Rss,
        BedChannel::RearLeft => SpeakerLabels::Lrs,
        BedChannel::RearRight => SpeakerLabels::Rrs,
        BedChannel::TopFrontLeft => SpeakerLabels::Lfh,
        BedChannel::TopFrontRight => SpeakerLabels::Rfh,
        BedChannel::TopSurroundLeft => SpeakerLabels::Lts,
        BedChannel::TopSurroundRight => SpeakerLabels::Rts,
        BedChannel::TopRearLeft => SpeakerLabels::Lrh,
        BedChannel::TopRearRight => SpeakerLabels::Rrh,
        BedChannel::WideLeft => SpeakerLabels::Lw,
        BedChannel::WideRight => SpeakerLabels::Rw,
        BedChannel::LowFrequencyEffects2 => SpeakerLabels::LFE2,
        // Rear-center has no truehd speaker equivalent — drop it.
        BedChannel::RearCenter => return None,
    };
    Some(label as usize)
}

fn convert_bed_assignment(eac3_beds: &[Vec<BedChannel>]) -> Vec<BedAssignment> {
    eac3_beds
        .iter()
        .map(|channels| {
            let mut assignment = BedAssignment::default();
            for &ch in channels {
                if let Some(idx) = bed_channel_to_speaker_index(ch) {
                    assignment.0[idx] = true;
                }
            }
            assignment
        })
        .collect()
}

fn gain_to_i8(gain_db: Option<f32>) -> i8 {
    match gain_db {
        Some(g) if g.is_finite() => g.round().clamp(-127.0, 127.0) as i8,
        Some(_) => GAIN_MINUS_INFINITY,
        None => 0, // default = 0 dB when absent
    }
}

fn convert_object_block(
    eac3_block: &OamdObjectBlock,
    in_bed: bool,
) -> ObjectInfoBlock {
    let mut info = ObjectInfoBlock {
        b_object_not_active: eac3_block.inactive,
        b_object_in_bed_or_isf: in_bed,
        ..Default::default()
    };

    info.object_basic_info = ObjectBasicInfo {
        object_gain: gain_to_i8(eac3_block.gain),
        object_priority: eac3_block.priority.map(|p| p as f64 / 31.0).unwrap_or(0.0),
    };

    let mut render = ObjectRenderInfo::default();
    if let Some(pos) = eac3_block.position {
        render.pos3d = [pos.x as f64, pos.y as f64, pos.z as f64];
    }
    if let Some(size) = eac3_block.size {
        render.object_size = [size[0] as f64, size[1] as f64, size[2] as f64];
    }
    if let Some(d) = eac3_block.distance {
        render.b_object_distance_specified = true;
        render.distance_factor = Some(d as f64);
    }
    if let Some(sf) = eac3_block.screen_factor {
        render.b_object_use_screen_ref = true;
        render.screen_factor = sf as f64;
    }
    if let Some(df) = eac3_block.depth_factor {
        render.depth_factor = df as f64;
    }
    info.object_render_info = render;

    info
}

fn synthesize_object_element(
    eac3_object: &OamdObjectElement,
    bed_count: usize,
) -> ObjectElement {
    let sample_offset = eac3_object
        .block_updates
        .first()
        .map(|b| b.offset.max(0) as usize)
        .unwrap_or(0);
    let ramp_duration = eac3_object
        .block_updates
        .first()
        .map(|b| b.ramp_duration.max(0) as u16)
        .unwrap_or(0);

    let md_update_info = MDUpdateInfo {
        sample_offset,
        num_obj_info_blocks: 1,
        block_update_info: vec![BlockUpdateInfo {
            block_offset_factor_bits: 0,
            ramp_duration_code: 0,
            ramp_duration,
        }],
    };

    let mut object_data: Vec<ObjectData> = Vec::with_capacity(eac3_object.object_blocks.len());
    for (object_index, blocks) in eac3_object.object_blocks.iter().enumerate() {
        let in_bed = object_index < bed_count;
        let first_block = blocks
            .first()
            .map(|b| convert_object_block(b, in_bed))
            .unwrap_or_else(|| ObjectInfoBlock {
                b_object_not_active: true,
                b_object_in_bed_or_isf: in_bed,
                ..Default::default()
            });
        object_data.push(vec![first_block]);
    }

    ObjectElement {
        md_update_info,
        b_reserved_data_not_present: true,
        reserved_data: 0,
        object_data,
    }
}

pub fn convert_oamd(
    eac3: &OamdPayload,
    sample_offset: Option<u16>,
) -> ObjectAudioMetadataPayload {
    let bed_assignment = convert_bed_assignment(&eac3.bed_assignment);

    let program_assignment = ProgramAssignment {
        b_bed_chan_distribute: false,
        bed_assignment,
        num_bed_objects: eac3.beds,
        num_isf_objects: 0,
        num_dynamic_objects: eac3.dynamic_objects,
    };

    let bed_count = program_assignment.beds_or_isf_count();
    let object_element = eac3
        .elements
        .iter()
        .find_map(|element| match &element.kind {
            OamdElementKind::Object(obj) => Some(synthesize_object_element(obj, bed_count)),
            _ => None,
        });

    ObjectAudioMetadataPayload {
        evo_sample_offset: sample_offset.map(|s| s as u64).unwrap_or(0),
        oamd_version: 0,
        object_count: eac3.object_count,
        program_assignment,
        b_alternate_object_data_present: eac3.alternate_object_present,
        object_element,
        trim_element: None,
        extended_object_element: None,
        oa_element_md: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use damf::{Data, SourceCodec};
    use eac3::{OamdBlockUpdate, OamdElement, OamdElementKind, ObjectAnchor, Vec3};
    use std::path::Path;

    fn empty_eac3_payload() -> OamdPayload {
        OamdPayload {
            version: 0,
            object_count: 0,
            alternate_object_present: false,
            element_count: 0,
            beds: 0,
            bed_instances: 0,
            bed_or_isf_objects: 0,
            dynamic_objects: 0,
            isf_in_use: false,
            isf_index: None,
            bed_assignment: Vec::new(),
            elements: Vec::new(),
        }
    }

    #[test]
    fn maps_5_1_bed() {
        let mut payload = empty_eac3_payload();
        payload.beds = 6;
        payload.object_count = 6;
        payload.bed_instances = 1;
        payload.bed_or_isf_objects = 6;
        payload.bed_assignment = vec![vec![
            BedChannel::FrontLeft,
            BedChannel::FrontRight,
            BedChannel::Center,
            BedChannel::LowFrequencyEffects,
            BedChannel::SurroundLeft,
            BedChannel::SurroundRight,
        ]];
        let out = convert_oamd(&payload, None);
        let bed = &out.program_assignment.bed_assignment[0];
        assert!(bed.0[SpeakerLabels::L as usize]);
        assert!(bed.0[SpeakerLabels::R as usize]);
        assert!(bed.0[SpeakerLabels::C as usize]);
        assert!(bed.0[SpeakerLabels::LFE as usize]);
        assert!(bed.0[SpeakerLabels::Lss as usize]);
        assert!(bed.0[SpeakerLabels::Rss as usize]);
    }

    #[test]
    fn converts_object_with_position() {
        let block = OamdObjectBlock {
            inactive: false,
            basic_info_status: 1,
            basic_info_blocks: Some(3),
            render_info_status: 1,
            render_info_blocks: Some(15),
            anchor: ObjectAnchor::Room,
            gain: Some(0.0),
            priority: Some(31),
            valid_position: true,
            differential_position: false,
            position: Some(Vec3 { x: 0.25, y: 0.5, z: 0.0 }),
            distance: None,
            size: Some([0.0, 0.0, 0.0]),
            screen_factor: None,
            depth_factor: None,
            additional_data_bytes: 0,
        };
        let element = OamdObjectElement {
            sample_offset: 0,
            block_updates: vec![OamdBlockUpdate {
                offset: 0,
                ramp_duration: 1536,
            }],
            object_blocks: vec![vec![block]],
        };
        let mut payload = empty_eac3_payload();
        payload.object_count = 1;
        payload.dynamic_objects = 1;
        payload.element_count = 1;
        payload.elements = vec![OamdElement {
            element_index: 0,
            byte_length: 0,
            kind: OamdElementKind::Object(element),
        }];
        let out = convert_oamd(&payload, Some(8));
        assert_eq!(out.evo_sample_offset, 8);
        let oe = out.object_element.expect("element");
        assert_eq!(oe.md_update_info.num_obj_info_blocks, 1);
        assert_eq!(oe.md_update_info.block_update_info[0].ramp_duration, 1536);
        assert_eq!(oe.object_data.len(), 1);
        let info = &oe.object_data[0][0];
        assert!(!info.b_object_in_bed_or_isf);
        assert_eq!(info.object_render_info.pos3d, [0.25, 0.5, 0.0]);
        assert_eq!(info.object_basic_info.object_gain, 0);
    }

    #[test]
    fn round_trip_into_damf_writer() {
        // Mimics a real EAC3 JOC payload: LFE-only bed + one dynamic object.
        let lfe_bed_block = OamdObjectBlock {
            inactive: false,
            basic_info_status: 1,
            basic_info_blocks: Some(3),
            render_info_status: 0,
            render_info_blocks: None,
            anchor: ObjectAnchor::Speaker,
            gain: Some(0.0),
            priority: Some(31),
            valid_position: false,
            differential_position: false,
            position: None,
            distance: None,
            size: None,
            screen_factor: None,
            depth_factor: None,
            additional_data_bytes: 0,
        };
        let object_block = OamdObjectBlock {
            inactive: false,
            basic_info_status: 1,
            basic_info_blocks: Some(3),
            render_info_status: 1,
            render_info_blocks: Some(15),
            anchor: ObjectAnchor::Room,
            gain: Some(0.0),
            priority: Some(31),
            valid_position: true,
            differential_position: false,
            position: Some(Vec3 { x: 0.0, y: 0.0, z: 0.0 }),
            distance: None,
            size: Some([0.0, 0.0, 0.0]),
            screen_factor: None,
            depth_factor: None,
            additional_data_bytes: 0,
        };
        let element = OamdObjectElement {
            sample_offset: 0,
            block_updates: vec![OamdBlockUpdate {
                offset: 0,
                ramp_duration: 1536,
            }],
            object_blocks: vec![vec![lfe_bed_block], vec![object_block]],
        };
        let mut payload = empty_eac3_payload();
        payload.object_count = 2;
        payload.beds = 1;
        payload.bed_or_isf_objects = 1;
        payload.bed_instances = 1;
        payload.dynamic_objects = 1;
        payload.element_count = 1;
        payload.bed_assignment = vec![vec![BedChannel::LowFrequencyEffects]];
        payload.elements = vec![OamdElement {
            element_index: 0,
            byte_length: 0,
            kind: OamdElementKind::Object(element),
        }];

        let converted = convert_oamd(&payload, None);
        let data = Data::with_oamd_payload(&converted, Path::new("sample"), SourceCodec::Eac3Joc, crate::CREATION_TOOL);
        let yaml = serde_yaml_ng::to_string(&data).unwrap();
        assert!(yaml.contains("sourceCodec: EAC3-JOC"));
        assert!(yaml.contains("- ID: 10"));
    }
}
