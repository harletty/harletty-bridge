// SPDX-License-Identifier: Apache-2.0
//
// Export frame-level D3 structural fields for offline correlation studies.
// Research tool only; it is not used by the realtime decoder.
//
// Usage:
//   cargo run -p dca --release --example xll_d3_frame_features -- <track.dts>

use std::io::{BufReader, ErrorKind, Read};

use dca::{HdDecoder, HdError, SYNCWORD_SUBSTREAM, parse_header};

const HEADER_BYTES: usize = 18;
const EXSS_PREFIX_BYTES: usize = 16;
const D3_SYNC: [u8; 4] = [0xf1, 0x40, 0x00, 0xd3];
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

fn header_size(payload: &[u8], offset: usize) -> Option<usize> {
    let mut bit = offset.checked_mul(8)?;
    read_bits(payload, &mut bit, 10).map(|size| size as usize + 1)
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

struct Layout<'a> {
    prefix: &'a [u8],
    outer: &'a [u8],
    inner: &'a [u8],
    first: usize,
    first_size: usize,
    second: usize,
    second_size: usize,
    outer_geometry: usize,
    inner_geometry: usize,
}

fn layout(payload: &[u8]) -> Option<Layout<'_>> {
    if payload.get(..4)? != D3_SYNC {
        return None;
    }
    let prefix_end = (49..payload.len().min(96)).find(|&offset| {
        payload.get(offset..offset + OUTER_SUFFIX.len()) == Some(&OUTER_SUFFIX)
            && crc16_ccitt(&payload[..offset]) == 0
    })?;
    let control_start = prefix_end + OUTER_SUFFIX.len();
    let tag = *payload.get(control_start)?;
    let control_size = if tag == 0xb2 { 7 } else { 8 };
    let outer = payload.get(control_start..control_start + control_size)?;
    let first = control_start + control_size;
    if !valid_header(payload, first, 4) {
        return None;
    }
    let outer_geometry = unique_geometry(outer, 18, 31)?;
    let mut bit = 9;
    let span = read_bits(outer, &mut bit, outer_geometry.checked_sub(14)?)? as usize;
    let nominal = span.checked_mul(2)?.checked_add(control_start + 12)?;
    for second in [Some(nominal), nominal.checked_sub(1)]
        .into_iter()
        .flatten()
    {
        if !valid_header(payload, second, 4) {
            continue;
        }
        let window = payload.get(second.checked_sub(24)?..second)?;
        let suffix = window.windows(6).rposition(|bytes| bytes == INNER_SUFFIX)?;
        let inner = window.get(suffix + INNER_SUFFIX.len()..)?;
        if !(8..=9).contains(&inner.len()) {
            continue;
        }
        let Some(inner_geometry) = unique_geometry(inner, 19, 26) else {
            continue;
        };
        return Some(Layout {
            prefix: &payload[..prefix_end],
            outer,
            inner,
            first,
            first_size: header_size(payload, first)?,
            second,
            second_size: header_size(payload, second)?,
            outer_geometry,
            inner_geometry,
        });
    }
    None
}

fn alternate_payload(exss: &[u8]) -> Option<&[u8]> {
    let start = exss.windows(4).position(|bytes| bytes == D3_SYNC)?;
    exss.get(start..)
}

fn main() {
    let path = std::env::args().nth(1).expect("input.dts");
    let input = std::fs::File::open(path).expect("open input");
    let mut reader = BufReader::with_capacity(1024 * 1024, input);
    let mut decoder = HdDecoder::new();
    let mut core = Vec::new();
    let mut exss = Vec::new();
    let mut header = [0u8; HEADER_BYTES];
    let mut decoded_index = 0u64;

    println!(
        "frame\ttime\tpayload_size\tx_payload_offset\tx_payload_size\t\
         descriptor_bits\tdescriptor_hex\tprefix_len\tprefix_hex\t\
         outer_len\touter_hex\touter_geometry\tfirst_offset\tfirst_size\t\
         inner_len\tinner_hex\tinner_geometry\tsecond_offset\tsecond_size\t\
         consumed_bytes\ttrailer_len"
    );

    loop {
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
                let Some(layout) = layout(payload) else {
                    continue;
                };
                let consumed_bytes = frame.x_bits_consumed.div_ceil(8);
                let trailer_len = payload.len().saturating_sub(consumed_bytes);
                println!(
                    "{decoded_index}\t{:.9}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    decoded_index as f64 * 512.0 / 48_000.0,
                    payload.len(),
                    frame.x_payload_offset,
                    frame.x_payload.len(),
                    frame.exss_descriptor_tail_bits,
                    hex(&frame.exss_descriptor_tail),
                    layout.prefix.len(),
                    hex(layout.prefix),
                    layout.outer.len(),
                    hex(layout.outer),
                    layout.outer_geometry,
                    layout.first,
                    layout.first_size,
                    layout.inner.len(),
                    hex(layout.inner),
                    layout.inner_geometry,
                    layout.second,
                    layout.second_size,
                    consumed_bytes,
                    trailer_len,
                );
                decoded_index += 1;
            }
            Err(HdError::Pending) => {}
            Err(error) => panic!("decode: {error:?}"),
        }
    }
}
