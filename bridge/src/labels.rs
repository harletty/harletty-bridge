use abi_stable::std_types::RVec;
use bridge_api::{RChannelLabel, RChannelPose};
use truehd::structs::channel::ChannelLabel;

/// Where DTS puts its lower-layer speakers, as azimuth in degrees at ear
/// level, for the bridge's channel declaration: the "approximate angle in
/// horizontal plane" column of Table 6-22 (Loudspeaker Masks) of ETSI
/// TS 102 114 V1.6.1 — C 0°, L/R ±30°, Ls/Rs ±110° ("on side in rear"),
/// Lsr/Rsr ±150°, Cs 180°, Lc/Rc ±15°, Lw/Rw ±60°. The table gives the
/// height speakers no angle, so they are not declared and take the
/// renderer's own nominal directions. Every label is declared whatever the
/// presentation carries: the renderer ignores the ones absent from the
/// frame.
pub(crate) fn dts_declared_poses() -> RVec<RChannelPose> {
    use RChannelLabel::*;
    [
        (C, 0.0),
        (L, -30.0),
        (R, 30.0),
        (Ls, -110.0),
        (Rs, 110.0),
        (Lb, -150.0),
        (Rb, 150.0),
        (Cb, 180.0),
        (Lsc, -15.0),
        (Rsc, 15.0),
        (Lw, -60.0),
        (Rw, 60.0),
    ]
    .into_iter()
    .map(|(label, azimuth_deg)| RChannelPose {
        label,
        azimuth_deg,
        elevation_deg: 0.0,
    })
    .collect()
}

/// Convert a TrueHD `ChannelLabel` to its ABI-stable counterpart.
pub(crate) fn channel_label_to_r(label: &ChannelLabel) -> RChannelLabel {
    match label {
        ChannelLabel::L => RChannelLabel::L,
        ChannelLabel::R => RChannelLabel::R,
        ChannelLabel::C => RChannelLabel::C,
        ChannelLabel::LFE => RChannelLabel::LFE,
        ChannelLabel::Ls => RChannelLabel::Ls,
        ChannelLabel::Rs => RChannelLabel::Rs,
        ChannelLabel::Tfl => RChannelLabel::Tfl,
        ChannelLabel::Tfr => RChannelLabel::Tfr,
        ChannelLabel::Tsl => RChannelLabel::Tsl,
        ChannelLabel::Tsr => RChannelLabel::Tsr,
        ChannelLabel::Tbl => RChannelLabel::Tbl,
        ChannelLabel::Tbr => RChannelLabel::Tbr,
        ChannelLabel::Lsc => RChannelLabel::Lsc,
        ChannelLabel::Rsc => RChannelLabel::Rsc,
        ChannelLabel::Lb => RChannelLabel::Lb,
        ChannelLabel::Rb => RChannelLabel::Rb,
        ChannelLabel::Cb => RChannelLabel::Cb,
        ChannelLabel::Tc => RChannelLabel::Tc,
        ChannelLabel::Lsd => RChannelLabel::Lsd,
        ChannelLabel::Rsd => RChannelLabel::Rsd,
        ChannelLabel::Lw => RChannelLabel::Lw,
        ChannelLabel::Rw => RChannelLabel::Rw,
        ChannelLabel::Tfc => RChannelLabel::Tfc,
        ChannelLabel::LFE2 => RChannelLabel::LFE2,
    }
}

/// Convert an E-AC3 `BedChannel` to its ABI-stable counterpart.
/// Map a DCA (DTS) bed channel to the renderer's channel label. DTS core beds
/// cover the 5.1/7.1 layout; the renderer places each at its canonical speaker.
pub(crate) fn dca_bed_channel_to_r(ch: dca::BedChannel) -> RChannelLabel {
    use dca::BedChannel;
    match ch {
        BedChannel::FrontLeft => RChannelLabel::L,
        BedChannel::FrontRight => RChannelLabel::R,
        BedChannel::Center => RChannelLabel::C,
        BedChannel::LowFrequencyEffects => RChannelLabel::LFE,
        BedChannel::SurroundLeft => RChannelLabel::Ls,
        BedChannel::SurroundRight => RChannelLabel::Rs,
        BedChannel::RearCenter => RChannelLabel::Cb,
        BedChannel::RearLeft => RChannelLabel::Lb,
        BedChannel::RearRight => RChannelLabel::Rb,
        BedChannel::WideLeft => RChannelLabel::Lw,
        BedChannel::WideRight => RChannelLabel::Rw,
    }
}

/// Map a DTS:X spatial-extension channel to the renderer's channel label.
///
/// Which extension waveform sits at which position is codec knowledge and lives
/// in `dca::spatial`; this is only the ABI projection of it, so the realtime
/// path and the offline ADM exporter cannot drift apart on the mapping.
pub(crate) fn dca_spatial_channel_to_r(ch: dca::SpatialChannel) -> RChannelLabel {
    use dca::SpatialChannel;
    match ch {
        SpatialChannel::TopFrontLeft => RChannelLabel::Tfl,
        SpatialChannel::TopFrontRight => RChannelLabel::Tfr,
        SpatialChannel::TopFrontCenter => RChannelLabel::Tfc,
        SpatialChannel::TopSideLeft => RChannelLabel::Tsl,
        SpatialChannel::TopSideRight => RChannelLabel::Tsr,
        SpatialChannel::TopBackLeft => RChannelLabel::Tbl,
        SpatialChannel::TopBackRight => RChannelLabel::Tbr,
        SpatialChannel::WideLeft => RChannelLabel::Lw,
        SpatialChannel::WideRight => RChannelLabel::Rw,
    }
}

pub(crate) fn bed_channel_to_r(ch: eac3::BedChannel) -> RChannelLabel {
    use eac3::BedChannel;
    match ch {
        BedChannel::FrontLeft => RChannelLabel::L,
        BedChannel::FrontRight => RChannelLabel::R,
        BedChannel::Center => RChannelLabel::C,
        BedChannel::LowFrequencyEffects => RChannelLabel::LFE,
        BedChannel::SurroundLeft => RChannelLabel::Ls,
        BedChannel::SurroundRight => RChannelLabel::Rs,
        BedChannel::RearCenter => RChannelLabel::Cb,
        BedChannel::RearLeft => RChannelLabel::Lb,
        BedChannel::RearRight => RChannelLabel::Rb,
        BedChannel::TopFrontLeft => RChannelLabel::Tfl,
        BedChannel::TopFrontRight => RChannelLabel::Tfr,
        BedChannel::TopSurroundLeft => RChannelLabel::Tsl,
        BedChannel::TopSurroundRight => RChannelLabel::Tsr,
        BedChannel::TopRearLeft => RChannelLabel::Tbl,
        BedChannel::TopRearRight => RChannelLabel::Tbr,
        BedChannel::WideLeft => RChannelLabel::Lw,
        BedChannel::WideRight => RChannelLabel::Rw,
        BedChannel::LowFrequencyEffects2 => RChannelLabel::LFE2,
    }
}

/// Channel label for an OAMD bed-assignment speaker index
/// (`truehd::structs::oamd::SpeakerLabels` order).
pub(crate) fn oamd_speaker_to_label(speaker_index: usize) -> RChannelLabel {
    match speaker_index {
        0 => RChannelLabel::L,
        1 => RChannelLabel::R,
        2 => RChannelLabel::C,
        3 => RChannelLabel::LFE,
        4 => RChannelLabel::Ls,   // Lss
        5 => RChannelLabel::Rs,   // Rss
        6 => RChannelLabel::Lb,   // Lrs
        7 => RChannelLabel::Rb,   // Rrs
        8 => RChannelLabel::Tfl,  // Lfh (front height)
        9 => RChannelLabel::Tfr,  // Rfh
        10 => RChannelLabel::Tsl, // Lts (top side)
        11 => RChannelLabel::Tsr, // Rts
        12 => RChannelLabel::Tbl, // Lrh (rear height)
        13 => RChannelLabel::Tbr, // Rrh
        14 => RChannelLabel::Lw,
        15 => RChannelLabel::Rw,
        16 => RChannelLabel::LFE2,
        _ => RChannelLabel::Unknown,
    }
}

/// Map an Auro stream to the renderer's channel label.
///
/// The floor is the shared 7.1 set. The height layer is the renderer's
/// height tier — `Lh`/`Rh`/`Ch`/`Lhs`/`Rhs`, 30° over the floor speaker of
/// the same name — not the top tier, whose labels mean the ceiling corners;
/// the Top is the single overhead `Tc`. Where each of them sits is declared
/// alongside ([`crate::auro_pipeline::auro_pose`]), so the renderer places
/// the layer at Auro's own angles whatever its room ratio is. Stream 15, the
/// second top of the `_2T` layouts, has no public position and stays
/// unlabelled.
pub(crate) fn auro_stream_to_r(stream: auro::StreamId) -> RChannelLabel {
    match stream.0 {
        0 => RChannelLabel::L,
        1 => RChannelLabel::R,
        2 => RChannelLabel::C,
        3 => RChannelLabel::LFE,
        4 => RChannelLabel::Ls,
        5 => RChannelLabel::Rs,
        6 => RChannelLabel::Cb,
        7 => RChannelLabel::Lb,
        8 => RChannelLabel::Rb,
        9 => RChannelLabel::Lh,
        10 => RChannelLabel::Rh,
        11 => RChannelLabel::Ch,
        12 => RChannelLabel::Tc,
        13 => RChannelLabel::Lhs,
        14 => RChannelLabel::Rhs,
        _ => RChannelLabel::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dts_declares_its_lower_layer_at_the_etsi_angles_and_no_heights() {
        let poses = dts_declared_poses();
        let angle = |label: RChannelLabel| {
            poses
                .iter()
                .find(|p| p.label == label)
                .map(|p| (p.azimuth_deg, p.elevation_deg))
        };
        assert_eq!(angle(RChannelLabel::Ls), Some((-110.0, 0.0)));
        assert_eq!(angle(RChannelLabel::Rb), Some((150.0, 0.0)));
        assert_eq!(angle(RChannelLabel::Cb), Some((180.0, 0.0)));
        assert_eq!(angle(RChannelLabel::LFE), None, "a subwoofer has no angle");
        assert_eq!(
            angle(RChannelLabel::Tfl),
            None,
            "heights take the renderer's directions"
        );
        assert!(poses.iter().all(|p| p.elevation_deg == 0.0));
    }
}
