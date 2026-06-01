// SPDX-License-Identifier: Apache-2.0
//
// DCA fixed-point QMF synthesis. Ports the self-contained integer path
// (dcadct.c `imdct_half_32`, synth_filter.c `synth_filter_fixed`,
// dcadsp.c `sub_qmf32_fixed` / `lfe_fir_fixed`). Chosen over the float path so
// the transform is fully defined here (no opaque av_tx IMDCT). Output is the
// 24-bit fixed PCM ffmpeg emits as S32P/24, converted to f32 by /2^23.

use super::core::{CoreDecoder, DCA_LFE_HISTORY, DCA_SUBBANDS};
use super::tables::{FIR_32BANDS_NONPERFECT_FIXED, FIR_32BANDS_PERFECT_FIXED, LFE_FIR_64_FLOAT};

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

// ───────────────────────── synth_filter_fixed (synth_filter.c) ─────────────

struct ChannelSynth {
    hist1: [i32; 512],
    hist2: [i32; 32],
    offset: usize,
}

impl Default for ChannelSynth {
    fn default() -> Self {
        Self {
            hist1: [0; 512],
            hist2: [0; 32],
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
}

/// Per-channel QMF synthesis state, persisted across frames.
#[derive(Default)]
pub(crate) struct SynthState {
    channels: Vec<ChannelSynth>,
}

impl SynthState {
    pub(crate) fn reset(&mut self) {
        for c in &mut self.channels {
            *c = ChannelSynth::default();
        }
    }

    /// Synthesize PCM for all primary channels + LFE of the decoded core frame.
    /// Returns `(fullband_channels, lfe)` as f32 in [-1, 1], in DCA primary
    /// channel order (caller maps to bed labels via `core::primary_bed_layout`).
    pub(crate) fn synthesize(&mut self, dec: &mut CoreDecoder) -> (Vec<Vec<f32>>, Option<Vec<f32>>) {
        let nch = dec.nchannels();
        let npcmblocks = dec.npcmblocks();
        let nsamples = npcmblocks * 32;
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
