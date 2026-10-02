// SPDX-License-Identifier: Apache-2.0
//
// XLL inverse adaptive prediction over four channels at once, with AVX2.
//
// A channel's prediction is serial: each sample needs the one before it. The
// scalar loop of `xll.rs` therefore runs one multiplier-bound chain per
// channel, one channel after the other. Here four channels advance together,
// one per 64-bit lane, so each step's `vpmuldq` carries four products. The
// arithmetic is the scalar one, value for value; `xll.rs` holds the reference
// and the test that both produce identical samples.

use std::arch::x86_64::*;

/// Mirrors `DCA_XLL_PRED_ORDER_MAX`.
const ORDER_MAX: usize = 16;

/// One sample of each of four channels, in the low half of each 64-bit lane.
#[repr(C, align(32))]
#[derive(Clone, Copy, Default)]
pub(super) struct Lanes([i64; 4]);

/// Buffers kept between frames.
#[derive(Default)]
pub(super) struct Scratch {
    /// The channels of a group, sample by sample.
    lanes: Vec<Lanes>,
    /// What an unused lane reads and writes.
    spare: Vec<i32>,
}

/// Two to four channels predicted together: the first `count` entries of
/// each array are a channel's samples, prediction coefficients and order.
pub(super) struct Group<'a> {
    pub(super) samples: [&'a mut [i32]; 4],
    pub(super) coeff: [&'a [i32; ORDER_MAX]; 4],
    pub(super) order: [usize; 4],
    pub(super) count: usize,
}

/// `inverse_adaptive_prediction` on the channels of a group: all of the same
/// length, each of an order from 1 to 16 that its samples outnumber.
///
/// `prelude` is the scalar prediction: it runs each channel up to the highest
/// order of the group, from where every channel has a full history.
#[target_feature(enable = "avx2")]
pub(super) fn predict_group(
    group: Group<'_>,
    scratch: &mut Scratch,
    prelude: fn(&mut [i32], &[i32; ORDER_MAX], usize),
) {
    let Group {
        mut samples,
        coeff,
        order,
        count,
    } = group;
    assert!((2..=4).contains(&count));
    let len = samples[0].len();
    let highest = order[..count].iter().copied().max().unwrap();
    assert!(samples[..count].iter().all(|s| s.len() == len));
    assert!(order[..count].iter().all(|&order| order >= 1));
    assert!(highest <= ORDER_MAX && highest < len);

    for lane in 0..count {
        prelude(&mut samples[lane][..highest], coeff[lane], order[lane]);
    }

    // Lag `l + 1` weighs `coeff[l]`; a channel has none past its order, nor
    // has an unused lane.
    let mut taps = [Lanes::default(); ORDER_MAX];
    for lane in 0..count {
        for (tap, &coeff) in taps.iter_mut().zip(&coeff[lane][..order[lane]]) {
            tap.0[lane] = coeff as i64;
        }
    }

    scratch.lanes.resize(len, Lanes::default());
    scratch.spare.clear();
    scratch.spare.resize(len, 0);
    let mut sources = [scratch.spare.as_ptr(); 4];
    for (source, samples) in sources.iter_mut().zip(&samples[..count]) {
        *source = samples.as_ptr();
    }
    // SAFETY: the four sources hold `len` samples each, as does `lanes`.
    unsafe { interleave(sources, &mut scratch.lanes) };

    let lanes = &mut scratch.lanes[..];
    match highest {
        1 => predict_1(lanes, &taps),
        2 => predict_2(lanes, &taps),
        3 => predict_3(lanes, &taps),
        4 => predict_4(lanes, &taps),
        5 => predict_5(lanes, &taps),
        6 => predict_6(lanes, &taps),
        7 => predict_7(lanes, &taps),
        8 => predict_8(lanes, &taps),
        9 => predict_9(lanes, &taps),
        10 => predict_10(lanes, &taps),
        11 => predict_11(lanes, &taps),
        12 => predict_12(lanes, &taps),
        13 => predict_13(lanes, &taps),
        14 => predict_14(lanes, &taps),
        15 => predict_15(lanes, &taps),
        16 => predict_16(lanes, &taps),
        _ => unreachable!("asserted above"),
    }

    let mut sinks = [scratch.spare.as_mut_ptr(); 4];
    for (sink, samples) in sinks.iter_mut().zip(&mut samples[..count]) {
        *sink = samples.as_mut_ptr();
    }
    // SAFETY: the sinks hold `len` samples each; those of the channels are
    // distinct buffers, and the unused lanes all write the same zeros to
    // `spare`.
    unsafe { deinterleave(&scratch.lanes, sinks) };
}

/// `a + b` in 64-bit lanes, as an addition the optimizer cannot move.
///
/// A step's prediction sums one product per lag, and the next step waits on
/// it through the product of the newest sample alone. Left to itself the
/// compiler adds that product first and the others after it, since a sum of
/// 64-bit integers may be taken in any order: every one of those additions
/// then stands between one step and the next. Taken here, it is the last.
#[inline]
#[target_feature(enable = "avx2")]
fn add_last(a: __m256i, b: __m256i) -> __m256i {
    let sum: __m256i;
    // SAFETY: a register-only instruction of AVX2, which this function has.
    unsafe {
        std::arch::asm!(
            "vpaddq {sum}, {a}, {b}",
            a = in(ymm_reg) a,
            b = in(ymm_reg) b,
            sum = lateout(ymm_reg) sum,
            options(pure, nomem, nostack, preserves_flags),
        );
    }
    sum
}

/// The prediction of one order: from sample `$order` on, subtract from each
/// sample the clipped prediction made from the `$order` before it, in all
/// four lanes. `$lag` lists the lags past the second, oldest first: written
/// out one by one, since a loop over them is not unrolled.
macro_rules! predict {
    ($name:ident, $order:literal, [$($lag:literal)*]) => {
        #[target_feature(enable = "avx2")]
        fn $name(lanes: &mut [Lanes], taps: &[Lanes; ORDER_MAX]) {
            const M: usize = $order;
            let len = lanes.len();
            if len <= M {
                return;
            }
            let low = _mm256_set1_epi32(-(1 << 23));
            let high = _mm256_set1_epi32((1 << 23) - 1);
            let round = _mm256_set1_epi64x(1 << 15);
            // `weights[lag - 1]` weighs the sample `lag` before.
            let mut weights = [_mm256_setzero_si256(); ORDER_MAX];
            for (weight, tap) in weights.iter_mut().zip(taps) {
                // SAFETY: a `Lanes` is 32 aligned bytes.
                *weight = unsafe { _mm256_load_si256(tap.0.as_ptr().cast()) };
            }
            let at = lanes.as_mut_ptr().cast::<__m256i>();
            // SAFETY: every `at.add(i)` below has `i < len`: `t - lag` with
            // `lag <= M <= t`, and `t < len`.
            unsafe {
                // The two samples before the current one stay in registers:
                // read back from memory they would add a store and a load
                // to what each step waits on from the ones before. The
                // product of the newest is added last (`add_last`) for the
                // same reason.
                let mut newest = _mm256_load_si256(at.add(M - 1));
                let mut second = _mm256_load_si256(at.add(M.saturating_sub(2)));
                for t in M..len {
                    let mut sum = round;
                    $(
                        sum = _mm256_add_epi64(
                            sum,
                            _mm256_mul_epi32(weights[$lag - 1], _mm256_load_si256(at.add(t - $lag))),
                        );
                    )*
                    if M >= 2 {
                        sum = _mm256_add_epi64(sum, _mm256_mul_epi32(weights[1], second));
                    }
                    let sum = add_last(sum, _mm256_mul_epi32(weights[0], newest));
                    // `norm16` keeps bits 16..48 of the sum: after the shift
                    // they are the low half of each lane, the only half ever
                    // read.
                    let prediction = _mm256_min_epi32(
                        _mm256_max_epi32(_mm256_srli_epi64::<16>(sum), low),
                        high,
                    );
                    second = newest;
                    newest = _mm256_sub_epi32(_mm256_load_si256(at.add(t)), prediction);
                    _mm256_store_si256(at.add(t), newest);
                }
            }
        }
    };
}

predict!(predict_1, 1, []);
predict!(predict_2, 2, []);
predict!(predict_3, 3, [3]);
predict!(predict_4, 4, [4 3]);
predict!(predict_5, 5, [5 4 3]);
predict!(predict_6, 6, [6 5 4 3]);
predict!(predict_7, 7, [7 6 5 4 3]);
predict!(predict_8, 8, [8 7 6 5 4 3]);
predict!(predict_9, 9, [9 8 7 6 5 4 3]);
predict!(predict_10, 10, [10 9 8 7 6 5 4 3]);
predict!(predict_11, 11, [11 10 9 8 7 6 5 4 3]);
predict!(predict_12, 12, [12 11 10 9 8 7 6 5 4 3]);
predict!(predict_13, 13, [13 12 11 10 9 8 7 6 5 4 3]);
predict!(predict_14, 14, [14 13 12 11 10 9 8 7 6 5 4 3]);
predict!(predict_15, 15, [15 14 13 12 11 10 9 8 7 6 5 4 3]);
predict!(predict_16, 16, [16 15 14 13 12 11 10 9 8 7 6 5 4 3]);

/// `lanes[t]` takes sample `t` of each source.
///
/// # Safety
/// Every source holds `lanes.len()` samples.
#[target_feature(enable = "avx2")]
unsafe fn interleave(sources: [*const i32; 4], lanes: &mut [Lanes]) {
    let len = lanes.len();
    let at = lanes.as_mut_ptr().cast::<__m256i>();
    let mut t = 0;
    // SAFETY: `t + 4 <= len` samples of every source and of `lanes`.
    unsafe {
        while t + 4 <= len {
            let a = _mm_loadu_si128(sources[0].add(t).cast());
            let b = _mm_loadu_si128(sources[1].add(t).cast());
            let c = _mm_loadu_si128(sources[2].add(t).cast());
            let d = _mm_loadu_si128(sources[3].add(t).cast());
            let ab_low = _mm_unpacklo_epi32(a, b);
            let ab_high = _mm_unpackhi_epi32(a, b);
            let cd_low = _mm_unpacklo_epi32(c, d);
            let cd_high = _mm_unpackhi_epi32(c, d);
            let rows = [
                _mm_unpacklo_epi64(ab_low, cd_low),
                _mm_unpackhi_epi64(ab_low, cd_low),
                _mm_unpacklo_epi64(ab_high, cd_high),
                _mm_unpackhi_epi64(ab_high, cd_high),
            ];
            for (i, row) in rows.into_iter().enumerate() {
                _mm256_store_si256(at.add(t + i), _mm256_cvtepi32_epi64(row));
            }
            t += 4;
        }
        while t < len {
            for (lane, source) in sources.iter().enumerate() {
                lanes[t].0[lane] = *source.add(t) as i64;
            }
            t += 1;
        }
    }
}

/// Sample `t` of each sink takes its lane of `lanes[t]`.
///
/// # Safety
/// Every sink holds `lanes.len()` samples; sinks that are the same buffer
/// are written the same values.
#[target_feature(enable = "avx2")]
unsafe fn deinterleave(lanes: &[Lanes], sinks: [*mut i32; 4]) {
    let len = lanes.len();
    let at = lanes.as_ptr().cast::<__m256i>();
    let low_halves = _mm256_setr_epi32(0, 2, 4, 6, 0, 2, 4, 6);
    let mut t = 0;
    // SAFETY: `t + 4 <= len` samples of `lanes` and of every sink.
    unsafe {
        while t + 4 <= len {
            let mut rows = [_mm_setzero_si128(); 4];
            for (i, row) in rows.iter_mut().enumerate() {
                let lanes = _mm256_load_si256(at.add(t + i));
                *row = _mm256_castsi256_si128(_mm256_permutevar8x32_epi32(lanes, low_halves));
            }
            let ab_low = _mm_unpacklo_epi32(rows[0], rows[1]);
            let ab_high = _mm_unpacklo_epi32(rows[2], rows[3]);
            let cd_low = _mm_unpackhi_epi32(rows[0], rows[1]);
            let cd_high = _mm_unpackhi_epi32(rows[2], rows[3]);
            _mm_storeu_si128(sinks[0].add(t).cast(), _mm_unpacklo_epi64(ab_low, ab_high));
            _mm_storeu_si128(sinks[1].add(t).cast(), _mm_unpackhi_epi64(ab_low, ab_high));
            _mm_storeu_si128(sinks[2].add(t).cast(), _mm_unpacklo_epi64(cd_low, cd_high));
            _mm_storeu_si128(sinks[3].add(t).cast(), _mm_unpackhi_epi64(cd_low, cd_high));
            t += 4;
        }
        while t < len {
            for (lane, sink) in sinks.iter().enumerate() {
                *sink.add(t) = lanes[t].0[lane] as i32;
            }
            t += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::xll::inverse_adaptive_prediction;
    use super::*;

    /// Groups of two to four channels of every order against the scalar
    /// prediction of each channel, on frames whose length is and is not a
    /// multiple of four, with residuals and coefficients large enough for
    /// the prediction to clip.
    #[test]
    fn groups_match_the_scalar_prediction() {
        if !crate::cpu::has_avx2() {
            return;
        }
        let mut state = 99u32;
        let mut next = |bits: u32| {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            ((state >> 4) & ((1 << bits) - 1)) as i32 - (1 << (bits - 1))
        };
        let mut scratch = Scratch::default();
        for round in 0..600 {
            let count = 2 + round % 3;
            let len = [17, 64, 255, 512, 1030][round % 5];
            let sample_bits = [8, 16, 24, 28][(round / 5) % 4];
            let coeff_bits = [12, 17, 22][(round / 20) % 3];
            let mut orders = [0usize; 4];
            let mut coeffs = [[0i32; ORDER_MAX]; 4];
            let mut channels: [Vec<i32>; 4] = Default::default();
            for lane in 0..count {
                orders[lane] = 1 + (next(8).unsigned_abs() as usize + round + lane * 5) % 16;
                for c in &mut coeffs[lane][..orders[lane]] {
                    *c = next(coeff_bits);
                }
                channels[lane] = (0..len).map(|_| next(sample_bits)).collect();
            }
            let mut expected = channels.clone();
            for lane in 0..count {
                inverse_adaptive_prediction(&mut expected[lane], &coeffs[lane], orders[lane]);
            }
            let mut group = Group {
                samples: Default::default(),
                coeff: [&coeffs[0]; 4],
                order: orders,
                count,
            };
            for (lane, channel) in channels.iter_mut().enumerate().take(count) {
                group.samples[lane] = channel;
                group.coeff[lane] = &coeffs[lane];
            }
            // SAFETY: AVX2 was detected above.
            unsafe { predict_group(group, &mut scratch, inverse_adaptive_prediction) };
            assert!(channels == expected, "round {round}");
        }
    }
}
