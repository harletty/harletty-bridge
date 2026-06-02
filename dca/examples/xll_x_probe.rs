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
    // Exact byte-level fixed prefix shared by ALL payloads, and per-byte modal
    // value over the first PREFIX_SCAN positions (to locate the divergence point).
    const PREFIX_SCAN: usize = 96;
    let mut common: Vec<u8> = Vec::new(); // first payload's bytes (prefix shrinks into it)
    let mut prefix_len = usize::MAX;
    let mut per_byte: Vec<std::collections::HashMap<u8, u64>> = Vec::new();
    // Per-frame audio energy of the decoded 7.1 bed, for size↔activity correlation.
    // DCA speaker indices (from the regression mapping): 0=C 1=L 2=R 3=Ls 4=Rs
    // 5=LFE 7=Lsr(BL) 8=Rsr(BR). Surround/back = {3,4,7,8} ≈ immersive activity.
    const SURR: [usize; 4] = [3, 4, 7, 8];
    let mut total_rms: Vec<f64> = Vec::new();
    let mut surr_rms: Vec<f64> = Vec::new();
    // Candidate field right after the 22-byte header: byte 22 nibble (a count?).
    let mut f22: Vec<f64> = Vec::new();

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

                    // Per-frame bed energy (RMS over active channels) + surround.
                    let mut tot = 0f64;
                    let mut sur = 0f64;
                    let mut nsamp = 0usize;
                    for (spk, ch) in fr.samples.iter().enumerate() {
                        if let Some(v) = ch {
                            nsamp = v.len();
                            let e: f64 = v.iter().map(|&s| (s as f64) * (s as f64)).sum();
                            tot += e;
                            if SURR.contains(&spk) {
                                sur += e;
                            }
                        }
                    }
                    let nn = nsamp.max(1) as f64;
                    total_rms.push((tot / nn).sqrt());
                    surr_rms.push((sur / nn).sqrt());
                    f22.push(if p.len() > 22 { p[22] as f64 } else { -1.0 });

                    // Exact fixed-prefix tracking + per-byte modal value.
                    if common.is_empty() {
                        common = p.clone();
                        prefix_len = p.len();
                    } else {
                        let lim = prefix_len.min(p.len());
                        let mut l = 0;
                        while l < lim && p[l] == common[l] {
                            l += 1;
                        }
                        prefix_len = l;
                    }
                    for (i, &b) in p.iter().take(PREFIX_SCAN).enumerate() {
                        if per_byte.len() <= i {
                            per_byte.resize(i + 1, std::collections::HashMap::new());
                        }
                        *per_byte[i].entry(b).or_insert(0) += 1;
                    }

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

    // Exact fixed prefix shared by every payload.
    println!(
        "\n=== fixed header: {prefix_len} bytes identical in ALL {} frames ===",
        sizes.len()
    );
    hexdump(&common[..prefix_len.min(common.len())], prefix_len.min(common.len()));
    // Per-byte cardinality map (structured low-cardinality fields vs random
    // payload) for the first PREFIX_SCAN positions, with value enumeration for
    // low-cardinality (≤16 distinct) bytes — these are the parseable fields.
    println!("per-byte cardinality / modal value (first {PREFIX_SCAN} positions):");
    for (i, m) in per_byte.iter().enumerate() {
        let (val, cnt) = m.iter().max_by_key(|(_, c)| **c).unwrap();
        let frac = 100.0 * *cnt as f64 / sizes.len() as f64;
        let mark = if i == prefix_len { "  <-- first varying byte" } else { "" };
        let mut line = format!(
            "  byte {i:>3}: {:>3} distinct, modal 0x{val:02x} ({frac:>5.1}%){mark}",
            m.len()
        );
        if m.len() <= 16 {
            let mut vs: Vec<(u8, u64)> = m.iter().map(|(&k, &c)| (k, c)).collect();
            vs.sort_by(|a, b| b.1.cmp(&a.1));
            let parts: Vec<String> = vs
                .iter()
                .map(|(v, c)| format!("0x{v:02x}:{:.0}%", 100.0 * *c as f64 / sizes.len() as f64))
                .collect();
            line.push_str(&format!("   {{{}}}", parts.join(" ")));
        }
        println!("{line}");
    }

    // Size ↔ scene-activity correlation.
    println!("\n=== payload size ↔ audio activity ===");
    let pr_tot = pearson(&sizes_f64(&sizes), &total_rms, 0);
    let pr_sur = pearson(&sizes_f64(&sizes), &surr_rms, 0);
    println!("Pearson r(payload_len, total bed RMS)      = {pr_tot:+.3}");
    println!("Pearson r(payload_len, surround/back RMS)  = {pr_sur:+.3}");
    println!("lag sweep r(payload_len, surround RMS), payload leading audio by k frames:");
    for k in -4i64..=4 {
        let r = pearson(&sizes_f64(&sizes), &surr_rms, k);
        println!("  lag {k:+}: {r:+.3}");
    }
    // Terciles of surround energy → mean payload size (monotone relationship?).
    {
        let mut idx: Vec<usize> = (0..surr_rms.len()).collect();
        idx.sort_by(|&a, &b| surr_rms[a].partial_cmp(&surr_rms[b]).unwrap());
        let t = idx.len() / 3;
        let mean = |sl: &[usize]| sl.iter().map(|&i| sizes[i] as f64).sum::<f64>() / sl.len() as f64;
        println!(
            "mean payload by surround-energy tercile: low={:.0}B  mid={:.0}B  high={:.0}B",
            mean(&idx[..t]),
            mean(&idx[t..2 * t]),
            mean(&idx[2 * t..])
        );
    }

    // Test the "byte 22 = object count" hypothesis: correlate it and group.
    println!("\n=== byte-22 nibble field (count hypothesis) ===");
    let r_sz = pearson(&f22, &sizes_f64(&sizes), 0);
    let r_su = pearson(&f22, &surr_rms, 0);
    println!("Pearson r(byte22, payload_len) = {r_sz:+.3}   r(byte22, surround RMS) = {r_su:+.3}");
    {
        use std::collections::BTreeMap;
        let mut g: BTreeMap<i64, (usize, f64, f64)> = BTreeMap::new();
        for i in 0..f22.len() {
            let e = g.entry(f22[i] as i64).or_insert((0, 0.0, 0.0));
            e.0 += 1;
            e.1 += sizes[i] as f64;
            e.2 += surr_rms[i];
        }
        println!("byte22 value: count  mean_payload  mean_surr_rms");
        for (v, (c, sp, sr)) in g {
            println!("  0x{v:02x} ({v:>2}): {c:>6}  {:>9.0}B  {:>10.3e}", sp / c as f64, sr / c as f64);
        }
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
        writeln!(w, "frame_index,payload_len,payload_offset,total_rms,surr_rms").unwrap();
        for i in 0..sizes.len() {
            writeln!(
                w,
                "{i},{},{},{:.6e},{:.6e}",
                sizes[i], offsets[i], total_rms[i], surr_rms[i]
            )
            .unwrap();
        }
        println!("\nwrote per-frame CSV to {path}");
    }
}

fn sizes_f64(sizes: &[usize]) -> Vec<f64> {
    sizes.iter().map(|&s| s as f64).collect()
}

/// Pearson correlation of `x[i]` against `y[i+lag]` over the overlapping range.
fn pearson(x: &[f64], y: &[f64], lag: i64) -> f64 {
    let n = x.len().min(y.len());
    let (mut sx, mut sy, mut sxx, mut syy, mut sxy, mut m) = (0f64, 0f64, 0f64, 0f64, 0f64, 0f64);
    for i in 0..n {
        let j = i as i64 + lag;
        if j < 0 || j as usize >= n {
            continue;
        }
        let (a, b) = (x[i], y[j as usize]);
        sx += a;
        sy += b;
        sxx += a * a;
        syy += b * b;
        sxy += a * b;
        m += 1.0;
    }
    let cov = m * sxy - sx * sy;
    let den = ((m * sxx - sx * sx) * (m * syy - sy * sy)).sqrt();
    if den == 0.0 { 0.0 } else { cov / den }
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
