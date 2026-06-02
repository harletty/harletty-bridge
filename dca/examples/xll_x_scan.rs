// SPDX-License-Identifier: Apache-2.0
//
// Does the DTS:X object blob reuse DCA framing? Scan each captured X-extension
// payload (syncword..frame-end, the bed's own XLL excluded) for known DCA
// syncwords. A nested XLL sync (0x41A29547) would mean objects are a full XLL
// substream; its absence means they reuse only the inner band-data coding.
//
// Usage: cargo run -p dca --release --example xll_x_scan -- <in.dts> [max_mb]

use std::io::Read;

use dca::parser::parse_header;
use dca::{HdDecoder, exss_substream_size};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: xll_x_scan <in.dts> [max_mb]");
    let max_mb: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(64);

    let mut f = std::fs::File::open(&path).expect("open");
    let mut bytes = vec![0u8; max_mb * 1024 * 1024];
    let n = f.read(&mut bytes).expect("read");
    bytes.truncate(n);

    // (name, syncword) — DCA syncwords that could appear if objects reuse framing.
    let syncs: [(&str, u32); 7] = [
        ("XLL", 0x41A2_9547),
        ("CORE_BE", 0x7FFE_8001),
        ("XCH", 0x5A5A_5A5A),
        ("XXCH", 0x4700_4A03),
        ("X96", 0x1D95_F262),
        ("XBR", 0x655E_315E),
        ("XLL_X(again)", 0x0200_0850),
    ];
    let mut hit_blobs = [0u32; 7];
    let mut total_occ = [0u64; 7];
    let mut first_off = [usize::MAX; 7];
    let mut blobs = 0u64;

    let mut dec = HdDecoder::new();
    let mut off = 0usize;
    let mut frames = 0usize;
    while off + 18 < bytes.len() && frames < 4000 {
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
        if let Ok(fr) = dec.decode(&bytes[off..exss_off], &bytes[exss_off..exss_off + exss_len]) {
            let p = &fr.x_payload;
            if (fr.x_present || fr.x_imax) && p.len() > 8 {
                blobs += 1;
                for (si, (_, sw)) in syncs.iter().enumerate() {
                    let pat = sw.to_be_bytes();
                    let mut found = false;
                    // skip the leading syncword at offset 0 for the XLL_X pattern
                    let start = if si == 6 { 4 } else { 0 };
                    let mut i = start;
                    while i + 4 <= p.len() {
                        if p[i..i + 4] == pat {
                            total_occ[si] += 1;
                            found = true;
                            if first_off[si] == usize::MAX {
                                first_off[si] = i;
                            }
                        }
                        i += 1;
                    }
                    if found {
                        hit_blobs[si] += 1;
                    }
                }
            }
        }
        off += core.frame_size + exss_len;
        frames += 1;
    }

    println!("scanned {blobs} object blobs (byte-aligned syncword search):\n");
    println!("  syncword       blobs-containing   total-occurrences   first-offset");
    for (si, (name, sw)) in syncs.iter().enumerate() {
        let fo = if first_off[si] == usize::MAX { -1 } else { first_off[si] as i64 };
        println!(
            "  {name:<14} {:>8} ({:>5.1}%)   {:>10}        {fo}",
            hit_blobs[si],
            100.0 * hit_blobs[si] as f64 / blobs.max(1) as f64,
            total_occ[si],
        );
        let _ = sw;
    }
}
