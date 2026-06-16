// SPDX-License-Identifier: Apache-2.0
//
// Hypothesis: the DTS:X object payload is residual AUDIO (one mono object per
// active object, XLL-style residual coding matrixed against the 7.1 bed), not
// opaque metadata. Decisive cheap test: the bit budget. If it is audio of
// `nframesamples` samples per object, then
//     bits_per_sample = (payload_bytes - overhead) * 8 / (n_objects * nsamples)
// should land in a sane audio-residual range (~1..8 b/sample) and be roughly
// stable across frames/counts. We also try object counts = byte22 and byte22-3
// (count 3 == the 48-byte null frame, i.e. likely 0 objects).
//
// Usage: cargo run -p dca --release --example xll_x_audio -- <in.dts> [max_mb]

use std::io::Read;

use dca::parser::parse_header;
use dca::{HdDecoder, exss_substream_size};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: xll_x_audio <in.dts> [max_mb]");
    let max_mb: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(96);

    let mut f = std::fs::File::open(&path).expect("open");
    let mut bytes = vec![0u8; max_mb * 1024 * 1024];
    let n = f.read(&mut bytes).expect("read");
    bytes.truncate(n);

    const OVERHEAD: usize = 27; // bytes before object data (syncword+hdr+fields)
    let mut nsamp_hist = std::collections::BTreeMap::<usize, u32>::new();
    // bits/sample buckets, per assumed object count, for the count-3 offset model
    let mut bps_by_obj: std::collections::BTreeMap<usize, Vec<f64>> = std::collections::BTreeMap::new();
    let mut all_bps: Vec<f64> = Vec::new();

    let mut dec = HdDecoder::new();
    let mut off = 0usize;
    let mut frames = 0usize;
    while off + 18 < bytes.len() && frames < 6000 {
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
            let nsamp = fr.samples.iter().find_map(|o| o.as_ref().map(|v| v.len())).unwrap_or(0);
            if (fr.x_present || fr.x_imax) && p.len() > OVERHEAD && nsamp > 0 {
                *nsamp_hist.entry(nsamp).or_insert(0) += 1;
                let raw = p[22] as usize;
                let nobj = raw.saturating_sub(3); // hypothesis: count 3 => 0 objects
                if nobj > 0 {
                    let obj_bits = (p.len() - OVERHEAD) * 8;
                    let bps = obj_bits as f64 / (nobj * nsamp) as f64;
                    bps_by_obj.entry(nobj).or_default().push(bps);
                    all_bps.push(bps);
                }
            }
        }
        off += core.frame_size + exss_len;
        frames += 1;
    }

    println!("nframesamples (bed) histogram:");
    for (s, c) in &nsamp_hist {
        println!("  {s} samples/frame: {c} frames");
    }
    let nsamp = nsamp_hist.iter().max_by_key(|(_, c)| **c).map(|(s, _)| *s).unwrap_or(0);
    println!("\nmodal nframesamples = {nsamp}");

    let med = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        if v.is_empty() { 0.0 } else { v[v.len() / 2] }
    };
    println!("\nbits/sample under model objects = byte22 - 3, nsamples = {nsamp}:");
    println!("  nobj    n     min    median     max   (bits/sample)");
    for (o, v) in bps_by_obj.iter_mut() {
        let mn = v.iter().cloned().fold(f64::INFINITY, f64::min);
        let mx = v.iter().cloned().fold(0.0, f64::max);
        let md = med(v);
        println!("  {o:>4} {:>6}  {mn:>6.2} {md:>8.2} {mx:>7.2}", v.len());
    }
    let md_all = med(&mut all_bps);
    println!("\noverall median bits/sample = {md_all:.2}  (sane audio residual ≈ 1..8)");
}
