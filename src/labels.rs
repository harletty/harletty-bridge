use bridge_api::RChannelLabel;
use eac3::CorePcmFrame;
use truehd::structs::channel::ChannelLabel;

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

/// Map a `BedChannel` to the speaker ID space used by the bridge.
pub(crate) fn bed_channel_to_speaker_id(ch: eac3::BedChannel) -> usize {
    use eac3::BedChannel;
    match ch {
        BedChannel::FrontLeft => 0,
        BedChannel::FrontRight => 1,
        BedChannel::Center => 2,
        BedChannel::LowFrequencyEffects => 3,
        BedChannel::SurroundLeft => 4,
        BedChannel::SurroundRight => 5,
        BedChannel::RearCenter => 6,
        BedChannel::RearLeft => 6,
        BedChannel::RearRight => 7,
        BedChannel::TopFrontLeft => 8,
        BedChannel::TopFrontRight => 9,
        BedChannel::TopSurroundLeft => 8,
        BedChannel::TopSurroundRight => 9,
        BedChannel::TopRearLeft => 8,
        BedChannel::TopRearRight => 9,
        BedChannel::WideLeft => 0,
        BedChannel::WideRight => 1,
        BedChannel::LowFrequencyEffects2 => 3,
    }
}

pub(crate) fn eac3_core_bed_indices(core: &CorePcmFrame) -> Vec<usize> {
    let mut bed_indices = Vec::with_capacity(core.total_channels());
    bed_indices.extend(
        core.fullband_channel_order
            .iter()
            .copied()
            .map(bed_channel_to_speaker_id),
    );
    if core.lfe_channel.is_some() {
        bed_indices.push(bed_channel_to_speaker_id(
            eac3::BedChannel::LowFrequencyEffects,
        ));
    }
    bed_indices
}

/// Remap a speaker index to the Atmos channel-ID space.
/// IDs 0-9 are bed channels; 10+ are dynamic objects.
pub(crate) fn speaker_to_id(speaker_index: usize) -> usize {
    match speaker_index {
        0..8 => speaker_index,
        8..10 => speaker_index + 122,
        10..12 => speaker_index - 2,
        _ => speaker_index + 120,
    }
}
