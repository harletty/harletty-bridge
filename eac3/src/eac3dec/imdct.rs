// SPDX-License-Identifier: Apache-2.0

use std::f32::consts::PI;
use std::sync::{Arc, OnceLock};

use rustfft::{Fft, FftPlanner, num_complex::Complex32};

#[derive(Debug, Clone)]
pub(crate) struct ImdctState {
    delay: [f32; 256],
    output: [f32; 512],
    intermediate_512: [Complex32; 128],
    intermediate_256_a: [Complex32; 64],
    intermediate_256_b: [Complex32; 64],
}

impl ImdctState {
    pub(crate) fn new() -> Self {
        Self {
            delay: [0.0; 256],
            output: [0.0; 512],
            intermediate_512: [Complex32::new(0.0, 0.0); 128],
            intermediate_256_a: [Complex32::new(0.0, 0.0); 64],
            intermediate_256_b: [Complex32::new(0.0, 0.0); 64],
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
        let x = x512();
        for (index, slot) in self.intermediate_512.iter_mut().enumerate() {
            *slot = Complex32::new(coeffs[255 - 2 * index], coeffs[2 * index]) * x[index];
        }
        imdct_fft_cache()
            .ifft_512
            .process(&mut self.intermediate_512);
        for (value, coeff) in self.intermediate_512.iter_mut().zip(x.iter().copied()) {
            *value *= coeff;
        }

        for index in 0..64 {
            const N8: usize = 64;
            const N4: usize = 128;
            const N2: usize = 256;
            self.output[2 * index] = -self.intermediate_512[N8 + index].im * WINDOW[2 * index];
            self.output[2 * index + 1] =
                self.intermediate_512[N8 - 1 - index].re * WINDOW[2 * index + 1];
            self.output[N4 + 2 * index] = -self.intermediate_512[index].re * WINDOW[N4 + 2 * index];
            self.output[N4 + 1 + 2 * index] =
                self.intermediate_512[N4 - 1 - index].im * WINDOW[N4 + 1 + 2 * index];
            self.output[N2 + 2 * index] =
                -self.intermediate_512[N8 + index].re * WINDOW[N2 - 1 - 2 * index];
            self.output[N2 + 1 + 2 * index] =
                self.intermediate_512[N8 - 1 - index].im * WINDOW[N2 - 2 - 2 * index];
            self.output[3 * N4 + 2 * index] =
                self.intermediate_512[index].im * WINDOW[N4 - 1 - 2 * index];
            self.output[3 * N4 + 1 + 2 * index] =
                -self.intermediate_512[N4 - 1 - index].re * WINDOW[N4 - 2 - 2 * index];
        }

        for (index, sample) in output.iter_mut().enumerate().take(256) {
            *sample = 2.0 * (self.output[index] + self.delay[index]);
        }
        self.delay.copy_from_slice(&self.output[256..512]);
    }

    fn apply_256(&mut self, coeffs: &[f32; 256], output: &mut [f32]) {
        self.prepare_256_intermediates(coeffs);

        let fft = imdct_fft_cache();
        fft.ifft_256.process(&mut self.intermediate_256_a);
        fft.ifft_256.process(&mut self.intermediate_256_b);
        let x = x256();
        for (value, coeff) in self.intermediate_256_a.iter_mut().zip(x.iter().copied()) {
            *value *= coeff;
        }
        for (value, coeff) in self.intermediate_256_b.iter_mut().zip(x.iter().copied()) {
            *value *= coeff;
        }

        for index in 0..64 {
            const N8: usize = 64;
            const N4: usize = 128;
            const N2: usize = 256;
            self.output[2 * index] = -self.intermediate_256_a[index].im * WINDOW[2 * index];
            self.output[2 * index + 1] =
                self.intermediate_256_a[N8 - 1 - index].re * WINDOW[2 * index + 1];
            self.output[N4 + 2 * index] =
                -self.intermediate_256_a[index].re * WINDOW[N4 + 2 * index];
            self.output[N4 + 1 + 2 * index] =
                self.intermediate_256_a[N8 - 1 - index].im * WINDOW[N4 + 1 + 2 * index];
            self.output[N2 + 2 * index] =
                -self.intermediate_256_b[index].re * WINDOW[N2 - 1 - 2 * index];
            self.output[N2 + 1 + 2 * index] =
                self.intermediate_256_b[N8 - 1 - index].im * WINDOW[N2 - 2 - 2 * index];
            self.output[3 * N4 + 2 * index] =
                self.intermediate_256_b[index].im * WINDOW[N4 - 1 - 2 * index];
            self.output[3 * N4 + 1 + 2 * index] =
                -self.intermediate_256_b[N8 - 1 - index].re * WINDOW[N4 - 2 - 2 * index];
        }

        for (index, sample) in output.iter_mut().enumerate().take(256) {
            *sample = 2.0 * (self.output[index] + self.delay[index]);
        }
        self.delay.copy_from_slice(&self.output[256..512]);
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
}

fn imdct_fft_cache() -> &'static ImdctFftCache {
    static CACHE: OnceLock<ImdctFftCache> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut planner = FftPlanner::<f32>::new();
        ImdctFftCache {
            ifft_512: planner.plan_fft_inverse(128),
            ifft_256: planner.plan_fft_inverse(64),
        }
    })
}

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
