// SPDX-License-Identifier: Apache-2.0
//
// The fixed-point QMF synthesis of `synth.rs` written with AVX-512
// intrinsics: `synth_avx2.rs` on vectors of sixteen values instead of eight.
// Same integer arithmetic, value for value: every 64-bit product is taken
// with `vpmuldq`, every rounding shift keeps the low 32 bits the scalar code
// keeps, every clip is the scalar clip. `synth.rs` holds the reference and
// the test that every build produces identical samples.
//
// Besides the width, what AVX-512 brings here is its two-source and
// arbitrary permutes: the de-interleaves, the one-place delays and the
// reversals of `synth_avx2.rs` are one instruction each, and the sums over
// eight values that feed the eight-point transforms are formed in one vector
// with its masked permutes.

use std::arch::x86_64::*;

use super::synth::{DCT_A, DCT_B, MOD_A, MOD_B, MOD_C, MOD64_A, MOD64_B, MOD64_C};
use super::tables::{FIR_32BANDS_NONPERFECT_FIXED, FIR_32BANDS_PERFECT_FIXED, FIR_64BANDS_FIXED};

/// The window coefficients of one bank, in the order `window` walks them:
/// the plan of `synth_avx2.rs` for vectors of sixteen, `m = 16q + p + 2l`
/// (`l` = lane) instead of `8q + p + 2l`.
#[repr(C, align(64))]
struct Plan<const LEN: usize>([i64; LEN]);

const fn window_plan<const LEN: usize>(window: &[i32], hn: usize) -> Plan<LEN> {
    assert!(LEN == 32 * hn && window.len() == 32 * hn);
    let mut plan = [0i64; LEN];
    let mut at = 0;
    let mut q = 0;
    while q < hn / 16 {
        let mut j = 0;
        while j < 8 {
            let base = j * 4 * hn;
            let mut pg = 0;
            while pg < 8 {
                let mut l = 0;
                while l < 8 {
                    let m = 16 * q + pg / 4 + 2 * l;
                    let index = match pg % 4 {
                        0 => base + m,
                        1 => base + 2 * hn - 1 - m,
                        2 => base + 2 * hn + m,
                        _ => base + 4 * hn - 1 - m,
                    };
                    plan[at] = window[index] as i64;
                    at += 1;
                    l += 1;
                }
                pg += 1;
            }
            j += 1;
        }
        q += 1;
    }
    Plan(plan)
}

static PLAN_32_PERFECT: Plan<512> = window_plan(&FIR_32BANDS_PERFECT_FIXED, 16);
static PLAN_32_NONPERFECT: Plan<512> = window_plan(&FIR_32BANDS_NONPERFECT_FIXED, 16);
static PLAN_64: Plan<1024> = window_plan(&FIR_64BANDS_FIXED, 32);

/// The weights of an eight-point transform by column: `[j][i]`, what input
/// `j` carries into the eight outputs, one vector of 64-bit lanes per input.
#[repr(C, align(64))]
struct Columns([[i64; 8]; 8]);

/// `rows` by column, behind a column of `first` when there is room for one.
const fn columns<const N: usize>(rows: &[[i32; N]; 8], first: i64) -> Columns {
    assert!(N == 8 || N == 7);
    let mut cols = [[0i64; 8]; 8];
    let skip = 8 - N;
    if skip == 1 {
        cols[0] = [first; 8];
    }
    let mut j = 0;
    while j < N {
        let mut i = 0;
        while i < 8 {
            cols[skip + j][i] = rows[i][j] as i64;
            i += 1;
        }
        j += 1;
    }
    Columns(cols)
}

static DCT_A_COLS: Columns = columns(&DCT_A, 0);
/// `DCT_B` behind the weight `2^23` every output gives the first input.
static DCT_B_COLS: Columns = columns(&DCT_B, 1 << 23);

/// `MOD_B` over sixteen values: the first eight pass through `mul23`
/// unchanged, `(x * 2^23 + 2^22) >> 23` being `x`.
const MOD_B_16: [i32; 16] = {
    let mut m = [1 << 23; 16];
    let mut i = 0;
    while i < 8 {
        m[8 + i] = MOD_B[i];
        i += 1;
    }
    m
};

#[inline]
#[target_feature(enable = "avx2,avx512f")]
fn clip23(v: __m512i) -> __m512i {
    let lo = _mm512_set1_epi32(-(1 << 23));
    let hi = _mm512_set1_epi32((1 << 23) - 1);
    _mm512_min_epi32(_mm512_max_epi32(v, lo), hi)
}

/// The even then the odd values of 32: `(a[0], a[2], …, b[14])` and
/// `(a[1], a[3], …, b[15])` of `a ++ b`.
#[inline]
#[target_feature(enable = "avx2,avx512f")]
fn deinterleave(a: __m512i, b: __m512i) -> (__m512i, __m512i) {
    let even = _mm512_setr_epi32(0, 2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 22, 24, 26, 28, 30);
    let odd = _mm512_setr_epi32(1, 3, 5, 7, 9, 11, 13, 15, 17, 19, 21, 23, 25, 27, 29, 31);
    (
        _mm512_permutex2var_epi32(a, even, b),
        _mm512_permutex2var_epi32(a, odd, b),
    )
}

#[inline]
#[target_feature(enable = "avx2,avx512f")]
fn reverse(v: __m512i) -> __m512i {
    _mm512_permutexvar_epi32(
        _mm512_setr_epi32(15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0),
        v,
    )
}

/// `(prev[15], cur[0], …, cur[14])`: `cur` one place later in its sequence.
#[inline]
#[target_feature(enable = "avx2,avx512f")]
fn delayed(prev: __m512i, cur: __m512i) -> __m512i {
    _mm512_alignr_epi32::<15>(cur, prev)
}

/// `sum_a` then `sum_b` of sixteen values `x`, clipped: the eight
/// `x[2i] + x[2i + 1]` then the eight `x[2i] + x[2i - 1]` (`x[0]` alone
/// first).
#[inline]
#[target_feature(enable = "avx2,avx512f")]
fn sums_ab(x: __m512i) -> __m512i {
    let even = _mm512_permutexvar_epi32(
        _mm512_setr_epi32(0, 2, 4, 6, 8, 10, 12, 14, 0, 2, 4, 6, 8, 10, 12, 14),
        x,
    );
    let odd = _mm512_maskz_permutexvar_epi32(
        0xFEFF,
        _mm512_setr_epi32(1, 3, 5, 7, 9, 11, 13, 15, 0, 1, 3, 5, 7, 9, 11, 13),
        x,
    );
    clip23(_mm512_add_epi32(even, odd))
}

/// `sum_c` then `sum_d` of sixteen values `x`, clipped: the eight `x[2i]`
/// then the eight `x[2i - 1] + x[2i + 1]` (`x[1]` alone first).
#[inline]
#[target_feature(enable = "avx2,avx512f")]
fn sums_cd(x: __m512i) -> __m512i {
    let both = _mm512_permutexvar_epi32(
        _mm512_setr_epi32(0, 2, 4, 6, 8, 10, 12, 14, 1, 3, 5, 7, 9, 11, 13, 15),
        x,
    );
    let before = _mm512_maskz_permutexvar_epi32(
        0xFE00,
        _mm512_setr_epi32(0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 3, 5, 7, 9, 11, 13),
        x,
    );
    clip23(_mm512_add_epi32(both, before))
}

/// The scalar `mul23` on sixteen values: `(coeff * x + 2^22) >> 23`, low
/// 32 bits.
#[inline]
#[target_feature(enable = "avx2,avx512f")]
fn mul23(coeff: &[i32], x: __m512i) -> __m512i {
    debug_assert!(coeff.len() >= 16);
    // SAFETY: `coeff` holds at least sixteen values.
    let c = unsafe { _mm512_loadu_si512(coeff.as_ptr().cast()) };
    let round = _mm512_set1_epi64(1 << 22);
    let even = _mm512_add_epi64(_mm512_mul_epi32(x, c), round);
    let odd = _mm512_add_epi64(
        _mm512_mul_epi32(_mm512_srli_epi64::<32>(x), _mm512_srli_epi64::<32>(c)),
        round,
    );
    // Bits 23..55 of each product, the odd ones moved to the odd lanes.
    _mm512_mask_blend_epi32(
        0xAAAA,
        _mm512_srli_epi64::<23>(even),
        _mm512_slli_epi64::<9>(odd),
    )
}

/// One eight-point transform: output `i` is `(sum_j cols[j][i] * input[j]
/// + 2^22) >> 23`, in the low half of 64-bit lane `i`.
#[inline]
#[target_feature(enable = "avx2,avx512f")]
fn dct(cols: &Columns, input: &[i32]) -> __m512i {
    debug_assert!(input.len() >= 8);
    let mut acc = _mm512_set1_epi64(1 << 22);
    for (col, &v) in cols.0.iter().zip(input) {
        // SAFETY: a column holds eight 64-bit weights.
        let w = unsafe { _mm512_load_si512(col.as_ptr().cast()) };
        acc = _mm512_add_epi64(acc, _mm512_mul_epi32(_mm512_set1_epi32(v), w));
    }
    _mm512_srli_epi64::<23>(acc)
}

/// The low halves of the 64-bit lanes of `a` then of `b`: two transforms'
/// outputs as sixteen values.
#[inline]
#[target_feature(enable = "avx2,avx512f")]
fn pack(a: __m512i, b: __m512i) -> __m512i {
    _mm512_permutex2var_epi32(
        a,
        _mm512_setr_epi32(0, 2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 22, 24, 26, 28, 30),
        b,
    )
}

/// The butterfly of `mod_a` and `mod_b` on sixteen values: the eight
/// `t[i] + t[8 + i]` then the eight `t[7 - i] - t[15 - i]`.
#[inline]
#[target_feature(enable = "avx2,avx512f")]
fn butterfly(t: __m512i) -> __m512i {
    // `v + w` in the low half, `v - w` in the high one.
    let v = _mm512_permutexvar_epi32(
        _mm512_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7, 7, 6, 5, 4, 3, 2, 1, 0),
        t,
    );
    let w = _mm512_permutexvar_epi32(
        _mm512_setr_epi32(8, 9, 10, 11, 12, 13, 14, 15, 15, 14, 13, 12, 11, 10, 9, 8),
        t,
    );
    _mm512_mask_sub_epi32(_mm512_add_epi32(v, w), 0xFF00, v, w)
}

/// `mod_a`: 16 values in, 16 out.
#[inline]
#[target_feature(enable = "avx2,avx512f")]
fn mod_a(t: __m512i) -> __m512i {
    mul23(&MOD_A, butterfly(t))
}

/// `mod_b`: 16 values in, 16 out.
#[inline]
#[target_feature(enable = "avx2,avx512f")]
fn mod_b(t: __m512i) -> __m512i {
    butterfly(mul23(&MOD_B_16, t))
}

/// The scaling the transforms apply to loud input: the vectors shifted down
/// by 2 with rounding when the sum of magnitudes passes 2^22, and the shift
/// to undo at the end.
///
/// Each magnitude is capped at 2^23 before the sum, which then fits 32
/// bits: a value past the cap passes 2^22 by itself, so whether the sum
/// passes it is unchanged.
#[inline]
#[target_feature(enable = "avx2,avx512f")]
fn prescale<const V: usize>(x: &mut [__m512i; V]) -> __m128i {
    let cap = _mm512_set1_epi32(1 << 23);
    let mut sum = _mm512_setzero_si512();
    for &v in x.iter() {
        sum = _mm512_add_epi32(sum, _mm512_min_epu32(_mm512_abs_epi32(v), cap));
    }
    let mag = _mm512_reduce_add_epi32(sum);
    let shift = if mag > 0x40_0000 { 2 } else { 0 };
    let count = _mm_cvtsi32_si128(shift);
    let round = _mm512_set1_epi32(if shift > 0 { 1 << (shift - 1) } else { 0 });
    for v in x.iter_mut() {
        *v = _mm512_sra_epi32(_mm512_add_epi32(*v, round), count);
    }
    count
}

#[inline]
#[target_feature(enable = "avx2,avx512f")]
fn load<const V: usize>(src: &[i32]) -> [__m512i; V] {
    assert!(src.len() >= 16 * V);
    let mut v = [_mm512_setzero_si512(); V];
    for (k, v) in v.iter_mut().enumerate() {
        // SAFETY: `src` holds at least `16 * V` values.
        *v = unsafe { _mm512_loadu_si512(src.as_ptr().add(16 * k).cast()) };
    }
    v
}

#[inline]
#[target_feature(enable = "avx2,avx512f")]
fn clip23_all<const V: usize>(mut v: [__m512i; V]) -> [__m512i; V] {
    for v in v.iter_mut() {
        *v = clip23(*v);
    }
    v
}

/// Give back the shift `prescale` took, clipping.
#[inline]
#[target_feature(enable = "avx2,avx512f")]
fn rescale<const V: usize>(mut v: [__m512i; V], shift: __m128i) -> [__m512i; V] {
    for v in v.iter_mut() {
        *v = clip23(_mm512_sll_epi32(*v, shift));
    }
    v
}

#[inline]
#[target_feature(enable = "avx2,avx512f")]
fn store<const V: usize>(dst: &mut [i32], v: &[__m512i; V]) {
    assert!(dst.len() >= 16 * V);
    for (k, &v) in v.iter().enumerate() {
        // SAFETY: `dst` holds at least `16 * V` values.
        unsafe { _mm512_storeu_si512(dst.as_mut_ptr().add(16 * k).cast(), v) };
    }
}

/// `imdct_half_32`.
#[inline]
#[target_feature(enable = "avx2,avx512f")]
fn imdct_half_32(output: &mut [i32; 32], input: &[i32; 32]) {
    let zero = _mm512_setzero_si512();
    let mut x: [__m512i; 2] = load(input);
    let shift = prescale(&mut x);

    // sum_a / sum_b over 32, then clip.
    let (e, o) = deinterleave(x[0], x[1]);
    let b = [
        clip23(_mm512_add_epi32(e, o)),
        clip23(_mm512_add_epi32(e, delayed(zero, o))),
    ];

    // sum_a / sum_b over the first half, sum_c / sum_d over the second:
    // the four inputs of the eight-point transforms.
    let mut a = [0i32; 32];
    store(&mut a, &[sums_ab(b[0]), sums_cd(b[1])]);

    let b = [
        clip23(pack(dct(&DCT_A_COLS, &a[..8]), dct(&DCT_B_COLS, &a[8..]))),
        clip23(pack(dct(&DCT_B_COLS, &a[16..]), dct(&DCT_B_COLS, &a[24..]))),
    ];

    let a = [clip23(mod_a(b[0])), clip23(mod_b(b[1]))];

    // mod_c, then the shift taken at the start is given back.
    let b = rescale(
        [
            mul23(&MOD_C[..16], _mm512_add_epi32(a[0], a[1])),
            mul23(&MOD_C[16..], reverse(_mm512_sub_epi32(a[0], a[1]))),
        ],
        shift,
    );

    let r = reverse(b[1]);
    store(
        output,
        &[
            clip23(_mm512_sub_epi32(b[0], r)),
            clip23(_mm512_add_epi32(b[0], r)),
        ],
    );
}

/// `imdct_half_64`.
#[inline]
#[target_feature(enable = "avx2,avx512f")]
fn imdct_half_64(output: &mut [i32; 64], input: &[i32]) {
    let zero = _mm512_setzero_si512();
    // 64 subbands, or the lower 32 alone (the upper ones are then zero).
    let mut x = [zero; 4];
    if input.len() == 32 {
        let low: [__m512i; 2] = load(input);
        x[..2].copy_from_slice(&low);
    } else {
        x = load(input);
    }
    let shift = prescale(&mut x);

    // sum_a / sum_b over 64.
    let (e0, o0) = deinterleave(x[0], x[1]);
    let (e1, o1) = deinterleave(x[2], x[3]);
    let t = [
        clip23(_mm512_add_epi32(e0, o0)),
        clip23(_mm512_add_epi32(e1, o1)),
        clip23(_mm512_add_epi32(e0, delayed(zero, o0))),
        clip23(_mm512_add_epi32(e1, delayed(o0, o1))),
    ];

    // sum_a / sum_b over the first 32, sum_c / sum_d over the last 32.
    let (e0, o0) = deinterleave(t[0], t[1]);
    let (e2, o2) = deinterleave(t[2], t[3]);
    let t = [
        clip23(_mm512_add_epi32(e0, o0)),
        clip23(_mm512_add_epi32(e0, delayed(zero, o0))),
        clip23(e2),
        clip23(_mm512_add_epi32(o2, delayed(zero, o2))),
    ];

    // sum_a / sum_b over the first 16, sum_c / sum_d over each later 16:
    // the eight inputs of the eight-point transforms.
    let mut a = [0i32; 64];
    store(
        &mut a,
        &[sums_ab(t[0]), sums_cd(t[1]), sums_cd(t[2]), sums_cd(t[3])],
    );

    let t = [
        clip23(pack(dct(&DCT_A_COLS, &a[..8]), dct(&DCT_B_COLS, &a[8..]))),
        clip23(pack(dct(&DCT_B_COLS, &a[16..]), dct(&DCT_B_COLS, &a[24..]))),
        clip23(pack(dct(&DCT_B_COLS, &a[32..]), dct(&DCT_B_COLS, &a[40..]))),
        clip23(pack(dct(&DCT_B_COLS, &a[48..]), dct(&DCT_B_COLS, &a[56..]))),
    ];

    let t = [
        clip23(mod_a(t[0])),
        clip23(mod_b(t[1])),
        clip23(mod_b(t[2])),
        clip23(mod_b(t[3])),
    ];

    // mod64_a over the first 32, mod64_b over the last 32.
    let h3 = mul23(&MOD64_B, t[3]);
    let t = clip23_all([
        mul23(&MOD64_A[..16], _mm512_add_epi32(t[0], t[1])),
        mul23(&MOD64_A[16..], reverse(_mm512_sub_epi32(t[0], t[1]))),
        _mm512_add_epi32(t[2], h3),
        reverse(_mm512_sub_epi32(t[2], h3)),
    ]);

    // mod64_c, then the shift taken at the start is given back.
    let b = rescale(
        [
            mul23(&MOD64_C[..16], _mm512_add_epi32(t[0], t[2])),
            mul23(&MOD64_C[16..32], _mm512_add_epi32(t[1], t[3])),
            mul23(&MOD64_C[32..48], reverse(_mm512_sub_epi32(t[1], t[3]))),
            mul23(&MOD64_C[48..], reverse(_mm512_sub_epi32(t[0], t[2]))),
        ],
        shift,
    );

    let (r3, r2) = (reverse(b[3]), reverse(b[2]));
    store(
        output,
        &[
            clip23(_mm512_sub_epi32(b[0], r3)),
            clip23(_mm512_sub_epi32(b[1], r2)),
            clip23(_mm512_add_epi32(b[0], r3)),
            clip23(_mm512_add_epi32(b[1], r2)),
        ],
    );
}

/// The window of `synth_filter` / `synth_filter_64` for half-size `hn`:
/// `hist` is the contiguous run of the delay line (`32 * hn` values),
/// `hist2` the overlap of the previous block (first `2 * hn` values used),
/// `out` receives `2 * hn` samples. `bits` is the rounding shift.
#[inline]
#[target_feature(enable = "avx2,avx512f")]
fn window(
    plan: &[i64],
    hist: &[i32],
    hist2: &mut [i32; 64],
    out: &mut [i32],
    hn: usize,
    bits: i32,
) {
    assert!(hn == 16 || hn == 32);
    assert!(plan.len() == 32 * hn && hist.len() == 32 * hn && out.len() == 2 * hn);
    let vectors = hn / 16;
    let zero = _mm512_setzero_si512();

    // The overlap is read whole before any of it is replaced.
    let mut init_a = [zero; 2];
    let mut init_b = [zero; 2];
    for q in 0..vectors {
        // SAFETY: both runs lie within the first `2 * hn <= 64` values.
        unsafe {
            init_a[q] = _mm512_loadu_si512(hist2.as_ptr().add(16 * q).cast());
            init_b[q] = reverse(_mm512_loadu_si512(
                hist2.as_ptr().add(2 * hn - 16 - 16 * q).cast(),
            ));
        }
    }

    let scale = _mm512_set1_epi64(1 << bits);
    let round = _mm512_set1_epi64(1 << (bits - 1));
    let down = _mm_cvtsi32_si128(bits);
    let up = _mm_cvtsi32_si128(32 - bits);
    let mut w = plan.as_ptr();
    for q in 0..vectors {
        // Sums of the even `m` then of the odd ones: [a, b, c, d] each. The
        // odd values of a load are brought down to the even lanes, which
        // are the ones `vpmuldq` reads.
        let mut even = [
            _mm512_mul_epi32(init_a[q], scale),
            _mm512_mul_epi32(init_b[q], scale),
            zero,
            zero,
        ];
        let mut odd = [
            _mm512_mul_epi32(_mm512_srli_epi64::<32>(init_a[q]), scale),
            _mm512_mul_epi32(_mm512_srli_epi64::<32>(init_b[q]), scale),
            zero,
            zero,
        ];
        for j in 0..8 {
            let at = j * 4 * hn + 16 * q;
            // SAFETY: `at + hn + 16 <= 28 * hn + hn + 16 * vectors <= 32 * hn`,
            // the length of `hist`; `w` walks the plan, 64 values per step,
            // `8 * vectors` steps in all.
            unsafe {
                let lo = _mm512_loadu_si512(hist.as_ptr().add(at).cast());
                let hi = _mm512_loadu_si512(hist.as_ptr().add(at + hn).cast());
                let lo_odd = _mm512_srli_epi64::<32>(lo);
                let hi_odd = _mm512_srli_epi64::<32>(hi);
                let coeff = |k: usize| _mm512_load_si512(w.add(8 * k).cast());
                even[0] = _mm512_add_epi64(even[0], _mm512_mul_epi32(lo, coeff(0)));
                even[1] = _mm512_add_epi64(even[1], _mm512_mul_epi32(lo, coeff(1)));
                even[2] = _mm512_add_epi64(even[2], _mm512_mul_epi32(hi, coeff(2)));
                even[3] = _mm512_add_epi64(even[3], _mm512_mul_epi32(hi, coeff(3)));
                odd[0] = _mm512_add_epi64(odd[0], _mm512_mul_epi32(lo_odd, coeff(4)));
                odd[1] = _mm512_add_epi64(odd[1], _mm512_mul_epi32(lo_odd, coeff(5)));
                odd[2] = _mm512_add_epi64(odd[2], _mm512_mul_epi32(hi_odd, coeff(6)));
                odd[3] = _mm512_add_epi64(odd[3], _mm512_mul_epi32(hi_odd, coeff(7)));
                w = w.add(64);
            }
        }
        // Bits `bits..bits + 32` of each rounded sum: the even values stay
        // in the even lanes, the odd ones move up into the odd lanes.
        let mut merged = [zero; 4];
        for (g, merged) in merged.iter_mut().enumerate() {
            *merged = _mm512_mask_blend_epi32(
                0xAAAA,
                _mm512_srl_epi64(_mm512_add_epi64(even[g], round), down),
                _mm512_sll_epi64(_mm512_add_epi64(odd[g], round), up),
            );
        }
        let [a, b, c, d] = merged;
        let back = 2 * hn - 16 - 16 * q;
        // SAFETY: `16 * q + 16 <= hn` and `back + 16 <= 2 * hn`, within
        // `out` and within the first `2 * hn <= 64` values of `hist2`.
        unsafe {
            _mm512_storeu_si512(out.as_mut_ptr().add(16 * q).cast(), clip23(a));
            _mm512_storeu_si512(out.as_mut_ptr().add(back).cast(), clip23(reverse(b)));
            _mm512_storeu_si512(hist2.as_mut_ptr().add(16 * q).cast(), c);
            _mm512_storeu_si512(hist2.as_mut_ptr().add(back).cast(), reverse(d));
        }
    }
}

/// One channel's frame through a bank of `N` bands (32 or 64), as
/// `synth_avx2::bank`: `subs[band][block]` in, `dst[block * N + n]` out,
/// `dst.len() / N` blocks; `history` holds the transforms of the channel's
/// last 15 blocks, newest first, `hist2` the overlap of its last block,
/// `work` is scratch. Every transform first, then every window.
#[inline]
#[target_feature(enable = "avx2,avx512f")]
fn bank<const N: usize>(
    history: &mut [i32; 2048],
    hist2: &mut [i32; 64],
    work: &mut Vec<i32>,
    plan: &[i64],
    bits: i32,
    subs: &[&[i32]; 32],
    dst: &mut [i32],
) {
    assert!((N == 32 || N == 64) && dst.len() % N == 0);
    let blocks = dst.len() / N;
    let kept = 15 * N;
    super::synth_avx2::gather_blocks::<N>(subs, dst);

    work.resize((blocks + 15) * N, 0);
    work[blocks * N..].copy_from_slice(&history[..kept]);
    for (input, output) in dst
        .chunks_exact(N)
        .zip(work.chunks_exact_mut(N).take(blocks).rev())
    {
        if N == 32 {
            imdct_half_32(output.try_into().unwrap(), input.try_into().unwrap());
        } else {
            imdct_half_64(output.try_into().unwrap(), &input[..32]);
        }
    }
    for (n, out) in dst.chunks_exact_mut(N).enumerate() {
        let at = (blocks - 1 - n) * N;
        window(plan, &work[at..at + 16 * N], hist2, out, N / 2, bits);
    }
    history[..kept].copy_from_slice(&work[..kept]);
}

/// The 32-band bank (`synth_filter_fixed` block after block).
#[target_feature(enable = "avx2,avx512f")]
pub(super) fn bank_32(
    history: &mut [i32; 2048],
    hist2: &mut [i32; 64],
    work: &mut Vec<i32>,
    perfect: bool,
    subs: &[&[i32]; 32],
    dst: &mut [i32],
) {
    let plan = if perfect {
        &PLAN_32_PERFECT
    } else {
        &PLAN_32_NONPERFECT
    };
    bank::<32>(history, hist2, work, &plan.0, 21, subs, dst);
}

/// The 64-band bank (`synth_filter_fixed_64` block after block), driven by
/// the 32 base subbands: the upper 32 are zero.
#[target_feature(enable = "avx2,avx512f")]
pub(super) fn bank_64(
    history: &mut [i32; 2048],
    hist2: &mut [i32; 64],
    work: &mut Vec<i32>,
    subs: &[&[i32]; 32],
    dst: &mut [i32],
) {
    bank::<64>(history, hist2, work, &PLAN_64.0, 20, subs, dst);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both transforms against the scalar ones, on quiet input (no
    /// prescale), loud input (prescale) and input past the 24-bit range,
    /// with every band of the 64-band transform driven.
    #[test]
    fn transforms_match_the_scalar_ones() {
        if !crate::cpu::has_avx512() {
            return;
        }
        let mut state = 4242u32;
        let mut next = |bits: u32| {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            ((state >> 4) & ((1 << bits) - 1)) as i32 - (1 << (bits - 1))
        };
        for round in 0..500 {
            let bits = [10, 16, 21, 24, 26][round % 5];
            let input: [i32; 64] = std::array::from_fn(|_| next(bits));
            let mut expected = [0i32; 64];
            let mut out = [0i32; 64];
            super::super::synth::imdct_half_64(&mut expected, &input);
            // SAFETY: AVX-512 was detected above.
            unsafe { imdct_half_64(&mut out, &input) };
            assert_eq!(out, expected, "64-point, round {round}");

            let input: [i32; 32] = input[..32].try_into().unwrap();
            let mut expected = [0i32; 32];
            let mut out = [0i32; 32];
            super::super::synth::imdct_half_32(&mut expected, &input);
            // SAFETY: AVX-512 was detected above.
            unsafe { imdct_half_32(&mut out, &input) };
            assert_eq!(out, expected, "32-point, round {round}");
        }
    }
}
