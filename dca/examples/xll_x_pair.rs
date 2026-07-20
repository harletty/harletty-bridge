// SPDX-License-Identifier: Apache-2.0
//
// Compare two encodes of the same programme (for example two language tracks)
// frame by frame.  Fields and independently coded object blocks shared by the
// two tracks are candidates for geometry/metadata or unchanged music/effects;
// divergent regions are candidates for language-specific object audio.
//
// Usage:
//   cargo run -p dca --release --example xll_x_pair -- <a.dts> <b.dts> [max_mb]

use std::collections::HashMap;
use std::io::Read;

use dca::parser::parse_header;
use dca::{exss_substream_size, HdDecoder};

const HEADER_SCAN: usize = 128;
const DATA_START: usize = 27;
const MAX_LAG: isize = 512;

struct Capture {
    payload: Vec<u8>,
    rms: [f64; 4],
    descriptor_word: Option<u64>,
}

fn load_payloads(path: &str, max_mb: usize) -> Vec<Capture> {
    let mut input = std::fs::File::open(path).expect("open input");
    let mut bytes = vec![0u8; max_mb * 1024 * 1024];
    let read = input.read(&mut bytes).expect("read input");
    bytes.truncate(read);

    let mut decoder = HdDecoder::new();
    let mut payloads = Vec::new();
    let mut offset = 0usize;
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
            if frame.x_present || frame.x_imax {
                let mut rms = [0.0; 4];
                for (slot, samples) in rms.iter_mut().zip(&frame.x_samples) {
                    *slot = (samples
                        .iter()
                        .map(|&sample| (sample as f64).powi(2))
                        .sum::<f64>()
                        / samples.len().max(1) as f64)
                        .sqrt();
                }
                payloads.push(Capture {
                    payload: frame.x_payload,
                    rms,
                    descriptor_word: frame
                        .exss_descriptor_tail
                        .get(..8)
                        .map(|bytes| u64::from_be_bytes(bytes.try_into().unwrap())),
                });
            }
        }
        offset += core.frame_size + exss_len;
    }
    eprintln!(
        "{path}: read {read} bytes, captured {} payloads",
        payloads.len()
    );
    payloads
}

fn aligned_range(a_len: usize, b_len: usize, lag: isize) -> (usize, usize, usize) {
    let a_start = (-lag).max(0) as usize;
    let b_start = lag.max(0) as usize;
    let count = (a_len - a_start).min(b_len - b_start);
    (a_start, b_start, count)
}

fn infer_lag(a: &[Capture], b: &[Capture]) -> (isize, f64, usize) {
    let mut best = (0, f64::NEG_INFINITY, 0);
    for lag in -MAX_LAG..=MAX_LAG {
        let (a_start, b_start, count) = aligned_range(a.len(), b.len(), lag);
        let a_energy = (0..count)
            .map(|i| a[a_start + i].rms.iter().sum::<f64>())
            .collect::<Vec<_>>();
        let b_energy = (0..count)
            .map(|i| b[b_start + i].rms.iter().sum::<f64>())
            .collect::<Vec<_>>();
        let correlation = pearson(&a_energy, &b_energy);
        if correlation > best.1 || (correlation == best.1 && count > best.2) {
            best = (lag, correlation, count);
        }
    }
    best
}

fn pearson(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len().min(b.len());
    if n == 0 {
        return f64::NAN;
    }
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

fn percentile(values: &mut [usize], numerator: usize, denominator: usize) -> usize {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[(values.len() - 1) * numerator / denominator]
}

fn common_prefix(a: &[u8], b: &[u8], start: usize) -> usize {
    let mut n = 0;
    while start + n < a.len() && start + n < b.len() && a[start + n] == b[start + n] {
        n += 1;
    }
    n
}

fn common_suffix(a: &[u8], b: &[u8]) -> usize {
    a.iter()
        .rev()
        .zip(b.iter().rev())
        .take_while(|(x, y)| x == y)
        .count()
}

fn u64_at(data: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes(data[offset..offset + 8].try_into().unwrap())
}

fn longest_common_run(a: &[u8], b: &[u8], start: usize) -> (usize, isize) {
    if a.len() < start + 8 || b.len() < start + 8 {
        return (0, 0);
    }
    let mut anchors: HashMap<u64, Vec<usize>> = HashMap::new();
    for j in start..=b.len() - 8 {
        let positions = anchors.entry(u64_at(b, j)).or_default();
        if positions.len() < 4 {
            positions.push(j);
        }
    }

    let mut best = (0usize, 0isize);
    for i in start..=a.len() - 8 {
        let Some(candidates) = anchors.get(&u64_at(a, i)) else {
            continue;
        };
        for &j in candidates {
            let mut left = 0;
            while left < i - start && left < j - start && a[i - left - 1] == b[j - left - 1] {
                left += 1;
            }
            let mut right = 8;
            while i + right < a.len() && j + right < b.len() && a[i + right] == b[j + right] {
                right += 1;
            }
            let length = left + right;
            if length > best.0 {
                best = (length, j as isize - i as isize);
            }
        }
    }
    best
}

fn main() {
    let mut args = std::env::args().skip(1);
    let a_path = args
        .next()
        .expect("usage: xll_x_pair <a.dts> <b.dts> [max_mb]");
    let b_path = args
        .next()
        .expect("usage: xll_x_pair <a.dts> <b.dts> [max_mb]");
    let max_mb: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(64);

    let a = load_payloads(&a_path, max_mb);
    let b = load_payloads(&b_path, max_mb);
    let (lag, alignment_correlation, alignment_count) = infer_lag(&a, &b);
    let (a_start, b_start, count) = aligned_range(a.len(), b.len(), lag);
    println!(
        "best alignment: B frame = A frame + {lag}; four-channel RMS correlation \
         {alignment_correlation:.6} over {alignment_count} frames"
    );

    let mut per_offset_equal = [0usize; HEADER_SCAN];
    let mut per_offset_total = [0usize; HEADER_SCAN];
    let mut exact_payloads = 0usize;
    let mut equal_data_bytes = 0usize;
    let mut total_data_bytes = 0usize;
    let mut prefixes = Vec::with_capacity(count);
    let mut suffixes = Vec::with_capacity(count);
    let mut common_runs = Vec::with_capacity(count);
    let mut run_deltas: HashMap<isize, usize> = HashMap::new();
    let mut lengths_a = Vec::with_capacity(count);
    let mut lengths_b = Vec::with_capacity(count);

    for i in 0..count {
        let pa = &a[a_start + i].payload;
        let pb = &b[b_start + i].payload;
        if pa == pb {
            exact_payloads += 1;
        }
        for offset in 0..HEADER_SCAN.min(pa.len()).min(pb.len()) {
            per_offset_total[offset] += 1;
            if pa[offset] == pb[offset] {
                per_offset_equal[offset] += 1;
            }
        }
        for (&x, &y) in pa.iter().skip(DATA_START).zip(pb.iter().skip(DATA_START)) {
            total_data_bytes += 1;
            if x == y {
                equal_data_bytes += 1;
            }
        }
        prefixes.push(common_prefix(pa, pb, 22));
        suffixes.push(common_suffix(pa, pb));
        let (run, delta) = longest_common_run(pa, pb, DATA_START);
        common_runs.push(run);
        if run >= 16 {
            *run_deltas.entry(delta).or_default() += 1;
        }
        lengths_a.push(pa.len() as f64);
        lengths_b.push(pb.len() as f64);
    }

    println!("aligned payloads: {count}; exactly identical: {exact_payloads}");
    let mut equal_descriptor_words = 0usize;
    let mut descriptor_a = Vec::new();
    let mut descriptor_b = Vec::new();
    for i in 0..count {
        if let (Some(a_word), Some(b_word)) = (
            a[a_start + i].descriptor_word,
            b[b_start + i].descriptor_word,
        ) {
            equal_descriptor_words += usize::from(a_word == b_word);
            descriptor_a.push(((a_word >> 45) & 0xff) as f64);
            descriptor_b.push(((b_word >> 45) & 0xff) as f64);
        }
    }
    println!(
        "EXSS reserved 64-bit words identical: {equal_descriptor_words}/{}; first 8-bit field r={:.6}",
        descriptor_a.len(),
        pearson(&descriptor_a, &descriptor_b)
    );
    for channel in 0..4 {
        let ea = (0..count)
            .map(|i| a[a_start + i].rms[channel])
            .collect::<Vec<_>>();
        let eb = (0..count)
            .map(|i| b[b_start + i].rms[channel])
            .collect::<Vec<_>>();
        println!(
            "decoded channel {channel} frame-RMS Pearson r: {:.6}",
            pearson(&ea, &eb)
        );
    }
    println!(
        "payload-length Pearson r: {:.4}",
        pearson(&lengths_a, &lengths_b)
    );
    println!(
        "same-offset data bytes after byte {DATA_START}: {equal_data_bytes}/{total_data_bytes} ({:.3}%)",
        100.0 * equal_data_bytes as f64 / total_data_bytes.max(1) as f64
    );
    println!("\nper-offset equality (first {HEADER_SCAN} bytes):");
    for offset in 0..HEADER_SCAN {
        if per_offset_total[offset] == 0 {
            break;
        }
        let fraction = 100.0 * per_offset_equal[offset] as f64 / per_offset_total[offset] as f64;
        if offset < 32 || fraction >= 5.0 {
            println!("  byte {offset:>3}: {fraction:>7.3}%");
        }
    }

    let mut prefixes_copy = prefixes.clone();
    let mut suffixes_copy = suffixes.clone();
    let mut runs_copy = common_runs.clone();
    println!("\nmatched-run distributions (bytes):");
    println!(
        "  prefix from byte 22: p50={} p90={} max={}",
        percentile(&mut prefixes_copy, 1, 2),
        percentile(&mut prefixes, 9, 10),
        prefixes.iter().copied().max().unwrap_or(0)
    );
    println!(
        "  suffix:              p50={} p90={} max={}",
        percentile(&mut suffixes_copy, 1, 2),
        percentile(&mut suffixes, 9, 10),
        suffixes.iter().copied().max().unwrap_or(0)
    );
    println!(
        "  longest run >=8:     p50={} p90={} max={}",
        percentile(&mut runs_copy, 1, 2),
        percentile(&mut common_runs, 9, 10),
        common_runs.iter().copied().max().unwrap_or(0)
    );

    let mut deltas: Vec<_> = run_deltas.into_iter().collect();
    deltas.sort_by(|a, b| b.1.cmp(&a.1));
    println!("most common offsets B-A for matching runs >=16 bytes:");
    for (delta, occurrences) in deltas.into_iter().take(12) {
        println!("  {delta:>6}: {occurrences}");
    }
}
