// SPDX-License-Identifier: Apache-2.0
//
// The fixed-point QMF synthesis of `synth.rs` written with AVX2 intrinsics.
// Same integer arithmetic, value for value: every 64-bit product is taken
// with `vpmuldq`, every rounding shift keeps the low 32 bits the scalar code
// keeps, every clip is the scalar clip. `synth.rs` holds the reference and
// the test that both produce identical samples.

use std::arch::x86_64::*;

use super::synth::{DCT_A, DCT_B, MOD_A, MOD_B, MOD_C, MOD64_A, MOD64_B, MOD64_C};
use super::tables::{
    FIR_32BANDS_NONPERFECT_FIXED, FIR_32BANDS_PERFECT_FIXED, FIR_64BANDS_FIXED, LFE_FIR_64_FIXED,
    LFE_FIR_64_FLOAT,
};

/// The window coefficients of one bank, in the order `window` walks them.
///
/// For half-size `HN` (16 for the 32-band bank, 32 for the 64-band one) the
/// scalar loop accumulates, for each of the 8 history slices `j` and each
/// `m < HN`:
///
/// ```text
/// a[m]          += window[j * 4HN + m]            * hist[j * 4HN + m]
/// b[HN - 1 - m] += window[j * 4HN + 2HN - 1 - m]  * hist[j * 4HN + m]
/// c[m]          += window[j * 4HN + 2HN + m]      * hist[j * 4HN + HN + m]
/// d[HN - 1 - m] += window[j * 4HN + 4HN - 1 - m]  * hist[j * 4HN + HN + m]
/// ```
///
/// A vector holds four of those sums, for `m = 8q + p + 2l` (`l` = lane):
/// the plan stores, for each `q`, then each `j`, the four coefficient
/// vectors (a, b, c, d) of the even `m` (`p = 0`) then of the odd ones, as
/// 64-bit lanes ready to be `vpmuldq` operands.
#[repr(C, align(32))]
struct Plan<const LEN: usize>([i64; LEN]);

const fn window_plan<const LEN: usize>(window: &[i32], hn: usize) -> Plan<LEN> {
    assert!(LEN == 32 * hn && window.len() == 32 * hn);
    let mut plan = [0i64; LEN];
    let mut at = 0;
    let mut q = 0;
    while q < hn / 8 {
        let mut j = 0;
        while j < 8 {
            let base = j * 4 * hn;
            let mut pg = 0;
            while pg < 8 {
                let mut l = 0;
                while l < 4 {
                    let m = 8 * q + pg / 4 + 2 * l;
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

/// `DCT_A` and `DCT_B` by column: `[j][i]`, the weights input `j` carries
/// into the eight outputs.
#[repr(C, align(32))]
struct Columns<const N: usize>([[i64; 8]; N]);

const fn columns<const N: usize>(rows: &[[i32; N]; 8]) -> Columns<N> {
    let mut cols = [[0i64; 8]; N];
    let mut j = 0;
    while j < N {
        let mut i = 0;
        while i < 8 {
            cols[j][i] = rows[i][j] as i64;
            i += 1;
        }
        j += 1;
    }
    Columns(cols)
}

static DCT_A_COLS: Columns<8> = columns(&DCT_A);
static DCT_B_COLS: Columns<7> = columns(&DCT_B);

#[inline]
#[target_feature(enable = "avx2")]
fn clip23(v: __m256i) -> __m256i {
    let lo = _mm256_set1_epi32(-(1 << 23));
    let hi = _mm256_set1_epi32((1 << 23) - 1);
    _mm256_min_epi32(_mm256_max_epi32(v, lo), hi)
}

/// `(a[0], a[2], …, b[6])` and `(a[1], a[3], …, b[7])` of the 16 values
/// `a ++ b`.
#[inline]
#[target_feature(enable = "avx2")]
fn deinterleave(a: __m256i, b: __m256i) -> (__m256i, __m256i) {
    let (a, b) = (_mm256_castsi256_ps(a), _mm256_castsi256_ps(b));
    let even = _mm256_castps_si256(_mm256_shuffle_ps::<0x88>(a, b));
    let odd = _mm256_castps_si256(_mm256_shuffle_ps::<0xDD>(a, b));
    (
        _mm256_permute4x64_epi64::<0xD8>(even),
        _mm256_permute4x64_epi64::<0xD8>(odd),
    )
}

#[inline]
#[target_feature(enable = "avx2")]
fn reverse(v: __m256i) -> __m256i {
    _mm256_permutevar8x32_epi32(v, _mm256_setr_epi32(7, 6, 5, 4, 3, 2, 1, 0))
}

/// `(prev[7], cur[0], …, cur[6])`: `cur` one place later in its sequence.
#[inline]
#[target_feature(enable = "avx2")]
fn delayed(prev: __m256i, cur: __m256i) -> __m256i {
    let shifted = _mm256_permutevar8x32_epi32(cur, _mm256_setr_epi32(0, 0, 1, 2, 3, 4, 5, 6));
    let last = _mm256_permutevar8x32_epi32(prev, _mm256_set1_epi32(7));
    _mm256_blend_epi32::<0x01>(shifted, last)
}

/// The scalar `mul23` on eight values: `(coeff * x + 2^22) >> 23`, low 32 bits.
#[inline]
#[target_feature(enable = "avx2")]
fn mul23(coeff: &[i32], x: __m256i) -> __m256i {
    debug_assert!(coeff.len() >= 8);
    // SAFETY: `coeff` holds at least eight values.
    let c = unsafe { _mm256_loadu_si256(coeff.as_ptr().cast()) };
    let round = _mm256_set1_epi64x(1 << 22);
    let even = _mm256_add_epi64(_mm256_mul_epi32(x, c), round);
    let odd = _mm256_add_epi64(
        _mm256_mul_epi32(_mm256_srli_epi64::<32>(x), _mm256_srli_epi64::<32>(c)),
        round,
    );
    // Bits 23..55 of each product, the odd ones moved to the odd lanes.
    _mm256_blend_epi32::<0xAA>(_mm256_srli_epi64::<23>(even), _mm256_slli_epi64::<9>(odd))
}

/// Eight rounded 64-bit sums (`lo` holds outputs 0..4, `hi` 4..8) as eight
/// 32-bit values: `(sum + 2^22) >> 23`.
#[inline]
#[target_feature(enable = "avx2")]
fn norm23_pack(lo: __m256i, hi: __m256i) -> __m256i {
    let round = _mm256_set1_epi64x(1 << 22);
    let lo = _mm256_srli_epi64::<23>(_mm256_add_epi64(lo, round));
    let hi = _mm256_srli_epi64::<23>(_mm256_add_epi64(hi, round));
    let packed = _mm256_shuffle_ps::<0x88>(_mm256_castsi256_ps(lo), _mm256_castsi256_ps(hi));
    _mm256_permute4x64_epi64::<0xD8>(_mm256_castps_si256(packed))
}

#[inline]
#[target_feature(enable = "avx2")]
fn dct_a(x: __m256i) -> __m256i {
    let mut input = [0i32; 8];
    // SAFETY: `input` holds eight values.
    unsafe { _mm256_storeu_si256(input.as_mut_ptr().cast(), x) };
    let mut lo = _mm256_setzero_si256();
    let mut hi = _mm256_setzero_si256();
    for (col, &v) in DCT_A_COLS.0.iter().zip(&input) {
        let v = _mm256_set1_epi32(v);
        // SAFETY: a column holds eight 64-bit weights.
        let (wl, wh) = unsafe {
            (
                _mm256_loadu_si256(col.as_ptr().cast()),
                _mm256_loadu_si256(col.as_ptr().add(4).cast()),
            )
        };
        lo = _mm256_add_epi64(lo, _mm256_mul_epi32(v, wl));
        hi = _mm256_add_epi64(hi, _mm256_mul_epi32(v, wh));
    }
    norm23_pack(lo, hi)
}

#[inline]
#[target_feature(enable = "avx2")]
fn dct_b(x: __m256i) -> __m256i {
    let mut input = [0i32; 8];
    // SAFETY: `input` holds eight values.
    unsafe { _mm256_storeu_si256(input.as_mut_ptr().cast(), x) };
    // Every output starts from `input[0] << 23`.
    let first = _mm256_mul_epi32(_mm256_set1_epi32(input[0]), _mm256_set1_epi64x(1 << 23));
    let mut lo = first;
    let mut hi = first;
    for (col, &v) in DCT_B_COLS.0.iter().zip(&input[1..]) {
        let v = _mm256_set1_epi32(v);
        // SAFETY: a column holds eight 64-bit weights.
        let (wl, wh) = unsafe {
            (
                _mm256_loadu_si256(col.as_ptr().cast()),
                _mm256_loadu_si256(col.as_ptr().add(4).cast()),
            )
        };
        lo = _mm256_add_epi64(lo, _mm256_mul_epi32(v, wl));
        hi = _mm256_add_epi64(hi, _mm256_mul_epi32(v, wh));
    }
    norm23_pack(lo, hi)
}

/// The scaling the transforms apply to loud input: the vectors shifted down
/// by 2 with rounding when the sum of magnitudes passes 2^22, and the shift
/// to undo at the end.
#[inline]
#[target_feature(enable = "avx2")]
fn prescale<const V: usize>(x: &mut [__m256i; V]) -> __m128i {
    let low = _mm256_set1_epi64x(0xffff_ffff);
    let mut sum = _mm256_setzero_si256();
    for &v in x.iter() {
        let abs = _mm256_abs_epi32(v);
        sum = _mm256_add_epi64(sum, _mm256_and_si256(abs, low));
        sum = _mm256_add_epi64(sum, _mm256_srli_epi64::<32>(abs));
    }
    let sum = _mm_add_epi64(
        _mm256_castsi256_si128(sum),
        _mm256_extracti128_si256::<1>(sum),
    );
    let mag = _mm_cvtsi128_si64(_mm_add_epi64(sum, _mm_unpackhi_epi64(sum, sum)));
    let shift = if mag > 0x40_0000 { 2 } else { 0 };
    let count = _mm_cvtsi32_si128(shift);
    let round = _mm256_set1_epi32(if shift > 0 { 1 << (shift - 1) } else { 0 });
    for v in x.iter_mut() {
        *v = _mm256_sra_epi32(_mm256_add_epi32(*v, round), count);
    }
    count
}

#[inline]
#[target_feature(enable = "avx2")]
fn load<const V: usize>(src: &[i32]) -> [__m256i; V] {
    assert!(src.len() >= 8 * V);
    let mut v = [_mm256_setzero_si256(); V];
    for (k, v) in v.iter_mut().enumerate() {
        // SAFETY: `src` holds at least `8 * V` values.
        *v = unsafe { _mm256_loadu_si256(src.as_ptr().add(8 * k).cast()) };
    }
    v
}

#[inline]
#[target_feature(enable = "avx2")]
fn clip23_all<const V: usize>(mut v: [__m256i; V]) -> [__m256i; V] {
    for v in v.iter_mut() {
        *v = clip23(*v);
    }
    v
}

/// Give back the shift `prescale` took, clipping.
#[inline]
#[target_feature(enable = "avx2")]
fn rescale<const V: usize>(mut v: [__m256i; V], shift: __m128i) -> [__m256i; V] {
    for v in v.iter_mut() {
        *v = clip23(_mm256_sll_epi32(*v, shift));
    }
    v
}

#[inline]
#[target_feature(enable = "avx2")]
fn store<const V: usize>(dst: &mut [i32], v: &[__m256i; V]) {
    assert!(dst.len() >= 8 * V);
    for (k, &v) in v.iter().enumerate() {
        // SAFETY: `dst` holds at least `8 * V` values.
        unsafe { _mm256_storeu_si256(dst.as_mut_ptr().add(8 * k).cast(), v) };
    }
}

/// `mod_a`: 16 values in, 16 out.
#[inline]
#[target_feature(enable = "avx2")]
fn mod_a(lo: __m256i, hi: __m256i) -> (__m256i, __m256i) {
    (
        mul23(&MOD_A[..8], _mm256_add_epi32(lo, hi)),
        mul23(&MOD_A[8..], reverse(_mm256_sub_epi32(lo, hi))),
    )
}

/// `mod_b`: 16 values in, 16 out.
#[inline]
#[target_feature(enable = "avx2")]
fn mod_b(lo: __m256i, hi: __m256i) -> (__m256i, __m256i) {
    let hi = mul23(&MOD_B, hi);
    (_mm256_add_epi32(lo, hi), reverse(_mm256_sub_epi32(lo, hi)))
}

/// `imdct_half_32`.
#[inline]
#[target_feature(enable = "avx2")]
fn imdct_half_32(output: &mut [i32; 32], input: &[i32; 32]) {
    let zero = _mm256_setzero_si256();
    let mut x: [__m256i; 4] = load(input);
    let shift = prescale(&mut x);

    // sum_a / sum_b over 32, then clip.
    let (e0, o0) = deinterleave(x[0], x[1]);
    let (e1, o1) = deinterleave(x[2], x[3]);
    let b = [
        clip23(_mm256_add_epi32(e0, o0)),
        clip23(_mm256_add_epi32(e1, o1)),
        clip23(_mm256_add_epi32(e0, delayed(zero, o0))),
        clip23(_mm256_add_epi32(e1, delayed(o0, o1))),
    ];

    // sum_a / sum_b over the first half, sum_c / sum_d over the second.
    let (e, o) = deinterleave(b[0], b[1]);
    let (e2, o2) = deinterleave(b[2], b[3]);
    let a = [
        clip23(_mm256_add_epi32(e, o)),
        clip23(_mm256_add_epi32(e, delayed(zero, o))),
        clip23(e2),
        clip23(_mm256_add_epi32(o2, delayed(zero, o2))),
    ];

    let b = [
        clip23(dct_a(a[0])),
        clip23(dct_b(a[1])),
        clip23(dct_b(a[2])),
        clip23(dct_b(a[3])),
    ];

    let (a0, a1) = mod_a(b[0], b[1]);
    let (a2, a3) = mod_b(b[2], b[3]);
    let a = [clip23(a0), clip23(a1), clip23(a2), clip23(a3)];

    // mod_c, then the shift taken at the start is given back.
    let b = [
        mul23(&MOD_C[..8], _mm256_add_epi32(a[0], a[2])),
        mul23(&MOD_C[8..16], _mm256_add_epi32(a[1], a[3])),
        mul23(&MOD_C[16..24], reverse(_mm256_sub_epi32(a[1], a[3]))),
        mul23(&MOD_C[24..], reverse(_mm256_sub_epi32(a[0], a[2]))),
    ];
    let b = rescale(b, shift);

    let (r3, r2) = (reverse(b[3]), reverse(b[2]));
    store(
        output,
        &[
            clip23(_mm256_sub_epi32(b[0], r3)),
            clip23(_mm256_sub_epi32(b[1], r2)),
            clip23(_mm256_add_epi32(b[0], r3)),
            clip23(_mm256_add_epi32(b[1], r2)),
        ],
    );
}

/// `imdct_half_64`.
#[inline]
#[target_feature(enable = "avx2")]
fn imdct_half_64(output: &mut [i32; 64], input: &[i32]) {
    let zero = _mm256_setzero_si256();
    // 64 subbands, or the lower 32 alone (the upper ones are then zero).
    let mut x = [zero; 8];
    if input.len() == 32 {
        let low: [__m256i; 4] = load(input);
        x[..4].copy_from_slice(&low);
    } else {
        x = load(input);
    }
    let shift = prescale(&mut x);

    // sum_a / sum_b over 64.
    let (e0, o0) = deinterleave(x[0], x[1]);
    let (e1, o1) = deinterleave(x[2], x[3]);
    let (e2, o2) = deinterleave(x[4], x[5]);
    let (e3, o3) = deinterleave(x[6], x[7]);
    let t = [
        clip23(_mm256_add_epi32(e0, o0)),
        clip23(_mm256_add_epi32(e1, o1)),
        clip23(_mm256_add_epi32(e2, o2)),
        clip23(_mm256_add_epi32(e3, o3)),
        clip23(_mm256_add_epi32(e0, delayed(zero, o0))),
        clip23(_mm256_add_epi32(e1, delayed(o0, o1))),
        clip23(_mm256_add_epi32(e2, delayed(o1, o2))),
        clip23(_mm256_add_epi32(e3, delayed(o2, o3))),
    ];

    // sum_a / sum_b over the first 32, sum_c / sum_d over the last 32.
    let (e0, o0) = deinterleave(t[0], t[1]);
    let (e1, o1) = deinterleave(t[2], t[3]);
    let (e2, o2) = deinterleave(t[4], t[5]);
    let (e3, o3) = deinterleave(t[6], t[7]);
    let t = [
        clip23(_mm256_add_epi32(e0, o0)),
        clip23(_mm256_add_epi32(e1, o1)),
        clip23(_mm256_add_epi32(e0, delayed(zero, o0))),
        clip23(_mm256_add_epi32(e1, delayed(o0, o1))),
        clip23(e2),
        clip23(e3),
        clip23(_mm256_add_epi32(o2, delayed(zero, o2))),
        clip23(_mm256_add_epi32(o3, delayed(o2, o3))),
    ];

    // sum_a / sum_b over the first 16, sum_c / sum_d over each later 16.
    let (e0, o0) = deinterleave(t[0], t[1]);
    let (e1, o1) = deinterleave(t[2], t[3]);
    let (e2, o2) = deinterleave(t[4], t[5]);
    let (e3, o3) = deinterleave(t[6], t[7]);
    let t = [
        clip23(_mm256_add_epi32(e0, o0)),
        clip23(_mm256_add_epi32(e0, delayed(zero, o0))),
        clip23(e1),
        clip23(_mm256_add_epi32(o1, delayed(zero, o1))),
        clip23(e2),
        clip23(_mm256_add_epi32(o2, delayed(zero, o2))),
        clip23(e3),
        clip23(_mm256_add_epi32(o3, delayed(zero, o3))),
    ];

    let t = [
        clip23(dct_a(t[0])),
        clip23(dct_b(t[1])),
        clip23(dct_b(t[2])),
        clip23(dct_b(t[3])),
        clip23(dct_b(t[4])),
        clip23(dct_b(t[5])),
        clip23(dct_b(t[6])),
        clip23(dct_b(t[7])),
    ];

    let (b0, b1) = mod_a(t[0], t[1]);
    let (b2, b3) = mod_b(t[2], t[3]);
    let (b4, b5) = mod_b(t[4], t[5]);
    let (b6, b7) = mod_b(t[6], t[7]);
    let t = clip23_all([b0, b1, b2, b3, b4, b5, b6, b7]);

    // mod64_a over the first 32, mod64_b over the last 32.
    let h6 = mul23(&MOD64_B[..8], t[6]);
    let h7 = mul23(&MOD64_B[8..], t[7]);
    let t = clip23_all([
        mul23(&MOD64_A[..8], _mm256_add_epi32(t[0], t[2])),
        mul23(&MOD64_A[8..16], _mm256_add_epi32(t[1], t[3])),
        mul23(&MOD64_A[16..24], reverse(_mm256_sub_epi32(t[1], t[3]))),
        mul23(&MOD64_A[24..], reverse(_mm256_sub_epi32(t[0], t[2]))),
        _mm256_add_epi32(t[4], h6),
        _mm256_add_epi32(t[5], h7),
        reverse(_mm256_sub_epi32(t[5], h7)),
        reverse(_mm256_sub_epi32(t[4], h6)),
    ]);

    // mod64_c, then the shift taken at the start is given back.
    let mut b = [zero; 8];
    for k in 0..4 {
        b[k] = mul23(&MOD64_C[8 * k..], _mm256_add_epi32(t[k], t[k + 4]));
        b[4 + k] = mul23(
            &MOD64_C[32 + 8 * k..],
            reverse(_mm256_sub_epi32(t[3 - k], t[7 - k])),
        );
    }
    let b = rescale(b, shift);

    let mut out = [zero; 8];
    for k in 0..4 {
        let mirrored = reverse(b[7 - k]);
        out[k] = clip23(_mm256_sub_epi32(b[k], mirrored));
        out[4 + k] = clip23(_mm256_add_epi32(b[k], mirrored));
    }
    store(output, &out);
}

/// The window of `synth_filter` / `synth_filter_64` for half-size `hn`:
/// `hist` is the contiguous run of the delay line (`32 * hn` values),
/// `hist2` the overlap of the previous block (first `2 * hn` values used),
/// `out` receives `2 * hn` samples. `bits` is the rounding shift.
#[inline]
#[target_feature(enable = "avx2")]
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
    let vectors = hn / 8;
    let zero = _mm256_setzero_si256();

    // The overlap is read whole before any of it is replaced.
    let mut init_a = [zero; 4];
    let mut init_b = [zero; 4];
    for q in 0..vectors {
        // SAFETY: both runs lie within the first `2 * hn <= 64` values.
        unsafe {
            init_a[q] = _mm256_loadu_si256(hist2.as_ptr().add(8 * q).cast());
            init_b[q] = reverse(_mm256_loadu_si256(
                hist2.as_ptr().add(2 * hn - 8 - 8 * q).cast(),
            ));
        }
    }

    let scale = _mm256_set1_epi64x(1 << bits);
    let round = _mm256_set1_epi64x(1 << (bits - 1));
    let down = _mm_cvtsi32_si128(bits);
    let up = _mm_cvtsi32_si128(32 - bits);
    let mut w = plan.as_ptr();
    for q in 0..vectors {
        // Sums of the even `m` then of the odd ones: [a, b, c, d] each. The
        // odd values of a load are brought down to the even lanes, which
        // are the ones `vpmuldq` reads.
        let mut even = [
            _mm256_mul_epi32(init_a[q], scale),
            _mm256_mul_epi32(init_b[q], scale),
            zero,
            zero,
        ];
        let mut odd = [
            _mm256_mul_epi32(_mm256_srli_epi64::<32>(init_a[q]), scale),
            _mm256_mul_epi32(_mm256_srli_epi64::<32>(init_b[q]), scale),
            zero,
            zero,
        ];
        for j in 0..8 {
            let at = j * 4 * hn + 8 * q;
            // SAFETY: `at + hn + 8 <= 28 * hn + hn + 8 * vectors <= 32 * hn`,
            // the length of `hist`; `w` walks the plan, 32 values per step,
            // `8 * vectors` steps in all.
            unsafe {
                let lo = _mm256_loadu_si256(hist.as_ptr().add(at).cast());
                let hi = _mm256_loadu_si256(hist.as_ptr().add(at + hn).cast());
                let lo_odd = _mm256_srli_epi64::<32>(lo);
                let hi_odd = _mm256_srli_epi64::<32>(hi);
                let coeff = |k: usize| _mm256_load_si256(w.add(4 * k).cast());
                even[0] = _mm256_add_epi64(even[0], _mm256_mul_epi32(lo, coeff(0)));
                even[1] = _mm256_add_epi64(even[1], _mm256_mul_epi32(lo, coeff(1)));
                even[2] = _mm256_add_epi64(even[2], _mm256_mul_epi32(hi, coeff(2)));
                even[3] = _mm256_add_epi64(even[3], _mm256_mul_epi32(hi, coeff(3)));
                odd[0] = _mm256_add_epi64(odd[0], _mm256_mul_epi32(lo_odd, coeff(4)));
                odd[1] = _mm256_add_epi64(odd[1], _mm256_mul_epi32(lo_odd, coeff(5)));
                odd[2] = _mm256_add_epi64(odd[2], _mm256_mul_epi32(hi_odd, coeff(6)));
                odd[3] = _mm256_add_epi64(odd[3], _mm256_mul_epi32(hi_odd, coeff(7)));
                w = w.add(32);
            }
        }
        // Bits `bits..bits + 32` of each rounded sum: the even values stay
        // in the even lanes, the odd ones move up into the odd lanes.
        let mut merged = [zero; 4];
        for (g, merged) in merged.iter_mut().enumerate() {
            *merged = _mm256_blend_epi32::<0xAA>(
                _mm256_srl_epi64(_mm256_add_epi64(even[g], round), down),
                _mm256_sll_epi64(_mm256_add_epi64(odd[g], round), up),
            );
        }
        let [a, b, c, d] = merged;
        let back = 2 * hn - 8 - 8 * q;
        // SAFETY: `8 * q + 8 <= hn` and `back + 8 <= 2 * hn`, within `out`
        // and within the first `2 * hn <= 64` values of `hist2`.
        unsafe {
            _mm256_storeu_si256(out.as_mut_ptr().add(8 * q).cast(), clip23(a));
            _mm256_storeu_si256(out.as_mut_ptr().add(back).cast(), clip23(reverse(b)));
            _mm256_storeu_si256(hist2.as_mut_ptr().add(8 * q).cast(), c);
            _mm256_storeu_si256(hist2.as_mut_ptr().add(back).cast(), reverse(d));
        }
    }
}

/// Lay subband samples out block by block: `dst[block * N + band]` takes
/// `subs[band][block]` for the 32 bands, the rest of each block of `N` is
/// left as it is.
#[target_feature(enable = "avx2")]
pub(super) fn gather_blocks<const N: usize>(subs: &[&[i32]; 32], dst: &mut [i32]) {
    assert!(N >= 32);
    let blocks = dst.len() / N;
    assert!(subs.iter().all(|s| s.len() >= blocks));
    let mut first = 0;
    // Eight blocks of eight bands at a time: an 8x8 transpose.
    while first + 8 <= blocks {
        for band in (0..32).step_by(8) {
            let mut r = [_mm256_setzero_si256(); 8];
            for (k, r) in r.iter_mut().enumerate() {
                // SAFETY: every band holds at least `blocks >= first + 8` samples.
                *r = unsafe { _mm256_loadu_si256(subs[band + k].as_ptr().add(first).cast()) };
            }
            let t = [
                _mm256_unpacklo_epi32(r[0], r[1]),
                _mm256_unpackhi_epi32(r[0], r[1]),
                _mm256_unpacklo_epi32(r[2], r[3]),
                _mm256_unpackhi_epi32(r[2], r[3]),
                _mm256_unpacklo_epi32(r[4], r[5]),
                _mm256_unpackhi_epi32(r[4], r[5]),
                _mm256_unpacklo_epi32(r[6], r[7]),
                _mm256_unpackhi_epi32(r[6], r[7]),
            ];
            let u = [
                _mm256_unpacklo_epi64(t[0], t[2]),
                _mm256_unpackhi_epi64(t[0], t[2]),
                _mm256_unpacklo_epi64(t[1], t[3]),
                _mm256_unpackhi_epi64(t[1], t[3]),
                _mm256_unpacklo_epi64(t[4], t[6]),
                _mm256_unpackhi_epi64(t[4], t[6]),
                _mm256_unpacklo_epi64(t[5], t[7]),
                _mm256_unpackhi_epi64(t[5], t[7]),
            ];
            for k in 0..4 {
                let near = _mm256_permute2x128_si256::<0x20>(u[k], u[k + 4]);
                let far = _mm256_permute2x128_si256::<0x31>(u[k], u[k + 4]);
                // SAFETY: blocks `first + k` and `first + k + 4` lie within
                // `dst`, and `band + 8 <= 32 <= N`.
                unsafe {
                    _mm256_storeu_si256(dst.as_mut_ptr().add((first + k) * N + band).cast(), near);
                    _mm256_storeu_si256(
                        dst.as_mut_ptr().add((first + k + 4) * N + band).cast(),
                        far,
                    );
                }
            }
        }
        first += 8;
    }
    for block in first..blocks {
        for (band, s) in subs.iter().enumerate() {
            dst[block * N + band] = s[block];
        }
    }
}

/// One channel's frame through a bank of `N` bands (32 or 64):
/// `subs[band][block]` in, `dst[block * N + n]` out, `dst.len() / N` blocks.
///
/// `history` holds the transforms of the channel's last 15 blocks, newest
/// first, and `hist2` the overlap of its last block; `work` is scratch.
///
/// The frame is done in two passes, every transform then every window. A
/// transform depends on nothing but its block, and a window only on
/// transforms and on the overlap the window before it left, so within each
/// pass consecutive blocks overlap in the pipeline instead of each waiting
/// for the one before.
#[inline]
#[target_feature(enable = "avx2")]
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
    gather_blocks::<N>(subs, dst);

    // The frame's transforms, newest first, then the 15 before them: each
    // window reads the 16 blocks from its own on.
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
#[target_feature(enable = "avx2")]
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
#[target_feature(enable = "avx2")]
pub(super) fn bank_64(
    history: &mut [i32; 2048],
    hist2: &mut [i32; 64],
    work: &mut Vec<i32>,
    subs: &[&[i32]; 32],
    dst: &mut [i32],
) {
    bank::<64>(history, hist2, work, &PLAN_64.0, 20, subs, dst);
}

/// The LFE interpolation filter by tap: `[half][k][j]` is the weight sample
/// `k` (counted back from the newest) carries into output `j` of the first
/// or second half of the 64 outputs an LFE sample expands to.
#[repr(C, align(32))]
struct LfeTaps<T>([[[T; 32]; 8]; 2]);

static LFE_TAPS_FIXED: LfeTaps<i64> = {
    let mut taps = [[[0i64; 32]; 8]; 2];
    let mut k = 0;
    while k < 8 {
        let mut j = 0;
        while j < 32 {
            taps[0][k][j] = LFE_FIR_64_FIXED[j * 8 + k] as i64;
            taps[1][k][j] = LFE_FIR_64_FIXED[255 - j * 8 - k] as i64;
            j += 1;
        }
        k += 1;
    }
    LfeTaps(taps)
};

static LFE_TAPS_FLOAT: LfeTaps<f32> = {
    let mut taps = [[[0f32; 32]; 8]; 2];
    let mut k = 0;
    while k < 8 {
        let mut j = 0;
        while j < 32 {
            taps[0][k][j] = LFE_FIR_64_FLOAT[j * 8 + k];
            taps[1][k][j] = LFE_FIR_64_FLOAT[255 - j * 8 - k];
            j += 1;
        }
        k += 1;
    }
    LfeTaps(taps)
};

/// `lfe_fir_fixed`: `lfe` holds 8 samples of history then one sample per 64
/// of `pcm`; each expands to 64 outputs, a weighted sum of it and the 7
/// before it.
#[target_feature(enable = "avx2")]
pub(super) fn lfe_fixed(lfe: &[i32], pcm: &mut [i32]) {
    assert!(pcm.len() % 64 == 0 && lfe.len() >= 8 + pcm.len() / 64);
    for (i, out) in pcm.chunks_exact_mut(64).enumerate() {
        // Sample `k` back from the newest, in every lane.
        let mut past = [_mm256_setzero_si256(); 8];
        for (k, past) in past.iter_mut().enumerate() {
            *past = _mm256_set1_epi32(lfe[8 + i - k]);
        }
        for (half, taps) in LFE_TAPS_FIXED.0.iter().enumerate() {
            // Eight outputs at a time, as two vectors of four 64-bit sums.
            for j in (0..32).step_by(8) {
                let mut low = _mm256_setzero_si256();
                let mut high = _mm256_setzero_si256();
                for (taps, &past) in taps.iter().zip(&past) {
                    // SAFETY: `j + 8 <= 32`, the length of a row of taps.
                    let (wl, wh) = unsafe {
                        (
                            _mm256_load_si256(taps.as_ptr().add(j).cast()),
                            _mm256_loadu_si256(taps.as_ptr().add(j + 4).cast()),
                        )
                    };
                    low = _mm256_add_epi64(low, _mm256_mul_epi32(past, wl));
                    high = _mm256_add_epi64(high, _mm256_mul_epi32(past, wh));
                }
                let at = half * 32 + j;
                // SAFETY: `at + 8 <= 64`, the length of `out`.
                unsafe {
                    _mm256_storeu_si256(
                        out.as_mut_ptr().add(at).cast(),
                        clip23(norm23_pack(low, high)),
                    );
                }
            }
        }
    }
}

/// `lfe_fir_float`, as [`lfe_fixed`] but in single precision: each output
/// is summed tap by tap in the scalar order, so the samples are the same.
#[target_feature(enable = "avx2")]
pub(super) fn lfe_float(lfe: &[i32], pcm: &mut [f32]) {
    assert!(pcm.len() % 64 == 0 && lfe.len() >= 8 + pcm.len() / 64);
    for (i, out) in pcm.chunks_exact_mut(64).enumerate() {
        let mut past = [_mm256_setzero_ps(); 8];
        for (k, past) in past.iter_mut().enumerate() {
            *past = _mm256_set1_ps(lfe[8 + i - k] as f32);
        }
        for (half, taps) in LFE_TAPS_FLOAT.0.iter().enumerate() {
            for j in (0..32).step_by(8) {
                let mut sum = _mm256_setzero_ps();
                for (taps, &past) in taps.iter().zip(&past) {
                    // SAFETY: `j + 8 <= 32`, the length of a row of taps.
                    let weights = unsafe { _mm256_load_ps(taps.as_ptr().add(j)) };
                    sum = _mm256_add_ps(sum, _mm256_mul_ps(weights, past));
                }
                // SAFETY: `half * 32 + j + 8 <= 64`, the length of `out`.
                unsafe { _mm256_storeu_ps(out.as_mut_ptr().add(half * 32 + j), sum) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both transforms against the scalar ones, on quiet input (no
    /// prescale), loud input (prescale) and input past the 24-bit range,
    /// with every band of the 64-band transform driven.
    #[test]
    fn transforms_match_the_scalar_ones() {
        if !crate::cpu::has_avx2() {
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
            // SAFETY: AVX2 was detected above.
            unsafe { imdct_half_64(&mut out, &input) };
            assert_eq!(out, expected, "64-point, round {round}");

            let input: [i32; 32] = input[..32].try_into().unwrap();
            let mut expected = [0i32; 32];
            let mut out = [0i32; 32];
            super::super::synth::imdct_half_32(&mut expected, &input);
            // SAFETY: AVX2 was detected above.
            unsafe { imdct_half_32(&mut out, &input) };
            assert_eq!(out, expected, "32-point, round {round}");
        }
    }
}
