// SPDX-License-Identifier: Apache-2.0
//
// Analyze the compact navigation word preceding the two bare XLL channel sets
// in the alternate end-of-frame extension profile.
//
// Usage:
//   cargo run -p dca --release --example xll_alt_nav -- <in.dts> [max_mb]

// This is an offline diagnostic. It deliberately discovers the channel-set
// boundaries by CRC rather than adding a per-frame scan to the realtime path.

use std::collections::{BTreeSet, HashMap};
use std::io::Read;

use dca::parser::parse_header;
use dca::{exss_substream_size, HdDecoder};

const ALT_SYNC_D0: [u8; 4] = 0xf140_00d0u32.to_be_bytes();
const ALT_SYNC_D1: [u8; 4] = 0xf140_00d1u32.to_be_bytes();
const INNER_SUFFIX: [u8; 6] = [0x02, 0x34, 0x38, 0x8c, 0x4f, 0x00];

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct Candidate {
    byte_offset: usize,
    header_size: usize,
    channels: usize,
    pcm_resolution: usize,
    storage_resolution: usize,
    residual_mask: u32,
}

#[derive(Clone, Debug)]
struct Observation {
    frame: usize,
    payload_size: usize,
    payload: Vec<u8>,
    prefix: Vec<u8>,
    navigation: Vec<u8>,
    first: Candidate,
    second: Candidate,
    first_header: Vec<u8>,
    second_header: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum TerminalNaviOrder {
    ChannelSetMajor,
    SegmentMajor,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct TerminalNaviLayout {
    segments: usize,
    size_bits: usize,
    trailer_bytes: usize,
    order: TerminalNaviOrder,
    first_gap: isize,
    second_gap: isize,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct CommonParameters {
    segments: usize,
    segment_samples: usize,
    segment_size_bits: usize,
    band_crc_mode: u32,
    scalable_lsbs: bool,
    channel_mask_bits: usize,
    fixed_lsb_width: usize,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum HeaderSyntax {
    StandardUnmapped,
    StandardOneToOne,
    AlternatePrefix(usize),
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct HeaderConfig {
    syntax: HeaderSyntax,
    channel_mask_bits: usize,
    channel_mask: Option<u32>,
    positions: Vec<(u16, u16, u8)>,
    segment_size_bits: usize,
    scalable_lsbs: bool,
    band_crc_mode: u32,
    lsb_section_size: usize,
    predictor_orders: Vec<u32>,
    tail_bits: usize,
}

#[derive(Clone, Copy)]
enum Target {
    PayloadSize,
    SecondOffset,
    FirstSpan,
    FirstBody,
    SecondSpan,
    SecondBody,
}

impl Target {
    const ALL: [Self; 6] = [
        Self::PayloadSize,
        Self::SecondOffset,
        Self::FirstSpan,
        Self::FirstBody,
        Self::SecondSpan,
        Self::SecondBody,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::PayloadSize => "payload_size",
            Self::SecondOffset => "second_offset",
            Self::FirstSpan => "first_span",
            Self::FirstBody => "first_body",
            Self::SecondSpan => "second_span",
            Self::SecondBody => "second_body",
        }
    }

    fn value(self, observation: &Observation) -> usize {
        match self {
            Self::PayloadSize => observation.payload_size,
            Self::SecondOffset => observation.second.byte_offset,
            Self::FirstSpan => observation.second.byte_offset - observation.first.byte_offset,
            Self::FirstBody => {
                observation.second.byte_offset
                    - observation.first.byte_offset
                    - observation.first.header_size
            }
            Self::SecondSpan => observation.payload_size - observation.second.byte_offset,
            Self::SecondBody => {
                observation.payload_size
                    - observation.second.byte_offset
                    - observation.second.header_size
            }
        }
    }
}

fn read_bits(data: &[u8], bit_offset: usize, width: usize) -> Option<u32> {
    if width > 32 || bit_offset.checked_add(width)? > data.len() * 8 {
        return None;
    }
    let mut value = 0u32;
    for bit in bit_offset..bit_offset + width {
        value = (value << 1) | ((data[bit / 8] >> (7 - bit % 8)) & 1) as u32;
    }
    Some(value)
}

fn take_bits(data: &[u8], bit: &mut usize, width: usize) -> Option<u32> {
    let value = read_bits(data, *bit, width)?;
    *bit = bit.checked_add(width)?;
    Some(value)
}

fn ceil_log2(value: usize) -> usize {
    if value <= 1 {
        0
    } else {
        (usize::BITS - (value - 1).leading_zeros()) as usize
    }
}

fn parse_header_config(
    header: &[u8],
    payload_size: usize,
    syntax: HeaderSyntax,
    channel_mask_bits: usize,
    segment_size_bits: usize,
    scalable_lsbs: bool,
    band_crc_mode: u32,
) -> Option<HeaderConfig> {
    let mut bit = 0usize;
    let header_size = take_bits(header, &mut bit, 10)? as usize + 1;
    let channels = take_bits(header, &mut bit, 4)? as usize + 1;
    if header_size != header.len() || channels > 16 {
        return None;
    }
    take_bits(header, &mut bit, channels)?;
    let pcm_resolution = take_bits(header, &mut bit, 5)? as usize + 1;
    let storage_resolution = take_bits(header, &mut bit, 5)? as usize + 1;
    let frequency_index = take_bits(header, &mut bit, 4)?;
    let frequency_modifier = take_bits(header, &mut bit, 2)?;
    let replacement_set = take_bits(header, &mut bit, 2)?;
    if pcm_resolution > storage_resolution
        || !matches!(storage_resolution, 16 | 20 | 24)
        || frequency_index != 12
        || frequency_modifier != 0
        || replacement_set != 0
    {
        return None;
    }

    let mut channel_mask = None;
    let mut positions = Vec::new();
    if syntax == HeaderSyntax::StandardOneToOne {
        take_bits(header, &mut bit, 1)?; // primary channel set
        let downmix_coefficients = take_bits(header, &mut bit, 1)? != 0;
        if downmix_coefficients {
            // The coefficient count depends on hierarchy state. Keep this
            // first probe strict rather than guessing through that syntax.
            return None;
        }
        take_bits(header, &mut bit, 1)?; // hierarchical channel set
        let mask_enabled = take_bits(header, &mut bit, 1)? != 0;
        if mask_enabled {
            channel_mask = Some(take_bits(header, &mut bit, channel_mask_bits)?);
            if channel_mask?.count_ones() as usize != channels {
                return None;
            }
        } else {
            for _ in 0..channels {
                positions.push((
                    take_bits(header, &mut bit, 9)? as u16,
                    take_bits(header, &mut bit, 9)? as u16,
                    take_bits(header, &mut bit, 7)? as u8,
                ));
            }
        }
    } else if syntax == HeaderSyntax::StandardUnmapped && take_bits(header, &mut bit, 1)? != 0 {
        let coefficient_bits = 6 + 2 * take_bits(header, &mut bit, 3)? as usize;
        let speaker_configurations = take_bits(header, &mut bit, 2)? as usize + 1;
        for _ in 0..speaker_configurations {
            let active_channels = take_bits(header, &mut bit, channels)?;
            let speakers = take_bits(header, &mut bit, 6)? as usize + 1;
            let mask_enabled = take_bits(header, &mut bit, 1)? != 0;
            if mask_enabled {
                take_bits(header, &mut bit, channel_mask_bits)?;
            }
            for _ in 0..speakers {
                if !mask_enabled {
                    take_bits(header, &mut bit, 9)?;
                    take_bits(header, &mut bit, 9)?;
                    take_bits(header, &mut bit, 7)?;
                }
                for channel in 0..channels {
                    if active_channels & (1 << channel) != 0 {
                        take_bits(header, &mut bit, coefficient_bits)?;
                    }
                }
            }
        }
    } else if let HeaderSyntax::AlternatePrefix(prefix_bits) = syntax {
        take_bits(header, &mut bit, prefix_bits)?;
    }

    let decorrelated = take_bits(header, &mut bit, 1)? != 0;
    if decorrelated && channels > 1 {
        let order_bits = ceil_log2(channels);
        let mut order_mask = 0u32;
        for _ in 0..channels {
            let order = take_bits(header, &mut bit, order_bits)? as usize;
            if order >= channels || order_mask & (1 << order) != 0 {
                return None;
            }
            order_mask |= 1 << order;
        }
        for _ in 0..channels / 2 {
            if take_bits(header, &mut bit, 1)? != 0 {
                take_bits(header, &mut bit, 7)?;
            }
        }
    }

    let mut predictor_orders = Vec::with_capacity(channels);
    for _ in 0..channels {
        predictor_orders.push(take_bits(header, &mut bit, 4)?);
    }
    for &order in &predictor_orders {
        if order == 0 {
            take_bits(header, &mut bit, 2)?;
        }
    }
    for &order in &predictor_orders {
        for _ in 0..order {
            if take_bits(header, &mut bit, 8)? == 0xff {
                return None;
            }
        }
    }

    let mut lsb_section_size = 0usize;
    if scalable_lsbs {
        lsb_section_size = take_bits(header, &mut bit, segment_size_bits)? as usize;
        if lsb_section_size > payload_size {
            return None;
        }
        if lsb_section_size != 0 && band_crc_mode > 1 {
            lsb_section_size += 2;
        }
        for _ in 0..channels {
            let width = take_bits(header, &mut bit, 4)?;
            if width != 0 && lsb_section_size == 0 {
                return None;
            }
        }
        for _ in 0..channels {
            take_bits(header, &mut bit, 4)?;
        }
    }

    let header_bits = header.len() * 8;
    if bit > header_bits {
        return None;
    }
    let tail_bits = header_bits - bit;
    if tail_bits < 16 {
        return None;
    }
    Some(HeaderConfig {
        syntax,
        channel_mask_bits: if channel_mask.is_some() {
            channel_mask_bits
        } else {
            0
        },
        channel_mask,
        positions,
        segment_size_bits,
        scalable_lsbs,
        band_crc_mode,
        lsb_section_size,
        predictor_orders,
        tail_bits,
    })
}

fn header_config_candidates(header: &[u8], payload_size: usize) -> Vec<HeaderConfig> {
    let mut candidates = BTreeSet::new();
    let syntaxes = [
        HeaderSyntax::StandardUnmapped,
        HeaderSyntax::StandardOneToOne,
    ]
    .into_iter()
    .chain((0usize..=24).map(HeaderSyntax::AlternatePrefix));
    for syntax in syntaxes {
        for channel_mask_bits in 1usize..=32 {
            for segment_size_bits in 4usize..=20 {
                for scalable_lsbs in [false, true] {
                    for band_crc_mode in 0u32..=3 {
                        if let Some(candidate) = parse_header_config(
                            header,
                            payload_size,
                            syntax,
                            channel_mask_bits,
                            segment_size_bits,
                            scalable_lsbs,
                            band_crc_mode,
                        ) {
                            candidates.insert(candidate);
                        }
                    }
                }
            }
        }
    }
    candidates.into_iter().collect()
}

fn candidate_at(data: &[u8], byte_offset: usize) -> Option<Candidate> {
    let mut bit = byte_offset * 8;
    let header_size = read_bits(data, bit, 10)? as usize + 1;
    bit += 10;
    let channels = read_bits(data, bit, 4)? as usize + 1;
    bit += 4;
    if channels > 16 {
        return None;
    }
    let residual_mask = read_bits(data, bit, channels)?;
    bit += channels;
    let pcm_resolution = read_bits(data, bit, 5)? as usize + 1;
    bit += 5;
    let storage_resolution = read_bits(data, bit, 5)? as usize + 1;
    bit += 5;
    let frequency_index = read_bits(data, bit, 4)?;
    bit += 4;
    let frequency_modifier = read_bits(data, bit, 2)?;
    bit += 2;
    let replacement_set = read_bits(data, bit, 2)?;

    let end = byte_offset.checked_add(header_size)?;
    if end > data.len()
        || !matches!(storage_resolution, 16 | 20 | 24)
        || pcm_resolution > storage_resolution
        || frequency_index != 12
        || frequency_modifier != 0
        || replacement_set != 0
    {
        return None;
    }

    Some(Candidate {
        byte_offset,
        header_size,
        channels,
        pcm_resolution,
        storage_resolution,
        residual_mask,
    })
}

fn crc16_ccitt(data: &[u8]) -> u16 {
    let mut crc = 0xffffu16;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn crc_candidates(payload: &[u8]) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    for byte_offset in 0..payload.len() {
        let Some(candidate) = candidate_at(payload, byte_offset) else {
            continue;
        };
        let end = byte_offset + candidate.header_size;
        if crc16_ccitt(&payload[byte_offset..end]) == 0 {
            candidates.push(candidate);
        }
    }
    candidates
}

fn crc_subranges(data: &[u8]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    for start in 0..data.len() {
        for end in start + 3..=data.len() {
            if crc16_ccitt(&data[start..end]) == 0 {
                ranges.push((start, end));
            }
        }
    }
    ranges
}

fn fixed_prefix_size(payload: &[u8]) -> Option<usize> {
    match payload.get(..4)? {
        sync if sync == ALT_SYNC_D0 => Some(54),
        sync if sync == ALT_SYNC_D1 => Some(55),
        _ => None,
    }
}

fn terminal_navi_layouts(observation: &Observation) -> Vec<TerminalNaviLayout> {
    let mut layouts = Vec::new();
    for segments in [1usize, 2, 4, 8] {
        for size_bits in 4usize..=20 {
            let Some(navi_payload_bits) = (segments * 2).checked_mul(size_bits) else {
                continue;
            };
            let navi_size = navi_payload_bits.div_ceil(8) + 2;
            for trailer_bytes in 0usize..=16 {
                let Some(navi_end) = observation.payload.len().checked_sub(trailer_bytes) else {
                    continue;
                };
                let Some(navi_start) = navi_end.checked_sub(navi_size) else {
                    continue;
                };
                if crc16_ccitt(&observation.payload[navi_start..navi_end]) != 0 {
                    continue;
                }
                let mut entries = Vec::with_capacity(segments * 2);
                for entry in 0..segments * 2 {
                    let Some(size) = read_bits(
                        &observation.payload,
                        navi_start * 8 + entry * size_bits,
                        size_bits,
                    ) else {
                        entries.clear();
                        break;
                    };
                    entries.push(size as usize + 1);
                }
                if entries.len() != segments * 2 {
                    continue;
                }
                for order in [
                    TerminalNaviOrder::ChannelSetMajor,
                    TerminalNaviOrder::SegmentMajor,
                ] {
                    let (first_bytes, second_bytes) = match order {
                        TerminalNaviOrder::ChannelSetMajor => (
                            entries[..segments].iter().sum::<usize>(),
                            entries[segments..].iter().sum::<usize>(),
                        ),
                        TerminalNaviOrder::SegmentMajor => (
                            entries.iter().step_by(2).sum::<usize>(),
                            entries.iter().skip(1).step_by(2).sum::<usize>(),
                        ),
                    };
                    let first_end =
                        observation.first.byte_offset + observation.first.header_size + first_bytes;
                    let second_end = observation.second.byte_offset
                        + observation.second.header_size
                        + second_bytes;
                    layouts.push(TerminalNaviLayout {
                        segments,
                        size_bits,
                        trailer_bytes,
                        order,
                        first_gap: observation.second.byte_offset as isize - first_end as isize,
                        second_gap: navi_start as isize - second_end as isize,
                    });
                }
            }
        }
    }
    layouts
}

fn common_parameters_at(control: &[u8], bit_offset: usize) -> Option<CommonParameters> {
    let segments = 1usize.checked_shl(read_bits(control, bit_offset, 4)?)?;
    let segment_samples = 1usize.checked_shl(read_bits(control, bit_offset + 4, 4)?)?;
    let segment_size_bits = read_bits(control, bit_offset + 8, 5)? as usize + 1;
    let band_crc_mode = read_bits(control, bit_offset + 13, 2)?;
    let scalable_lsbs = read_bits(control, bit_offset + 15, 1)? != 0;
    let channel_mask_bits = read_bits(control, bit_offset + 16, 5)? as usize + 1;
    let fixed_lsb_width = if scalable_lsbs {
        read_bits(control, bit_offset + 21, 4)? as usize
    } else {
        0
    };
    Some(CommonParameters {
        segments,
        segment_samples,
        segment_size_bits,
        band_crc_mode,
        scalable_lsbs,
        channel_mask_bits,
        fixed_lsb_width,
    })
}

fn common_parameters(control: &[u8]) -> Option<CommonParameters> {
    common_parameters_at(control, 24)
}

fn geometry_candidates(control: &[u8]) -> Vec<(usize, usize, usize)> {
    let mut candidates = Vec::new();
    for bit_offset in 0..=control.len().saturating_mul(8).saturating_sub(13) {
        let Some(parameters) = common_parameters_at(control, bit_offset) else {
            continue;
        };
        if parameters.segments <= 8
            && parameters.segments * parameters.segment_samples == 512
            && (4..=20).contains(&parameters.segment_size_bits)
        {
            candidates.push((
                bit_offset,
                parameters.segments,
                parameters.segment_size_bits,
            ));
        }
    }
    candidates
}

fn inner_control(observation: &Observation) -> Option<&[u8]> {
    let start = observation.second.byte_offset.saturating_sub(24);
    let window = observation
        .payload
        .get(start..observation.second.byte_offset)?;
    let suffix = window
        .windows(INNER_SUFFIX.len())
        .rposition(|bytes| bytes == INNER_SUFFIX)?;
    window.get(suffix + INNER_SUFFIX.len()..)
}

fn print_control_layouts(group: &[&Observation]) {
    let mut outer = HashMap::<(Vec<(usize, usize, usize)>, isize), usize>::new();
    let mut inner = HashMap::<(usize, u8, Vec<(usize, usize, usize)>), usize>::new();
    let mut missing_inner = 0usize;

    for observation in group {
        let candidates = geometry_candidates(&observation.navigation);
        let residual = if let [(bit_offset, _, _)] = candidates.as_slice() {
            let width = bit_offset.saturating_sub(14);
            read_bits(&observation.navigation, 9, width)
                .map(|span| observation.second.byte_offset as isize - span as isize * 2)
                .unwrap_or(isize::MIN)
        } else {
            isize::MIN
        };
        *outer.entry((candidates, residual)).or_default() += 1;

        if let Some(control) = inner_control(observation) {
            *inner
                .entry((
                    control.len(),
                    control.first().copied().unwrap_or_default(),
                    geometry_candidates(control),
                ))
                .or_default() += 1;
        } else {
            missing_inner += 1;
        }
    }

    let mut outer = outer.into_iter().collect::<Vec<_>>();
    outer.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    println!(
        "  outer geometry candidates / boundary residual: {:?}",
        outer.iter().take(12).collect::<Vec<_>>()
    );
    let mut inner = inner.into_iter().collect::<Vec<_>>();
    inner.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    println!(
        "  inner controls (bytes, tag, candidates): {:?}; missing {missing_inner}",
        inner.iter().take(16).collect::<Vec<_>>()
    );
}

fn immediate_navi_geometries(
    payload: &[u8],
    header: Candidate,
    boundary: usize,
) -> BTreeSet<(usize, usize)> {
    let mut geometries = BTreeSet::new();
    let navi_start = header.byte_offset + header.header_size;
    for segments in [1usize, 2, 4, 8] {
        for size_bits in 4usize..=20 {
            let navi_size = (segments * size_bits).div_ceil(8) + 2;
            let Some(navi_end) = navi_start.checked_add(navi_size) else {
                continue;
            };
            if navi_end > boundary || crc16_ccitt(&payload[navi_start..navi_end]) != 0 {
                continue;
            }
            let mut audio_bytes = 0usize;
            for segment in 0..segments {
                let Some(size) =
                    read_bits(payload, navi_start * 8 + segment * size_bits, size_bits)
                else {
                    audio_bytes = usize::MAX;
                    break;
                };
                let Some(next) = audio_bytes.checked_add(size as usize + 1) else {
                    audio_bytes = usize::MAX;
                    break;
                };
                audio_bytes = next;
            }
            let Some(audio_end) = navi_end.checked_add(audio_bytes) else {
                continue;
            };
            if audio_end <= boundary && boundary - audio_end <= 20 {
                geometries.insert((segments, size_bits));
            }
        }
    }
    geometries
}

fn immediate_navi_gap(
    payload: &[u8],
    header: Candidate,
    boundary: usize,
    parameters: CommonParameters,
) -> Option<usize> {
    let navi_start = header.byte_offset.checked_add(header.header_size)?;
    let navi_size = (parameters.segments * parameters.segment_size_bits).div_ceil(8) + 2;
    let navi_end = navi_start.checked_add(navi_size)?;
    if navi_end > boundary || crc16_ccitt(payload.get(navi_start..navi_end)?) != 0 {
        return None;
    }
    let mut audio_bytes = 0usize;
    for segment in 0..parameters.segments {
        let size = read_bits(
            payload,
            navi_start * 8 + segment * parameters.segment_size_bits,
            parameters.segment_size_bits,
        )? as usize
            + 1;
        audio_bytes = audio_bytes.checked_add(size)?;
    }
    boundary.checked_sub(navi_end.checked_add(audio_bytes)?)
}

fn print_d1_common_geometry(group: &[&Observation]) {
    if group.len() < 16 || group[0].navigation.first() != Some(&0xc5) {
        return;
    }

    let mut geometries = HashMap::<(CommonParameters, Option<usize>, Option<usize>), usize>::new();
    let mut first_gaps = HashMap::<Option<usize>, usize>::new();
    let mut second_gaps = HashMap::<Option<usize>, usize>::new();
    let mut second_geometry_counts = HashMap::<Vec<(usize, usize)>, usize>::new();
    let mut second_prefix_geometry_counts = HashMap::<(u32, Vec<(usize, usize)>), usize>::new();
    let mut unique_second_geometries = Vec::new();
    let mut rare_second_geometries = Vec::new();
    let mut second_control_partial_offsets = HashMap::<(u8, usize, usize), usize>::new();
    let mut gap_examples = HashMap::<(usize, usize), (usize, Vec<u8>)>::new();
    let mut unusual_first_controls = Vec::new();
    for observation in group {
        let Some(parameters) = common_parameters(&observation.navigation) else {
            continue;
        };
        if parameters.segments * parameters.segment_samples != 512 {
            unusual_first_controls.push((
                observation.frame,
                observation.navigation.clone(),
                observation.second.byte_offset,
                observation.second.byte_offset as isize
                    - read_bits(&observation.navigation, 9, 10).unwrap() as isize * 2,
                immediate_navi_geometries(
                    &observation.payload,
                    observation.first,
                    observation.second.byte_offset,
                ),
            ));
        }
        let first_gap = immediate_navi_gap(
            &observation.payload,
            observation.first,
            observation.second.byte_offset,
            parameters,
        );
        let second_gap = immediate_navi_gap(
            &observation.payload,
            observation.second,
            observation.payload_size,
            parameters,
        );
        *geometries
            .entry((parameters, first_gap, second_gap))
            .or_default() += 1;
        *first_gaps.entry(first_gap).or_default() += 1;
        *second_gaps.entry(second_gap).or_default() += 1;
        let second_geometries = immediate_navi_geometries(
            &observation.payload,
            observation.second,
            observation.payload_size,
        )
        .into_iter()
        .collect::<Vec<_>>();
        *second_geometry_counts
            .entry(second_geometries.clone())
            .or_default() += 1;
        let second_prefix = read_bits(&observation.second_header, 36, 2).unwrap();
        *second_prefix_geometry_counts
            .entry((second_prefix, second_geometries.clone()))
            .or_default() += 1;
        if second_geometries.as_slice() != [(2, 10)] && rare_second_geometries.len() < 16 {
            rare_second_geometries.push((
                observation.frame,
                observation.navigation.clone(),
                parameters,
                read_bits(&observation.first_header, 36, 2).unwrap(),
                second_prefix,
                second_geometries.clone(),
            ));
        }
        if let [geometry] = second_geometries.as_slice() {
            unique_second_geometries.push((observation, *geometry));
            if let Some(gap) = first_gap {
                let gap_bytes = &observation.payload
                    [observation.second.byte_offset - gap..observation.second.byte_offset];
                gap_examples
                    .entry(*geometry)
                    .or_insert_with(|| (observation.frame, gap_bytes.to_vec()));
                const SECOND_PREFIX: [u8; 6] = [0x02, 0x34, 0x38, 0x8c, 0x4f, 0x00];
                if let Some(prefix_offset) = gap_bytes
                    .windows(SECOND_PREFIX.len())
                    .rposition(|window| window == SECOND_PREFIX)
                {
                    let control = &gap_bytes[prefix_offset + SECOND_PREFIX.len()..];
                    let expected = (geometry.0.ilog2() << 9)
                        | ((512 / geometry.0).ilog2() << 5)
                        | (geometry.1 - 1) as u32;
                    for bit_offset in 0..=control.len() * 8 - 13 {
                        if read_bits(control, bit_offset, 13) == Some(expected) {
                            *second_control_partial_offsets
                                .entry((control[0], control.len(), bit_offset))
                                .or_default() += 1;
                        }
                    }
                }
            }
        }
    }

    let mut geometries = geometries.into_iter().collect::<Vec<_>>();
    geometries.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    println!("  D1 common geometry (params, first gap, second gap):");
    for (geometry, count) in geometries.iter().take(16) {
        println!("    {count}/{}: {geometry:?}", group.len());
    }
    let mut first_gaps = first_gaps.into_iter().collect::<Vec<_>>();
    let mut second_gaps = second_gaps.into_iter().collect::<Vec<_>>();
    first_gaps.sort_unstable_by_key(|entry| entry.0);
    second_gaps.sort_unstable_by_key(|entry| entry.0);
    println!("  D1 common first-gap distribution: {first_gaps:?}");
    println!("  D1 common second-gap distribution: {second_gaps:?}");

    let mut second_geometry_counts = second_geometry_counts.into_iter().collect::<Vec<_>>();
    second_geometry_counts.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    println!(
        "  D1 second structural geometries ({} uniquely identified): {:?}",
        unique_second_geometries.len(),
        second_geometry_counts.iter().take(12).collect::<Vec<_>>()
    );
    let mut second_prefix_geometry_counts = second_prefix_geometry_counts
        .into_iter()
        .collect::<Vec<_>>();
    second_prefix_geometry_counts.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    println!(
        "  D1 second prefix bits / geometry: {:?}",
        second_prefix_geometry_counts
            .iter()
            .take(16)
            .collect::<Vec<_>>()
    );
    println!("  D1 rare second geometries: {rare_second_geometries:02x?}");
    println!("  D1 unusual first controls: {unusual_first_controls:02x?}");
    let mut gap_examples = gap_examples.into_iter().collect::<Vec<_>>();
    gap_examples.sort_unstable_by_key(|entry| entry.0);
    println!("  D1 interstitial examples by second geometry: {gap_examples:02x?}");
    let mut second_control_partial_offsets = second_control_partial_offsets
        .into_iter()
        .collect::<Vec<_>>();
    second_control_partial_offsets.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    println!(
        "  D1 second-control tag/bytes/common-prefix offset: {:?}",
        second_control_partial_offsets
            .iter()
            .take(16)
            .collect::<Vec<_>>()
    );
    let mut boundary_residuals = HashMap::<isize, Vec<(usize, Vec<u8>, usize)>>::new();
    for observation in group {
        let field = read_bits(&observation.navigation, 9, 10).unwrap() as isize;
        let residual = observation.second.byte_offset as isize - field * 2;
        boundary_residuals.entry(residual).or_default().push((
            observation.frame,
            observation.navigation.clone(),
            observation.second.byte_offset,
        ));
    }
    let mut boundary_residuals = boundary_residuals.into_iter().collect::<Vec<_>>();
    boundary_residuals.sort_unstable_by(|a, b| b.1.len().cmp(&a.1.len()));
    println!("  D1 second boundary minus bits 9..19 * 2:");
    for (residual, frames) in &boundary_residuals {
        println!(
            "    {residual:+}: {} frames; first {:?}",
            frames.len(),
            frames.first()
        );
    }
}

fn print_common_parameter_offsets(group: &[&Observation]) {
    if group.len() < 16 {
        return;
    }
    let control_bits = group[0].navigation.len() * 8;
    let mut matches = Vec::new();
    for bit_offset in 0..=control_bits.saturating_sub(21) {
        let mut first_matches = 0usize;
        let mut second_matches = 0usize;
        let mut both_matches = 0usize;
        for observation in group {
            let Some(parameters) = common_parameters_at(&observation.navigation, bit_offset) else {
                continue;
            };
            if parameters.segments * parameters.segment_samples != 512
                || parameters.band_crc_mode != 0
                || parameters.scalable_lsbs
            {
                continue;
            }
            let first = immediate_navi_geometries(
                &observation.payload,
                observation.first,
                observation.second.byte_offset,
            )
            .contains(&(parameters.segments, parameters.segment_size_bits));
            let second = immediate_navi_geometries(
                &observation.payload,
                observation.second,
                observation.payload_size,
            )
            .contains(&(parameters.segments, parameters.segment_size_bits));
            first_matches += usize::from(first);
            second_matches += usize::from(second);
            both_matches += usize::from(first && second);
        }
        matches.push((both_matches, first_matches, second_matches, bit_offset));
    }
    matches.sort_unstable_by(|a, b| b.cmp(a));
    println!("  candidate common-parameter offsets (both/first/second immediate):");
    for &(both, first, second, bit_offset) in matches.iter().take(8) {
        println!(
            "    bit {bit_offset:>2}: {both}/{first}/{second} of {} frames",
            group.len()
        );
    }
}

fn print_field_relations(observations: &[&Observation], navigation_bytes: usize) {
    if observations.len() < 16 {
        return;
    }
    let navigation_bits = navigation_bytes * 8;
    let sample = &observations[..observations.len().min(4000)];
    println!(
        "field relations for {navigation_bytes}-byte navigation ({} frames):",
        sample.len()
    );
    for target in Target::ALL {
        let first = target.value(sample[0]);
        if sample
            .iter()
            .all(|observation| target.value(observation) == first)
        {
            continue;
        }
        let mut matches = Vec::new();
        for width in 4usize..=20 {
            if width > navigation_bits {
                break;
            }
            for bit_offset in 0..=navigation_bits - width {
                for scale in [1isize, 2, 4, 8] {
                    let mut biases = HashMap::<isize, usize>::new();
                    for observation in sample {
                        let field = read_bits(&observation.navigation, bit_offset, width)
                            .expect("bounded navigation field")
                            as isize;
                        let bias = target.value(observation) as isize - field * scale;
                        *biases.entry(bias).or_default() += 1;
                    }
                    let Some((&bias, &count)) = biases.iter().max_by_key(|(_, count)| **count)
                    else {
                        continue;
                    };
                    if count * 100 >= sample.len() * 50 {
                        matches.push((count, width, bit_offset, scale, bias));
                    }
                }
            }
        }
        matches.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        println!("  {}:", target.name());
        for &(count, width, bit_offset, scale, bias) in matches.iter().take(8) {
            println!(
                "    bits {bit_offset:>2}..{:<2} * {scale} {bias:+} => {count}/{}",
                bit_offset + width,
                sample.len()
            );
        }
    }
}

fn print_stable_header_parameters(observations: &[Observation]) {
    type StaticConfig = (HeaderSyntax, usize, bool, u32);
    let mut first_counts = HashMap::<StaticConfig, usize>::new();
    let mut second_counts = HashMap::<StaticConfig, usize>::new();
    for observation in observations {
        for (header, counts) in [
            (&observation.first_header, &mut first_counts),
            (&observation.second_header, &mut second_counts),
        ] {
            let configs = header_config_candidates(header, observation.payload_size)
                .into_iter()
                .map(|config| {
                    (
                        config.syntax,
                        config.segment_size_bits,
                        config.scalable_lsbs,
                        config.band_crc_mode,
                    )
                })
                .collect::<BTreeSet<_>>();
            for config in configs {
                *counts.entry(config).or_default() += 1;
            }
        }
    }
    for (name, counts) in [("first", first_counts), ("second", second_counts)] {
        let mut counts = counts.into_iter().collect::<Vec<_>>();
        counts.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        println!("stable {name}-header parameter candidates:");
        for (config, frames) in counts.iter().take(16) {
            println!("  {frames}/{} frames: {config:?}", observations.len());
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: xll_alt_nav <in.dts> [max_mb]");
    let max_mb: usize = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(64);
    let quick = args.next().as_deref() == Some("quick");

    let mut input = std::fs::File::open(&path).expect("open input");
    let mut bytes = vec![0u8; max_mb * 1024 * 1024];
    let read = input.read(&mut bytes).expect("read input");
    bytes.truncate(read);

    let mut decoder = HdDecoder::new();
    let mut observations = Vec::new();
    let mut offset = 0usize;
    let mut frame = 0usize;
    let mut rejected_layouts = 0usize;

    while offset + 18 < bytes.len() {
        let core = match parse_header(&bytes[offset..]) {
            Ok(header) => header,
            Err(_) => break,
        };
        let exss_offset = offset + core.frame_size;
        let exss_len = match bytes
            .get(exss_offset..)
            .and_then(exss_substream_size)
            .filter(|length| exss_offset + length <= bytes.len())
        {
            Some(length) => length,
            None => break,
        };

        if let Ok(decoded) = decoder.decode(
            &bytes[offset..exss_offset],
            &bytes[exss_offset..exss_offset + exss_len],
        ) {
            if decoded.x_imax {
                let payload = &decoded.x_payload;
                if let Some(prefix_size) = fixed_prefix_size(payload) {
                    let candidates = crc_candidates(payload);
                    if let [first, second] = candidates.as_slice() {
                        if first.byte_offset >= prefix_size {
                            observations.push(Observation {
                                frame,
                                payload_size: payload.len(),
                                payload: payload.to_vec(),
                                prefix: payload[..prefix_size].to_vec(),
                                navigation: payload[prefix_size..first.byte_offset].to_vec(),
                                first: *first,
                                second: *second,
                                first_header: payload
                                    [first.byte_offset..first.byte_offset + first.header_size]
                                    .to_vec(),
                                second_header: payload
                                    [second.byte_offset..second.byte_offset + second.header_size]
                                    .to_vec(),
                            });
                        } else {
                            rejected_layouts += 1;
                        }
                    } else {
                        rejected_layouts += 1;
                    }
                }
            }
            frame += 1;
        }
        offset += core.frame_size + exss_len;
    }

    println!("read {read} bytes from {path}");
    println!(
        "accepted observations: {}; rejected layouts: {rejected_layouts}",
        observations.len()
    );
    let mut prefixes = HashMap::<Vec<u8>, usize>::new();
    for observation in &observations {
        *prefixes.entry(observation.prefix.clone()).or_default() += 1;
    }
    let mut prefixes = prefixes.into_iter().collect::<Vec<_>>();
    prefixes.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    println!("fixed prefixes: {}", prefixes.len());
    for (prefix, frames) in prefixes.iter().take(4) {
        println!(
            "  {frames} frames: {prefix:02x?}; CRC-valid subranges: {:?}",
            crc_subranges(prefix)
        );
    }
    if !quick {
        let mut parseable_second_headers = 0usize;
        let mut first_parseable_second_header = None;
        let mut first_parseable_boundaries = Vec::new();
        let mut second_header_mask_widths = HashMap::<usize, usize>::new();
        for observation in &observations {
            let configs =
                header_config_candidates(&observation.second_header, observation.payload_size);
            if !configs.is_empty() {
                parseable_second_headers += 1;
                let widths = configs
                    .iter()
                    .filter_map(|config| config.channel_mask.map(|_| config.channel_mask_bits))
                    .collect::<BTreeSet<_>>();
                for width in widths {
                    *second_header_mask_widths.entry(width).or_default() += 1;
                }
                if first_parseable_boundaries.len() < 8 {
                    first_parseable_boundaries.push((
                        observation.frame,
                        observation.navigation.clone(),
                        observation.first.byte_offset,
                        observation.first.header_size,
                        observation.second.byte_offset,
                        observation.second.header_size,
                        observation.payload_size,
                    ));
                }
                if first_parseable_second_header.is_none() {
                    first_parseable_second_header = Some((
                        observation.frame,
                        observation.first_header.clone(),
                        header_config_candidates(
                            &observation.first_header,
                            observation.payload_size,
                        )
                        .into_iter()
                        .next(),
                        observation.second,
                        observation.payload_size,
                        observation.second_header.clone(),
                        configs.len(),
                        configs.into_iter().next(),
                    ));
                }
            }
        }
        println!(
            "parseable second headers: {parseable_second_headers}/{}; first: {first_parseable_second_header:02x?}",
            observations.len()
        );
        println!("first parseable boundaries: {first_parseable_boundaries:02x?}");
        println!("second-header mask widths by frame: {second_header_mask_widths:?}");
        print_stable_header_parameters(&observations);
    }
    let mut terminal_layout_counts = HashMap::<TerminalNaviLayout, usize>::new();
    let mut terminal_frames = 0usize;
    for observation in &observations {
        let layouts = terminal_navi_layouts(observation);
        if !layouts.is_empty() {
            terminal_frames += 1;
        }
        for layout in layouts {
            *terminal_layout_counts.entry(layout).or_default() += 1;
        }
    }
    let mut terminal_layout_counts = terminal_layout_counts.into_iter().collect::<Vec<_>>();
    terminal_layout_counts.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    println!(
        "CRC-valid terminal navigation: {terminal_frames}/{} frames; top structural layouts:",
        observations.len()
    );
    for (layout, frames) in terminal_layout_counts.iter().take(16) {
        println!("  {frames} frames: {layout:?}");
    }
    let mut groups = HashMap::<(usize, u8), Vec<&Observation>>::new();
    for observation in &observations {
        groups
            .entry((
                observation.navigation.len(),
                observation.navigation.first().copied().unwrap_or_default(),
            ))
            .or_default()
            .push(observation);
    }
    let mut groups = groups.into_iter().collect::<Vec<_>>();
    groups.sort_unstable_by_key(|&((bytes, tag), _)| (bytes, tag));
    for ((navigation_bytes, tag), group) in groups {
        let first = group[0];
        println!(
            "navigation bytes {navigation_bytes}, tag {tag:#04x}: {} frames; first frame {}; control {:02x?}; common {:?}; first/second offsets {}/{}; payload {}",
            group.len(),
            first.frame,
            first.navigation,
            common_parameters(&first.navigation),
            first.first.byte_offset,
            first.second.byte_offset,
            first.payload_size
        );
        let mut common_counts = HashMap::<CommonParameters, usize>::new();
        for observation in &group {
            if let Some(parameters) = common_parameters(&observation.navigation) {
                *common_counts.entry(parameters).or_default() += 1;
            }
        }
        let mut common_counts = common_counts.into_iter().collect::<Vec<_>>();
        common_counts.sort_unstable_by(|a, b| b.1.cmp(&a.1));
        println!(
            "  common@bit24 variants: {:?}",
            common_counts.iter().take(8).collect::<Vec<_>>()
        );
        if !quick {
            let first_configs = header_config_candidates(&first.first_header, first.payload_size);
            let second_configs = header_config_candidates(&first.second_header, first.payload_size);
            println!(
                "  first header {:02x?}: {} configurations; first {:?}",
                first.first_header,
                first_configs.len(),
                first_configs.iter().take(8).collect::<Vec<_>>()
            );
            println!(
                "  second header {:02x?}: {} configurations; first {:?}",
                first.second_header,
                second_configs.len(),
                second_configs.iter().take(8).collect::<Vec<_>>()
            );
        }
        print_control_layouts(&group);
        print_common_parameter_offsets(&group);
        print_d1_common_geometry(&group);
        print_field_relations(&group, navigation_bytes);
    }
}
