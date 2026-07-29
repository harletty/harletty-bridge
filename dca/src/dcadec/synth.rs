// SPDX-License-Identifier: Apache-2.0
//
// DCA fixed-point QMF synthesis. Ports the self-contained integer path
// (dcadct.c `imdct_half_32`, synth_filter.c `synth_filter_fixed`,
// dcadsp.c `sub_qmf32_fixed` / `lfe_fir_fixed`). Chosen over the float path so
// the transform is fully defined here (no opaque av_tx IMDCT). Output is the
// 24-bit fixed PCM ffmpeg emits as S32P/24, converted to f32 by /2^23.

use super::core::{
    CoreDecoder, DCA_LFE_HISTORY, DCA_SPEAKER_COUNT, DCA_SPEAKER_LFE1, DCA_SUBBANDS,
};
use super::tables::{
    FIR_32BANDS_NONPERFECT_FIXED, FIR_32BANDS_PERFECT_FIXED, FIR_64BANDS_FIXED, LFE_FIR_64_FIXED,
    LFE_FIR_64_FLOAT,
};

const PCM_SCALE: f32 = 8_388_608.0; // 2^23

#[inline]
fn clip23(a: i32) -> i32 {
    a.clamp(-(1 << 23), (1 << 23) - 1)
}
#[inline]
fn norm(a: i64, bits: u32) -> i32 {
    ((a + (1i64 << (bits - 1))) >> bits) as i32
}
#[inline]
fn norm23(a: i64) -> i32 {
    norm(a, 23)
}
#[inline]
fn mul23(a: i32, b: i32) -> i32 {
    norm(a as i64 * b as i64, 23)
}

// ───────────────────────── imdct_half_32 (dcadct.c) ───────────────────────

fn sum_a(input: &[i32], output: &mut [i32], len: usize) {
    for i in 0..len {
        output[i] = input[2 * i] + input[2 * i + 1];
    }
}
fn sum_b(input: &[i32], output: &mut [i32], len: usize) {
    output[0] = input[0];
    for i in 1..len {
        output[i] = input[2 * i] + input[2 * i - 1];
    }
}
fn sum_c(input: &[i32], output: &mut [i32], len: usize) {
    for i in 0..len {
        output[i] = input[2 * i];
    }
}
fn sum_d(input: &[i32], output: &mut [i32], len: usize) {
    output[0] = input[1];
    for i in 1..len {
        output[i] = input[2 * i - 1] + input[2 * i + 1];
    }
}
fn clp_v(buf: &mut [i32], len: usize) {
    for x in buf.iter_mut().take(len) {
        *x = clip23(*x);
    }
}

#[rustfmt::skip]
const DCT_A: [[i32; 8]; 8] = [
    [ 8348215,  8027397,  7398092,  6484482,  5321677,  3954362,  2435084,   822227 ],
    [ 8027397,  5321677,   822227, -3954362, -7398092, -8348215, -6484482, -2435084 ],
    [ 7398092,   822227, -6484482, -8027397, -2435084,  5321677,  8348215,  3954362 ],
    [ 6484482, -3954362, -8027397,   822227,  8348215,  2435084, -7398092, -5321677 ],
    [ 5321677, -7398092, -2435084,  8348215,  -822227, -8027397,  3954362,  6484482 ],
    [ 3954362, -8348215,  5321677,  2435084, -8027397,  6484482,   822227, -7398092 ],
    [ 2435084, -6484482,  8348215, -7398092,  3954362,   822227, -5321677,  8027397 ],
    [  822227, -2435084,  3954362, -5321677,  6484482, -7398092,  8027397, -8348215 ],
];

#[rustfmt::skip]
const DCT_B: [[i32; 7]; 8] = [
    [  8227423,  7750063,  6974873,  5931642,  4660461,  3210181,  1636536 ],
    [  6974873,  3210181, -1636536, -5931642, -8227423, -7750063, -4660461 ],
    [  4660461, -3210181, -8227423, -5931642,  1636536,  7750063,  6974873 ],
    [  1636536, -7750063, -4660461,  5931642,  6974873, -3210181, -8227423 ],
    [ -1636536, -7750063,  4660461,  5931642, -6974873, -3210181,  8227423 ],
    [ -4660461, -3210181,  8227423, -5931642, -1636536,  7750063, -6974873 ],
    [ -6974873,  3210181,  1636536, -5931642,  8227423, -7750063,  4660461 ],
    [ -8227423,  7750063, -6974873,  5931642, -4660461,  3210181, -1636536 ],
];

const MOD_A: [i32; 16] = [
    4199362, 4240198, 4323885, 4454708, 4639772, 4890013, 5221943, 5660703, -6245623, -7040975,
    -8158494, -9809974, -12450076, -17261920, -28585092, -85479984,
];
const MOD_B: [i32; 8] = [
    4214598, 4383036, 4755871, 5425934, 6611520, 8897610, 14448934, 42791536,
];
#[rustfmt::skip]
const MOD_C: [i32; 32] = [
     1048892,  1051425,   1056522,   1064244,  1074689,  1087987,   1104313,   1123884,
     1146975,  1173922,   1205139,   1241133,  1282529,  1330095,   1384791,   1447815,
    -1520688, -1605358,  -1704360,  -1821051, -1959964, -2127368,  -2332183,  -2587535,
    -2913561, -3342802,  -3931480,  -4785806, -6133390, -8566050, -14253820, -42727120,
];

fn dct_a(input: &[i32], output: &mut [i32]) {
    for i in 0..8 {
        let mut res = 0i64;
        for j in 0..8 {
            res += DCT_A[i][j] as i64 * input[j] as i64;
        }
        output[i] = norm23(res);
    }
}
fn dct_b(input: &[i32], output: &mut [i32]) {
    for i in 0..8 {
        let mut res = input[0] as i64 * (1i64 << 23);
        for j in 0..7 {
            res += DCT_B[i][j] as i64 * input[1 + j] as i64;
        }
        output[i] = norm23(res);
    }
}
fn mod_a(input: &[i32], output: &mut [i32]) {
    for i in 0..8 {
        output[i] = mul23(MOD_A[i], input[i] + input[8 + i]);
    }
    let mut k = 7usize;
    for i in 8..16 {
        output[i] = mul23(MOD_A[i], input[k] - input[8 + k]);
        k = k.wrapping_sub(1);
    }
}
fn mod_b(input: &mut [i32], output: &mut [i32]) {
    for i in 0..8 {
        input[8 + i] = mul23(MOD_B[i], input[8 + i]);
    }
    for i in 0..8 {
        output[i] = input[i] + input[8 + i];
    }
    let mut k = 7usize;
    for i in 8..16 {
        output[i] = input[k] - input[8 + k];
        k = k.wrapping_sub(1);
    }
}
fn mod_c(input: &[i32], output: &mut [i32]) {
    for i in 0..16 {
        output[i] = mul23(MOD_C[i], input[i] + input[16 + i]);
    }
    let mut k = 15usize;
    for i in 16..32 {
        output[i] = mul23(MOD_C[i], input[k] - input[16 + k]);
        k = k.wrapping_sub(1);
    }
}

fn imdct_half_32(output: &mut [i32; 32], input: &[i32; 32]) {
    let mut buf_a = [0i32; 32];
    let mut buf_b = [0i32; 32];

    let mut mag = 0i64;
    for &x in input.iter() {
        mag += (x as i64).abs();
    }
    let shift = if mag > 0x40_0000 { 2 } else { 0 };
    let round = if shift > 0 { 1 << (shift - 1) } else { 0 };
    for i in 0..32 {
        buf_a[i] = (input[i] + round) >> shift;
    }

    let tmp = buf_a;
    sum_a(&tmp, &mut buf_b[0..], 16);
    sum_b(&tmp, &mut buf_b[16..], 16);
    clp_v(&mut buf_b, 32);

    let tmp = buf_b;
    sum_a(&tmp[0..], &mut buf_a[0..], 8);
    sum_b(&tmp[0..], &mut buf_a[8..], 8);
    sum_c(&tmp[16..], &mut buf_a[16..], 8);
    sum_d(&tmp[16..], &mut buf_a[24..], 8);
    clp_v(&mut buf_a, 32);

    let tmp = buf_a;
    dct_a(&tmp[0..], &mut buf_b[0..]);
    dct_b(&tmp[8..], &mut buf_b[8..]);
    dct_b(&tmp[16..], &mut buf_b[16..]);
    dct_b(&tmp[24..], &mut buf_b[24..]);
    clp_v(&mut buf_b, 32);

    let tmp = buf_b;
    mod_a(&tmp[0..], &mut buf_a[0..]);
    let mut mb_in = [0i32; 16];
    mb_in.copy_from_slice(&tmp[16..32]);
    mod_b(&mut mb_in, &mut buf_a[16..]);
    clp_v(&mut buf_a, 32);

    mod_c(&buf_a, &mut buf_b);

    for x in buf_b.iter_mut() {
        *x = clip23(*x * (1 << shift));
    }

    let mut k = 31usize;
    for i in 0..16 {
        output[i] = clip23(buf_b[i] - buf_b[k]);
        output[16 + i] = clip23(buf_b[i] + buf_b[k]);
        k -= 1;
    }
}

// ───────────────────────── imdct_half_64 (dcadct.c) ───────────────────────

#[rustfmt::skip]
const MOD64_A: [i32; 32] = [
      4195568,   4205700,   4226086,    4256977,
      4298755,   4351949,   4417251,    4495537,
      4587901,   4695690,   4820557,    4964534,
      5130115,   5320382,   5539164,    5791261,
     -6082752,  -6421430,  -6817439,   -7284203,
     -7839855,  -8509474,  -9328732,  -10350140,
    -11654242, -13371208, -15725922,  -19143224,
    -24533560, -34264200, -57015280, -170908480,
];

#[rustfmt::skip]
const MOD64_B: [i32; 16] = [
     4199362,  4240198,  4323885,  4454708,
     4639772,  4890013,  5221943,  5660703,
     6245623,  7040975,  8158494,  9809974,
    12450076, 17261920, 28585092, 85479984,
];

#[rustfmt::skip]
const MOD64_C: [i32; 64] = [
      741511,    741958,    742853,    744199,
      746001,    748262,    750992,    754197,
      757888,    762077,    766777,    772003,
      777772,    784105,    791021,    798546,
      806707,    815532,    825054,    835311,
      846342,    858193,    870912,    884554,
      899181,    914860,    931667,    949686,
      969011,    989747,   1012012,   1035941,
    -1061684,  -1089412,  -1119320,  -1151629,
    -1186595,  -1224511,  -1265719,  -1310613,
    -1359657,  -1413400,  -1472490,  -1537703,
    -1609974,  -1690442,  -1780506,  -1881904,
    -1996824,  -2128058,  -2279225,  -2455101,
    -2662128,  -2909200,  -3208956,  -3579983,
    -4050785,  -4667404,  -5509372,  -6726913,
    -8641940, -12091426, -20144284, -60420720,
];

fn mod64_a(input: &[i32], output: &mut [i32]) {
    for i in 0..16 {
        output[i] = mul23(MOD64_A[i], input[i] + input[16 + i]);
    }
    let mut k = 15usize;
    for i in 16..32 {
        output[i] = mul23(MOD64_A[i], input[k] - input[16 + k]);
        k = k.wrapping_sub(1);
    }
}

fn mod64_b(input: &mut [i32], output: &mut [i32]) {
    for i in 0..16 {
        input[16 + i] = mul23(MOD64_B[i], input[16 + i]);
    }
    for i in 0..16 {
        output[i] = input[i] + input[16 + i];
    }
    let mut k = 15usize;
    for i in 16..32 {
        output[i] = input[k] - input[16 + k];
        k = k.wrapping_sub(1);
    }
}

fn mod64_c(input: &[i32], output: &mut [i32]) {
    for i in 0..32 {
        output[i] = mul23(MOD64_C[i], input[i] + input[32 + i]);
    }
    let mut k = 31usize;
    for i in 32..64 {
        output[i] = mul23(MOD64_C[i], input[k] - input[32 + k]);
        k = k.wrapping_sub(1);
    }
}

fn imdct_half_64(output: &mut [i32; 64], input: &[i32; 64]) {
    let mut buf_a = [0i32; 64];
    let mut buf_b = [0i32; 64];

    let mut mag = 0i64;
    for &x in input.iter() {
        mag += (x as i64).abs();
    }
    let shift = if mag > 0x40_0000 { 2 } else { 0 };
    let round = if shift > 0 { 1 << (shift - 1) } else { 0 };
    for i in 0..64 {
        buf_a[i] = (input[i] + round) >> shift;
    }

    let tmp = buf_a;
    sum_a(&tmp, &mut buf_b[0..], 32);
    sum_b(&tmp, &mut buf_b[32..], 32);
    clp_v(&mut buf_b, 64);

    let tmp = buf_b;
    sum_a(&tmp[0..], &mut buf_a[0..], 16);
    sum_b(&tmp[0..], &mut buf_a[16..], 16);
    sum_c(&tmp[32..], &mut buf_a[32..], 16);
    sum_d(&tmp[32..], &mut buf_a[48..], 16);
    clp_v(&mut buf_a, 64);

    let tmp = buf_a;
    sum_a(&tmp[0..], &mut buf_b[0..], 8);
    sum_b(&tmp[0..], &mut buf_b[8..], 8);
    sum_c(&tmp[16..], &mut buf_b[16..], 8);
    sum_d(&tmp[16..], &mut buf_b[24..], 8);
    sum_c(&tmp[32..], &mut buf_b[32..], 8);
    sum_d(&tmp[32..], &mut buf_b[40..], 8);
    sum_c(&tmp[48..], &mut buf_b[48..], 8);
    sum_d(&tmp[48..], &mut buf_b[56..], 8);
    clp_v(&mut buf_b, 64);

    let tmp = buf_b;
    dct_a(&tmp[0..], &mut buf_a[0..]);
    dct_b(&tmp[8..], &mut buf_a[8..]);
    dct_b(&tmp[16..], &mut buf_a[16..]);
    dct_b(&tmp[24..], &mut buf_a[24..]);
    dct_b(&tmp[32..], &mut buf_a[32..]);
    dct_b(&tmp[40..], &mut buf_a[40..]);
    dct_b(&tmp[48..], &mut buf_a[48..]);
    dct_b(&tmp[56..], &mut buf_a[56..]);
    clp_v(&mut buf_a, 64);

    let tmp = buf_a;
    mod_a(&tmp[0..], &mut buf_b[0..]);
    let mut mb_in = [0i32; 16];
    mb_in.copy_from_slice(&tmp[16..32]);
    mod_b(&mut mb_in, &mut buf_b[16..]);
    mb_in.copy_from_slice(&tmp[32..48]);
    mod_b(&mut mb_in, &mut buf_b[32..]);
    mb_in.copy_from_slice(&tmp[48..64]);
    mod_b(&mut mb_in, &mut buf_b[48..]);
    clp_v(&mut buf_b, 64);

    let tmp = buf_b;
    mod64_a(&tmp[0..], &mut buf_a[0..]);
    let mut m64b_in = [0i32; 32];
    m64b_in.copy_from_slice(&tmp[32..64]);
    mod64_b(&mut m64b_in, &mut buf_a[32..]);
    clp_v(&mut buf_a, 64);

    mod64_c(&buf_a, &mut buf_b);

    for x in buf_b.iter_mut() {
        *x = clip23(*x * (1 << shift));
    }

    let mut k = 63usize;
    for i in 0..32 {
        output[i] = clip23(buf_b[i] - buf_b[k]);
        output[32 + i] = clip23(buf_b[i] + buf_b[k]);
        k -= 1;
    }
}

// ───────────────────────── synth_filter_fixed (synth_filter.c) ─────────────

/// Per-channel QMF delay line. Sized for the 64-band (X96) filter bank; the
/// 32-band path uses only the leading half of each buffer. Both are kept in one
/// fixed-size struct so switching bank size never allocates.
struct ChannelSynth {
    hist1: [i32; 1024],
    hist2: [i32; 64],
    offset: usize,
}

impl Default for ChannelSynth {
    fn default() -> Self {
        Self {
            hist1: [0; 1024],
            hist2: [0; 64],
            offset: 0,
        }
    }
}

impl ChannelSynth {
    fn synth_filter(&mut self, window: &[i32; 512], out: &mut [i32; 32], input: &[i32; 32]) {
        let mut imdct = [0i32; 32];
        imdct_half_32(&mut imdct, input);
        let off = self.offset;
        self.hist1[off..off + 32].copy_from_slice(&imdct);

        let h1 = &self.hist1;
        for i in 0..16 {
            let mut a = self.hist2[i] as i64 * (1i64 << 21);
            let mut b = self.hist2[i + 16] as i64 * (1i64 << 21);
            let mut c = 0i64;
            let mut d = 0i64;
            let mut j = 0usize;
            while j < 512 - off {
                a += window[i + j] as i64 * h1[off + i + j] as i64;
                b += window[i + j + 16] as i64 * h1[off + 15 - i + j] as i64;
                c += window[i + j + 32] as i64 * h1[off + 16 + i + j] as i64;
                d += window[i + j + 48] as i64 * h1[off + 31 - i + j] as i64;
                j += 64;
            }
            while j < 512 {
                a += window[i + j] as i64 * h1[off + i + j - 512] as i64;
                b += window[i + j + 16] as i64 * h1[off + 15 - i + j - 512] as i64;
                c += window[i + j + 32] as i64 * h1[off + 16 + i + j - 512] as i64;
                d += window[i + j + 48] as i64 * h1[off + 31 - i + j - 512] as i64;
                j += 64;
            }
            out[i] = clip23(norm(a, 21));
            out[i + 16] = clip23(norm(b, 21));
            self.hist2[i] = norm(c, 21);
            self.hist2[i + 16] = norm(d, 21);
        }
        self.offset = (off.wrapping_sub(32)) & 511;
    }

    /// `synth_filter_fixed_64` — 64-band bank used for X96 synthesis. One
    /// subband sample per band produces 64 interpolated PCM samples, i.e. twice
    /// the output rate of the 32-band path.
    fn synth_filter_64(&mut self, window: &[i32; 1024], out: &mut [i32; 64], input: &[i32; 64]) {
        let mut imdct = [0i32; 64];
        imdct_half_64(&mut imdct, input);
        let off = self.offset;
        self.hist1[off..off + 64].copy_from_slice(&imdct);

        let h1 = &self.hist1;
        for i in 0..32 {
            let mut a = self.hist2[i] as i64 * (1i64 << 20);
            let mut b = self.hist2[i + 32] as i64 * (1i64 << 20);
            let mut c = 0i64;
            let mut d = 0i64;
            let mut j = 0usize;
            while j < 1024 - off {
                a += window[i + j] as i64 * h1[off + i + j] as i64;
                b += window[i + j + 32] as i64 * h1[off + 31 - i + j] as i64;
                c += window[i + j + 64] as i64 * h1[off + 32 + i + j] as i64;
                d += window[i + j + 96] as i64 * h1[off + 63 - i + j] as i64;
                j += 128;
            }
            while j < 1024 {
                a += window[i + j] as i64 * h1[off + i + j - 1024] as i64;
                b += window[i + j + 32] as i64 * h1[off + 31 - i + j - 1024] as i64;
                c += window[i + j + 64] as i64 * h1[off + 32 + i + j - 1024] as i64;
                d += window[i + j + 96] as i64 * h1[off + 63 - i + j - 1024] as i64;
                j += 128;
            }
            out[i] = clip23(norm(a, 20));
            out[i + 32] = clip23(norm(b, 20));
            self.hist2[i] = norm(c, 20);
            self.hist2[i + 32] = norm(d, 20);
        }
        self.offset = (off.wrapping_sub(64)) & 1023;
    }
}

/// Per-channel QMF synthesis state, persisted across frames.
#[derive(Default)]
pub(crate) struct SynthState {
    channels: Vec<ChannelSynth>,
    /// Whether the delay lines currently hold 64-band (X96) history. Switching
    /// bank size invalidates them, so they are erased on change — this mirrors
    /// ffmpeg's `set_filter_mode` / `erase_dsp_history`.
    x96_mode: bool,
    /// One-sample history for the 96 kHz LFE image-rejection filter
    /// (`output_history_lfe_fixed`).
    lfe_x96_hist: i32,
}

impl SynthState {
    pub(crate) fn reset(&mut self) {
        for c in &mut self.channels {
            *c = ChannelSynth::default();
        }
        self.lfe_x96_hist = 0;
    }

    /// Erase the delay lines when the filter-bank size changes.
    fn set_x96_mode(&mut self, x96: bool) {
        if self.x96_mode != x96 {
            self.x96_mode = x96;
            self.reset();
        }
    }

    /// Synthesize PCM for all primary channels + LFE of the decoded core frame.
    /// Returns `(fullband_channels, lfe)` as f32 in [-1, 1], in DCA primary
    /// channel order (caller maps to bed labels via `core::primary_bed_layout`).
    pub(crate) fn synthesize(&mut self, dec: &mut CoreDecoder) -> (Vec<Vec<f32>>, Option<Vec<f32>>) {
        let nch = dec.nchannels();
        let npcmblocks = dec.npcmblocks();
        let nsamples = npcmblocks * 32;
        self.set_x96_mode(false);
        if self.channels.len() < nch {
            self.channels.resize_with(nch, ChannelSynth::default);
        }

        let window: &[i32; 512] = if dec.filter_perfect() {
            &FIR_32BANDS_PERFECT_FIXED
        } else {
            &FIR_32BANDS_NONPERFECT_FIXED
        };

        let mut fullband = vec![vec![0f32; nsamples]; nch];
        for ch in 0..nch {
            // Gather subband samples [band][block] into the per-block input.
            let mut subs: [&[i32]; DCA_SUBBANDS] = [&[]; DCA_SUBBANDS];
            for (band, s) in subs.iter_mut().enumerate() {
                *s = dec.subband(ch, band);
            }
            let mut out = [0i32; 32];
            let mut input = [0i32; 32];
            let dst = &mut fullband[ch];
            for j in 0..npcmblocks {
                for i in 0..32 {
                    input[i] = subs[i][j];
                }
                self.channels[ch].synth_filter(window, &mut out, &input);
                for i in 0..32 {
                    dst[j * 32 + i] = out[i] as f32 / PCM_SCALE;
                }
            }
        }

        // LFE. lfe_present==2 (DCA_LFE_FLAG_64) uses the 64-tap interpolator,
        // which is what BluRay DTS streams carry. ==1 (128x) is not supported by
        // the fixed path (matches ffmpeg's ff_dca_core_filter_fixed).
        let lfe = if dec.lfe_present() == 2 {
            Some(lfe_synth(dec, nsamples))
        } else {
            None
        };

        (fullband, lfe)
    }
}

/// Fixed-point core output indexed by DCA speaker — the residual base for XLL.
pub(crate) struct CoreOutput {
    /// `samples[spkr]` = Some(int32 24-bit PCM) for active speakers.
    pub(crate) samples: Vec<Option<Vec<i32>>>,
    pub(crate) npcmsamples: usize,
    pub(crate) output_rate: u32,
    pub(crate) ch_mask: u32,
}

impl SynthState {
    /// Synthesize the core via the fixed-point path (matching ffmpeg's
    /// `ff_dca_core_filter_fixed`), producing int32 24-bit PCM indexed by DCA
    /// speaker. Used as the residual base when combining with XLL.
    /// `x96_synth` runs the core through the 64-band bank instead of the 32-band
    /// one, doubling both the output rate and the sample count. The upper 32
    /// subbands are fed zeros: there is no X96 payload here, the oversampling
    /// exists purely so a 48 kHz core can serve as the residual base for a
    /// 96 kHz XLL channel set (ffmpeg's `ff_dca_core_filter_fixed` special case).
    pub(crate) fn synthesize_fixed_by_speaker(
        &mut self,
        dec: &mut CoreDecoder,
        x96_synth: bool,
    ) -> CoreOutput {
        let nch = dec.nchannels();
        let npcmblocks = dec.npcmblocks();
        let nsamples = if x96_synth {
            npcmblocks * 64
        } else {
            npcmblocks * 32
        };
        self.set_x96_mode(x96_synth);
        if self.channels.len() < nch {
            self.channels.resize_with(nch, ChannelSynth::default);
        }
        let window: &[i32; 512] = if dec.filter_perfect() {
            &FIR_32BANDS_PERFECT_FIXED
        } else {
            &FIR_32BANDS_NONPERFECT_FIXED
        };

        let mut samples: Vec<Option<Vec<i32>>> = (0..DCA_SPEAKER_COUNT).map(|_| None).collect();

        for ch in 0..nch {
            let mut subs: [&[i32]; DCA_SUBBANDS] = [&[]; DCA_SUBBANDS];
            for (band, s) in subs.iter_mut().enumerate() {
                *s = dec.subband(ch, band);
            }
            let mut dst = vec![0i32; nsamples];
            if x96_synth {
                // input[32..64] stays zero: only the base 32 subbands exist.
                let mut out = [0i32; 64];
                let mut input = [0i32; 64];
                for j in 0..npcmblocks {
                    for i in 0..32 {
                        input[i] = subs[i][j];
                    }
                    self.channels[ch].synth_filter_64(&FIR_64BANDS_FIXED, &mut out, &input);
                    dst[j * 64..j * 64 + 64].copy_from_slice(&out);
                }
            } else {
                let mut out = [0i32; 32];
                let mut input = [0i32; 32];
                for j in 0..npcmblocks {
                    for i in 0..32 {
                        input[i] = subs[i][j];
                    }
                    self.channels[ch].synth_filter(window, &mut out, &input);
                    dst[j * 32..j * 32 + 32].copy_from_slice(&out);
                }
            }
            samples[dec.primary_speaker(ch)] = Some(dst);
        }

        if dec.lfe_present() == 2 {
            let lfe = if x96_synth {
                // Interpolate at the core rate, then expand to 96 kHz through the
                // image-rejection filter.
                let base = lfe_synth_fixed(dec, nsamples / 2);
                let mut out = vec![0i32; nsamples];
                lfe_x96_fixed(&mut out, &base, &mut self.lfe_x96_hist);
                out
            } else {
                lfe_synth_fixed(dec, nsamples)
            };
            samples[DCA_SPEAKER_LFE1] = Some(lfe);
        }

        CoreOutput {
            samples,
            npcmsamples: nsamples,
            output_rate: if x96_synth {
                dec.sample_rate() * 2
            } else {
                dec.sample_rate()
            },
            ch_mask: dec.ch_mask(),
        }
    }
}

/// `lfe_x96_fixed` — 2x oversampling filter that attenuates the 47.6-48.0 kHz
/// interpolation image when the LFE is carried into a 96 kHz output.
fn lfe_x96_fixed(dst: &mut [i32], src: &[i32], hist: &mut i32) {
    let mut prev = *hist;
    for (i, &s) in src.iter().enumerate() {
        let a = 2_097_471i64 * s as i64 + 6_291_137i64 * prev as i64;
        let b = 6_291_137i64 * s as i64 + 2_097_471i64 * prev as i64;
        prev = s;
        dst[2 * i] = clip23(norm23(a));
        dst[2 * i + 1] = clip23(norm23(b));
    }
    *hist = prev;
}

/// `lfe_fir_fixed` (int32) — fixed-point LFE interpolation, for the XLL residual
/// base (ffmpeg's residual combine reads the fixed core, not the float one).
fn lfe_synth_fixed(dec: &mut CoreDecoder, nsamples: usize) -> Vec<i32> {
    let npcmblocks = dec.npcmblocks();
    let nlfesamples = npcmblocks >> 1;
    let mut pcm = vec![0i32; nsamples];
    let coeff = &LFE_FIR_64_FIXED;
    {
        let lfe = dec.lfe();
        let mut out_pos = 0usize;
        for i in 0..nlfesamples {
            let center = DCA_LFE_HISTORY + i;
            for j in 0..32 {
                let mut a = 0i64;
                let mut b = 0i64;
                for k in 0..8 {
                    let s = lfe[center - k] as i64;
                    a += coeff[j * 8 + k] as i64 * s;
                    b += coeff[255 - j * 8 - k] as i64 * s;
                }
                pcm[out_pos + j] = clip23(norm23(a));
                pcm[out_pos + 32 + j] = clip23(norm23(b));
            }
            out_pos += 64;
        }
    }
    dec.shift_lfe_history(nlfesamples);
    pcm
}

/// `lfe_fir_float` over the persistent LFE history buffer, then shift history.
/// Uses the float interpolation filter (matching ffmpeg's float output path);
/// the float LFE coefficients already embed the 1/2^23 scale.
fn lfe_synth(dec: &mut CoreDecoder, nsamples: usize) -> Vec<f32> {
    let npcmblocks = dec.npcmblocks();
    let nlfesamples = npcmblocks >> 1;
    let mut pcm = vec![0f32; nsamples];
    let coeff = &LFE_FIR_64_FLOAT;
    {
        let lfe = dec.lfe(); // DCA_LFE_HISTORY history + data
        let mut out_pos = 0usize;
        for i in 0..nlfesamples {
            // lfe_samples pointer starts at DCA_LFE_HISTORY + i; reads [-k].
            let center = DCA_LFE_HISTORY + i;
            for j in 0..32 {
                let mut a = 0f32;
                let mut b = 0f32;
                for k in 0..8 {
                    let s = lfe[center - k] as f32;
                    a += coeff[j * 8 + k] * s;
                    b += coeff[255 - j * 8 - k] * s;
                }
                pcm[out_pos + j] = a;
                pcm[out_pos + 32 + j] = b;
            }
            out_pos += 64;
        }
    }
    // Update LFE history: move the last DCA_LFE_HISTORY decimated samples to the
    // front (mirrors the post-filter shift in ff_dca_core_filter_fixed).
    dec.shift_lfe_history(nlfesamples);
    pcm
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden output of the 64-band bank, produced by compiling ffmpeg's own
    /// `imdct_half_64` (dcadct.c) and `synth_filter_fixed_64` (synth_filter.c)
    /// against `ff_dca_fir_64bands_fixed` and feeding them the pseudo-random
    /// subband sequence generated below. The upper 32 subbands are left at zero,
    /// which is exactly how the X96 path drives the bank when it oversamples a
    /// 48 kHz core for a 96 kHz XLL channel set.
    ///
    /// Eight blocks are covered so the delay line wraps and the split window
    /// loop (the `j >= 1024 - offset` half) is exercised, not just the first
    /// block's straight-through path.
    #[rustfmt::skip]
    const GOLDEN_QMF64: [i32; 512] = [
    -19, -29, -21, -6, -17, -44, -31, 31,
    55, -7, -70, -28, 70, 104, 71, 51,
    38, -18, -48, 56, 190, 135, -90, -198,
    -54, 131, 141, 45, 14, 51, 60, 24,
    -26, -71, -68, -21, -75, -267, -280, 129,
    533, 273, -459, -729, -239, 231, 98, -236,
    -353, -556, -919, -699, 318, 894, 109, -916,
    -579, 667, 1096, 480, 195, 796, 1319, 908,
    -402, -2281, -3744, -3019, 141, 2754, 1961, -618,
    -1135, 435, 646, -924, -653, 2272, 3415, 476,
    -1997, 356, 4247, 3854, 366, -85, 3434, 5336,
    2723, -487, -649, -6, -1796, -4127, -3363, -824,
    -923, -3630, -4280, -1513, 258, -1163, -948, 4542,
    9599, 6978, 876, 1871, 9200, 10296, 1442, -4309,
    2271, 10644, 7275, -2265, -3629, 1714, 919, -5233,
    -2593, 9947, 14617, 1339, -16940, -23048, -15627, -3394,
    7207, 11737, 7557, 1091, 2566, 8791, 5720, -6677,
    -10227, 2197, 12454, 4868, -9777, -13540, -8370, -6111,
    -5763, -692, 2877, -4271, -13757, -9745, 4842, 11551,
    3661, -6945, -9545, -6199, -3087, -2417, -3863, -5484,
    -3947, 134, 936, -3056, -4790, -349, 3235, -268,
    -5070, -2976, 3164, 4817, 559, -4371, -6171, -4509,
    -1525, -1424, -4825, -4391, 4450, 11626, 5297, -6926,
    -7893, 724, 2105, -4501, -1535, 14932, 25918, 16826,
    -7839, -36749, -55102, -41052, 7424, 47772, 36362, -6148,
    -21327, -3145, -194, -20998, -12850, 37467, 61738, 20187,
    -19651, 14425, 77231, 75657, 20607, 7971, 60139, 91110,
    47467, -11280, -16635, 3521, -12762, -50972, -50959, -17973,
    -16629, -56820, -72142, -34116, -2640, -15347, -10315, 61438,
    120453, 72023, -15303, 6434, 116326, 134761, 15025, -62641,
    22166, 129273, 87266, -31008, -49382, 13316, 7172, -56969,
    -22239, 116403, 163184, 17712, -173426, -229406, -144208, -19031,
    75338, 104350, 59266, 476, 7465, 50728, 27323, -53676,
    -62628, 33830, 99445, 39128, -59245, -78714, -42074, -30628,
    -42197, -32904, -17212, -28860, -43145, -20054, 22240, 38615,
    19544, -18689, -59827, -77063, -50464, -12852, -18877, -58410,
    -68277, -36825, -21593, -51949, -86286, -78905, -30803, 26863,
    51671, 13420, -54801, -73294, -31563, -15225, -61044, -90741,
    -66257, -71765, -128542, -95062, 83199, 198402, 75692, -114843,
    -106492, 40161, 66410, -32419, -19553, 160176, 289192, 202953,
    -59427, -383726, -593843, -443702, 66665, 474737, 355613, -53970,
    -188166, -15222, 7240, -181638, -110040, 314809, 498097, 131840,
    -195346, 92057, 590961, 550073, 79679, -53382, 328958, 581405,
    306264, -81880, -119445, -2453, -123631, -362384, -347242, -145889,
    -142520, -344312, -366613, -130326, -11338, -132095, -74191, 362531,
    652573, 339021, -100072, 89612, 680069, 740434, 135788, -216079,
    182510, 624769, 379335, -177110, -300356, -85429, -96404, -254166,
    -17472, 532981, 675767, 134420, -531746, -724792, -438565, -1314,
    326946, 391128, 169532, -125969, -244129, -198096, -150008, -94195,
    63935, 230456, 234130, 124109, 79491, 115805, 113405, 20826,
    -143971, -319560, -314320, 14880, 386335, 311767, -145179, -342441,
    -34962, 215839, -31607, -384658, -322953, -49029, -50262, -280931,
    -372502, -292091, -275781, -411229, -637002, -795180, -523144, 314253,
    980720, 534965, -655408, -1151003, -524596, 55713, -196338, -596155,
    -584597, -758857, -1266209, -915941, 652894, 1603331, 532939, -1019886,
    -816295, 602453, 1029321, 232123, -23730, 919422, 1802391, 1475263,
    -157729, -2629775, -4503879, -3552163, 302988, 3391438, 2498651, -439035,
    -1207847, 205853, 317053, -1193137, -787093, 2113635, 3171775, 375774,
    -1887798, 307264, 3790651, 3265945, -195509, -1103842, 1630753, 3495653,
    1908181, -369356, -601702, -252820, -1445280, -2870790, -2326269, -864329,
    -1002512, -2251805, -1996875, -260333, 185685, -1077536, -800700, 2383119,
    4758112, 3065594, 291144, 1393031, 5037571, 5237048, 1115189, -1210873,
    1632361, 4752949, 2951041, -1110618, -1946445, -276967, -560580, -2197896,
    -503142, 4221417, 5705079, 946144, -5420326, -7701615, -5307360, -857543,
    ];

    /// Same LCG as the C reference harness.
    fn lcg_block(state: &mut u32) -> [i32; 64] {
        let mut input = [0i32; 64];
        for slot in input.iter_mut().take(32) {
            *state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            *slot = (((*state >> 8) & 0x1f_ffff) as i32) - 0x10_0000;
        }
        input
    }

    #[test]
    fn qmf64_matches_ffmpeg_reference() {
        let mut synth = ChannelSynth::default();
        let mut state = 12_345u32;
        let mut out = [0i32; 64];
        for block in 0..8 {
            let input = lcg_block(&mut state);
            synth.synth_filter_64(&FIR_64BANDS_FIXED, &mut out, &input);
            let expected = &GOLDEN_QMF64[block * 64..block * 64 + 64];
            assert_eq!(&out[..], expected, "mismatch in block {block}");
        }
    }

    /// The 96 kHz LFE image-rejection filter (`lfe_x96_fixed`), which doubles
    /// the LFE rate to match the oversampled fullband channels.
    #[test]
    fn lfe_x96_doubles_and_carries_history() {
        let src = [1000i32, -2000, 3000];
        let mut dst = [0i32; 6];
        let mut hist = 0i32;
        lfe_x96_fixed(&mut dst, &src, &mut hist);
        assert_eq!(hist, 3000, "history must retain the last input sample");

        // First output pair only sees src[0] (history starts at zero).
        assert_eq!(dst[0], norm23(2_097_471i64 * 1000));
        assert_eq!(dst[1], norm23(6_291_137i64 * 1000));
        // Second pair mixes src[1] with the previous sample.
        assert_eq!(dst[2], norm23(2_097_471i64 * -2000 + 6_291_137i64 * 1000));
        assert_eq!(dst[3], norm23(6_291_137i64 * -2000 + 2_097_471i64 * 1000));
    }
}
