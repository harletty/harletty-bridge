// SPDX-License-Identifier: Apache-2.0
//
// Characterize the DTS:X end-of-frame extension blob (syncword 0x02000850).
// ffmpeg detects this syncword and discards the payload; we retain it (see
// XllDecoder::detect_x_extension) so we can study its structure offline.
//
// Walks a raw [core][exss][core][exss]… DTS-HD stream, decodes each frame, and
// reports: how many frames carry the blob, a payload-size histogram, hexdumps of
// the first payloads, a scan for recurring 32-bit markers, byte entropy, and a
// frame_index,payload_len CSV (for correlating blob size with scene activity).
//
// Usage:
//   cargo run -p dca --example xll_x_probe -- <in.dts> [max_mb] [csv_out]
// max_mb caps how much of the (multi-GB) file is read (default 64 MB).

use std::io::Read;

use dca::hd::HdError;
use dca::parser::parse_header;
use dca::{HdDecoder, exss_substream_size};

fn main() {
    let mut args = std::env::args().skip(1);
    let in_path = args
        .next()
        .expect("usage: xll_x_probe <in.dts> [max_mb] [csv_out]");
    let max_mb: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(64);
    let csv_out = args.next();
    let cap = max_mb * 1024 * 1024;

    // Read a bounded prefix (the dump can be multiple GB).
    let mut f = std::fs::File::open(&in_path).expect("open input");
    let mut bytes = vec![0u8; cap];
    let n = f.read(&mut bytes).expect("read");
    bytes.truncate(n);
    println!("read {} MiB of {in_path}", n / (1024 * 1024));

    let mut dec = HdDecoder::new();
    let mut off = 0usize;
    let mut frames = 0usize; // frames that produced output
    let mut pending = 0usize; // PBR-buffered frames (no output)
    let mut x_frames = 0usize;
    let mut imax_frames = 0usize;

    let mut sizes: Vec<usize> = Vec::new();
    let mut offsets: Vec<usize> = Vec::new();
    let mut first_payloads: Vec<Vec<u8>> = Vec::new();
    // Byte histogram over concatenated payloads (entropy) + 32-bit word counts.
    let mut byte_hist = [0u64; 256];
    let mut word_count: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
    // Recurring value per dword-offset within the payload (after the syncword).
    let mut per_off: Vec<std::collections::HashMap<u32, u64>> = Vec::new();

    while off + 18 < bytes.len() {
        let core = match parse_header(&bytes[off..]) {
            Ok(c) => c,
            Err(_) => break,
        };
        let exss_off = off + core.frame_size;
        if exss_off + 4 > bytes.len() {
            break;
        }
        let exss_len = match exss_substream_size(&bytes[exss_off..]) {
            Some(l) if exss_off + l <= bytes.len() => l,
            _ => break,
        };
        let exss = &bytes[exss_off..exss_off + exss_len];
        match dec.decode(&bytes[off..exss_off], exss) {
            Ok(fr) => {
                if fr.x_present {
                    x_frames += 1;
                }
                if fr.x_imax {
                    imax_frames += 1;
                }
                if fr.x_present || fr.x_imax {
                    let p = &fr.x_payload;
                    sizes.push(p.len());
                    offsets.push(fr.x_payload_offset);
                    for &b in p {
                        byte_hist[b as usize] += 1;
                    }
                    // 32-bit big-endian words at every byte offset (find markers).
                    for w in p.windows(4) {
                        let v = u32::from_be_bytes([w[0], w[1], w[2], w[3]]);
                        *word_count.entry(v).or_insert(0) += 1;
                    }
                    // Per-position dwords (payload incl. syncword, dword stride).
                    for (i, ch) in p.chunks_exact(4).enumerate() {
                        let v = u32::from_be_bytes([ch[0], ch[1], ch[2], ch[3]]);
                        if per_off.len() <= i {
                            per_off.resize(i + 1, std::collections::HashMap::new());
                        }
                        *per_off[i].entry(v).or_insert(0) += 1;
                    }
                    if first_payloads.len() < 10 {
                        first_payloads.push(p.clone());
                    }
                }
                frames += 1;
            }
            Err(HdError::Pending) => pending += 1,
            Err(e) => {
                eprintln!("decode error at byte {off} (frame {frames}): {e:?}");
                break;
            }
        }
        off += core.frame_size + exss_len;
    }

    println!("\n=== summary ===");
    println!("decoded frames (with output): {frames}");
    println!("PBR-pending frames (no output): {pending}");
    println!(
        "DTS:X present: {x_frames}/{frames} ({:.1}%)   IMAX: {imax_frames}",
        100.0 * x_frames as f64 / frames.max(1) as f64
    );

    if sizes.is_empty() {
        println!("no DTS:X payloads captured.");
        return;
    }

    // Size histogram.
    let smin = *sizes.iter().min().unwrap();
    let smax = *sizes.iter().max().unwrap();
    let smean = sizes.iter().sum::<usize>() as f64 / sizes.len() as f64;
    println!(
        "\npayload size: min={smin} max={smax} mean={smean:.1} bytes  fixed={}",
        smin == smax
    );
    println!(
        "payload offset within XLL frame: min={} max={}",
        offsets.iter().min().unwrap(),
        offsets.iter().max().unwrap()
    );
    {
        use std::collections::BTreeMap;
        let mut h: BTreeMap<usize, usize> = BTreeMap::new();
        for &s in &sizes {
            *h.entry(s).or_insert(0) += 1;
        }
        println!("size histogram (bytes: count):");
        for (s, c) in h.iter().take(40) {
            println!("  {s:>6}: {c}");
        }
        if h.len() > 40 {
            println!("  … ({} distinct sizes total)", h.len());
        }
    }

    // Byte entropy.
    let total: u64 = byte_hist.iter().sum();
    let mut ent = 0f64;
    for &c in &byte_hist {
        if c > 0 {
            let p = c as f64 / total as f64;
            ent -= p * p.log2();
        }
    }
    println!("\nbyte entropy over {total} payload bytes: {ent:.3} bits/byte (8.0 = random)");

    // Most common 32-bit words (candidate secondary syncwords / fixed fields).
    let mut words: Vec<(u32, u64)> = word_count.into_iter().collect();
    words.sort_by(|a, b| b.1.cmp(&a.1));
    println!("\ntop recurring 32-bit values (any offset):");
    for (v, c) in words.iter().take(12) {
        println!("  {v:#010x}: {c}");
    }

    // Per-position dword stability (which positions are constant across frames).
    println!("\nper-dword-position constancy (first 16 positions):");
    for (i, m) in per_off.iter().take(16).enumerate() {
        let (val, cnt) = m.iter().max_by_key(|(_, c)| **c).unwrap();
        let frac = 100.0 * *cnt as f64 / sizes.len() as f64;
        let tag = if i == 0 { " <- syncword dword" } else { "" };
        println!(
            "  +{:>2} (byte {:>3}): {val:#010x} in {frac:>5.1}% of frames ({} distinct){tag}",
            i,
            i * 4,
            m.len()
        );
    }

    // Hexdump first few payloads.
    println!("\n=== first {} payloads (hex) ===", first_payloads.len());
    for (k, p) in first_payloads.iter().enumerate() {
        println!("-- payload #{k} ({} bytes) --", p.len());
        hexdump(p, 64);
    }

    // CSV.
    if let Some(path) = csv_out {
        use std::io::Write;
        let mut w = std::fs::File::create(&path).expect("create csv");
        writeln!(w, "frame_index,payload_len,payload_offset").unwrap();
        for (i, (s, o)) in sizes.iter().zip(offsets.iter()).enumerate() {
            writeln!(w, "{i},{s},{o}").unwrap();
        }
        println!("\nwrote per-frame CSV to {path}");
    }
}

fn hexdump(data: &[u8], max: usize) {
    let n = data.len().min(max);
    for row in data[..n].chunks(16) {
        let hex: Vec<String> = row.iter().map(|b| format!("{b:02x}")).collect();
        let ascii: String = row
            .iter()
            .map(|&b| {
                if (0x20..0x7f).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        println!("  {:<48} {ascii}", hex.join(" "));
    }
    if data.len() > n {
        println!("  … (+{} more bytes)", data.len() - n);
    }
}
