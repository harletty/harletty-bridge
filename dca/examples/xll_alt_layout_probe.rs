// SPDX-License-Identifier: Apache-2.0
//
// Survey the stable D0/D1/D3 payload prefix, channel-set mapping bits, and bytes
// left after decoded audio in a bounded number of alternate-profile frames.
// Research tool only; it is not used by the realtime decoder.
//
// Usage:
//   cargo run -p dca --release --example xll_alt_layout_probe -- \
//     <track.dts> [max-frames]

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufReader, ErrorKind, Read};

use dca::{HdDecoder, HdError, SYNCWORD_SUBSTREAM, parse_header};

const HEADER_BYTES: usize = 18;
const EXSS_PREFIX_BYTES: usize = 16;
const OUTER_SUFFIX: [u8; 6] = [0x03, 0x34, 0x38, 0x8c, 0x4f, 0x00];
const INNER_SUFFIX: [u8; 6] = [0x02, 0x34, 0x38, 0x8c, 0x4f, 0x00];

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn read_bits(data: &[u8], position: &mut usize, width: usize) -> Option<u32> {
    if width > 32 || position.checked_add(width)? > data.len() * 8 {
        return None;
    }
    let mut value = 0;
    for _ in 0..width {
        value = (value << 1) | ((data[*position / 8] >> (7 - *position % 8)) & 1) as u32;
        *position += 1;
    }
    Some(value)
}

fn exss_size(data: &[u8]) -> Option<usize> {
    let mut bit = 0;
    if read_bits(data, &mut bit, 32)? != SYNCWORD_SUBSTREAM {
        return None;
    }
    read_bits(data, &mut bit, 8)?;
    read_bits(data, &mut bit, 2)?;
    let wide = read_bits(data, &mut bit, 1)? as usize;
    read_bits(data, &mut bit, 8 + 4 * wide)?;
    Some(read_bits(data, &mut bit, 16 + 4 * wide)? as usize + 1)
}

fn geometry_at(control: &[u8], offset: usize) -> bool {
    let mut bit = offset;
    let Some(a) = read_bits(control, &mut bit, 4) else {
        return false;
    };
    let Some(b) = read_bits(control, &mut bit, 4) else {
        return false;
    };
    let Some(c) = read_bits(control, &mut bit, 5) else {
        return false;
    };
    let (Some(segments), Some(samples)) = (1usize.checked_shl(a), 1usize.checked_shl(b)) else {
        return false;
    };
    segments <= 8 && segments * samples == 512 && (3..=19).contains(&(c as usize))
}

fn unique_geometry(control: &[u8], start: usize, end: usize) -> Option<usize> {
    let values: Vec<_> = (start..=end)
        .filter(|&offset| geometry_at(control, offset))
        .collect();
    (values.len() == 1).then(|| values[0])
}

fn mapping_bits(payload: &[u8], offset: usize) -> Option<(usize, u32)> {
    let mut bit = offset * 8;
    read_bits(payload, &mut bit, 10)?;
    let channels = read_bits(payload, &mut bit, 4)? as usize + 1;
    read_bits(payload, &mut bit, channels)?;
    read_bits(payload, &mut bit, 5)?;
    read_bits(payload, &mut bit, 5)?;
    read_bits(payload, &mut bit, 4)?;
    read_bits(payload, &mut bit, 2)?;
    read_bits(payload, &mut bit, 2)?;
    Some((channels, read_bits(payload, &mut bit, 2)?))
}

fn header_size(payload: &[u8], offset: usize) -> Option<usize> {
    let mut bit = offset.checked_mul(8)?;
    read_bits(payload, &mut bit, 10).map(|size| size as usize + 1)
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

fn valid_header(payload: &[u8], offset: usize, expected_channels: usize) -> bool {
    let mut bit = offset * 8;
    let Some(size) = read_bits(payload, &mut bit, 10).map(|value| value as usize + 1) else {
        return false;
    };
    let Some(channels) = read_bits(payload, &mut bit, 4).map(|value| value as usize + 1) else {
        return false;
    };
    let Some(end) = offset.checked_add(size) else {
        return false;
    };
    channels == expected_channels && end <= payload.len() && crc16_ccitt(&payload[offset..end]) == 0
}

fn layout(payload: &[u8]) -> Option<(u8, usize, usize)> {
    let profile = match payload.get(..4)? {
        [0xf1, 0x40, 0x00, 0xd0] => 0u8,
        [0xf1, 0x40, 0x00, 0xd1] => 1u8,
        [0xf1, 0x40, 0x00, 0xd3] => 3u8,
        _ => return None,
    };
    let minimum_prefix = if profile == 0 { 48 } else { 49 };
    let prefix_end = (minimum_prefix..payload.len().min(96)).find(|&offset| {
        payload.get(offset..offset + OUTER_SUFFIX.len()) == Some(&OUTER_SUFFIX)
            && crc16_ccitt(&payload[..offset]) == 0
    })?;
    let control_start = prefix_end + OUTER_SUFFIX.len();
    let bias = control_start + 12;
    let inner_start = if profile == 0 { 18 } else { 19 };
    let tag = *payload.get(control_start)?;
    let control_size = if tag == 0xb2 { 7 } else { 8 };
    let outer = payload.get(control_start..control_start + control_size)?;
    let first = control_start + control_size;
    let first_channels = match profile {
        0 => 1,
        1 => 2,
        3 => 4,
        _ => return None,
    };
    if !valid_header(payload, first, first_channels) {
        return None;
    }
    let common = unique_geometry(outer, 18, if profile == 3 { 31 } else { 25 })?;
    let mut bit = 9;
    let span = read_bits(outer, &mut bit, common.checked_sub(14)?)? as usize;
    let nominal = span.checked_mul(2)?.checked_add(bias)?;
    for second in [Some(nominal), nominal.checked_sub(1)]
        .into_iter()
        .flatten()
    {
        if !valid_header(payload, second, 4) {
            continue;
        }
        let window = payload.get(second.checked_sub(24)?..second)?;
        let suffix = window.windows(6).rposition(|bytes| bytes == INNER_SUFFIX)?;
        let inner = window.get(suffix + 6..)?;
        if (8..=9).contains(&inner.len()) && unique_geometry(inner, inner_start, 26).is_some() {
            return Some((profile, control_start + control_size, second));
        }
    }
    None
}

fn alternate_payload(exss: &[u8]) -> Option<&[u8]> {
    let start = exss.windows(4).position(|bytes| {
        matches!(
            bytes,
            [0xf1, 0x40, 0x00, 0xd0] | [0xf1, 0x40, 0x00, 0xd1] | [0xf1, 0x40, 0x00, 0xd3]
        )
    })?;
    exss.get(start..)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("input.dts");
    let max_frames = args
        .next()
        .map(|value| value.parse::<u64>().expect("max-frames"))
        .unwrap_or(20_000);
    let input = std::fs::File::open(path).expect("open input");
    let mut reader = BufReader::with_capacity(1024 * 1024, input);
    let mut decoder = HdDecoder::new();
    let mut core = Vec::new();
    let mut exss = Vec::new();
    let mut header = [0u8; HEADER_BYTES];
    let mut patterns = BTreeMap::<(u8, usize, u32, usize, u32), u64>::new();
    let mut header_layouts = BTreeMap::<(usize, usize, usize), u64>::new();
    let mut outer_controls = BTreeMap::<Vec<u8>, u64>::new();
    let mut inner_controls = BTreeMap::<Vec<u8>, u64>::new();
    let mut trailers = BTreeMap::<Vec<u8>, u64>::new();
    let mut prefixes = BTreeSet::new();
    let mut frames = 0u64;
    while frames < max_frames {
        match reader.read_exact(&mut header) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => break,
            Err(error) => panic!("read core: {error}"),
        }
        let info = parse_header(&header).expect("parse core");
        core.resize(info.frame_size, 0);
        core[..HEADER_BYTES].copy_from_slice(&header);
        reader.read_exact(&mut core[HEADER_BYTES..]).unwrap();
        exss.resize(EXSS_PREFIX_BYTES, 0);
        reader.read_exact(&mut exss).unwrap();
        let size = exss_size(&exss).unwrap();
        exss.resize(size, 0);
        reader.read_exact(&mut exss[EXSS_PREFIX_BYTES..]).unwrap();
        match decoder.decode(&core, &exss) {
            Ok(frame) => {
                let payload = if frame.x_imax {
                    frame.x_payload.as_slice()
                } else if let Some(payload) = alternate_payload(&exss) {
                    payload
                } else {
                    continue;
                };
                let Some((profile, first, second)) = layout(payload) else {
                    let candidates = (0..payload.len().min(256))
                        .filter_map(|offset| {
                            (1..=8)
                                .find(|&channels| valid_header(payload, offset, channels))
                                .map(|channels| (offset, channels))
                        })
                        .collect::<Vec<_>>();
                    panic!(
                        "alternate layout; sync={:02x?} payload={} header candidates={candidates:?}",
                        &payload[..4],
                        payload.len()
                    );
                };
                let (first_channels, first_bits) = mapping_bits(payload, first).unwrap();
                let (second_channels, second_bits) = mapping_bits(payload, second).unwrap();
                *patterns
                    .entry((
                        profile,
                        first_channels,
                        first_bits,
                        second_channels,
                        second_bits,
                    ))
                    .or_default() += 1;
                let first_size = header_size(payload, first).expect("first header size");
                let second_size = header_size(payload, second).expect("second header size");
                *header_layouts
                    .entry((first_size, second - first, second_size))
                    .or_default() += 1;
                let prefix_end = payload
                    .windows(OUTER_SUFFIX.len())
                    .position(|bytes| bytes == OUTER_SUFFIX)
                    .expect("outer suffix");
                *outer_controls
                    .entry(payload[prefix_end + OUTER_SUFFIX.len()..first].to_vec())
                    .or_default() += 1;
                let inner_window = &payload[second.saturating_sub(24)..second];
                let inner_suffix = inner_window
                    .windows(INNER_SUFFIX.len())
                    .rposition(|bytes| bytes == INNER_SUFFIX)
                    .expect("inner suffix");
                *inner_controls
                    .entry(inner_window[inner_suffix + INNER_SUFFIX.len()..].to_vec())
                    .or_default() += 1;
                let trailer_start = frame.x_bits_consumed.div_ceil(8);
                if trailer_start != 0 && trailer_start <= payload.len() {
                    *trailers
                        .entry(payload[trailer_start..].to_vec())
                        .or_default() += 1;
                }
                prefixes.insert(payload[..prefix_end].to_vec());
                frames += 1;
            }
            Err(HdError::Pending) => {}
            Err(error) => panic!("decode: {error:?}"),
        }
    }
    println!(
        "frames={frames} patterns={patterns:?} prefixes={}",
        prefixes.len(),
    );
    let mut header_layouts = header_layouts.into_iter().collect::<Vec<_>>();
    header_layouts.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    println!("unique_header_layouts={}", header_layouts.len());
    for (layout, count) in header_layouts.into_iter().take(20) {
        println!("header_layout count={count} value={layout:?}");
    }
    println!(
        "outer_controls={} inner_controls={}",
        outer_controls.len(),
        inner_controls.len()
    );
    let mut outer_controls = outer_controls.into_iter().collect::<Vec<_>>();
    outer_controls.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    for (control, count) in outer_controls.into_iter().take(20) {
        println!("outer count={count} hex={}", hex(&control));
    }
    let mut inner_controls = inner_controls.into_iter().collect::<Vec<_>>();
    inner_controls.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    for (control, count) in inner_controls.into_iter().take(20) {
        println!("inner count={count} hex={}", hex(&control));
    }
    for prefix in prefixes {
        println!(
            "prefix={}",
            prefix
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
    }
    let mut trailer_counts: Vec<_> = trailers.into_iter().collect();
    trailer_counts.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    println!("unique_trailers={}", trailer_counts.len());
    for (trailer, count) in trailer_counts.into_iter().take(20) {
        println!(
            "trailer count={count} len={} hex={}",
            trailer.len(),
            trailer
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
    }
}
