//! Project an IAMF sequence onto the OAMD payload the DAMF writer consumes:
//! the bed its non-object elements render into, and its objects.
//!
//! The sibling of `dts_to_oamd`. A payload describes the master set's header
//! and the first state of its objects; the objects' later moves are written
//! as position events straight from the trajectory the decoder evaluates
//! (see the IAMF decode handler), since an IAMF object states where it is
//! every few milliseconds rather than in payloads restating every field.

use truehd::structs::oamd::{
    BedAssignment, BlockUpdateInfo, MDUpdateInfo, ObjectAudioMetadataPayload, ObjectBasicInfo,
    ObjectData, ObjectElement, ObjectInfoBlock, ObjectRenderInfo, ProgramAssignment, SpeakerLabels,
};

/// The speakers of BS.2051 System J, in the decoder's IAMF channel order
/// (L, R, C, LFE, Lss, Rss, Lrs, Rrs, Ltf, Rtf, Ltb, Rtb): the upper layer at
/// ±45° and ±135° is the front and rear heights. The order is also DAMF's,
/// so a bed taken from it in this order needs no sorting.
pub const SYSTEM_J_SPEAKERS: [SpeakerLabels; 12] = [
    SpeakerLabels::L,
    SpeakerLabels::R,
    SpeakerLabels::C,
    SpeakerLabels::LFE,
    SpeakerLabels::Lss,
    SpeakerLabels::Rss,
    SpeakerLabels::Lrs,
    SpeakerLabels::Rrs,
    SpeakerLabels::Lfh,
    SpeakerLabels::Rfh,
    SpeakerLabels::Lrh,
    SpeakerLabels::Rrh,
];

/// Convert a DAMF-space coordinate (`-1.0..=1.0`, y to the front) to the
/// OAMD `pos3d` encoding, whose x and y run `0.0..=1.0` and whose y runs to
/// the back: what `ObjectAudioMetadataPayload::get_damf_pos` reads back, to
/// within the rounding of the halving.
fn damf_pos_to_oamd(pos: [f64; 3]) -> [f64; 3] {
    [
        pos[0].clamp(-1.0, 1.0) / 2.0 + 0.5,
        0.5 - pos[1].clamp(-1.0, 1.0) / 2.0,
        pos[2].clamp(-1.0, 1.0),
    ]
}

fn object_block(position: [f64; 3]) -> ObjectInfoBlock {
    ObjectInfoBlock {
        b_object_not_active: false,
        b_object_in_bed_or_isf: false,
        object_basic_info: ObjectBasicInfo {
            // harlettizer bakes each object's gain into its PCM, and the
            // decoder applies the mix gains: what is left is unity.
            object_gain: 0,
            // IAMF states no priority; full importance is what the masters
            // these streams are made from carry.
            object_priority: 1.0,
        },
        object_render_info: ObjectRenderInfo {
            pos3d: damf_pos_to_oamd(position),
            ..Default::default()
        },
    }
}

/// The payload of a master set with the bed `bed` (DAMF order) and objects
/// at `objects` (DAMF space): its header, and the first event of each
/// object at the sample it is stated at, with no ramp.
pub fn convert_iamf(bed: &[SpeakerLabels], objects: &[[f64; 3]]) -> ObjectAudioMetadataPayload {
    let mut assignment = BedAssignment::default();
    for speaker in bed {
        assignment.0[*speaker as usize] = true;
    }

    let program_assignment = ProgramAssignment {
        b_bed_chan_distribute: false,
        bed_assignment: vec![assignment],
        num_bed_objects: bed.len(),
        num_isf_objects: 0,
        num_dynamic_objects: objects.len(),
    };

    let object_element = (!objects.is_empty()).then(|| ObjectElement {
        md_update_info: MDUpdateInfo {
            sample_offset: 0,
            num_obj_info_blocks: 1,
            block_update_info: vec![BlockUpdateInfo {
                block_offset_factor_bits: 0,
                ramp_duration_code: 0,
                ramp_duration: 0,
            }],
        },
        b_reserved_data_not_present: true,
        reserved_data: 0,
        object_data: objects
            .iter()
            .map(|position| -> ObjectData { vec![object_block(*position)] })
            .collect(),
    });

    ObjectAudioMetadataPayload {
        evo_sample_offset: 0,
        oamd_version: 0,
        object_count: objects.len(),
        program_assignment,
        b_alternate_object_data_present: false,
        object_element,
        trim_element: None,
        extended_object_element: None,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use damf::{Configuration, CreationTool, Data, SourceCodec};

    #[test]
    fn object_positions_survive_the_round_trip_to_damf() {
        let objects = [[-0.5, 0.75, 0.25], [0.5, -0.75, 0.0], [0.0, 0.0, 1.0]];
        let oamd = convert_iamf(&[SpeakerLabels::LFE], &objects);
        let read_back = oamd.get_damf_pos();
        for (index, expected) in objects.iter().enumerate() {
            for axis in 0..3 {
                assert!((read_back[index][0][axis] - expected[axis]).abs() < 1e-12);
            }
        }
    }

    /// The bed the header declares and the IDs of the objects it lists are
    /// the ones the first events name: an LFE-only bed at ID 3, the objects
    /// from 10, as the masters these streams are made from have them.
    #[test]
    fn an_lfe_bed_and_objects_make_the_header_and_first_events() {
        let oamd = convert_iamf(&[SpeakerLabels::LFE], &[[-1.0, 1.0, 0.0], [1.0, 1.0, 0.0]]);
        let tool = CreationTool {
            name: "test",
            version: "0",
        };
        let header =
            Data::with_oamd_payload(&oamd, std::path::Path::new("t"), SourceCodec::Iamf, tool)
                .serialize_damf();
        assert!(header.contains("sourceCodec: IAMF\n"), "{header}");
        assert!(
            header.contains("- channel: LFE\n            ID: 3\n"),
            "{header}"
        );
        assert!(header.contains("- ID: 10\n      - ID: 11\n"), "{header}");

        let mut events = Configuration::with_oamd_payload(&oamd, 48_000, 0).unwrap();
        let written = events.serialize_events(false);
        assert!(
            written.starts_with("sampleRate: 48000\nevents:\n"),
            "{written}"
        );
        assert!(written.contains("ID: 10\n    samplePos: 0\n"), "{written}");
        assert!(written.contains("pos: [-1, 1, 0]"), "{written}");
        assert!(written.contains("pos: [1, 1, 0]"), "{written}");
        assert!(written.contains("rampLength: 0"), "{written}");
        assert!(!written.contains("ID: 3\n"), "no bed event: {written}");
    }
}
