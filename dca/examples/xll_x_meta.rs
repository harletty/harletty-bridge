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
    let mut half_peaks = Vec::new();
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
                let mut peaks = [0.0f64; 2];
                for samples in frame.samples.iter().flatten().chain(&frame.x_samples) {
                    for (half, peak) in peaks.iter_mut().enumerate() {
                        let start = half * samples.len() / 2;
                        let end = (half + 1) * samples.len() / 2;
                        for &sample in &samples[start..end] {
                            *peak = peak.max((sample as f64).abs());
                        }
                    }
                }
                half_peaks.push(peaks);
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
    println!("first words (frame, seconds, u64, four u16 lanes):");
    for (frame, &word) in words.iter().take(32).enumerate() {
        let lanes = [
            (word >> 48) as u16,
            (word >> 32) as u16,
            (word >> 16) as u16,
            word as u16,
        ];
        println!(
            "  {frame:>5} {:>9.5}  {word:016x}  {lanes:04x?}",
            frame as f64 * 512.0 / 48_000.0
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

    let field_a = words
        .iter()
        .map(|&word| ((word >> 45) & 0xff) as f64)
        .collect::<Vec<_>>();
    let field_b = words
        .iter()
        .map(|&word| ((word >> 6) & 0xff) as f64)
        .collect::<Vec<_>>();
    let peak_a = half_peaks.iter().map(|peaks| peaks[0]).collect::<Vec<_>>();
    let peak_b = half_peaks.iter().map(|peaks| peaks[1]).collect::<Vec<_>>();
    let peak_db_a = peak_a
        .iter()
        .map(|&peak| 20.0 * peak.max(1e-12).log10())
        .collect::<Vec<_>>();
    let peak_db_b = peak_b
        .iter()
        .map(|&peak| 20.0 * peak.max(1e-12).log10())
        .collect::<Vec<_>>();
    println!(
        "candidate 8-bit fields A/B ranges: {:?}/{:?}",
        field_a
            .iter()
            .fold((255.0f64, 0.0f64), |(lo, hi), &v| (lo.min(v), hi.max(v))),
        field_b
            .iter()
            .fold((255.0f64, 0.0f64), |(lo, hi), &v| (lo.min(v), hi.max(v)))
    );
    println!(
        "correlation with decoded half-frame peak A/B: {:.6}/{:.6}",
        pearson(&field_a, &peak_a),
        pearson(&field_b, &peak_b)
    );
    println!(
        "correlation matrix fields A/B vs peak dB halves A/B: [[{:.6}, {:.6}], [{:.6}, {:.6}]]",
        pearson(&field_a, &peak_db_a),
        pearson(&field_a, &peak_db_b),
        pearson(&field_b, &peak_db_a),
        pearson(&field_b, &peak_db_b)
    );
}

fn pearson(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len().min(b.len());
    let am = a[..n].iter().sum::<f64>() / n as f64;
    let bm = b[..n].iter().sum::<f64>() / n as f64;
    let mut covariance = 0.0;
    let mut av = 0.0;
    let mut bv = 0.0;
    for (&x, &y) in a[..n].iter().zip(&b[..n]) {
        covariance += (x - am) * (y - bm);
        av += (x - am).powi(2);
        bv += (y - bm).powi(2);
    }
    covariance / (av * bv).sqrt()
}
