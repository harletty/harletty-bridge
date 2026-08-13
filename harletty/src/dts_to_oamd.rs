//! Project a decoded DTS frame's layout onto the OAMD payload the DAMF writer
//! consumes.
//!
//! The sibling of `eac3_to_oamd`: decoder crates expose their native metadata
//! and each consumer owns its own mapping. `dca::spatial` says which extension
//! waveform sits where; this decides how that becomes a DAMF bed and objects.
//!
//! Only the DTS:X D3 presentation produces objects, and their positions are
//! static (see [`dca::XPresentation::object_positions`]). Every other
//! presentation is a fixed bed. When per-frame object coordinates are decoded
//! from the extension blob, they slot into [`DtsLayout::objects`] without any
//! change to this module's shape.

use dca::{BedChannel, SpatialChannel, XPresentation};
use truehd::structs::oamd::{
    BedAssignment, BlockUpdateInfo, MDUpdateInfo, ObjectAudioMetadataPayload, ObjectBasicInfo,
    ObjectData, ObjectElement, ObjectInfoBlock, ObjectRenderInfo, ProgramAssignment, SpeakerLabels,
};

/// Where one output bed channel's audio comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BedSource {
    /// `HdFrame::samples[i]`, or the i-th core fullband channel.
    Speaker(usize),
    /// `HdFrame::x_samples[i]` — a spatial-extension feed.
    Feed(usize),
}

/// One decoded DTS frame's channel layout, independent of whether the core or
/// the lossless HD decoder produced it.
///
/// `bed` is sorted ascending by [`SpeakerLabels`], because that is the order
/// DAMF declares a bed in: `BedAssignment` is a bitmask, so the writer emits
/// its channels in enum order regardless of the order the decoder produced
/// them. `bed_sources` is parallel to it and tells the caller which decoded
/// channel to interleave at each position — without it the `.atmos` would
/// describe channel 0 as L while the audio carried C, which is exactly what an
/// early version of this did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DtsLayout {
    /// Bed speakers, ascending by `SpeakerLabels`. Channels with no OAMD
    /// speaker equivalent are dropped and never appear here.
    pub bed: Vec<SpeakerLabels>,
    /// Source of each entry in `bed`, same length and order.
    pub bed_sources: Vec<BedSource>,
    /// Static object positions in DAMF space: `[x, y, z]`, each `-1.0..=1.0`,
    /// x left-to-right, y back-to-front, z floor-to-ceiling.
    pub objects: Vec<[f64; 3]>,
    /// Extension feed index backing each entry in `objects`.
    pub object_sources: Vec<usize>,
}

impl DtsLayout {
    /// Sort a `(speaker, source)` set into DAMF bed order and split it.
    fn from_pairs(mut pairs: Vec<(SpeakerLabels, BedSource)>) -> (Vec<SpeakerLabels>, Vec<BedSource>) {
        pairs.sort_by_key(|(speaker, _)| *speaker as u8);
        pairs.into_iter().unzip()
    }
}

/// Map a DCA core bed channel to its OAMD speaker.
///
/// `RearCenter` has no OAMD equivalent and is dropped, matching what the
/// E-AC-3 mapper does with the same channel.
pub fn bed_channel_to_speaker(channel: BedChannel) -> Option<SpeakerLabels> {
    Some(match channel {
        BedChannel::FrontLeft => SpeakerLabels::L,
        BedChannel::FrontRight => SpeakerLabels::R,
        BedChannel::Center => SpeakerLabels::C,
        BedChannel::LowFrequencyEffects => SpeakerLabels::LFE,
        BedChannel::SurroundLeft => SpeakerLabels::Lss,
        BedChannel::SurroundRight => SpeakerLabels::Rss,
        BedChannel::RearLeft => SpeakerLabels::Lrs,
        BedChannel::RearRight => SpeakerLabels::Rrs,
        BedChannel::WideLeft => SpeakerLabels::Lw,
        BedChannel::WideRight => SpeakerLabels::Rw,
        BedChannel::RearCenter => return None,
    })
}

/// Map a DCA speaker index (the index into `HdFrame::samples`) to its OAMD
/// speaker. Mirrors the realtime pipeline's `speaker_to_label`.
pub fn hd_speaker_to_speaker(index: usize) -> Option<SpeakerLabels> {
    Some(match index {
        0 => SpeakerLabels::C,
        1 => SpeakerLabels::L,
        2 => SpeakerLabels::R,
        3 => SpeakerLabels::Lss,
        4 => SpeakerLabels::Rss,
        5 => SpeakerLabels::LFE,
        // 6 is rear centre: no OAMD equivalent, same as the core mapper.
        7 => SpeakerLabels::Lrs,
        8 => SpeakerLabels::Rrs,
        _ => return None,
    })
}

/// Map a DTS:X spatial-extension channel to its OAMD speaker.
///
/// `TopFrontCenter` (the D0 profile's first feed) has no OAMD equivalent and is
/// dropped, so a D0 bed exports as 7.1.4 rather than 7.1.5.
pub fn spatial_channel_to_speaker(channel: SpatialChannel) -> Option<SpeakerLabels> {
    Some(match channel {
        SpatialChannel::TopFrontLeft => SpeakerLabels::Lfh,
        SpatialChannel::TopFrontRight => SpeakerLabels::Rfh,
        SpatialChannel::TopSideLeft => SpeakerLabels::Lts,
        SpatialChannel::TopSideRight => SpeakerLabels::Rts,
        SpatialChannel::TopBackLeft => SpeakerLabels::Lrh,
        SpatialChannel::TopBackRight => SpeakerLabels::Rrh,
        SpatialChannel::WideLeft => SpeakerLabels::Lw,
        SpatialChannel::WideRight => SpeakerLabels::Rw,
        SpatialChannel::TopFrontCenter => return None,
    })
}

impl DtsLayout {
    /// Layout of a plain DTS core frame: a bed, no objects.
    ///
    /// The decoder hands back fullband channels then LFE; this reorders them
    /// into DAMF bed order and records where each came from.
    pub fn from_core(channel_order: &[BedChannel], has_lfe: bool) -> Self {
        let mut pairs: Vec<(SpeakerLabels, BedSource)> = channel_order
            .iter()
            .enumerate()
            .filter_map(|(index, channel)| {
                bed_channel_to_speaker(*channel).map(|s| (s, BedSource::Speaker(index)))
            })
            .collect();
        if has_lfe {
            // LFE trails the fullband channels in the decoder's output.
            pairs.push((SpeakerLabels::LFE, BedSource::Speaker(channel_order.len())));
        }
        let (bed, bed_sources) = Self::from_pairs(pairs);
        Self {
            bed,
            bed_sources,
            objects: Vec::new(),
            object_sources: Vec::new(),
        }
    }

    /// Layout of a DTS-HD frame: the lossless bed, extended by whatever spatial
    /// presentation the frame carries.
    ///
    /// `active_speakers` are the DCA speaker indices present in the frame, in
    /// ascending order — the same order the realtime pipeline emits channels
    /// in, so the ADM channel order matches the audio it describes.
    pub fn from_hd(active_speakers: &[usize], presentation: Option<XPresentation>) -> Self {
        let mut pairs: Vec<(SpeakerLabels, BedSource)> = active_speakers
            .iter()
            .enumerate()
            .filter_map(|(position, speaker)| {
                hd_speaker_to_speaker(*speaker).map(|s| (s, BedSource::Speaker(position)))
            })
            .collect();
        let mut objects = Vec::new();
        let mut object_sources = Vec::new();

        if let Some(p) = presentation {
            match p.object_positions() {
                // Object presentation: the feeds are objects, not bed channels.
                Some(positions) => {
                    objects.extend_from_slice(positions);
                    object_sources.extend(0..positions.len());
                }
                // Fixed presentation: the feeds extend the bed.
                None => pairs.extend(p.channels().iter().enumerate().filter_map(
                    |(feed, channel)| {
                        spatial_channel_to_speaker(*channel).map(|s| (s, BedSource::Feed(feed)))
                    },
                )),
            }
        }

        let (bed, bed_sources) = Self::from_pairs(pairs);
        Self {
            bed,
            bed_sources,
            objects,
            object_sources,
        }
    }
}

/// Convert a DAMF-space coordinate (`-1.0..=1.0`) to the OAMD `pos3d` encoding.
///
/// OAMD stores x and y in `0.0..=1.0` and z in `-1.0..=1.0`, and its y axis runs
/// the other way: `ObjectAudioMetadataPayload::get_damf_pos` reads it back as
/// `(0.5 - y) * 2`. Getting this backwards silently mirrors every object
/// front-to-back, so it is asserted by a round-trip test below.
fn damf_pos_to_oamd(pos: [f64; 3]) -> [f64; 3] {
    [
        pos[0].clamp(-1.0, 1.0) / 2.0 + 0.5,
        0.5 - pos[1].clamp(-1.0, 1.0) / 2.0,
        pos[2].clamp(-1.0, 1.0),
    ]
}

fn static_object_block(position: [f64; 3]) -> ObjectInfoBlock {
    ObjectInfoBlock {
        b_object_not_active: false,
        b_object_in_bed_or_isf: false,
        object_basic_info: ObjectBasicInfo {
            object_gain: 0,
            object_priority: 0.0,
        },
        object_render_info: ObjectRenderInfo {
            pos3d: damf_pos_to_oamd(position),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Build the OAMD payload describing `layout`.
pub fn convert_dts(layout: &DtsLayout) -> ObjectAudioMetadataPayload {
    let mut assignment = BedAssignment::default();
    for speaker in &layout.bed {
        assignment.0[*speaker as usize] = true;
    }

    let program_assignment = ProgramAssignment {
        b_bed_chan_distribute: false,
        bed_assignment: vec![assignment],
        num_bed_objects: layout.bed.len(),
        num_isf_objects: 0,
        num_dynamic_objects: layout.objects.len(),
    };

    // One block per object, at a fixed position for the whole frame. Per-frame
    // coordinates from the extension blob would become several blocks here.
    let object_element = (!layout.objects.is_empty()).then(|| ObjectElement {
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
        object_data: layout
            .objects
            .iter()
            .map(|position| -> ObjectData { vec![static_object_block(*position)] })
            .collect(),
    });

    ObjectAudioMetadataPayload {
        evo_sample_offset: 0,
        oamd_version: 0,
        object_count: layout.objects.len(),
        program_assignment,
        b_alternate_object_data_present: false,
        object_element,
        trim_element: None,
        extended_object_element: None,
        // The elements a synthesized payload states nothing about: headphone
        // rendering intent and per-object dialogue indication.
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of `damf_pos_to_oamd`: what the DAMF writer reads back
    /// must be what `dca::spatial` declared, y axis included.
    #[test]
    fn object_positions_survive_the_round_trip_to_damf() {
        let layout = DtsLayout {
            bed: vec![SpeakerLabels::L, SpeakerLabels::R],
            bed_sources: vec![BedSource::Speaker(0), BedSource::Speaker(1)],
            objects: XPresentation::ObjectsD3
                .object_positions()
                .unwrap()
                .to_vec(),
            object_sources: (0..8).collect(),
        };
        let oamd = convert_dts(&layout);
        let read_back = oamd.get_damf_pos();

        assert_eq!(read_back.len(), layout.objects.len());
        for (index, expected) in layout.objects.iter().enumerate() {
            let actual = read_back[index][0];
            for axis in 0..3 {
                assert!(
                    (actual[axis] - expected[axis]).abs() < 1e-9,
                    "object {index} axis {axis}: got {} want {}",
                    actual[axis],
                    expected[axis]
                );
            }
        }
    }

    /// Guards the y inversion specifically — a mirrored mapping would still
    /// round-trip if it were applied symmetrically, so pin the encoding too.
    #[test]
    fn oamd_y_axis_is_inverted_relative_to_damf() {
        let front = damf_pos_to_oamd([0.0, 1.0, 0.0]);
        let back = damf_pos_to_oamd([0.0, -1.0, 0.0]);
        assert_eq!(front[1], 0.0, "damf front must encode as oamd y = 0");
        assert_eq!(back[1], 1.0, "damf back must encode as oamd y = 1");
        // x is not inverted.
        assert_eq!(damf_pos_to_oamd([-1.0, 0.0, 0.0])[0], 0.0);
        assert_eq!(damf_pos_to_oamd([1.0, 0.0, 0.0])[0], 1.0);
        // z passes through untouched.
        assert_eq!(damf_pos_to_oamd([0.0, 0.0, -1.0])[2], -1.0);
        assert_eq!(damf_pos_to_oamd([0.0, 0.0, 1.0])[2], 1.0);
    }

    #[test]
    fn core_layout_is_a_bed_with_no_objects() {
        let layout = DtsLayout::from_core(
            &[
                BedChannel::FrontLeft,
                BedChannel::FrontRight,
                BedChannel::Center,
                BedChannel::SurroundLeft,
                BedChannel::SurroundRight,
            ],
            true,
        );
        assert_eq!(
            layout.bed,
            vec![
                SpeakerLabels::L,
                SpeakerLabels::R,
                SpeakerLabels::C,
                SpeakerLabels::LFE,
                SpeakerLabels::Lss,
                SpeakerLabels::Rss,
            ],
            "declared in DAMF order, so LFE sits third rather than last"
        );
        assert!(layout.objects.is_empty());

        let oamd = convert_dts(&layout);
        assert_eq!(oamd.program_assignment.num_bed_objects, 6);
        assert_eq!(oamd.program_assignment.num_dynamic_objects, 0);
        assert!(oamd.object_element.is_none());
    }

    /// Rear centre exists in DTS but not in OAMD; it must be dropped, not
    /// mapped onto some other speaker.
    #[test]
    fn rear_centre_is_dropped_from_the_bed() {
        assert_eq!(bed_channel_to_speaker(BedChannel::RearCenter), None);
        assert_eq!(hd_speaker_to_speaker(6), None);
        let layout = DtsLayout::from_core(
            &[BedChannel::FrontLeft, BedChannel::RearCenter],
            false,
        );
        assert_eq!(layout.bed, vec![SpeakerLabels::L]);
    }

    #[test]
    fn standard_height_extends_the_bed_to_7_1_4() {
        // A full 7.1 lossless bed: every DCA speaker except rear centre.
        let layout = DtsLayout::from_hd(&[0, 1, 2, 3, 4, 5, 7, 8], Some(XPresentation::Height));
        assert_eq!(
            layout.bed,
            vec![
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
            ],
            "7.1 bed plus the four height feeds, in DAMF order"
        );
        assert!(layout.objects.is_empty(), "heights are channels, not objects");
    }

    /// D0 carries a top-front-centre feed that OAMD cannot name, so the bed
    /// comes out as 7.1.4 rather than 7.1.5.
    #[test]
    fn d0_drops_top_front_centre() {
        let layout = DtsLayout::from_hd(&[0, 1, 2, 3, 4, 5, 7, 8], Some(XPresentation::FixedD0));
        assert_eq!(XPresentation::FixedD0.feed_count(), 5, "D0 carries five feeds");
        assert_eq!(
            layout.bed.len(),
            8 + 4,
            "only four of the five reach the bed: top-front-centre has no OAMD speaker"
        );
        assert_eq!(layout.bed.len(), layout.bed_sources.len());
        assert!(layout.objects.is_empty());
    }

    #[test]
    fn d1_maps_its_wides_into_the_bed() {
        let layout = DtsLayout::from_hd(&[0, 1, 2, 3, 4, 5, 7, 8], Some(XPresentation::FixedD1));
        assert!(layout.bed.contains(&SpeakerLabels::Lw));
        assert!(layout.bed.contains(&SpeakerLabels::Rw));
        assert!(layout.objects.is_empty());
    }

    #[test]
    fn d3_becomes_objects_and_leaves_the_bed_alone() {
        let bed_only = DtsLayout::from_hd(&[0, 1, 2, 3, 4, 5, 7, 8], None);
        let layout = DtsLayout::from_hd(&[0, 1, 2, 3, 4, 5, 7, 8], Some(XPresentation::ObjectsD3));
        assert_eq!(layout.bed, bed_only.bed, "D3 feeds must not join the bed");
        assert_eq!(layout.objects.len(), 8);

        let oamd = convert_dts(&layout);
        assert_eq!(oamd.program_assignment.num_dynamic_objects, 8);
        assert_eq!(oamd.object_count, 8);
        let element = oamd.object_element.expect("D3 must emit an object element");
        assert_eq!(element.object_data.len(), 8);
        assert!(
            element
                .object_data
                .iter()
                .all(|blocks| blocks.len() == 1 && !blocks[0].b_object_in_bed_or_isf),
            "each object gets one block and none of them are bed channels"
        );
    }

    #[test]
    fn a_frame_with_no_presentation_is_just_its_bed() {
        let layout = DtsLayout::from_hd(&[1, 2], None);
        assert_eq!(layout.bed, vec![SpeakerLabels::L, SpeakerLabels::R]);
        assert!(layout.objects.is_empty());
    }

    /// Regression guard. The decoders emit channels in their own order (DCA
    /// core is C L R Ls Rs with LFE last), but DAMF declares a bed in
    /// `SpeakerLabels` order. Interleaving in decoder order while declaring in
    /// DAMF order silently mislabels every channel — verified against an
    /// external reference decode, which showed a pure permutation.
    #[test]
    fn bed_is_declared_in_damf_order_with_matching_sources() {
        // DCA core hands back C L R Ls Rs, then LFE.
        let layout = DtsLayout::from_core(
            &[
                BedChannel::Center,
                BedChannel::FrontLeft,
                BedChannel::FrontRight,
                BedChannel::SurroundLeft,
                BedChannel::SurroundRight,
            ],
            true,
        );

        assert_eq!(
            layout.bed,
            vec![
                SpeakerLabels::L,
                SpeakerLabels::R,
                SpeakerLabels::C,
                SpeakerLabels::LFE,
                SpeakerLabels::Lss,
                SpeakerLabels::Rss,
            ],
            "declared bed must be ascending by SpeakerLabels"
        );
        assert_eq!(
            layout.bed_sources,
            vec![
                BedSource::Speaker(1), // L was decoded second
                BedSource::Speaker(2), // R third
                BedSource::Speaker(0), // C first
                BedSource::Speaker(5), // LFE trails the fullband channels
                BedSource::Speaker(3),
                BedSource::Speaker(4),
            ],
            "each declared position must point at the channel that carries it"
        );

        // The invariant that matters: bed and sources stay parallel.
        assert_eq!(layout.bed.len(), layout.bed_sources.len());
    }

    /// The same ordering rule applies once height feeds join the bed: they are
    /// sorted in among the speakers, not appended.
    #[test]
    fn height_feeds_sort_into_the_bed_rather_than_trailing_it() {
        let layout = DtsLayout::from_hd(&[0, 1, 2, 3, 4, 5, 7, 8], Some(XPresentation::Height));
        assert_eq!(layout.bed.len(), layout.bed_sources.len());

        let mut sorted = layout.bed.clone();
        sorted.sort_by_key(|s| *s as u8);
        assert_eq!(layout.bed, sorted, "bed must already be in DAMF order");

        // Heights come from extension feeds, the 7.1 bed from speakers.
        let feeds: Vec<_> = layout
            .bed_sources
            .iter()
            .filter(|s| matches!(s, BedSource::Feed(_)))
            .collect();
        assert_eq!(feeds.len(), 4, "four height feeds land in the bed");
    }
}
