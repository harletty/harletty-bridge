// SPDX-License-Identifier: Apache-2.0
//
// Hypothesis test: is the DTS:X object payload ([27:]) a stream of N*F Rice
// codes (N = object count at byte 22, F = fixed values per object)? With Rice
// coding, a fixed F per object still yields variable record sizes (matches the
// observed spread). If some (unary-convention, k, F, start) decodes exactly
// N*F codes and lands at the payload end (within a dword of padding) on a large
// fraction of frames, we've found the record grammar. If nothing beats chance,
// the coder isn't plain global-k Rice from a fixed offset.
//
// Usage: cargo run -p dca --release --example xll_x_rice -- <in.dts> [max_mb]

use std::collections::HashMap;
use std::io::Read;

use dca::hd::HdError;
use dca::parser::parse_header;
use dca::{HdDecoder, exss_substream_size};

/// MSB-first bit cursor over a byte slice (DCA bitstream order).
struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
    end: usize,
}
impl<'a> Bits<'a> {
    fn new(d: &'a [u8], start_bit: usize) -> Self {
        Bits { d, pos: start_bit, end: d.len() * 8 }
    }
    #[inline]
    fn bit(&mut self) -> Option<u32> {
        if self.pos >= self.end {
            return None;
        }
        let b = (self.d[self.pos >> 3] >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        Some(b as u32)
    }
    /// Consume one Rice code with parameter k; `conv` = the bit value counted in
    /// the unary prefix (terminator is the opposite). Returns false if it runs
    /// out of bits. We only care about bits consumed (boundary search), not value.
    #[inline]
    fn rice(&mut self, k: u32, conv: u32) -> bool {
        // unary prefix
        loop {
            match self.bit() {
                Some(b) if b == conv => continue,
                Some(_) => break,
                None => return false,
            }
        }
        for _ in 0..k {
            if self.bit().is_none() {
                return false;
            }
        }
        true
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: xll_x_rice <in.dts> [max_mb]");
    let max_mb: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(96);

    let mut f = std::fs::File::open(&path).expect("open");
    let mut bytes = vec![0u8; max_mb * 1024 * 1024];
    let n = f.read(&mut bytes).expect("read");
    bytes.truncate(n);

    // Search: greedily Rice-decode the whole object region; test whether the
    // code count is a clean multiple of N (= F values/object) with little
    // leftover (padding). Start = base*8 + a*N bits (per-object header of `a`
    // bits before the Rice stream). Signal = divisible-rate + F consistency.
    let convs = [0u32, 1u32];
    let ks: Vec<u32> = (0..=14).collect();
    let bases = [26usize, 27, 28];
    let aa = [0usize, 1, 2, 3, 4];
    const TOL: usize = 40;

    let mut consumed: HashMap<(u32, u32, usize, usize), u32> = HashMap::new();
    let mut divis: HashMap<(u32, u32, usize, usize), u32> = HashMap::new();
    let mut fhist: HashMap<(u32, u32, usize, usize), HashMap<usize, u32>> = HashMap::new();
    let mut tested = 0u32;

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
            if (fr.x_present || fr.x_imax) && p.len() > 40 {
                let count = p[22] as usize;
                if (4..=14).contains(&count) {
                    tested += 1;
                    let end_bit = p.len() * 8;
                    for &conv in &convs {
                        for &k in &ks {
                            for &base in &bases {
                                for &a in &aa {
                                    let start = base * 8 + a * count;
                                    if start >= end_bit {
                                        continue;
                                    }
                                    let mut b = Bits::new(p, start);
                                    let mut codes = 0usize;
                                    loop {
                                        let save = b.pos;
                                        if b.rice(k, conv) && b.pos <= end_bit {
                                            codes += 1;
                                        } else {
                                            b.pos = save;
                                            break;
                                        }
                                    }
                                    let leftover = end_bit - b.pos;
                                    let key = (conv, k, base, a);
                                    if leftover < TOL && codes > 0 {
                                        *consumed.entry(key).or_insert(0) += 1;
                                        if codes % count == 0 {
                                            *divis.entry(key).or_insert(0) += 1;
                                            *fhist
                                                .entry(key)
                                                .or_default()
                                                .entry(codes / count)
                                                .or_insert(0) += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        off += core.frame_size + exss_len;
        frames += 1;
    }

    println!("tested {tested} object-bearing frames (count 4..14)\n");
    println!("top combos by divisible-rate (codes % N == 0, leftover < {TOL}b):");
    println!("  conv  k base a   consumed%  divisible%   modal-F (share)");
    let mut v: Vec<((u32, u32, usize, usize), u32)> = divis.iter().map(|(k, &c)| (*k, c)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    for (key, d) in v.into_iter().take(20) {
        let (conv, k, base, a) = key;
        let cons = *consumed.get(&key).unwrap_or(&0);
        let fh = &fhist[&key];
        let (mf, mc) = fh.iter().max_by_key(|(_, c)| **c).unwrap();
        println!(
            "  {conv:>4} {k:>2} {base:>4} {a}   {:>6.1}    {:>6.1}     F={mf} ({:.0}% of div)",
            100.0 * cons as f64 / tested.max(1) as f64,
            100.0 * d as f64 / tested.max(1) as f64,
            100.0 * *mc as f64 / d.max(1) as f64,
        );
    }
    let _ = HdError::Pending;
}
