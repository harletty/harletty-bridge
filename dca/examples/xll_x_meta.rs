// SPDX-License-Identifier: Apache-2.0
//
// Characterize the 64 profile-specific bits in the EXSS asset descriptor of
// XLL-X streams. Legacy TS 102 114 decoders skip this reserved region.
//
// Usage: cargo run -p dca --release --example xll_x_meta -- <in.dts> [max_mb]

use std::collections::{HashMap, HashSet};
use std::io::Read;

use dca::parser::parse_header;
use dca::{exss_substream_size, HdDecoder};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: xll_x_meta <in.dts> [max_mb]");
    let max_mb: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(64);

    let mut input = std::fs::File::open(&path).expect("open input");
    let mut bytes = vec![0u8; max_mb * 1024 * 1024];
    let read = input.read(&mut bytes).expect("read input");
    bytes.truncate(read);

    let mut decoder = HdDecoder::new();
    let mut offset = 0usize;
    let mut words = Vec::new();
    let mut x_payload_offsets = Vec::new();
    let mut x_payload_sizes = Vec::new();
    let mut tail_lengths = HashMap::new();

    while offset + 18 < bytes.len() {
        let core = match parse_header(&bytes[offset..]) {
            Ok(header) => header,
            Err(_) => break,
        };
        let exss_offset = offset + core.frame_size;
        if exss_offset + 4 > bytes.len() {
            break;
        }
        let exss_len = match exss_substream_size(&bytes[exss_offset..]) {
            Some(len) if exss_offset + len <= bytes.len() => len,
            _ => break,
        };
        if let Ok(frame) = decoder.decode(
            &bytes[offset..exss_offset],
            &bytes[exss_offset..exss_offset + exss_len],
        ) {
            *tail_lengths
                .entry(frame.exss_descriptor_tail_bits)
                .or_insert(0usize) += 1;
            if frame.exss_descriptor_tail.len() >= 8 {
                words.push(u64::from_be_bytes(
                    frame.exss_descriptor_tail[..8].try_into().unwrap(),
                ));
                x_payload_offsets.push(frame.x_payload_offset);
                x_payload_sizes.push(frame.x_payload.len());
            }
        }
        offset += core.frame_size + exss_len;
    }

    println!("read {read} bytes from {path}; tail lengths: {tail_lengths:?}");
    println!(
        "captured {} words; {} distinct",
        words.len(),
        words.iter().copied().collect::<HashSet<_>>().len()
    );
    let navigation_matches = words
        .iter()
        .enumerate()
        .filter(|&(index, &word)| {
            let offset = ((word >> 45) & 0x7ff) as usize * 4;
            let size = ((word >> 6) & 0x3ff) as usize * 4 + 24;
            offset == x_payload_offsets[index] && size == x_payload_sizes[index]
        })
        .count();
    let syntax_matches = words
        .iter()
        .filter(|&&word| {
            let high = (word >> 32) as u32;
            let low = word as u32;
            high & !0x00ff_e000 == 0x1800_0000 && low & !0x0000_ffc0 == 0x8a28_0000
        })
        .count();
    println!(
        "XLL-X navigation syntax matches: {syntax_matches}/{}",
        words.len()
    );
    println!(
        "XLL-X navigation identity matches: {navigation_matches}/{}",
        words.len()
    );
    let first_mismatches = words
        .iter()
        .enumerate()
        .filter_map(|(index, &word)| {
            let encoded_offset = ((word >> 45) & 0x7ff) as usize * 4;
            let encoded_size = ((word >> 6) & 0x3ff) as usize * 4 + 24;
            let actual_offset = x_payload_offsets[index];
            let actual_size = x_payload_sizes[index];
            (encoded_offset != actual_offset || encoded_size != actual_size).then_some((
                index,
                encoded_offset,
                actual_offset,
                encoded_size,
                actual_size,
            ))
        })
        .take(12)
        .collect::<Vec<_>>();
    println!("first navigation mismatches (frame, encoded/actual offset, encoded/actual size): {first_mismatches:?}");
    println!("navigation matches by decoded-frame lag (actual index = metadata index + lag):");
    for lag in -4isize..=4 {
        let mut matches = 0usize;
        let mut compared = 0usize;
        for (index, &word) in words.iter().enumerate() {
            let actual = index as isize + lag;
            if !(0..words.len() as isize).contains(&actual) {
                continue;
            }
            let actual = actual as usize;
            let offset = ((word >> 45) & 0x7ff) as usize * 4;
            let size = ((word >> 6) & 0x3ff) as usize * 4 + 24;
            matches +=
                usize::from(offset == x_payload_offsets[actual] && size == x_payload_sizes[actual]);
            compared += 1;
        }
        println!("  {lag:+}: {matches}/{compared}");
    }
    println!("first words (frame, seconds, u64, four u16 lanes, bed/ext bytes):");
    for (frame, &word) in words.iter().take(32).enumerate() {
        let lanes = [
            (word >> 48) as u16,
            (word >> 32) as u16,
            (word >> 16) as u16,
            word as u16,
        ];
        println!(
            "  {frame:>5} {:>9.5}  {word:016x}  {lanes:04x?}  {:>5}/{:>4}",
            frame as f64 * 512.0 / 48_000.0,
            x_payload_offsets[frame],
            x_payload_sizes[frame]
        );
    }

    let mut ones = [0usize; 64];
    let mut toggles = [0usize; 64];
    for (index, &word) in words.iter().enumerate() {
        for bit in 0..64 {
            ones[bit] += ((word >> (63 - bit)) & 1) as usize;
            if index > 0 {
                toggles[bit] += (((word ^ words[index - 1]) >> (63 - bit)) & 1) as usize;
            }
        }
    }
    println!("bit statistics (MSB-first index: ones/toggles):");
    for bit in 0..64 {
        if ones[bit] != 0 || toggles[bit] != 0 {
            println!("  {bit:>2}: {:>6}/{:>6}", ones[bit], toggles[bit]);
        }
    }

    println!("signed delta ranges for four 16-bit lanes:");
    for lane in 0..4 {
        let shift = 48 - 16 * lane;
        let mut minimum = i32::MAX;
        let mut maximum = i32::MIN;
        let mut zero = 0usize;
        for pair in words.windows(2) {
            let previous = ((pair[0] >> shift) & 0xffff) as i32;
            let current = ((pair[1] >> shift) & 0xffff) as i32;
            let delta = current - previous;
            minimum = minimum.min(delta);
            maximum = maximum.max(delta);
            zero += usize::from(delta == 0);
        }
        println!("  lane {lane}: {minimum}..{maximum}, unchanged {zero}");
    }
}
