// SPDX-License-Identifier: Apache-2.0
//
// Undo the fold: from one carrier and its residuals, restore the two or
// three streams that were mixed into it.
//
// The carrier, with its borrowed bits dropped, is the exact sum of the
// folded streams. Each sample, one stream is extrapolated from its own
// recent history while the others are derived from the sum; the residuals
// steer the extrapolation so the derived values land on the originals. Two
// streams alternate sample by sample with a linear extrapolator; three
// streams rotate through a three-phase one. The arithmetic below is the
// integer arithmetic a decoder has to reproduce exactly — the outputs are
// defined by it, not by any nicer formulation.

use crate::stream::{MAX_STREAMS, StreamBlock};

/// Gain codes count tenths of a decibel; the table holds codes 0..244 with
/// the exact powers of two pinned at 6, 12, 18 and 24 dB.
pub const GAIN_CODES: usize = 244;

pub struct GainTable {
    pub linear: [f32; GAIN_CODES],
}

impl GainTable {
    pub fn new() -> Self {
        let step = 0.011_512_926_f32; // ln(10) / 200
        let mut linear = [0f32; GAIN_CODES];
        for (i, g) in linear.iter_mut().enumerate() {
            *g = ((i as f32) * step).exp();
        }
        linear[60] = 2.0;
        linear[120] = 4.0;
        linear[180] = 8.0;
        linear[240] = 16.0;
        Self { linear }
    }

    /// Linear gain for `code + offset`, or `None` past the table.
    pub fn get(&self, code: u8, offset: u8) -> Option<f32> {
        self.linear
            .get(usize::from(code) + usize::from(offset))
            .copied()
    }
}

impl Default for GainTable {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn clamp24(v: i32) -> i32 {
    v.clamp(-0x7f_ffff, 0x7f_ffff)
}

/// Scale a restored stream back to 24-bit: multiply in single precision,
/// truncate, restore the borrowed bits' weight, clamp.
#[inline]
fn scale(v: i32, gain: f32, m: u32) -> i32 {
    let f = (v as f32) * gain;
    // `as i32` truncates toward zero and saturates, as the reference does.
    clamp24((f as i32).wrapping_shl(m))
}

/// Restore the streams of one block. `carrier` holds the block's samples,
/// `residuals` what [`crate::rice::decode_residuals`] produced (one or two
/// per sample), and `out[j]` receives stream `j` for `j < mode`. Each `out`
/// slice must be at least as long as `carrier`.
///
/// Returns `false` when a gain index falls outside the table.
pub fn unmix(
    block: &StreamBlock,
    gains: &GainTable,
    lsb_bits: u8,
    carrier: &[i32],
    residuals: &[i32],
    out: &mut [&mut [i32]; MAX_STREAMS],
) -> bool {
    let n = carrier.len();
    let m = u32::from(lsb_bits);
    let mode = usize::from(block.mode);
    let mut g = [1f32; MAX_STREAMS];
    for j in 0..mode {
        match gains.get(block.gain_codes[j], block.gain_offset) {
            Some(v) => g[j] = v,
            None => return false,
        }
    }
    match mode {
        1 => {
            let mask = -1i32 << m;
            for i in 0..n {
                out[0][i] = clamp24(((carrier[i] & mask) as f32 * g[0]) as i32);
            }
        }
        2 => {
            let (a, rest) = out.split_at_mut(1);
            let (a, b) = (&mut *a[0], &mut *rest[0]);
            let c = |i: usize| carrier[i] >> m;
            if n == 0 {
                return true;
            }
            b[0] = block.seeds[1].wrapping_add(residuals[0]);
            a[0] = c(0).wrapping_sub(b[0]);
            let mut s3 = a[0];
            if n == 1 {
                a[0] = scale(a[0], g[0], m);
                b[0] = scale(b[0], g[1], m);
                return true;
            }
            a[1] = residuals[1].wrapping_add(block.seeds[0]);
            b[1] = c(1).wrapping_sub(a[1]);
            let mut s2 = block.seeds[0];
            let mut s4 = b[1];
            // From here the roles alternate: `ext` is extrapolated, `der`
            // derived from the sum.
            let mut a_is_ext = true;
            for i in 2..n {
                let pred = s2.wrapping_mul(2).wrapping_sub(s3);
                let der = c(i).wrapping_sub(pred);
                if a_is_ext {
                    a[i] = pred;
                    b[i] = der;
                } else {
                    b[i] = pred;
                    a[i] = der;
                }
                s3 = s4;
                s2 = der.wrapping_sub(residuals[i]);
                s4 = pred;
                a_is_ext = !a_is_ext;
            }
            for i in 0..n {
                a[i] = scale(a[i], g[0], m);
                b[i] = scale(b[i], g[1], m);
            }
        }
        3 => {
            let (a, rest) = out.split_at_mut(1);
            let (b, rest) = rest.split_at_mut(1);
            let (a, b, cc) = (&mut *a[0], &mut *b[0], &mut *rest[0]);
            let c = |i: usize| carrier[i] >> m;
            let r = |i: usize, j: usize| residuals[2 * i + j];
            let s = &block.seeds;
            if n == 0 {
                return true;
            }
            // Three warm-up samples seeded from the header.
            b[0] = s[0];
            cc[0] = s[1];
            a[0] = c(0)
                .wrapping_sub(r(0, 1))
                .wrapping_sub(s[1])
                .wrapping_sub(r(0, 0))
                .wrapping_sub(b[0]);
            let mut s7 = cc[0];
            let mut s9 = a[0];
            let mut s8 = 0i32;
            let mut s6 = 0i32;
            if n > 1 {
                cc[1] = s[3];
                a[1] = s[2];
                b[1] = c(1)
                    .wrapping_sub(r(1, 1))
                    .wrapping_sub(r(1, 0))
                    .wrapping_sub(a[1])
                    .wrapping_sub(cc[1]);
                s7 = a[1];
                s8 = s9;
                s9 = b[1];
            }
            if n > 2 {
                let pred = s7.wrapping_mul(4).wrapping_sub(s8.wrapping_mul(3));
                let mut t = s8.wrapping_add(pred.wrapping_mul(3));
                if t < 0 {
                    t = t.wrapping_add(3);
                }
                a[2] = t >> 2;
                b[2] = s[4];
                cc[2] = c(2)
                    .wrapping_sub(r(2, 1))
                    .wrapping_sub(r(2, 0))
                    .wrapping_sub(b[2])
                    .wrapping_sub(a[2]);
                s7 = b[2];
                s8 = s9;
                s9 = cc[2];
                s6 = pred;
            }
            // Roles rotate: `l78` gets the fresh extrapolation, `p16` the
            // previous one, `lres8` is derived from the sum.
            let (mut l70, mut lres8, mut l78) = (1usize, 2usize, 0usize); // B, C, A
            for i in 3..n {
                let p16 = l78;
                l78 = l70;
                let pred = s7.wrapping_mul(4).wrapping_sub(s8.wrapping_mul(3));
                let mut t = s8.wrapping_add(pred.wrapping_mul(3));
                if t < 0 {
                    t = t.wrapping_add(3);
                }
                let derived = c(i)
                    .wrapping_sub(r(i, 1))
                    .wrapping_sub(s6)
                    .wrapping_sub(r(i, 0))
                    .wrapping_sub(t >> 2);
                set(a, b, cc, lres8, i, derived);
                set(a, b, cc, p16, i, s6);
                set(a, b, cc, l78, i, t >> 2);
                s7 = derived;
                s8 = s9;
                s9 = s6;
                s6 = pred;
                l70 = lres8;
                lres8 = p16;
            }
            // Put the residuals back on the two non-derived streams.
            let mut ph = 0usize;
            for i in 0..n {
                let (x, y) = match ph % 3 {
                    1 => (0usize, 2usize),
                    2 => (0, 1),
                    _ => (1, 2),
                };
                add(a, b, cc, x, i, r(i, 0));
                add(a, b, cc, y, i, r(i, 1));
                ph += 1;
            }
            for i in 0..n {
                a[i] = scale(a[i], g[0], m);
                b[i] = scale(b[i], g[1], m);
                cc[i] = scale(cc[i], g[2], m);
            }
        }
        _ => {}
    }
    true
}

#[inline]
fn set(a: &mut [i32], b: &mut [i32], c: &mut [i32], which: usize, i: usize, v: i32) {
    match which {
        0 => a[i] = v,
        1 => b[i] = v,
        _ => c[i] = v,
    }
}

#[inline]
fn add(a: &mut [i32], b: &mut [i32], c: &mut [i32], which: usize, i: usize, v: i32) {
    match which {
        0 => a[i] = a[i].wrapping_add(v),
        1 => b[i] = b[i].wrapping_add(v),
        _ => c[i] = c[i].wrapping_add(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_codes_are_tenths_of_a_decibel_with_exact_octaves() {
        let g = GainTable::new();
        assert_eq!(g.get(0, 0), Some(1.0));
        assert!((g.get(20, 0).unwrap() - 1.258_925).abs() < 1e-5);
        assert_eq!(g.get(60, 0), Some(2.0));
        assert_eq!(g.get(120, 0), Some(4.0));
        assert_eq!(g.get(40, 20), Some(2.0));
        assert_eq!(g.get(243, 0).is_some(), true);
        assert_eq!(g.get(243, 1), None);
    }

    /// Fold two smooth streams the way an encoder would: an honest sum for
    /// the carrier, and residuals that hand the decoder the derived
    /// stream's error so its predictor tracks the truth.
    fn fold_two(a_true: &[i32], b_true: &[i32], m: u32) -> (Vec<i32>, Vec<i32>, [i32; 5]) {
        let n = a_true.len();
        let mut carrier = vec![0i32; n];
        let mut res = vec![0i32; n];
        for i in 0..n {
            carrier[i] = (a_true[i] + b_true[i]) << m;
        }
        let seeds = [a_true[1], b_true[0], 0, 0, 0];
        // Replay the decoder to find the derived values it will see.
        let c = |i: usize| carrier[i] >> m;
        let b0 = seeds[1];
        let mut s3 = c(0) - b0;
        let mut s2 = seeds[0];
        let mut s4 = c(1) - seeds[0];
        let mut a_is_ext = true;
        for i in 2..n {
            let pred = 2 * s2 - s3;
            let der = c(i) - pred;
            let truth = if a_is_ext { b_true[i] } else { a_true[i] };
            res[i] = der - truth;
            s3 = s4;
            s2 = der - res[i];
            s4 = pred;
            a_is_ext = !a_is_ext;
        }
        (carrier, res, seeds)
    }

    #[test]
    fn two_folded_streams_come_back_close_and_sum_exactly() {
        let n = 512;
        let a_true: Vec<i32> = (0..n)
            .map(|i| (30000.0 * (i as f64 * 0.05).sin()) as i32)
            .collect();
        let b_true: Vec<i32> = (0..n)
            .map(|i| (12000.0 * (i as f64 * 0.11 + 1.0).cos()) as i32)
            .collect();
        let m = 3u32;
        let (carrier, res, seeds) = fold_two(&a_true, &b_true, m);
        let block = StreamBlock {
            ids: [0, 9, 0xff],
            mode: 2,
            seeds,
            gain_codes: [0; 3],
            gain_offset: 0,
            config: None,
            k: 0,
            adaptive: false,
            count: 1,
            width: 1,
            codebook_pos: 0,
            rice_pos: 0,
        };
        let mut a = vec![0i32; n];
        let mut b = vec![0i32; n];
        let mut cc = vec![0i32; n];
        let mut out: [&mut [i32]; 3] = [&mut a, &mut b, &mut cc];
        assert!(unmix(
            &block,
            &GainTable::new(),
            m as u8,
            &carrier,
            &res,
            &mut out
        ));
        let mut err = 0f64;
        let mut sig = 0f64;
        for i in 0..n {
            assert_eq!(a[i] + b[i], carrier[i], "sample {i}");
            err += f64::from(a[i] - (a_true[i] << m)).powi(2);
            sig += f64::from(a_true[i] << m).powi(2);
        }
        let rel_db = 10.0 * (err / sig).log10();
        assert!(rel_db < -40.0, "stream A is {rel_db:.1} dB off");
    }

    #[test]
    fn three_streams_sum_back_to_the_carrier() {
        let block = StreamBlock {
            ids: [0, 9, 12],
            mode: 3,
            seeds: [7, -3, 11, 2, -9],
            gain_codes: [0; 3],
            gain_offset: 0,
            config: None,
            k: 0,
            adaptive: false,
            count: 1,
            width: 1,
            codebook_pos: 0,
            rice_pos: 0,
        };
        let m = 4u8;
        let n = 12;
        let carrier: Vec<i32> = (0..n)
            .map(|i| (((i * 611) % 300) as i32 - 150) * 16)
            .collect();
        let residuals: Vec<i32> = (0..2 * n).map(|i| (i % 7) as i32 - 3).collect();
        let mut a = vec![0i32; n];
        let mut b = vec![0i32; n];
        let mut cc = vec![0i32; n];
        let mut out: [&mut [i32]; 3] = [&mut a, &mut b, &mut cc];
        assert!(unmix(
            &block,
            &GainTable::new(),
            m,
            &carrier,
            &residuals,
            &mut out
        ));
        for i in 0..n {
            assert_eq!(a[i] + b[i] + cc[i], carrier[i], "sample {i}");
        }
    }
}
