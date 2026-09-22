// SPDX-License-Identifier: Apache-2.0
//
// DTS Coherent Acoustics core decoder — subband-domain decode. Ported from
// ffmpeg's dca_core.c (HEADER_CORE and HEADER_XXCH paths) + dcadsp.c kernels +
// dcaadpcm.h. Handles the primary channel set (the 5.1 lossy bed), the XXCH
// extension set a DTS-HD stream adds to it (the 7.1 rears), and a bare
// extension set coded in the same syntax without a frame header of its own
// (the DTS:X heights of a lossy carrier). Not handled: XCH, XBR, X96. The
// subband samples produced here are fed to the QMF synthesis in `synth.rs`.

use super::huffman::core_vlcs;
use super::tables::{
    ADPCM_VB, DMIXTABLE, HIGH_FREQ_VQ, INV_DMIXTABLE, JOINT_SCALE_FACTORS, LOSSLESS_QUANT,
    LOSSY_QUANT, QUANT_INDEX_GROUP_SIZE, QUANT_INDEX_SEL_NBITS, QUANT_LEVELS, SCALE_FACTOR_ADJ,
    SCALE_FACTOR_QUANT6, SCALE_FACTOR_QUANT7,
};
use super::xll::crc16_ccitt;
use crate::bitstream::BitReader;
use crate::parser::{AudioMode, FrameInfo};
use crate::types::BedChannel;

pub(crate) const DCA_SUBBANDS: usize = 32;
pub(crate) const DCA_SUBBAND_SAMPLES: usize = 8;
const DCA_ADPCM_COEFFS: usize = 4;
pub(crate) const DCA_LFE_HISTORY: usize = 8;
const DCA_ABITS_MAX: i32 = 26;
const DCA_CODE_BOOKS: usize = 10;
/// Channels the decoder holds at once: the primary set (at most five), an
/// XXCH set (at most two, as ffmpeg) and a bare extension set of up to four.
const DCA_CHANNELS: usize = 12;
/// `DCA_XXCH_CHANNELS_MAX`.
const DCA_XXCH_CHANNELS_MAX: usize = 2;
/// `DCA_CORE_CHANNELS_MAX`: bounds the XXCH downmix coefficient list.
const DCA_CORE_CHANNELS_MAX: usize = 6;
const DCA_SYNCWORD_XXCH: u32 = 0x4700_4A03;

const BLOCK_CODE_NBITS: [u8; 7] = [7, 10, 12, 13, 15, 17, 19];

// DCA speaker enum indices (`enum DCASpeaker`); mask bit = 1 << index.
pub(crate) const DCA_SPEAKER_C: usize = 0;
pub(crate) const DCA_SPEAKER_L: usize = 1;
pub(crate) const DCA_SPEAKER_R: usize = 2;
pub(crate) const DCA_SPEAKER_LS: usize = 3;
pub(crate) const DCA_SPEAKER_RS: usize = 4;
pub(crate) const DCA_SPEAKER_LFE1: usize = 5;
pub(crate) const DCA_SPEAKER_CS: usize = 6;
pub(crate) const DCA_SPEAKER_LSS: usize = 9;
pub(crate) const DCA_SPEAKER_RSS: usize = 10;
pub(crate) const DCA_SPEAKER_COUNT: usize = 32;

/// `audio_mode_ch_mask` — speaker layout mask (excluding LFE) per audio mode.
fn audio_mode_ch_mask(mode: AudioMode) -> u32 {
    let c = 1 << DCA_SPEAKER_C;
    let l = 1 << DCA_SPEAKER_L;
    let r = 1 << DCA_SPEAKER_R;
    let ls = 1 << DCA_SPEAKER_LS;
    let rs = 1 << DCA_SPEAKER_RS;
    let cs = 1 << DCA_SPEAKER_CS;
    let stereo = l | r;
    match mode {
        AudioMode::Mono => c,
        AudioMode::MonoDual
        | AudioMode::Stereo
        | AudioMode::StereoSumDiff
        | AudioMode::StereoTotal => stereo,
        AudioMode::ThreeF => stereo | c,
        AudioMode::TwoF1R => stereo | cs,
        AudioMode::ThreeF1R => stereo | c | cs,
        AudioMode::TwoF2R => stereo | ls | rs,
        AudioMode::ThreeF2R => stereo | c | ls | rs,
    }
}

/// `prm_ch_to_spkr_map[mode][ch]` — DCA speaker for each primary channel.
fn prm_ch_to_spkr(mode: AudioMode, ch: usize) -> usize {
    use AudioMode::*;
    let row: &[usize] = match mode {
        Mono | MonoDual => &[DCA_SPEAKER_C],
        Stereo | StereoSumDiff | StereoTotal => &[DCA_SPEAKER_L, DCA_SPEAKER_R],
        ThreeF => &[DCA_SPEAKER_C, DCA_SPEAKER_L, DCA_SPEAKER_R],
        TwoF1R => &[DCA_SPEAKER_L, DCA_SPEAKER_R, DCA_SPEAKER_CS],
        ThreeF1R => &[DCA_SPEAKER_C, DCA_SPEAKER_L, DCA_SPEAKER_R, DCA_SPEAKER_CS],
        TwoF2R => &[DCA_SPEAKER_L, DCA_SPEAKER_R, DCA_SPEAKER_LS, DCA_SPEAKER_RS],
        ThreeF2R => &[
            DCA_SPEAKER_C,
            DCA_SPEAKER_L,
            DCA_SPEAKER_R,
            DCA_SPEAKER_LS,
            DCA_SPEAKER_RS,
        ],
    };
    row[ch]
}

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

/// True when the `nbytes` of `data` at bit position `pos` (byte aligned)
/// carry a valid trailing CRC16 (`ff_dca_check_crc`).
fn crc_clean(data: &[u8], pos: usize, nbytes: usize) -> bool {
    if pos % 8 != 0 {
        return false;
    }
    data.get(pos / 8..pos / 8 + nbytes)
        .is_some_and(|bytes| crc16_ccitt(bytes) == 0)
}

/// Two distinct slots of the speaker-indexed output, borrowed apart.
fn two_slots(
    samples: &mut [Option<Vec<i32>>],
    src: usize,
    dst: usize,
) -> (Option<&[i32]>, Option<&mut Vec<i32>>) {
    debug_assert_ne!(src, dst);
    if src < dst {
        let (head, tail) = samples.split_at_mut(dst);
        (head[src].as_deref(), tail[0].as_mut())
    } else {
        let (head, tail) = samples.split_at_mut(src);
        (tail[0].as_deref(), head[dst].as_mut())
    }
}

/// Which channel set a coding header describes (`enum HeaderType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Header {
    /// The primary set, from the core frame's own coding header.
    Core,
    /// An XXCH set (`HEADER_XXCH`): channels beyond the core's, with the
    /// downmix the encoder folded into the core.
    Xxch,
    /// A bare set coded in the core syntax without a frame header of its
    /// own: the DTS:X extension of a lossy carrier.
    Extension,
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
    sample_rate: u32,
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
    /// Channels of the primary set; extension sets are appended after it.
    nchannels_core: usize,
    /// First channel of the bare extension set (`nchannels` when none).
    extension_base: usize,

    // XXCH set of the frame (`parse_xxch_frame`); reset per core frame.
    xxch_present: bool,
    xxch_crc_present: bool,
    xxch_mask_nbits: usize,
    xxch_core_mask: u32,
    xxch_spkr_mask: u32,
    xxch_dmix_embedded: bool,
    xxch_dmix_scale_inv: i32,
    xxch_dmix_mask: [u32; DCA_XXCH_CHANNELS_MAX],
    xxch_dmix_coeff: [i32; DCA_XXCH_CHANNELS_MAX * DCA_CORE_CHANNELS_MAX],

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
    Unsupported(&'static str),
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
        self.sample_rate = info.sample_rate;
        self.lfe_present = info.lfe_present;
        self.es_format = info.es_format;
        self.predictor_history = info.predictor_history;
        self.crc_present = info.crc_present;
        self.audio_mode = info.audio_mode;
        // bit_rate==3 marks the lossless quantizer; we don't parse br_code's
        // exact value here (open core), default to lossy. (Affects only the
        // step-size table selection; lossless core is rare.)
        self.bit_rate_lossless = false;
        // A new access unit: any extension set is decoded after it.
        self.xxch_present = false;
        self.nchannels = 0;
        self.nchannels_core = 0;
        self.extension_base = 0;

        self.alloc_buffers();

        // Start parsing right after the 32-bit syncword; re-walk the header to
        // reach the coding header position deterministically by re-reading it.
        let mut gb = BitReader::new(data);
        self.skip_frame_header(&mut gb)?;

        self.parse_frame_data(&mut gb, data, Header::Core, 0)?;
        self.nchannels_core = self.nchannels;
        self.extension_base = self.nchannels;
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

    fn parse_coding_header(
        &mut self,
        gb: &mut BitReader,
        data: &[u8],
        header: Header,
        xch_base: usize,
    ) -> R<()> {
        let header_pos = gb.position();
        let mut header_size = 0usize;
        match header {
            Header::Core => {
                self.nsubframes = rb(gb, 4)? as usize + 1;
                self.nchannels = rb(gb, 3)? as usize + 1;
                let expect = self.audio_mode.channel_count();
                if self.nchannels != expect {
                    return Err(CoreError::Invalid("nchannels mismatch"));
                }
            }
            Header::Xxch => {
                header_size = rb(gb, 7)? as usize + 1;
                let nchannels = rb(gb, 3)? as usize + 1;
                if nchannels > DCA_XXCH_CHANNELS_MAX {
                    return Err(CoreError::Unsupported("xxch channel count"));
                }
                self.nchannels = xch_base + nchannels;
                let mask = rb(gb, self.xxch_mask_nbits - DCA_SPEAKER_CS)? << DCA_SPEAKER_CS;
                if mask.count_ones() as usize != nchannels {
                    return Err(CoreError::Invalid("xxch speaker layout mask"));
                }
                if self.xxch_core_mask & mask != 0 {
                    return Err(CoreError::Invalid("xxch speaker mask overlaps core"));
                }
                self.xxch_spkr_mask = mask;
                if rb1(gb)? {
                    self.xxch_dmix_embedded = rb1(gb)?;
                    // `get_bits(6) * 4 - FF_DCA_DMIXTABLE_OFFSET - 3`, with the
                    // offset the difference of the two table sizes (41).
                    let index =
                        rb(gb, 6)? as i64 * 4 - (DMIXTABLE.len() - INV_DMIXTABLE.len()) as i64 - 3;
                    if !(0..INV_DMIXTABLE.len() as i64).contains(&index) {
                        return Err(CoreError::Invalid("xxch downmix scale index"));
                    }
                    self.xxch_dmix_scale_inv = INV_DMIXTABLE[index as usize] as i32;
                    for ch in 0..nchannels {
                        let mask = rb(gb, self.xxch_mask_nbits)?;
                        if mask & self.xxch_core_mask != mask {
                            return Err(CoreError::Invalid("xxch downmix mapping mask"));
                        }
                        self.xxch_dmix_mask[ch] = mask;
                    }
                    let mut n = 0usize;
                    for ch in 0..nchannels {
                        for bit in 0..self.xxch_mask_nbits {
                            if self.xxch_dmix_mask[ch] & (1 << bit) == 0 {
                                continue;
                            }
                            let code = rb(gb, 7)? as i32;
                            let sign = (code >> 6) - 1;
                            let code = code & 63;
                            let coeff = if code != 0 {
                                let index = code as usize * 4 - 3;
                                if index >= DMIXTABLE.len() {
                                    return Err(CoreError::Invalid(
                                        "xxch downmix coefficient index",
                                    ));
                                }
                                (DMIXTABLE[index] as i32 ^ sign) - sign
                            } else {
                                0
                            };
                            if n >= self.xxch_dmix_coeff.len() {
                                return Err(CoreError::Invalid("xxch downmix coefficient count"));
                            }
                            self.xxch_dmix_coeff[n] = coeff;
                            n += 1;
                        }
                    }
                } else {
                    self.xxch_dmix_embedded = false;
                }
            }
            Header::Extension => {
                // The bare set's header: its byte size, three bits observed
                // zero, the common fields, padding, then a CRC16 over the
                // whole header. The channel count is the caller's (the
                // wrapper states it), not the header's.
                header_size = rb(gb, 8)? as usize + 1;
                if !crc_clean(data, header_pos, header_size) {
                    return Err(CoreError::Invalid("extension set header crc"));
                }
                if rb(gb, 3)? != 0 {
                    return Err(CoreError::Unsupported("extension set header flags"));
                }
            }
        }

        if header == Header::Extension {
            // The bare set writes its per-band sections over all 32 subbands
            // (prediction flags, scale factors) and codes the bands from the
            // first five-bit field on by VQ; the second five-bit field's
            // meaning is not established (27, 15, 15, 15 across the corpus)
            // and nothing here depends on it.
            for ch in xch_base..self.nchannels {
                self.subband_vq_start[ch] = rb(gb, 5)? as usize + 1;
                self.nsubbands[ch] = DCA_SUBBANDS;
            }
            for _ in xch_base..self.nchannels {
                rb(gb, 5)?;
            }
        } else {
            for ch in xch_base..self.nchannels {
                let n = rb(gb, 5)? as usize + 2;
                if n > DCA_SUBBANDS {
                    return Err(CoreError::Invalid("subband activity"));
                }
                self.nsubbands[ch] = n;
            }
            for ch in xch_base..self.nchannels {
                self.subband_vq_start[ch] = rb(gb, 5)? as usize + 1;
            }
        }
        for ch in xch_base..self.nchannels {
            let mut n = rb(gb, 3)? as usize;
            match header {
                Header::Core => {}
                // An XXCH set's joint index counts from its own first
                // channel (`n += xch_base - 1` in ffmpeg).
                Header::Xxch if n != 0 => n += xch_base - 1,
                Header::Xxch => {}
                // The bare set carries this field (3, 0, 0, 0 across the
                // corpus) but no joint codebook select after it: whatever
                // it means, its channels are not joint-coded.
                Header::Extension => n = 0,
            }
            if n > self.nchannels {
                return Err(CoreError::Invalid("joint intensity"));
            }
            self.joint_intensity_index[ch] = n;
        }
        for ch in xch_base..self.nchannels {
            self.transition_mode_sel[ch] = rb(gb, 2)? as usize;
        }
        for ch in xch_base..self.nchannels {
            let sel = rb(gb, 3)? as usize;
            if sel == 7 {
                return Err(CoreError::Invalid("scale factor codebook"));
            }
            self.scale_factor_sel[ch] = sel;
        }
        for ch in xch_base..self.nchannels {
            let sel = rb(gb, 3)? as usize;
            if sel == 7 {
                return Err(CoreError::Invalid("bit allocation select"));
            }
            self.bit_allocation_sel[ch] = sel;
        }
        for n in 0..DCA_CODE_BOOKS {
            for ch in xch_base..self.nchannels {
                self.quant_index_sel[ch][n] = rb(gb, QUANT_INDEX_SEL_NBITS[n] as usize)? as usize;
            }
        }
        for n in 0..DCA_CODE_BOOKS {
            for ch in xch_base..self.nchannels {
                if self.quant_index_sel[ch][n] < QUANT_INDEX_GROUP_SIZE[n] as usize {
                    self.scale_factor_adj[ch][n] = SCALE_FACTOR_ADJ[rb(gb, 2)? as usize] as i32;
                }
            }
        }
        match header {
            Header::Core => {
                if self.crc_present {
                    rb(gb, 16)?;
                }
            }
            // Reserved bits, byte alignment and the header CRC16.
            Header::Xxch | Header::Extension => {
                let end = header_pos + header_size * 8;
                if gb.position() > end || !gb.seek(end) {
                    return Err(CoreError::Invalid("channel set header size"));
                }
            }
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
            core_vlcs().scale_factor[sel]
                .get(gb)
                .ok_or(CoreError::Bitstream)?
        } else {
            rb(gb, sel + 1)? as i32
        };
        scale_index += 64;
        if (scale_index as usize) >= JOINT_SCALE_FACTORS.len() {
            return Err(CoreError::Invalid("joint scale index"));
        }
        Ok(JOINT_SCALE_FACTORS[scale_index as usize] as i32)
    }

    fn parse_subframe_header(
        &mut self,
        gb: &mut BitReader,
        sf: usize,
        header: Header,
        xch_base: usize,
    ) -> R<()> {
        // The subsubframe layout is the primary set's; extension sets share it.
        if header == Header::Core {
            self.nsubsubframes[sf] = rb(gb, 2)? as usize + 1;
            rb(gb, 3)?; // partial subsubframe sample count
        }

        for ch in xch_base..self.nchannels {
            for band in 0..self.nsubbands[ch] {
                self.prediction_mode[ch][band] = rb1(gb)?;
            }
        }
        for ch in xch_base..self.nchannels {
            for band in 0..self.nsubbands[ch] {
                if self.prediction_mode[ch][band] {
                    self.prediction_vq_index[ch][band] = rb(gb, 12)? as usize;
                }
            }
        }
        // Bit allocation index
        for ch in xch_base..self.nchannels {
            let sel = self.bit_allocation_sel[ch];
            for band in 0..self.subband_vq_start[ch] {
                let abits = if sel < 5 {
                    core_vlcs().bit_allocation[sel]
                        .get(gb)
                        .ok_or(CoreError::Bitstream)?
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
        for ch in xch_base..self.nchannels {
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
        for ch in xch_base..self.nchannels {
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
        for ch in xch_base..self.nchannels {
            if self.joint_intensity_index[ch] != 0 {
                let sel = rb(gb, 3)? as usize;
                if sel == 7 {
                    return Err(CoreError::Invalid("joint scale codebook"));
                }
                self.joint_scale_sel[ch] = sel;
            }
        }
        // Scale factors for joint subband coding
        for ch in xch_base..self.nchannels {
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
        header: Header,
        xch_base: usize,
        sub_pos: &mut usize,
        lfe_pos: &mut usize,
    ) -> R<()> {
        let nsamples = self.nsubsubframes[sf] * DCA_SUBBAND_SAMPLES;
        if *sub_pos + nsamples > self.npcmblocks {
            return Err(CoreError::Invalid("subband overflow"));
        }

        // VQ encoded subbands
        for ch in xch_base..self.nchannels {
            let mut vq_index = [0i32; DCA_SUBBANDS];
            for band in self.subband_vq_start[ch]..self.nsubbands[ch] {
                vq_index[band] = rb(gb, 10)? as i32;
            }
            if self.subband_vq_start[ch] < self.nsubbands[ch] {
                self.decode_hf(ch, &vq_index, *sub_pos, nsamples);
            }
        }

        // LFE: the primary set's only.
        if self.lfe_present != 0 && header == Header::Core {
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
            for ch in xch_base..self.nchannels {
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
        for ch in xch_base..self.nchannels {
            self.inverse_adpcm(ch, *sub_pos, nsamples);
        }

        // Joint subband coding
        for ch in xch_base..self.nchannels {
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

    /// Parse one channel set — the primary set from the core's coding
    /// header, or an extension set whose channels start at `xch_base` — and
    /// decode its subband samples for the frame.
    fn parse_frame_data(
        &mut self,
        gb: &mut BitReader,
        data: &[u8],
        header: Header,
        xch_base: usize,
    ) -> R<()> {
        self.parse_coding_header(gb, data, header, xch_base)?;
        // transition_mode needs nsubframes slots; kept across frames.
        if self.transition_mode.len() != self.nsubframes {
            self.transition_mode
                .resize(self.nsubframes, [[0i32; DCA_SUBBANDS]; DCA_CHANNELS]);
        }

        let mut sub_pos = 0usize;
        let mut lfe_pos = DCA_LFE_HISTORY;
        for sf in 0..self.nsubframes {
            self.parse_subframe_header(gb, sf, header, xch_base)?;
            self.parse_subframe_audio(gb, sf, header, xch_base, &mut sub_pos, &mut lfe_pos)?;
        }

        // Update ADPCM history & clear inactive subbands.
        for ch in xch_base..self.nchannels {
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

    /// Decode an XXCH frame (`parse_xxch_frame`): the extra channels a DTS-HD
    /// stream carries beyond the core's 5.1, from the EXSS XXCH component
    /// bytes (`data` starts at the 0x47004A03 syncword). Appends them after
    /// the primary channels; must follow `decode_frame` of the same core
    /// access unit, whose subframe layout the set shares. On error the
    /// decoder falls back to the primary set alone.
    pub(crate) fn decode_xxch(&mut self, data: &[u8]) -> R<()> {
        let result = self.parse_xxch_frame(data);
        if result.is_err() {
            self.drop_extension_sets();
        }
        result
    }

    fn parse_xxch_frame(&mut self, data: &[u8]) -> R<()> {
        let mut gb = BitReader::new(data);
        if rb(&mut gb, 32)? != DCA_SYNCWORD_XXCH {
            return Err(CoreError::Invalid("xxch sync"));
        }
        let header_size = rb(&mut gb, 6)? as usize + 1;
        self.xxch_crc_present = rb1(&mut gb)?;
        self.xxch_mask_nbits = rb(&mut gb, 5)? as usize + 1;
        if self.xxch_mask_nbits <= DCA_SPEAKER_CS {
            return Err(CoreError::Invalid("xxch speaker mask width"));
        }
        if rb(&mut gb, 2)? != 0 {
            return Err(CoreError::Unsupported("xxch channel sets"));
        }
        let frame_size = rb(&mut gb, 14)? as usize + 1;
        self.xxch_core_mask = rb(&mut gb, self.xxch_mask_nbits)?;

        // The set's view of the core must agree with the core, allowing the
        // side-surround naming of the core's surrounds.
        let mut mask = self.core_ch_mask();
        if mask & (1 << DCA_SPEAKER_LS) != 0 && self.xxch_core_mask & (1 << DCA_SPEAKER_LSS) != 0 {
            mask = (mask & !(1 << DCA_SPEAKER_LS)) | (1 << DCA_SPEAKER_LSS);
        }
        if mask & (1 << DCA_SPEAKER_RS) != 0 && self.xxch_core_mask & (1 << DCA_SPEAKER_RSS) != 0 {
            mask = (mask & !(1 << DCA_SPEAKER_RS)) | (1 << DCA_SPEAKER_RSS);
        }
        if mask != self.xxch_core_mask {
            return Err(CoreError::Invalid("xxch core speaker mask"));
        }
        if !gb.seek(header_size * 8) {
            return Err(CoreError::Invalid("xxch frame header size"));
        }
        if header_size + frame_size > data.len() {
            return Err(CoreError::Invalid("xxch channel set size"));
        }
        self.parse_frame_data(&mut gb, data, Header::Xxch, self.nchannels_core)?;
        self.xxch_present = true;
        Ok(())
    }

    /// Decode a bare channel set of `nchannels` channels coded in the core
    /// syntax without a frame header of its own (the DTS:X extension of a
    /// lossy carrier), appended after the sets already decoded for this
    /// frame. Returns the bytes consumed up to the end of the last DSYNC
    /// marker. On error the set is dropped.
    pub(crate) fn decode_extension_set(&mut self, data: &[u8], nchannels: usize) -> R<usize> {
        let base = self.nchannels;
        if nchannels == 0 || base + nchannels > DCA_CHANNELS {
            return Err(CoreError::Unsupported("extension set channel count"));
        }
        self.nchannels = base + nchannels;
        self.extension_base = base;
        let mut gb = BitReader::new(data);
        match self.parse_frame_data(&mut gb, data, Header::Extension, base) {
            Ok(()) => Ok(gb.position().div_ceil(8)),
            Err(e) => {
                self.nchannels = base;
                self.extension_base = base;
                for ch in base..base + nchannels {
                    for band in &mut self.bands[ch].sub {
                        band.iter_mut().for_each(|x| *x = 0);
                    }
                }
                Err(e)
            }
        }
    }

    /// Forget every extension set of this frame: the primary set alone.
    fn drop_extension_sets(&mut self) {
        self.nchannels = self.nchannels_core;
        self.extension_base = self.nchannels_core;
        self.xxch_present = false;
    }

    /// Undo the downmix an XXCH encoder folded into the core's channels, in
    /// the PCM domain after synthesis (`ff_dca_core_filter_fixed`), on the
    /// speaker-indexed 24-bit output.
    pub(crate) fn undo_xxch_dmix(&self, samples: &mut [Option<Vec<i32>>]) {
        if !self.xxch_present || !self.xxch_dmix_embedded {
            return;
        }
        let scale_inv = self.xxch_dmix_scale_inv;
        for spkr in 0..self.xxch_mask_nbits {
            if self.xxch_core_mask & (1 << spkr) == 0 {
                continue;
            }
            if let Some(buf) = samples
                .get_mut(self.output_slot(spkr))
                .and_then(Option::as_mut)
            {
                for x in buf.iter_mut() {
                    *x = mul(*x, scale_inv, 16);
                }
            }
        }
        let mut coeff = self.xxch_dmix_coeff.iter();
        for ch in self.nchannels_core..self.nchannels {
            if ch >= self.extension_base {
                break;
            }
            let Some(src) = self.speaker_for(ch) else {
                break;
            };
            let mask = self.xxch_dmix_mask[ch - self.nchannels_core];
            for spkr in 0..self.xxch_mask_nbits {
                if mask & (1 << spkr) == 0 {
                    continue;
                }
                let Some(&c) = coeff.next() else {
                    return;
                };
                let c = mul(c, scale_inv, 16);
                let dst = self.output_slot(spkr);
                if c == 0 || dst == src {
                    continue;
                }
                let (src_buf, dst_buf) = two_slots(samples, src, dst);
                let (Some(src_buf), Some(dst_buf)) = (src_buf, dst_buf) else {
                    continue;
                };
                for (d, &s) in dst_buf.iter_mut().zip(src_buf.iter()) {
                    *d = d.wrapping_sub(mul(s, c, 15));
                }
            }
        }
    }

    /// The output slot a speaker of the XXCH masks lands in: the core's
    /// surrounds keep their Ls/Rs slots when the set names them Lss/Rss.
    fn output_slot(&self, spkr: usize) -> usize {
        let core = audio_mode_ch_mask(self.audio_mode);
        match spkr {
            DCA_SPEAKER_LSS if core & (1 << DCA_SPEAKER_LS) != 0 => DCA_SPEAKER_LS,
            DCA_SPEAKER_RSS if core & (1 << DCA_SPEAKER_RS) != 0 => DCA_SPEAKER_RS,
            s => s,
        }
    }

    /// The DCA speaker channel `ch` plays through: a primary channel's from
    /// the audio mode, an XXCH channel's from the set's layout mask; `None`
    /// for a bare extension set's channels, which have no speaker.
    pub(crate) fn speaker_for(&self, ch: usize) -> Option<usize> {
        if ch < self.nchannels_core {
            return Some(prm_ch_to_spkr(self.audio_mode, ch));
        }
        if ch >= self.extension_base {
            return None;
        }
        let mut index = ch - self.nchannels_core;
        for spkr in DCA_SPEAKER_CS..self.xxch_mask_nbits {
            if self.xxch_spkr_mask & (1 << spkr) != 0 {
                if index == 0 {
                    return Some(spkr);
                }
                index -= 1;
            }
        }
        None
    }

    /// Channels of the bare extension set decoded this frame, if any.
    pub(crate) fn extension_channels(&self) -> std::ops::Range<usize> {
        self.extension_base..self.nchannels
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

    /// The primary set's speaker layout mask (incl. LFE), as `ff_dca_core`
    /// builds it before any extension.
    fn core_ch_mask(&self) -> u32 {
        let mut mask = audio_mode_ch_mask(self.audio_mode);
        if self.lfe_present != 0 {
            mask |= 1 << DCA_SPEAKER_LFE1;
        }
        mask
    }

    /// Speaker layout mask of the frame's output (incl. LFE): the primary
    /// set's, plus the XXCH set's speakers when one decoded. The core's
    /// surrounds keep their Ls/Rs slots (see `output_slot`).
    pub(crate) fn ch_mask(&self) -> u32 {
        let mut mask = self.core_ch_mask();
        if self.xxch_present {
            mask |= self.xxch_spkr_mask;
        }
        mask
    }

    /// Speaker layout mask of the frame's output as the carrier names it:
    /// [`Self::ch_mask`] with the core's surrounds under the side-surround
    /// names (Lss/Rss) when the XXCH set calls them that.
    pub(crate) fn coded_mask(&self) -> u32 {
        let mut mask = self.core_ch_mask();
        if self.xxch_present {
            if mask & (1 << DCA_SPEAKER_LS) != 0
                && self.xxch_core_mask & (1 << DCA_SPEAKER_LSS) != 0
            {
                mask = (mask & !(1 << DCA_SPEAKER_LS)) | (1 << DCA_SPEAKER_LSS);
            }
            if mask & (1 << DCA_SPEAKER_RS) != 0
                && self.xxch_core_mask & (1 << DCA_SPEAKER_RSS) != 0
            {
                mask = (mask & !(1 << DCA_SPEAKER_RS)) | (1 << DCA_SPEAKER_RSS);
            }
            mask |= self.xxch_spkr_mask;
        }
        mask
    }

    pub(crate) fn nchannels(&self) -> usize {
        self.nchannels
    }
    pub(crate) fn npcmblocks(&self) -> usize {
        self.npcmblocks
    }
    pub(crate) fn sample_rate(&self) -> u32 {
        self.sample_rate
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
