// SPDX-License-Identifier: Apache-2.0
//
// DTS Coherent Acoustics core decoder — subband-domain decode. Ported from
// ffmpeg's dca_core.c (HEADER_CORE path) + dcadsp.c kernels + dcaadpcm.h. Only
// the base core is handled (no XCH/XXCH/XBR/X96 core extensions); that yields
// the 5.1 lossy bed, which is the Phase-1 target. The subband samples produced
// here are fed to the QMF synthesis in `synth.rs`.

use super::huffman::core_vlcs;
use super::tables::{
    ADPCM_VB, HIGH_FREQ_VQ, JOINT_SCALE_FACTORS, LOSSLESS_QUANT, LOSSY_QUANT, QUANT_INDEX_GROUP_SIZE,
    QUANT_INDEX_SEL_NBITS, QUANT_LEVELS, SCALE_FACTOR_ADJ, SCALE_FACTOR_QUANT6, SCALE_FACTOR_QUANT7,
};
use crate::bitstream::BitReader;
use crate::parser::{AudioMode, FrameInfo};
use crate::types::BedChannel;

pub(crate) const DCA_SUBBANDS: usize = 32;
pub(crate) const DCA_SUBBAND_SAMPLES: usize = 8;
const DCA_ADPCM_COEFFS: usize = 4;
pub(crate) const DCA_LFE_HISTORY: usize = 8;
const DCA_ABITS_MAX: i32 = 26;
const DCA_CODE_BOOKS: usize = 10;
const DCA_CHANNELS: usize = 7;

const BLOCK_CODE_NBITS: [u8; 7] = [7, 10, 12, 13, 15, 17, 19];

// ───────────────────────── fixed-point helpers (dcamath.h) ─────────────────

#[inline]
fn clip23(a: i32) -> i32 {
    let lo = -(1 << 23);
    let hi = (1 << 23) - 1;
    a.clamp(lo, hi)
}

#[inline]
fn norm(a: i64, bits: u32) -> i32 {
    if bits > 0 {
        ((a + (1i64 << (bits - 1))) >> bits) as i32
    } else {
        a as i32
    }
}

#[inline]
fn mul(a: i32, b: i32, bits: u32) -> i32 {
    norm(a as i64 * b as i64, bits)
}

/// `ff_dcaadpcm_predict`.
#[inline]
fn adpcm_predict(pred_vq_index: usize, input: &[i32]) -> i32 {
    let coeff = &ADPCM_VB[pred_vq_index];
    let mut pred = 0i64;
    for i in 0..DCA_ADPCM_COEFFS {
        pred += input[DCA_ADPCM_COEFFS - 1 - i] as i64 * coeff[i] as i64;
    }
    clip23(norm(pred, 13))
}

/// `ff_dca_core_dequantize` (residual=false).
fn dequantize(output: &mut [i32], input: &[i32], step_size: i32, scale: i32) {
    let mut step_scale = step_size as i64 * scale as i64;
    let mut shift = 0u32;
    if step_scale > (1 << 23) {
        shift = (63 - (step_scale >> 23).leading_zeros()) + 1; // av_log2(x)+1
        step_scale >>= shift;
    }
    for n in 0..output.len() {
        output[n] = clip23(norm(input[n] as i64 * step_scale, 22 - shift));
    }
}

// ───────────────────────── decoder state ──────────────────────────────────

/// Per-channel subband storage: `[band]` -> buffer of `DCA_ADPCM_COEFFS`
/// history words followed by `npcmblocks` decoded samples. Persists across
/// frames for ADPCM history.
#[derive(Default, Clone)]
struct ChannelBands {
    /// `sub[band]`, each `DCA_ADPCM_COEFFS + npcmblocks` long.
    sub: Vec<Vec<i32>>,
}

#[derive(Default)]
pub(crate) struct CoreDecoder {
    npcmblocks: usize,
    // coding header
    nsubframes: usize,
    nchannels: usize,
    lfe_present: u8,
    bit_rate_lossless: bool,
    es_format: bool,
    predictor_history: bool,
    filter_perfect: bool,
    crc_present: bool,
    sync_ssf: bool,
    audio_mode: AudioMode,

    nsubbands: [usize; DCA_CHANNELS],
    subband_vq_start: [usize; DCA_CHANNELS],
    joint_intensity_index: [usize; DCA_CHANNELS],
    transition_mode_sel: [usize; DCA_CHANNELS],
    scale_factor_sel: [usize; DCA_CHANNELS],
    bit_allocation_sel: [usize; DCA_CHANNELS],
    quant_index_sel: [[usize; DCA_CODE_BOOKS]; DCA_CHANNELS],
    scale_factor_adj: [[i32; DCA_CODE_BOOKS]; DCA_CHANNELS],
    joint_scale_sel: [usize; DCA_CHANNELS],

    nsubsubframes: [usize; 16],
    prediction_mode: [[bool; DCA_SUBBANDS]; DCA_CHANNELS],
    prediction_vq_index: [[usize; DCA_SUBBANDS]; DCA_CHANNELS],
    bit_allocation: [[i32; DCA_SUBBANDS]; DCA_CHANNELS],
    transition_mode: Vec<[[i32; DCA_SUBBANDS]; DCA_CHANNELS]>, // [sf][ch][band]
    scale_factors: [[[i32; 2]; DCA_SUBBANDS]; DCA_CHANNELS],
    joint_scale_factors: [[i32; DCA_SUBBANDS]; DCA_CHANNELS],

    // persistent sample buffers
    bands: Vec<ChannelBands>, // [ch]
    lfe_samples: Vec<i32>,    // DCA_LFE_HISTORY + npcmblocks/2
}

#[derive(Debug)]
pub(crate) enum CoreError {
    Bitstream,
    Invalid(&'static str),
}

type R<T> = Result<T, CoreError>;

#[inline]
fn rb(gb: &mut BitReader, n: usize) -> R<u32> {
    gb.read_bits(n).ok_or(CoreError::Bitstream)
}
#[inline]
fn rb1(gb: &mut BitReader) -> R<bool> {
    gb.read_bit().ok_or(CoreError::Bitstream)
}
#[inline]
fn rsb(gb: &mut BitReader, n: usize) -> R<i32> {
    gb.read_signed_bits(n).ok_or(CoreError::Bitstream)
}

impl CoreDecoder {
    pub(crate) fn reset(&mut self) {
        for ch in &mut self.bands {
            for b in &mut ch.sub {
                b.iter_mut().for_each(|x| *x = 0);
            }
        }
        self.lfe_samples.iter_mut().for_each(|x| *x = 0);
    }

    fn alloc_buffers(&mut self) {
        let band_len = DCA_ADPCM_COEFFS + self.npcmblocks;
        if self.bands.len() != DCA_CHANNELS {
            self.bands = vec![ChannelBands::default(); DCA_CHANNELS];
        }
        for ch in &mut self.bands {
            if ch.sub.len() != DCA_SUBBANDS || ch.sub[0].len() != band_len {
                ch.sub = vec![vec![0i32; band_len]; DCA_SUBBANDS];
            }
        }
        let lfe_len = DCA_LFE_HISTORY + self.npcmblocks / 2;
        if self.lfe_samples.len() != lfe_len {
            self.lfe_samples = vec![0i32; lfe_len];
        }
        if !self.predictor_history {
            for ch in &mut self.bands {
                for b in &mut ch.sub {
                    b[..DCA_ADPCM_COEFFS].iter_mut().for_each(|x| *x = 0);
                }
            }
        }
    }

    /// Decode one core access unit (header already validated by the caller).
    pub(crate) fn decode_frame(&mut self, info: &FrameInfo, data: &[u8]) -> R<()> {
        self.npcmblocks = info.npcmblocks as usize;
        self.lfe_present = info.lfe_present;
        self.es_format = info.es_format;
        self.predictor_history = info.predictor_history;
        self.crc_present = info.crc_present;
        self.audio_mode = info.audio_mode;
        // bit_rate==3 marks the lossless quantizer; we don't parse br_code's
        // exact value here (open core), default to lossy. (Affects only the
        // step-size table selection; lossless core is rare.)
        self.bit_rate_lossless = false;

        self.alloc_buffers();

        // Start parsing right after the 32-bit syncword; re-walk the header to
        // reach the coding header position deterministically by re-reading it.
        let mut gb = BitReader::new(data);
        self.skip_frame_header(&mut gb)?;

        self.parse_frame_data(&mut gb)?;
        Ok(())
    }

    /// Advance `gb` past the core frame header to the coding header. Mirrors the
    /// field order of ff_dca_parse_core_frame_header.
    fn skip_frame_header(&mut self, gb: &mut BitReader) -> R<()> {
        rb(gb, 32)?; // sync
        rb1(gb)?; // normal_frame
        rb(gb, 5)?; // deficit
        let crc_present = rb1(gb)?;
        rb(gb, 7)?; // npcmblocks
        rb(gb, 14)?; // frame_size
        rb(gb, 6)?; // audio_mode
        rb(gb, 4)?; // sr_code
        let br_code = rb(gb, 5)?;
        self.bit_rate_lossless = br_code == 3;
        rb1(gb)?; // reserved
        rb1(gb)?; // drc
        let _ts = rb1(gb)?;
        let _aux = rb1(gb)?;
        rb1(gb)?; // hdcd
        rb(gb, 3)?; // ext_audio_type
        rb1(gb)?; // ext_audio_present
        self.sync_ssf = rb1(gb)?; // sync_ssf
        rb(gb, 2)?; // lfe
        rb1(gb)?; // predictor_history
        if crc_present {
            rb(gb, 16)?;
        }
        self.filter_perfect = rb1(gb)?; // filter_perfect
        rb(gb, 4)?; // encoder_rev
        rb(gb, 2)?; // copy_hist
        rb(gb, 3)?; // pcmr
        rb1(gb)?; // sumdiff_front
        rb1(gb)?; // sumdiff_surround
        rb(gb, 4)?; // dn_code
        self.crc_present = crc_present;
        Ok(())
    }

    fn parse_coding_header(&mut self, gb: &mut BitReader) -> R<()> {
        self.nsubframes = rb(gb, 4)? as usize + 1;
        self.nchannels = rb(gb, 3)? as usize + 1;
        let expect = self.audio_mode.channel_count();
        if self.nchannels != expect {
            return Err(CoreError::Invalid("nchannels mismatch"));
        }

        for ch in 0..self.nchannels {
            let n = rb(gb, 5)? as usize + 2;
            if n > DCA_SUBBANDS {
                return Err(CoreError::Invalid("subband activity"));
            }
            self.nsubbands[ch] = n;
        }
        for ch in 0..self.nchannels {
            self.subband_vq_start[ch] = rb(gb, 5)? as usize + 1;
        }
        for ch in 0..self.nchannels {
            let n = rb(gb, 3)? as usize;
            if n > self.nchannels {
                return Err(CoreError::Invalid("joint intensity"));
            }
            self.joint_intensity_index[ch] = n;
        }
        for ch in 0..self.nchannels {
            self.transition_mode_sel[ch] = rb(gb, 2)? as usize;
        }
        for ch in 0..self.nchannels {
            let sel = rb(gb, 3)? as usize;
            if sel == 7 {
                return Err(CoreError::Invalid("scale factor codebook"));
            }
            self.scale_factor_sel[ch] = sel;
        }
        for ch in 0..self.nchannels {
            let sel = rb(gb, 3)? as usize;
            if sel == 7 {
                return Err(CoreError::Invalid("bit allocation select"));
            }
            self.bit_allocation_sel[ch] = sel;
        }
        for n in 0..DCA_CODE_BOOKS {
            for ch in 0..self.nchannels {
                self.quant_index_sel[ch][n] = rb(gb, QUANT_INDEX_SEL_NBITS[n] as usize)? as usize;
            }
        }
        for n in 0..DCA_CODE_BOOKS {
            for ch in 0..self.nchannels {
                if self.quant_index_sel[ch][n] < QUANT_INDEX_GROUP_SIZE[n] as usize {
                    self.scale_factor_adj[ch][n] =
                        SCALE_FACTOR_ADJ[rb(gb, 2)? as usize] as i32;
                }
            }
        }
        if self.crc_present {
            rb(gb, 16)?;
        }
        Ok(())
    }

    fn parse_scale(&self, gb: &mut BitReader, scale_index: &mut i32, sel: usize) -> R<i32> {
        let (table, size): (&[u32], usize) = if sel > 5 {
            (&SCALE_FACTOR_QUANT7, SCALE_FACTOR_QUANT7.len())
        } else {
            (&SCALE_FACTOR_QUANT6, SCALE_FACTOR_QUANT6.len())
        };
        if sel < 5 {
            *scale_index += core_vlcs().scale_factor[sel]
                .get(gb)
                .ok_or(CoreError::Bitstream)?;
        } else {
            *scale_index = rb(gb, sel + 1)? as i32;
        }
        if (*scale_index as usize) >= size {
            return Err(CoreError::Invalid("scale factor index"));
        }
        Ok(table[*scale_index as usize] as i32)
    }

    fn parse_joint_scale(&self, gb: &mut BitReader, sel: usize) -> R<i32> {
        let mut scale_index = if sel < 5 {
            core_vlcs().scale_factor[sel].get(gb).ok_or(CoreError::Bitstream)?
        } else {
            rb(gb, sel + 1)? as i32
        };
        scale_index += 64;
        if (scale_index as usize) >= JOINT_SCALE_FACTORS.len() {
            return Err(CoreError::Invalid("joint scale index"));
        }
        Ok(JOINT_SCALE_FACTORS[scale_index as usize] as i32)
    }

    fn parse_subframe_header(&mut self, gb: &mut BitReader, sf: usize) -> R<()> {
        self.nsubsubframes[sf] = rb(gb, 2)? as usize + 1;
        rb(gb, 3)?; // partial subsubframe sample count

        for ch in 0..self.nchannels {
            for band in 0..self.nsubbands[ch] {
                self.prediction_mode[ch][band] = rb1(gb)?;
            }
        }
        for ch in 0..self.nchannels {
            for band in 0..self.nsubbands[ch] {
                if self.prediction_mode[ch][band] {
                    self.prediction_vq_index[ch][band] = rb(gb, 12)? as usize;
                }
            }
        }
        // Bit allocation index
        for ch in 0..self.nchannels {
            let sel = self.bit_allocation_sel[ch];
            for band in 0..self.subband_vq_start[ch] {
                let abits = if sel < 5 {
                    core_vlcs().bit_allocation[sel].get(gb).ok_or(CoreError::Bitstream)?
                } else {
                    rb(gb, sel - 1)? as i32
                };
                if abits > DCA_ABITS_MAX {
                    return Err(CoreError::Invalid("bit allocation index"));
                }
                self.bit_allocation[ch][band] = abits;
            }
        }
        // Transition mode
        for ch in 0..self.nchannels {
            self.transition_mode[sf][ch] = [0; DCA_SUBBANDS];
            if self.nsubsubframes[sf] > 1 {
                let sel = self.transition_mode_sel[ch];
                for band in 0..self.subband_vq_start[ch] {
                    if self.bit_allocation[ch][band] != 0 {
                        self.transition_mode[sf][ch][band] = core_vlcs().transition_mode[sel]
                            .get(gb)
                            .ok_or(CoreError::Bitstream)?;
                    }
                }
            }
        }
        // Scale factors
        for ch in 0..self.nchannels {
            let sel = self.scale_factor_sel[ch];
            let mut scale_index = 0i32;
            for band in 0..self.subband_vq_start[ch] {
                if self.bit_allocation[ch][band] != 0 {
                    let s = self.parse_scale(gb, &mut scale_index, sel)?;
                    self.scale_factors[ch][band][0] = s;
                    if self.transition_mode[sf][ch][band] != 0 {
                        let s2 = self.parse_scale(gb, &mut scale_index, sel)?;
                        self.scale_factors[ch][band][1] = s2;
                    }
                } else {
                    self.scale_factors[ch][band][0] = 0;
                }
            }
            for band in self.subband_vq_start[ch]..self.nsubbands[ch] {
                let s = self.parse_scale(gb, &mut scale_index, sel)?;
                self.scale_factors[ch][band][0] = s;
            }
        }
        // Joint subband codebook select
        for ch in 0..self.nchannels {
            if self.joint_intensity_index[ch] != 0 {
                let sel = rb(gb, 3)? as usize;
                if sel == 7 {
                    return Err(CoreError::Invalid("joint scale codebook"));
                }
                self.joint_scale_sel[ch] = sel;
            }
        }
        // Scale factors for joint subband coding
        for ch in 0..self.nchannels {
            let src_ch = self.joint_intensity_index[ch] as i32 - 1;
            if src_ch >= 0 {
                let src_ch = src_ch as usize;
                let sel = self.joint_scale_sel[ch];
                for band in self.nsubbands[ch]..self.nsubbands[src_ch] {
                    self.joint_scale_factors[ch][band] = self.parse_joint_scale(gb, sel)?;
                }
            }
        }
        // Dynamic range coefficient (drc_present) — drc flag not retained; the
        // header parser skipped it. The core path here assumes drc absent.
        if self.crc_present {
            rb(gb, 16)?;
        }
        Ok(())
    }

    /// `extract_audio` — returns (huffman_used, samples[8]).
    fn extract_audio(&self, gb: &mut BitReader, abits: i32, ch: usize) -> R<(bool, [i32; 8])> {
        let mut audio = [0i32; 8];
        if abits == 0 {
            return Ok((false, audio));
        }
        if abits as usize <= DCA_CODE_BOOKS {
            let sel = self.quant_index_sel[ch][abits as usize - 1];
            if sel < QUANT_INDEX_GROUP_SIZE[abits as usize - 1] as usize {
                let vlc = &core_vlcs().quant_index[abits as usize - 1][sel];
                for a in &mut audio {
                    *a = vlc.get(gb).ok_or(CoreError::Bitstream)?;
                }
                return Ok((true, audio));
            }
            if abits <= 7 {
                self.parse_block_codes(gb, &mut audio, abits)?;
                return Ok((false, audio));
            }
        }
        // No further encoding: abits-3 signed bits each.
        for a in &mut audio {
            *a = rsb(gb, abits as usize - 3)?;
        }
        Ok((false, audio))
    }

    fn parse_block_codes(&self, gb: &mut BitReader, audio: &mut [i32; 8], abits: i32) -> R<()> {
        let nbits = BLOCK_CODE_NBITS[abits as usize - 1] as usize;
        let code1 = rb(gb, nbits)? as i32;
        let code2 = rb(gb, nbits)? as i32;
        let levels = QUANT_LEVELS[abits as usize] as i32;
        if decode_blockcodes(code1, code2, levels, audio) != 0 {
            return Err(CoreError::Invalid("block code"));
        }
        Ok(())
    }

    fn parse_subframe_audio(
        &mut self,
        gb: &mut BitReader,
        sf: usize,
        sub_pos: &mut usize,
        lfe_pos: &mut usize,
    ) -> R<()> {
        let nsamples = self.nsubsubframes[sf] * DCA_SUBBAND_SAMPLES;
        if *sub_pos + nsamples > self.npcmblocks {
            return Err(CoreError::Invalid("subband overflow"));
        }

        // VQ encoded subbands
        for ch in 0..self.nchannels {
            let mut vq_index = [0i32; DCA_SUBBANDS];
            for band in self.subband_vq_start[ch]..self.nsubbands[ch] {
                vq_index[band] = rb(gb, 10)? as i32;
            }
            if self.subband_vq_start[ch] < self.nsubbands[ch] {
                self.decode_hf(ch, &vq_index, *sub_pos, nsamples);
            }
        }

        // LFE
        if self.lfe_present != 0 {
            let nlfesamples = 2 * self.lfe_present as usize * self.nsubsubframes[sf];
            let mut audio = [0i32; 16];
            for a in audio.iter_mut().take(nlfesamples) {
                *a = rsb(gb, 8)?;
            }
            let index = rb(gb, 8)? as usize;
            if index >= SCALE_FACTOR_QUANT7.len() {
                return Err(CoreError::Invalid("lfe scale"));
            }
            let mut scale = SCALE_FACTOR_QUANT7[index] as i32;
            scale = mul(4_697_620, scale, 23); // 0.035 * (1<<27)
            let mut ofs = *lfe_pos;
            for &a in audio.iter().take(nlfesamples) {
                self.lfe_samples[ofs] = clip23(((a as i64 * scale as i64) >> 4) as i32);
                ofs += 1;
            }
            *lfe_pos = ofs;
        }

        // Audio data
        let mut ofs = *sub_pos;
        for ssf in 0..self.nsubsubframes[sf] {
            for ch in 0..self.nchannels {
                for band in 0..self.subband_vq_start[ch] {
                    let abits = self.bit_allocation[ch][band];
                    let (huff, audio) = self.extract_audio(gb, abits, ch)?;
                    let step_size = if self.bit_rate_lossless {
                        LOSSLESS_QUANT[abits as usize] as i32
                    } else {
                        LOSSY_QUANT[abits as usize] as i32
                    };
                    let trans_ssf = self.transition_mode[sf][ch][band];
                    let mut scale = if trans_ssf == 0 || (ssf as i32) < trans_ssf {
                        self.scale_factors[ch][band][0]
                    } else {
                        self.scale_factors[ch][band][1]
                    };
                    if huff {
                        let adj = self.scale_factor_adj[ch][abits as usize - 1] as i64;
                        scale = clip23((adj * scale as i64 >> 22) as i32);
                    }
                    let base = DCA_ADPCM_COEFFS + ofs;
                    let buf = &mut self.bands[ch].sub[band][base..base + DCA_SUBBAND_SAMPLES];
                    dequantize(buf, &audio, step_size, scale);
                }
            }
            // DSYNC
            if (ssf == self.nsubsubframes[sf] - 1 || self.sync_ssf) && rb(gb, 16)? != 0xffff {
                return Err(CoreError::Invalid("dsync"));
            }
            ofs += DCA_SUBBAND_SAMPLES;
        }

        // Inverse ADPCM
        for ch in 0..self.nchannels {
            self.inverse_adpcm(ch, *sub_pos, nsamples);
        }

        // Joint subband coding
        for ch in 0..self.nchannels {
            let src_ch = self.joint_intensity_index[ch] as i32 - 1;
            if src_ch >= 0 {
                self.decode_joint(ch, src_ch as usize, *sub_pos, nsamples);
            }
        }

        *sub_pos = ofs;
        Ok(())
    }

    fn decode_hf(&mut self, ch: usize, vq_index: &[i32], ofs: usize, len: usize) {
        for i in self.subband_vq_start[ch]..self.nsubbands[ch] {
            let coeff = &HIGH_FREQ_VQ[vq_index[i] as usize];
            let scale = self.scale_factors[ch][i][0];
            let base = DCA_ADPCM_COEFFS + ofs;
            for j in 0..len {
                self.bands[ch].sub[i][base + j] =
                    clip23(((coeff[j] as i32 * scale) + (1 << 3)) >> 4);
            }
        }
    }

    fn inverse_adpcm(&mut self, ch: usize, sub_pos: usize, len: usize) {
        for band in 0..self.nsubbands[ch] {
            if self.prediction_mode[ch][band] {
                let pred_id = self.prediction_vq_index[ch][band];
                let buf = &mut self.bands[ch].sub[band];
                for j in 0..len {
                    // input window = buf[sub_pos+j .. sub_pos+j+4]
                    let win = [
                        buf[sub_pos + j],
                        buf[sub_pos + j + 1],
                        buf[sub_pos + j + 2],
                        buf[sub_pos + j + 3],
                    ];
                    let x = adpcm_predict(pred_id, &win);
                    let idx = DCA_ADPCM_COEFFS + sub_pos + j;
                    buf[idx] = clip23(buf[idx] + x);
                }
            }
        }
    }

    fn decode_joint(&mut self, ch: usize, src_ch: usize, ofs: usize, len: usize) {
        for band in self.nsubbands[ch]..self.nsubbands[src_ch] {
            let scale = self.joint_scale_factors[ch][band];
            let base = DCA_ADPCM_COEFFS + ofs;
            for j in 0..len {
                let src = self.bands[src_ch].sub[band][base + j];
                self.bands[ch].sub[band][base + j] = clip23(mul(src, scale, 17));
            }
        }
    }

    fn parse_frame_data(&mut self, gb: &mut BitReader) -> R<()> {
        self.parse_coding_header(gb)?;
        // transition_mode needs nsubframes slots.
        self.transition_mode = vec![[[0i32; DCA_SUBBANDS]; DCA_CHANNELS]; self.nsubframes];

        let mut sub_pos = 0usize;
        let mut lfe_pos = DCA_LFE_HISTORY;
        for sf in 0..self.nsubframes {
            self.parse_subframe_header(gb, sf)?;
            self.parse_subframe_audio(gb, sf, &mut sub_pos, &mut lfe_pos)?;
        }

        // Update ADPCM history & clear inactive subbands.
        for ch in 0..self.nchannels {
            let mut nsubbands = self.nsubbands[ch];
            if self.joint_intensity_index[ch] != 0 {
                nsubbands = nsubbands.max(self.nsubbands[self.joint_intensity_index[ch] - 1]);
            }
            for band in 0..nsubbands {
                let buf = &mut self.bands[ch].sub[band];
                // history (first 4) = last 4 decoded samples
                for k in 0..DCA_ADPCM_COEFFS {
                    buf[k] = buf[self.npcmblocks + k];
                }
            }
            for band in nsubbands..DCA_SUBBANDS {
                self.bands[ch].sub[band].iter_mut().for_each(|x| *x = 0);
            }
        }
        Ok(())
    }

    /// Decoded samples for channel `ch`, band `band` (npcmblocks long).
    pub(crate) fn subband(&self, ch: usize, band: usize) -> &[i32] {
        &self.bands[ch].sub[band][DCA_ADPCM_COEFFS..DCA_ADPCM_COEFFS + self.npcmblocks]
    }

    pub(crate) fn lfe(&self) -> &[i32] {
        &self.lfe_samples
    }

    /// Shift the last `DCA_LFE_HISTORY` decimated LFE samples to the front,
    /// matching the post-synthesis history update in ff_dca_core_filter_fixed.
    pub(crate) fn shift_lfe_history(&mut self, nlfesamples: usize) {
        for n in 0..DCA_LFE_HISTORY {
            self.lfe_samples[n] = self.lfe_samples[nlfesamples + n];
        }
    }

    pub(crate) fn filter_perfect(&self) -> bool {
        self.filter_perfect
    }

    pub(crate) fn nchannels(&self) -> usize {
        self.nchannels
    }
    pub(crate) fn npcmblocks(&self) -> usize {
        self.npcmblocks
    }
    pub(crate) fn lfe_present(&self) -> u8 {
        self.lfe_present
    }
}

/// Bed-channel label for each primary channel, in DCA decode order
/// (`prm_ch_to_spkr_map`). The renderer places beds by label, so this order
/// (not WAV order) is what the synthesized `fullband_channels` follow.
pub(crate) fn primary_bed_layout(mode: AudioMode) -> Vec<BedChannel> {
    use crate::types::BedChannel::*;
    match mode {
        AudioMode::Mono => vec![Center],
        AudioMode::MonoDual
        | AudioMode::Stereo
        | AudioMode::StereoSumDiff
        | AudioMode::StereoTotal => vec![FrontLeft, FrontRight],
        AudioMode::ThreeF => vec![Center, FrontLeft, FrontRight],
        AudioMode::TwoF1R => vec![FrontLeft, FrontRight, RearCenter],
        AudioMode::ThreeF1R => vec![Center, FrontLeft, FrontRight, RearCenter],
        AudioMode::TwoF2R => vec![FrontLeft, FrontRight, SurroundLeft, SurroundRight],
        AudioMode::ThreeF2R => {
            vec![Center, FrontLeft, FrontRight, SurroundLeft, SurroundRight]
        }
    }
}

/// `decode_blockcodes` — returns leftover (nonzero => error).
fn decode_blockcodes(mut code1: i32, mut code2: i32, levels: i32, audio: &mut [i32; 8]) -> i32 {
    let offset = (levels - 1) / 2;
    for n in 0..DCA_SUBBAND_SAMPLES / 2 {
        let div = code1 / levels;
        audio[n] = code1 - div * levels - offset;
        code1 = div;
    }
    for n in DCA_SUBBAND_SAMPLES / 2..DCA_SUBBAND_SAMPLES {
        let div = code2 / levels;
        audio[n] = code2 - div * levels - offset;
        code2 = div;
    }
    code1 | code2
}
