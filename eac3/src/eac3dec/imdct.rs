// SPDX-License-Identifier: Apache-2.0

use std::f32::consts::PI;
use std::sync::{Arc, OnceLock};

use rustfft::{Fft, FftPlanner, num_complex::Complex32};

#[derive(Debug, Clone)]
pub(crate) struct ImdctState {
    /// The second half of the previous transform, before its window: the 128
    /// values the 256 samples it spans are mirrored from.
    delay: [f32; 128],
    intermediate_512: [Complex32; 128],
    intermediate_256_a: [Complex32; 64],
    intermediate_256_b: [Complex32; 64],
    /// Working space for the FFTs. `Fft::process` allocates its own on every
    /// call when the plan needs any, which was once per channel per block.
    scratch: Vec<Complex32>,
}

impl ImdctState {
    pub(crate) fn new() -> Self {
        Self {
            delay: [0.0; 128],
            intermediate_512: [Complex32::new(0.0, 0.0); 128],
            intermediate_256_a: [Complex32::new(0.0, 0.0); 64],
            intermediate_256_b: [Complex32::new(0.0, 0.0); 64],
            scratch: vec![Complex32::new(0.0, 0.0); imdct_fft_cache().scratch_len],
        }
    }

    pub(crate) fn apply(&mut self, coeffs: &[f32; 256], block_switch: bool, output: &mut [f32]) {
        debug_assert_eq!(output.len(), 256);
        if block_switch {
            self.apply_256(coeffs, output);
        } else {
            self.apply_512(coeffs, output);
        }
    }

    fn apply_512(&mut self, coeffs: &[f32; 256], output: &mut [f32]) {
        let Ok(output) = <&mut [f32; 256]>::try_from(output) else {
            return;
        };
        let x = &tables().long;
        kernel::pre_rotate_512(coeffs, x, &mut self.intermediate_512);
        imdct_fft_cache()
            .ifft_512
            .process_with_scratch(&mut self.intermediate_512, &mut self.scratch);
        kernel::fold_512(&self.intermediate_512, x, &mut self.delay, output);
    }

    fn apply_256(&mut self, coeffs: &[f32; 256], output: &mut [f32]) {
        let Ok(output) = <&mut [f32; 256]>::try_from(output) else {
            return;
        };
        self.prepare_256_intermediates(coeffs);

        let fft = imdct_fft_cache();
        fft.ifft_256
            .process_with_scratch(&mut self.intermediate_256_a, &mut self.scratch);
        fft.ifft_256
            .process_with_scratch(&mut self.intermediate_256_b, &mut self.scratch);
        kernel::fold_256(
            &self.intermediate_256_a,
            &self.intermediate_256_b,
            &tables().short,
            &mut self.delay,
            output,
        );
    }

    /// Pre-rotation for the two 128-coefficient short transforms.
    ///
    /// Each transform sees only its own half of the spectrum: the first takes
    /// the even coefficients `even[j] = coeffs[2 * j]`, the second the odd
    /// `odd[j] = coeffs[2 * j + 1]`. The rotation pairs `input[2 * index]`
    /// with `input[127 - 2 * index]`, exactly as `apply_512` pairs
    /// `coeffs[2 * index]` with `coeffs[255 - 2 * index]` over all 256. Both
    /// halves of every pair therefore stride by 4 in the flat array - the
    /// long-block stride of 2 would feed each transform three quarters of the
    /// wrong spectrum.
    fn prepare_256_intermediates(&mut self, coeffs: &[f32; 256]) {
        let x = x256();
        for (index, slot) in self.intermediate_256_a.iter_mut().enumerate() {
            *slot = Complex32::new(coeffs[254 - 4 * index], coeffs[4 * index]) * x[index];
        }
        // https://github.com/FFmpeg/FFmpeg/blob/415b466d41ac81856abc76d7a9341132b0f668b0/libavcodec/ac3dec.c#L587
        for (index, slot) in self.intermediate_256_b.iter_mut().enumerate() {
            *slot = Complex32::new(coeffs[255 - 4 * index], coeffs[4 * index + 1]) * x[index];
        }
    }
}

struct ImdctFftCache {
    ifft_512: Arc<dyn Fft<f32>>,
    ifft_256: Arc<dyn Fft<f32>>,
    scratch_len: usize,
}

fn imdct_fft_cache() -> &'static ImdctFftCache {
    static CACHE: OnceLock<ImdctFftCache> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut planner = FftPlanner::<f32>::new();
        let ifft_512 = planner.plan_fft_inverse(128);
        let ifft_256 = planner.plan_fft_inverse(64);
        let scratch_len = ifft_512
            .get_inplace_scratch_len()
            .max(ifft_256.get_inplace_scratch_len());
        ImdctFftCache {
            ifft_512,
            ifft_256,
            scratch_len,
        }
    })
}

/// The rotations around the FFT and the overlap-add, four bins at a time.
///
/// The post-rotation goes straight to the samples. A transform's 512 samples
/// are an odd and an even mirror of 128 values each: `first[255 - n] ==
/// -first[n]` for the half that overlaps the previous block, `second[255 - n]
/// == second[n]` for the half kept for the next one. With `w` the window, the
/// block's samples are `2 * (first[n] * w[n] + delay[n] * w[255 - n])` at `n`
/// and, by the mirrors, `2 * (delay[n] * w[n] - first[n] * w[255 - n])` at
/// `255 - n`; `delay` then takes `second`.
///
/// Every sample is the same products and the same sum as when the transform's
/// 512 samples were windowed first and overlapped after.
#[cfg(target_arch = "x86_64")]
use sse as kernel;

#[cfg(not(target_arch = "x86_64"))]
use scalar as kernel;

#[cfg(target_arch = "x86_64")]
mod sse {
    use std::arch::x86_64::*;

    use super::{Complex32, Rotation, WINDOW, WINDOW_BACKWARDS};

    /// Bin `k` pairs the coefficient `2k` with `255 - 2k`.
    pub(super) fn pre_rotate_512(
        coeffs: &[f32; 256],
        x: &Rotation<128, 64>,
        z: &mut [Complex32; 128],
    ) {
        let coeffs = coeffs.as_ptr();
        let z = z.as_mut_ptr().cast::<f32>();
        for k in (0..128).step_by(4) {
            // SAFETY: `2k + 8 <= 256` and `248 - 2k >= 0` bound the reads of
            // `coeffs`, `k + 4 <= 128` those of `x` and the writes of `z`,
            // whose `Complex32` is two `f32` in a row.
            unsafe {
                let rising_0 = _mm_loadu_ps(coeffs.add(2 * k));
                let rising_1 = _mm_loadu_ps(coeffs.add(2 * k + 4));
                let falling_0 = _mm_loadu_ps(coeffs.add(248 - 2 * k));
                let falling_1 = _mm_loadu_ps(coeffs.add(252 - 2 * k));
                let im = _mm_shuffle_ps::<0b10_00_10_00>(rising_0, rising_1);
                let re = _mm_shuffle_ps::<0b01_11_01_11>(falling_1, falling_0);
                let x_re = _mm_loadu_ps(x.re.as_ptr().add(k));
                let x_im = _mm_loadu_ps(x.im.as_ptr().add(k));
                let out_re = _mm_sub_ps(_mm_mul_ps(re, x_re), _mm_mul_ps(im, x_im));
                let out_im = _mm_add_ps(_mm_mul_ps(re, x_im), _mm_mul_ps(im, x_re));
                _mm_storeu_ps(z.add(2 * k), _mm_unpacklo_ps(out_re, out_im));
                _mm_storeu_ps(z.add(2 * k + 4), _mm_unpackhi_ps(out_re, out_im));
            }
        }
    }

    /// Four bins from `at` on as their real and their imaginary parts.
    ///
    /// SAFETY: `at + 4` bins must be readable from `z`.
    #[inline(always)]
    unsafe fn split(z: *const f32, at: usize) -> (__m128, __m128) {
        unsafe {
            let low = _mm_loadu_ps(z.add(2 * at));
            let high = _mm_loadu_ps(z.add(2 * at + 4));
            (
                _mm_shuffle_ps::<0b10_00_10_00>(low, high),
                _mm_shuffle_ps::<0b11_01_11_01>(low, high),
            )
        }
    }

    /// [`split`], last bin first.
    ///
    /// SAFETY: `at + 4` bins must be readable from `z`.
    #[inline(always)]
    unsafe fn split_backwards(z: *const f32, at: usize) -> (__m128, __m128) {
        unsafe {
            let low = _mm_loadu_ps(z.add(2 * at));
            let high = _mm_loadu_ps(z.add(2 * at + 4));
            (
                _mm_shuffle_ps::<0b00_10_00_10>(high, low),
                _mm_shuffle_ps::<0b01_11_01_11>(high, low),
            )
        }
    }

    /// Window four values of `first` against the delay, write the samples at
    /// `at` and mirrored from `255 - at`, and keep `second`.
    ///
    /// SAFETY: `at + 4 <= 128`.
    #[inline(always)]
    unsafe fn overlap_add(
        delay: *mut f32,
        output: *mut f32,
        first: __m128,
        second: __m128,
        at: usize,
    ) {
        unsafe {
            let delayed = _mm_loadu_ps(delay.add(at));
            let rising = _mm_loadu_ps(WINDOW.as_ptr().add(at));
            let falling = _mm_loadu_ps(WINDOW_BACKWARDS.as_ptr().add(at));
            let head = _mm_add_ps(_mm_mul_ps(first, rising), _mm_mul_ps(delayed, falling));
            let tail = _mm_sub_ps(_mm_mul_ps(delayed, rising), _mm_mul_ps(first, falling));
            _mm_storeu_ps(output.add(at), _mm_add_ps(head, head));
            let tail = _mm_add_ps(tail, tail);
            _mm_storeu_ps(
                output.add(252 - at),
                _mm_shuffle_ps::<0b00_01_10_11>(tail, tail),
            );
            _mm_storeu_ps(delay.add(at), second);
        }
    }

    /// The even samples come from bins 64.. and the odd ones from bins ..64
    /// read backwards.
    pub(super) fn fold_512(
        z: &[Complex32; 128],
        x: &Rotation<128, 64>,
        delay: &mut [f32; 128],
        output: &mut [f32; 256],
    ) {
        let z = z.as_ptr().cast::<f32>();
        let delay = delay.as_mut_ptr();
        let output = output.as_mut_ptr();
        // SAFETY: with `i + 4 <= 64`, the bins read are below 128, the
        // factors below their 128 and 64, and `overlap_add` gets `2i + 8 <=
        // 128`.
        unsafe {
            let sign = _mm_set1_ps(-0.0);
            for i in (0..64).step_by(4) {
                let (re, im) = split(z, 64 + i);
                let x_re = _mm_loadu_ps(x.re.as_ptr().add(64 + i));
                let x_im = _mm_loadu_ps(x.im.as_ptr().add(64 + i));
                let high_re = _mm_sub_ps(_mm_mul_ps(re, x_re), _mm_mul_ps(im, x_im));
                let high_im = _mm_add_ps(_mm_mul_ps(re, x_im), _mm_mul_ps(im, x_re));
                let (re, im) = split_backwards(z, 60 - i);
                let x_re = _mm_loadu_ps(x.re_backwards.as_ptr().add(i));
                let x_im = _mm_loadu_ps(x.im_backwards.as_ptr().add(i));
                let low_re = _mm_sub_ps(_mm_mul_ps(re, x_re), _mm_mul_ps(im, x_im));
                let low_im = _mm_add_ps(_mm_mul_ps(re, x_im), _mm_mul_ps(im, x_re));
                let first_even = _mm_xor_ps(high_im, sign);
                let second_even = _mm_xor_ps(high_re, sign);
                overlap_add(
                    delay,
                    output,
                    _mm_unpacklo_ps(first_even, low_re),
                    _mm_unpacklo_ps(second_even, low_im),
                    2 * i,
                );
                overlap_add(
                    delay,
                    output,
                    _mm_unpackhi_ps(first_even, low_re),
                    _mm_unpackhi_ps(second_even, low_im),
                    2 * i + 4,
                );
            }
        }
    }

    /// The first transform gives the half that overlaps the previous block,
    /// the second the half kept for the next. Only one part of each rotated
    /// bin is a sample.
    pub(super) fn fold_256(
        a: &[Complex32; 64],
        b: &[Complex32; 64],
        x: &Rotation<64, 64>,
        delay: &mut [f32; 128],
        output: &mut [f32; 256],
    ) {
        let a = a.as_ptr().cast::<f32>();
        let b = b.as_ptr().cast::<f32>();
        let delay = delay.as_mut_ptr();
        let output = output.as_mut_ptr();
        // SAFETY: with `i + 4 <= 64`, the bins and the factors read are
        // below 64, and `overlap_add` gets `2i + 8 <= 128`.
        unsafe {
            let sign = _mm_set1_ps(-0.0);
            for i in (0..64).step_by(4) {
                let x_re = _mm_loadu_ps(x.re.as_ptr().add(i));
                let x_im = _mm_loadu_ps(x.im.as_ptr().add(i));
                let x_re_back = _mm_loadu_ps(x.re_backwards.as_ptr().add(i));
                let x_im_back = _mm_loadu_ps(x.im_backwards.as_ptr().add(i));
                let (re, im) = split(a, i);
                let a_im = _mm_add_ps(_mm_mul_ps(re, x_im), _mm_mul_ps(im, x_re));
                let (re, im) = split_backwards(a, 60 - i);
                let a_re = _mm_sub_ps(_mm_mul_ps(re, x_re_back), _mm_mul_ps(im, x_im_back));
                let (re, im) = split(b, i);
                let b_re = _mm_sub_ps(_mm_mul_ps(re, x_re), _mm_mul_ps(im, x_im));
                let (re, im) = split_backwards(b, 60 - i);
                let b_im = _mm_add_ps(_mm_mul_ps(re, x_im_back), _mm_mul_ps(im, x_re_back));
                let first_even = _mm_xor_ps(a_im, sign);
                let second_even = _mm_xor_ps(b_re, sign);
                overlap_add(
                    delay,
                    output,
                    _mm_unpacklo_ps(first_even, a_re),
                    _mm_unpacklo_ps(second_even, b_im),
                    2 * i,
                );
                overlap_add(
                    delay,
                    output,
                    _mm_unpackhi_ps(first_even, a_re),
                    _mm_unpackhi_ps(second_even, b_im),
                    2 * i + 4,
                );
            }
        }
    }
}

/// The same arithmetic one bin at a time, where there is no vector version;
/// what the tests hold the vector version to.
#[cfg(any(test, not(target_arch = "x86_64")))]
mod scalar {
    use super::{Complex32, Rotation, WINDOW, WINDOW_BACKWARDS};

    pub(super) fn pre_rotate_512(
        coeffs: &[f32; 256],
        x: &Rotation<128, 64>,
        z: &mut [Complex32; 128],
    ) {
        for (k, slot) in z.iter_mut().enumerate() {
            *slot = Complex32::new(coeffs[255 - 2 * k], coeffs[2 * k])
                * Complex32::new(x.re[k], x.im[k]);
        }
    }

    fn overlap_add(
        delay: &mut [f32; 128],
        output: &mut [f32; 256],
        first: f32,
        second: f32,
        at: usize,
    ) {
        let delayed = delay[at];
        let (rising, falling) = (WINDOW[at], WINDOW_BACKWARDS[at]);
        output[at] = 2.0 * (first * rising + delayed * falling);
        output[255 - at] = 2.0 * (delayed * rising - first * falling);
        delay[at] = second;
    }

    pub(super) fn fold_512(
        z: &[Complex32; 128],
        x: &Rotation<128, 64>,
        delay: &mut [f32; 128],
        output: &mut [f32; 256],
    ) {
        for i in 0..64 {
            let high = z[64 + i] * Complex32::new(x.re[64 + i], x.im[64 + i]);
            let low = z[63 - i] * Complex32::new(x.re_backwards[i], x.im_backwards[i]);
            overlap_add(delay, output, -high.im, -high.re, 2 * i);
            overlap_add(delay, output, low.re, low.im, 2 * i + 1);
        }
    }

    pub(super) fn fold_256(
        a: &[Complex32; 64],
        b: &[Complex32; 64],
        x: &Rotation<64, 64>,
        delay: &mut [f32; 128],
        output: &mut [f32; 256],
    ) {
        for i in 0..64 {
            let forward = Complex32::new(x.re[i], x.im[i]);
            let backward = Complex32::new(x.re_backwards[i], x.im_backwards[i]);
            overlap_add(
                delay,
                output,
                -(a[i] * forward).im,
                -(b[i] * forward).re,
                2 * i,
            );
            overlap_add(
                delay,
                output,
                (a[63 - i] * backward).re,
                (b[63 - i] * backward).im,
                2 * i + 1,
            );
        }
    }
}

/// The rotation factors of one transform length, parts apart, and those of
/// its lower half read backwards (`re_backwards[i] == re[HALF - 1 - i]`).
struct Rotation<const N: usize, const HALF: usize> {
    re: [f32; N],
    im: [f32; N],
    re_backwards: [f32; HALF],
    im_backwards: [f32; HALF],
}

impl<const N: usize, const HALF: usize> Rotation<N, HALF> {
    fn new(factors: &[Complex32; N]) -> Self {
        Self {
            re: std::array::from_fn(|index| factors[index].re),
            im: std::array::from_fn(|index| factors[index].im),
            re_backwards: std::array::from_fn(|index| factors[HALF - 1 - index].re),
            im_backwards: std::array::from_fn(|index| factors[HALF - 1 - index].im),
        }
    }
}

struct Tables {
    long: Rotation<128, 64>,
    /// The short transforms mirror their whole length, not a half.
    short: Rotation<64, 64>,
}

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| Tables {
        long: Rotation::new(x512()),
        short: Rotation::new(x256()),
    })
}

/// The window read backwards: `WINDOW_BACKWARDS[n] == WINDOW[255 - n]`.
const WINDOW_BACKWARDS: [f32; 256] = {
    let mut result = [0.0; 256];
    let mut index = 0;
    while index < 256 {
        result[index] = WINDOW[255 - index];
        index += 1;
    }
    result
};

fn x512() -> &'static [Complex32; 128] {
    static X512: OnceLock<[Complex32; 128]> = OnceLock::new();
    X512.get_or_init(create_coefficients::<128>)
}

fn x256() -> &'static [Complex32; 64] {
    static X256: OnceLock<[Complex32; 64]> = OnceLock::new();
    X256.get_or_init(create_coefficients::<64>)
}

fn create_coefficients<const N: usize>() -> [Complex32; N] {
    let mut result = [Complex32::new(0.0, 0.0); N];
    let mul = 2.0 * PI / ((N as f32) * 32.0);
    let mut index = 0usize;
    while index < N {
        let phi = mul * (8 * index + 1) as f32;
        result[index] = Complex32::new(-phi.cos(), -phi.sin());
        index += 1;
    }
    result
}

#[allow(clippy::approx_constant)]
const WINDOW: [f32; 256] = [
    0.00014, 0.00024, 0.00037, 0.00051, 0.00067, 0.00086, 0.00107, 0.00130, 0.00157, 0.00187,
    0.00220, 0.00256, 0.00297, 0.00341, 0.00390, 0.00443, 0.00501, 0.00564, 0.00632, 0.00706,
    0.00785, 0.00871, 0.00962, 0.01061, 0.01166, 0.01279, 0.01399, 0.01526, 0.01662, 0.01806,
    0.01959, 0.02121, 0.02292, 0.02472, 0.02662, 0.02863, 0.03073, 0.03294, 0.03527, 0.03770,
    0.04025, 0.04292, 0.04571, 0.04862, 0.05165, 0.05481, 0.05810, 0.06153, 0.06508, 0.06878,
    0.07261, 0.07658, 0.08069, 0.08495, 0.08935, 0.09389, 0.09859, 0.10343, 0.10842, 0.11356,
    0.11885, 0.12429, 0.12988, 0.13563, 0.14152, 0.14757, 0.15376, 0.16011, 0.16661, 0.17325,
    0.18005, 0.18699, 0.19407, 0.20130, 0.20867, 0.21618, 0.22382, 0.23161, 0.23952, 0.24757,
    0.25574, 0.26404, 0.27246, 0.28100, 0.28965, 0.29841, 0.30729, 0.31626, 0.32533, 0.33450,
    0.34376, 0.35311, 0.36253, 0.37204, 0.38161, 0.39126, 0.40096, 0.41072, 0.42054, 0.43040,
    0.44030, 0.45023, 0.46020, 0.47019, 0.48020, 0.49022, 0.50025, 0.51028, 0.52031, 0.53033,
    0.54033, 0.55031, 0.56026, 0.57019, 0.58007, 0.58991, 0.59970, 0.60944, 0.61912, 0.62873,
    0.63827, 0.64774, 0.65713, 0.66643, 0.67564, 0.68476, 0.69377, 0.70269, 0.71150, 0.72019,
    0.72877, 0.73723, 0.74557, 0.75378, 0.76186, 0.76981, 0.77762, 0.78530, 0.79283, 0.80022,
    0.80747, 0.81457, 0.82151, 0.82831, 0.83496, 0.84145, 0.84779, 0.85398, 0.86001, 0.86588,
    0.87160, 0.87716, 0.88257, 0.88782, 0.89291, 0.89785, 0.90264, 0.90728, 0.91176, 0.91610,
    0.92028, 0.92432, 0.92822, 0.93197, 0.93558, 0.93906, 0.94240, 0.94560, 0.94867, 0.95162,
    0.95444, 0.95713, 0.95971, 0.96217, 0.96451, 0.96674, 0.96887, 0.97089, 0.97281, 0.97463,
    0.97635, 0.97799, 0.97953, 0.98099, 0.98236, 0.98366, 0.98488, 0.98602, 0.98710, 0.98811,
    0.98905, 0.98994, 0.99076, 0.99153, 0.99225, 0.99291, 0.99353, 0.99411, 0.99464, 0.99513,
    0.99558, 0.99600, 0.99639, 0.99674, 0.99706, 0.99736, 0.99763, 0.99788, 0.99811, 0.99831,
    0.99850, 0.99867, 0.99882, 0.99895, 0.99908, 0.99919, 0.99929, 0.99938, 0.99946, 0.99953,
    0.99959, 0.99965, 0.99969, 0.99974, 0.99978, 0.99981, 0.99984, 0.99986, 0.99988, 0.99990,
    0.99992, 0.99993, 0.99994, 0.99995, 0.99996, 0.99997, 0.99998, 0.99998, 0.99998, 0.99999,
    0.99999, 0.99999, 0.99999, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0,
];

#[cfg(test)]
mod tests {
    use super::{Complex32, ImdctState, x256};

    /// The vector kernels do the scalar ones' arithmetic: same bits out, for
    /// the samples and for what is kept for the next block.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn vector_kernels_match_the_scalar_ones_bit_for_bit() {
        use super::{scalar, sse, tables};

        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            ((seed >> 40) as f32 / 8_388_608.0) - 1.0
        };
        let bits = |values: &[f32]| values.iter().map(|v| v.to_bits()).collect::<Vec<_>>();
        let tables = tables();

        for _ in 0..16 {
            let coeffs: [f32; 256] = std::array::from_fn(|_| next());
            let mut z_scalar = [Complex32::new(0.0, 0.0); 128];
            let mut z_sse = z_scalar;
            scalar::pre_rotate_512(&coeffs, &tables.long, &mut z_scalar);
            sse::pre_rotate_512(&coeffs, &tables.long, &mut z_sse);
            assert_eq!(z_scalar, z_sse);

            let z: [Complex32; 128] = std::array::from_fn(|_| Complex32::new(next(), next()));
            let delay: [f32; 128] = std::array::from_fn(|_| next());
            let (mut delay_scalar, mut delay_sse) = (delay, delay);
            let (mut out_scalar, mut out_sse) = ([0.0f32; 256], [0.0f32; 256]);
            scalar::fold_512(&z, &tables.long, &mut delay_scalar, &mut out_scalar);
            sse::fold_512(&z, &tables.long, &mut delay_sse, &mut out_sse);
            assert_eq!(bits(&out_scalar), bits(&out_sse));
            assert_eq!(bits(&delay_scalar), bits(&delay_sse));

            let a: [Complex32; 64] = std::array::from_fn(|_| Complex32::new(next(), next()));
            let b: [Complex32; 64] = std::array::from_fn(|_| Complex32::new(next(), next()));
            let (mut delay_scalar, mut delay_sse) = (delay, delay);
            scalar::fold_256(&a, &b, &tables.short, &mut delay_scalar, &mut out_scalar);
            sse::fold_256(&a, &b, &tables.short, &mut delay_sse, &mut out_sse);
            assert_eq!(bits(&out_scalar), bits(&out_sse));
            assert_eq!(bits(&delay_scalar), bits(&delay_sse));
        }
    }

    #[test]
    fn zero_coefficients_decode_to_silence() {
        let mut state = ImdctState::new();
        let coeffs = [0.0f32; 256];
        let mut output = [1.0f32; 256];

        state.apply(&coeffs, false, &mut output);
        assert!(output.iter().all(|sample| *sample == 0.0));

        state.apply(&coeffs, true, &mut output);
        assert!(output.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn short_block_second_pre_ifft_uses_odd_coefficients() {
        let mut state = ImdctState::new();
        let mut coeffs = [0.0f32; 256];
        coeffs[0] = 2.0;
        coeffs[1] = 3.0;
        coeffs[255] = 5.0;

        state.prepare_256_intermediates(&coeffs);

        let sample = state.intermediate_256_b[0];
        let expected = Complex32::new(5.0, 3.0) * x256()[0];
        assert!((sample.re - expected.re).abs() < 1e-6);
        assert!((sample.im - expected.im).abs() < 1e-6);
    }

    /// `index == 0` cannot tell the two strides apart, since `2 * 0 == 4 * 0`.
    /// Every other index can, so pin one.
    #[test]
    fn short_block_pre_ifft_strides_by_four() {
        let mut state = ImdctState::new();
        let coeffs: [f32; 256] = std::array::from_fn(|i| i as f32);

        state.prepare_256_intermediates(&coeffs);

        let even = state.intermediate_256_a[1];
        let expected_even = Complex32::new(250.0, 4.0) * x256()[1];
        assert!((even.re - expected_even.re).abs() < 1e-3);
        assert!((even.im - expected_even.im).abs() < 1e-3);

        let odd = state.intermediate_256_b[1];
        let expected_odd = Complex32::new(251.0, 5.0) * x256()[1];
        assert!((odd.re - expected_odd.re).abs() < 1e-3);
        assert!((odd.im - expected_odd.im).abs() < 1e-3);
    }
}

/// Parity against the reference decoder, for both transform lengths.
///
/// The reference is FFmpeg's own direct O(N^2) inverse MDCT
/// (`ff_tx_mdct_naive_inv`, libavutil/tx_template.c), driven the way
/// libavcodec/ac3dec.c `do_imdct` drives it and folded by
/// `vector_fmul_window` (libavutil/float_dsp.c). Nothing here shares code
/// with [`ImdctState`], so a shared misreading of the transform cannot make
/// both sides agree, and no corpus is needed to run it.
#[cfg(test)]
mod reference_parity {
    use super::{ImdctState, WINDOW};

    /// `ff_tx_mdct_naive_inv` with `s->len = len` and `scale = 1.0`: `len`
    /// coefficients in, `len` samples out (the half-length transform).
    fn naive_imdct(coeffs: &[f64], len: usize) -> Vec<f64> {
        let half = len / 2;
        let phase = std::f64::consts::PI / (4.0 * len as f64);
        let mut out = vec![0.0f64; len];
        for i in 0..half {
            let down = phase * (4.0 * half as f64 - 2.0 * i as f64 - 1.0);
            let up = phase * (3.0 * len as f64 + 2.0 * i as f64 + 1.0);
            let mut sum_down = 0.0f64;
            let mut sum_up = 0.0f64;
            for (j, coeff) in coeffs.iter().enumerate().take(len) {
                let odd = (2 * j + 1) as f64;
                sum_down += (odd * down).cos() * coeff;
                sum_up += (odd * up).cos() * coeff;
            }
            out[i] = sum_down;
            out[i + half] = -sum_up;
        }
        out
    }

    /// `do_imdct` for one channel: the long block runs one 256-coefficient
    /// transform and splits it, the short block runs two 128-coefficient
    /// transforms over the even and the odd coefficients. Either way the first
    /// half folds against the delay and the second half becomes the new delay.
    struct Reference {
        delay: [f64; 128],
    }

    impl Reference {
        fn new() -> Self {
            Self { delay: [0.0; 128] }
        }

        fn apply(&mut self, coeffs: &[f64; 256], block_switch: bool) -> [f64; 256] {
            let (head, tail) = if block_switch {
                let even: Vec<f64> = (0..128).map(|i| coeffs[2 * i]).collect();
                let odd: Vec<f64> = (0..128).map(|i| coeffs[2 * i + 1]).collect();
                (naive_imdct(&even, 128), naive_imdct(&odd, 128))
            } else {
                let full = naive_imdct(coeffs, 256);
                (full[..128].to_vec(), full[128..].to_vec())
            };

            // vector_fmul_window(out, delay, head, WINDOW, 128).
            let mut out = [0.0f64; 256];
            for k in 0..128 {
                let delayed = self.delay[k];
                let fresh = head[127 - k];
                let rising = WINDOW[k] as f64;
                let falling = WINDOW[255 - k] as f64;
                out[k] = delayed * falling - fresh * rising;
                out[255 - k] = delayed * rising + fresh * falling;
            }
            self.delay.copy_from_slice(&tail);
            out
        }
    }

    /// Spectrally shaped pseudo-random coefficients, so the blocks look more
    /// like audio than like noise. xorshift64, seeded per test.
    struct Rng(u64);

    impl Rng {
        fn coefficients(&mut self) -> [f32; 256] {
            std::array::from_fn(|i| {
                let mut x = self.0;
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                self.0 = x;
                let uniform = ((x >> 40) as f32 / 8_388_608.0) - 1.0;
                uniform / (1.0 + i as f32 / 24.0)
            })
        }
    }

    /// `ImdctState` doubles the overlap-add sum where the reference does not.
    const SCALE: f64 = 2.0;

    fn assert_matches_reference(pattern: &[bool], seed: u64) {
        let mut rng = Rng(seed);
        let mut state = ImdctState::new();
        let mut reference = Reference::new();

        for (block, &block_switch) in pattern.iter().enumerate() {
            let coeffs = rng.coefficients();
            let mut got = [0.0f32; 256];
            state.apply(&coeffs, block_switch, &mut got);

            let wide: [f64; 256] = std::array::from_fn(|i| coeffs[i] as f64);
            let want = reference.apply(&wide, block_switch);

            let peak = want.iter().fold(0.0f64, |a, w| a.max((SCALE * w).abs()));
            let error = got
                .iter()
                .zip(want)
                .fold(0.0f64, |a, (g, w)| a.max((*g as f64 - SCALE * w).abs()));
            assert!(
                error <= peak * 1e-5,
                "block {block} (block_switch={block_switch}): peak error {error:e} \
                 against a reference peak of {peak:e}"
            );
        }
    }

    #[test]
    fn long_blocks_match_reference() {
        assert_matches_reference(&[false; 8], 0x1234_5678_9abc_def0);
    }

    #[test]
    fn short_blocks_match_reference() {
        assert_matches_reference(&[true; 8], 0x1234_5678_9abc_def0);
    }

    /// A transient the way a real stream carries one: a short block among long
    /// ones. The block after a short block matters as much as the short block
    /// itself, because it reads back the delay the short block left behind.
    #[test]
    fn transitions_between_block_lengths_match_reference() {
        let pattern = [false, false, true, false, true, true, false, true, false];
        assert_matches_reference(&pattern, 0xdead_beef_cafe_1234);
    }

    /// Every position a short block can take inside a six-block AC-3 frame,
    /// including the last, whose delay crosses into the next frame.
    #[test]
    fn every_short_block_position_matches_reference() {
        for position in 0..6 {
            let mut pattern = [false; 12];
            pattern[position] = true;
            assert_matches_reference(&pattern, 0xa5a5_0000_0000_0001 + position as u64);
        }
    }
}
