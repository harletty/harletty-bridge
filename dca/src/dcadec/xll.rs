// SPDX-License-Identifier: Apache-2.0
//
// DTS-HD Master Audio lossless (XLL) decoder. Ported from ffmpeg's dca_xll.c.
// Supports the single-frequency-band path (freq <= 96 kHz, the common BluRay
// case incl. Ex Machina's 7.1) with residual-coded channels combined against
// the fixed-point core output. Two frequency bands and embedded hierarchical
// downmix are not supported (rejected) — they don't occur in 48 kHz 7.1 MA.

use super::core::DCA_SPEAKER_L;
use super::core::DCA_SPEAKER_LSS;
use super::core::DCA_SPEAKER_R;
use super::core::DCA_SPEAKER_RSS;
use super::exss::{sampling_freq, ExssAsset};
use super::synth::CoreOutput;
use super::tables::{DMIXTABLE, DMIX_PRIMARY_NCH, INV_DMIXTABLE, XLL_REFL_COEFF};
use crate::bitstream::BitReader;

const DCA_XLL_CHANNELS_MAX: usize = 8;
const DCA_XLL_CHSETS_MAX: usize = 3;
const DCA_XLL_PRED_ORDER_MAX: usize = 16;
const DCA_SYNCWORD_XLL: u32 = 0x41A2_9547;
const FF_DCA_DMIXTABLE_OFFSET: usize = 242 - 201; // SIZE - INV_SIZE
const FF_DCA_DMIXTABLE_SIZE: usize = 242;
const FF_DCA_INV_DMIXTABLE_SIZE: usize = 201;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum XllError {
    Eagain, // no sync word (PBR mid-stream)
    Bitstream,
    Invalid(&'static str),
    Unsupported(&'static str),
}
type R<T> = Result<T, XllError>;

#[inline]
fn rb(gb: &mut BitReader, n: usize) -> R<u32> {
    gb.read_bits(n).ok_or(XllError::Bitstream)
}
#[inline]
fn rb1(gb: &mut BitReader) -> R<bool> {
    gb.read_bit().ok_or(XllError::Bitstream)
}
#[inline]
fn seek(gb: &mut BitReader, pos: usize) -> R<()> {
    if gb.seek(pos) {
        Ok(())
    } else {
        Err(XllError::Invalid("seek past end"))
    }
}

#[inline]
fn mul16(a: i32, b: i32) -> i32 {
    (((a as i64 * b as i64) + (1 << 15)) >> 16) as i32
}
#[inline]
fn norm16(a: i64) -> i32 {
    ((a + (1 << 15)) >> 16) as i32
}
#[inline]
fn clip23(a: i32) -> i32 {
    a.clamp(-(1 << 23), (1 << 23) - 1)
}
#[inline]
fn rmul15(a: i32, b: i32) -> i32 {
    (((a as i64 * b as i64) + (1 << 14)) >> 15) as i32
}

#[inline]
fn get_linear(gb: &mut BitReader, n: usize) -> R<i32> {
    if n == 0 {
        return Ok(0);
    }
    let v = rb(gb, n)?;
    Ok(((v >> 1) ^ (0u32.wrapping_sub(v & 1))) as i32)
}

#[inline]
fn get_rice(gb: &mut BitReader, k: usize) -> R<i32> {
    let q = gb.get_unary(1 << 20) as u32;
    let low = if k > 0 { rb(gb, k)? } else { 0 };
    let v = (q << k) | low;
    Ok(((v >> 1) ^ (0u32.wrapping_sub(v & 1))) as i32)
}

#[derive(Clone)]
struct XllBand {
    decor_enabled: bool,
    orig_order: [usize; DCA_XLL_CHANNELS_MAX],
    decor_coeff: [i32; DCA_XLL_CHANNELS_MAX / 2],
    adapt_pred_order: [usize; DCA_XLL_CHANNELS_MAX],
    highest_pred_order: usize,
    fixed_pred_order: [usize; DCA_XLL_CHANNELS_MAX],
    adapt_refl_coeff: [[i32; DCA_XLL_PRED_ORDER_MAX]; DCA_XLL_CHANNELS_MAX],
    dmix_embedded: bool,
    lsb_section_size: usize,
    nscalablelsbs: [usize; DCA_XLL_CHANNELS_MAX],
    bit_width_adjust: [usize; DCA_XLL_CHANNELS_MAX],
    /// `msb[ch]` and `lsb[ch]` sample buffers (per original-after-reorder channel).
    msb: Vec<Vec<i32>>,
    lsb: Vec<Vec<i32>>,
}

impl Default for XllBand {
    fn default() -> Self {
        XllBand {
            decor_enabled: false,
            orig_order: [0; DCA_XLL_CHANNELS_MAX],
            decor_coeff: [0; DCA_XLL_CHANNELS_MAX / 2],
            adapt_pred_order: [0; DCA_XLL_CHANNELS_MAX],
            highest_pred_order: 0,
            fixed_pred_order: [0; DCA_XLL_CHANNELS_MAX],
            adapt_refl_coeff: [[0; DCA_XLL_PRED_ORDER_MAX]; DCA_XLL_CHANNELS_MAX],
            dmix_embedded: false,
            lsb_section_size: 0,
            nscalablelsbs: [0; DCA_XLL_CHANNELS_MAX],
            bit_width_adjust: [0; DCA_XLL_CHANNELS_MAX],
            msb: Vec::new(),
            lsb: Vec::new(),
        }
    }
}

#[derive(Default, Clone)]
struct XllChSet {
    nchannels: usize,
    residual_encode: u32,
    pcm_bit_res: usize,
    storage_bit_res: usize,
    freq: u32,
    primary_chset: bool,
    dmix_coeffs_present: bool,
    dmix_embedded: bool,
    dmix_type: usize,
    hier_chset: bool,
    hier_ofs: usize,
    dmix_coeff: Vec<i32>,
    dmix_scale: Vec<i32>,
    dmix_scale_inv: Vec<i32>,
    ch_mask: u32,
    ch_remap: [usize; DCA_XLL_CHANNELS_MAX],
    nfreqbands: usize,
    nabits: usize,
    band: XllBand,
    // band coding params (reused per segment)
    seg_common: bool,
    rice_code_flag: [bool; DCA_XLL_CHANNELS_MAX],
    bitalloc_hybrid_linear: [usize; DCA_XLL_CHANNELS_MAX],
    bitalloc_part_a: [usize; DCA_XLL_CHANNELS_MAX],
    bitalloc_part_b: [usize; DCA_XLL_CHANNELS_MAX],
    nsamples_part_a: [usize; DCA_XLL_CHANNELS_MAX],
    header_tail_bits: usize,
}

#[derive(Default)]
pub(crate) struct XllDecoder {
    frame_size: usize,
    nchsets: usize,
    pub(crate) nframesegs: usize,
    nsegsamples_log2: usize,
    pub(crate) nsegsamples: usize,
    nframesamples: usize,
    pub(crate) seg_size_nbits: usize,
    pub(crate) band_crc_present: u32,
    pub(crate) scalable_lsbs: bool,
    ch_mask_nbits: usize,
    fixed_lsb_width: usize,
    chset: Vec<XllChSet>,
    navi: Vec<usize>,
    nfreqbands: usize,
    nchannels: usize,
    nactivechsets: usize,
    hd_stream_id: u32,
    /// Asset-level `one_to_one_map_ch_to_spkr` flag, copied from the EXSS asset
    /// each decode. False selects the Lt/Rt-style stereo channel-set parse.
    one_to_one: bool,
    // PBR smoothing
    pbr_buffer: Vec<u8>,
    pbr_delay: u32,
    /// Output: speaker -> lossless int32 (24-bit) samples.
    pub(crate) output: Vec<Option<Vec<i32>>>,
    pub(crate) output_mask: u32,
    pub(crate) sample_rate: u32,
    pub(crate) pcm_bit_res: usize,
    // DTS:X end-of-frame extension (ffmpeg only flags it, never parses it).
    // Captured here for offline characterization; not used in the audio path.
    pub(crate) x_syncword_present: bool,
    pub(crate) x_imax_syncword_present: bool,
    /// Raw DTS:X extension payload (syncword + data) for the current frame.
    pub(crate) x_payload: Vec<u8>,
    /// Byte offset of `x_payload` within the XLL frame.
    pub(crate) x_payload_offset: usize,
    /// Decoded, speaker-unmapped waveforms from the bare XLL channel set in the
    /// DTS:X extension. These are kept separate from the 7.1 bed because the
    /// extension does not carry a one-to-one speaker map in its channel-set
    /// header.
    pub(crate) x_output: Vec<Vec<i32>>,
    pub(crate) x_pcm_bit_res: usize,
    pub(crate) x_bits_consumed: usize,
    pub(crate) x_decode_error: Option<String>,
    pub(crate) x_header_tail_bits: usize,
}

impl XllDecoder {
    pub(crate) fn new() -> Self {
        let mut s = XllDecoder::default();
        s.output = (0..32).map(|_| None).collect();
        s
    }

    pub(crate) fn reset(&mut self) {
        let out: Vec<Option<Vec<i32>>> = (0..32).map(|_| None).collect();
        *self = XllDecoder::default();
        self.output = out;
    }

    /// Parse + decode one XLL frame (`data` = full asset bytes; the XLL bytes are
    /// at `asset.xll_offset`), combining residual channels with `core`.
    pub(crate) fn decode(
        &mut self,
        data: &[u8],
        asset: &ExssAsset,
        core: Option<&CoreOutput>,
    ) -> R<()> {
        if self.hd_stream_id != asset.hd_stream_id {
            self.clear_pbr();
            self.hd_stream_id = asset.hd_stream_id;
        }
        self.one_to_one = asset.one_to_one_map_ch_to_spkr;
        let xll = &data[asset.xll_offset..asset.xll_offset + asset.xll_size];
        if self.pbr_delay > 0 || !self.pbr_buffer.is_empty() {
            self.parse_frame_pbr(xll)?;
        } else {
            self.parse_frame_no_pbr(xll, asset)?;
        }
        self.filter_frame(core)
    }

    fn clear_pbr(&mut self) {
        self.pbr_buffer.clear();
        self.pbr_delay = 0;
    }

    fn parse_frame_no_pbr(&mut self, data: &[u8], asset: &ExssAsset) -> R<()> {
        match self.parse_frame(data) {
            Ok(()) => {}
            Err(XllError::Eagain)
                if asset.xll_sync_present && asset.xll_sync_offset < data.len() =>
            {
                let d = &data[asset.xll_sync_offset..];
                if asset.xll_delay_nframes > 0 {
                    self.pbr_buffer.clear();
                    self.pbr_buffer.extend_from_slice(d);
                    self.pbr_delay = asset.xll_delay_nframes;
                    return Err(XllError::Eagain);
                }
                self.parse_frame(d)?;
            }
            Err(e) => return Err(e),
        }
        if self.frame_size > data.len() {
            return Err(XllError::Invalid("xll frame larger than data"));
        }
        if self.frame_size < data.len() {
            self.pbr_buffer.clear();
            self.pbr_buffer.extend_from_slice(&data[self.frame_size..]);
        }
        Ok(())
    }

    fn parse_frame_pbr(&mut self, data: &[u8]) -> R<()> {
        self.pbr_buffer.extend_from_slice(data);
        if self.pbr_delay > 0 {
            self.pbr_delay -= 1;
            if self.pbr_delay > 0 {
                return Err(XllError::Eagain);
            }
        }
        let buf = std::mem::take(&mut self.pbr_buffer);
        let res = self.parse_frame(&buf);
        match res {
            Ok(()) => {}
            Err(e) => {
                self.clear_pbr();
                return Err(e);
            }
        }
        if self.frame_size > buf.len() {
            self.clear_pbr();
            return Err(XllError::Invalid("xll pbr frame too large"));
        }
        if self.frame_size == buf.len() {
            self.clear_pbr();
        } else {
            self.pbr_buffer = buf[self.frame_size..].to_vec();
        }
        Ok(())
    }

    fn parse_frame(&mut self, data: &[u8]) -> R<()> {
        let mut gb = BitReader::new(data);
        self.parse_common_header(&mut gb)?;
        self.parse_sub_headers(&mut gb)?;
        self.parse_navi_table(&mut gb)?;
        self.parse_band_data(&mut gb)?;
        self.detect_x_extension(&mut gb, data);
        self.decode_x_extension_audio();
        Ok(())
    }

    /// DTS:X end-of-frame extension detection (mirrors `dca_xll.c:1060`). ffmpeg
    /// only dword-aligns, peeks the syncword, sets a profile flag, then seeks to
    /// end of frame. We do the same, but additionally retain the raw payload
    /// (syncword + remaining bytes up to `frame_size`) for offline analysis. This
    /// runs after all band data is decoded, so it never affects the PCM output.
    fn detect_x_extension(&mut self, gb: &mut BitReader, data: &[u8]) {
        self.x_syncword_present = false;
        self.x_imax_syncword_present = false;
        self.x_payload.clear();
        self.x_payload_offset = 0;
        self.x_output.clear();
        self.x_pcm_bit_res = 0;
        self.x_bits_consumed = 0;
        self.x_decode_error = None;
        self.x_header_tail_bits = 0;

        let frame_bits = self.frame_size * 8;
        let aligned = (gb.position() + 31) & !31; // FFALIGN(get_bits_count, 32)
        if frame_bits <= aligned {
            return;
        }
        gb.align_bits(32);
        match gb.show_bits(32) {
            Some(0x0200_0850) => self.x_syncword_present = true,
            Some(sw) if (sw >> 1) == (0xF140_00D0u32 >> 1) => self.x_imax_syncword_present = true,
            _ => return,
        }
        let start = gb.position() / 8;
        let end = self.frame_size.min(data.len());
        if end > start {
            self.x_payload_offset = start;
            self.x_payload.extend_from_slice(&data[start..end]);
        }
    }

    fn decode_x_extension_audio(&mut self) {
        if !self.x_syncword_present {
            return;
        }
        let payload = std::mem::take(&mut self.x_payload);
        let result = self.try_decode_x_extension_audio(&payload);
        self.x_payload = payload;
        if let Err(error) = result {
            self.x_output.clear();
            self.x_decode_error = Some(format!("{error:?}"));
        }
    }

    /// Decode the bare XLL channel set following the 22-byte DTS:X wrapper.
    ///
    /// The channel-set header is normative XLL syntax and has its own valid
    /// CRC16. It deliberately has no one-to-one speaker mapping: the four
    /// decoded waveforms therefore stay separate from the regular bed output.
    fn try_decode_x_extension_audio(&mut self, payload: &[u8]) -> R<()> {
        const X_CHSET_OFFSET: usize = 22;
        const X_CHANNELS: usize = 4;

        if payload.len() <= X_CHSET_OFFSET {
            return Err(XllError::Invalid("short XLL-X payload"));
        }
        let mut gb = BitReader::with_offset(payload, X_CHSET_OFFSET * 8);
        let mut chset = XllChSet::default();
        self.chs_parse_header(&mut gb, &mut chset, true, true)?;
        if chset.nchannels != X_CHANNELS {
            return Err(XllError::Invalid("unexpected XLL-X channel count"));
        }
        let full_mask = (1u32 << chset.nchannels) - 1;
        if chset.residual_encode != full_mask {
            return Err(XllError::Unsupported("XLL-X core-referenced channels"));
        }
        self.x_header_tail_bits = chset.header_tail_bits;

        // The bare sub-header is followed by the standard XLL navigation
        // table: one segment-size entry per inherited frame segment, followed
        // by byte alignment and CRC16. The segment data then runs up to the
        // final DWORD padding.
        let navi_payload_bits = self.nframesegs * self.seg_size_nbits;
        let navi_size = navi_payload_bits.div_ceil(8) + 2;
        let navi_start = gb.position() / 8;
        if payload.len() < navi_start + navi_size {
            return Err(XllError::Invalid("short XLL-X navigation table"));
        }
        let mut navi_reader = BitReader::with_offset(payload, navi_start * 8);
        let mut navi = Vec::with_capacity(self.nframesegs);
        for _ in 0..self.nframesegs {
            navi.push(rb(&mut navi_reader, self.seg_size_nbits)? as usize + 1);
        }
        navi_reader.align_bits(8);
        rb(&mut navi_reader, 16)?;
        let data_start = navi_start + navi_size;
        let data_bytes = navi.iter().sum::<usize>();
        // The extension carries a two-byte trailer after the channel-set data,
        // then zero to three bytes of DWORD padding.
        if data_start + data_bytes > payload.len() || payload.len() - (data_start + data_bytes) > 5
        {
            return Err(XllError::Invalid("XLL-X navigation size mismatch"));
        }
        seek(&mut gb, data_start * 8)?;

        self.chset.push(chset);
        let chset_index = self.chset.len() - 1;
        let decode_result = (|| {
            let channels = self.chset[chset_index].nchannels;
            self.chset[chset_index].band.msb = vec![vec![0i32; self.nframesamples]; channels];
            self.chset[chset_index].band.lsb = if self.chset[chset_index].band.lsb_section_size != 0
            {
                vec![vec![0i32; self.nframesamples]; channels]
            } else {
                Vec::new()
            };

            let mut band_end = gb.position();
            for (segment, &segment_bytes) in navi.iter().enumerate() {
                band_end += segment_bytes * 8;
                self.chs_parse_band_data(&mut gb, chset_index, segment, band_end)?;
                seek(&mut gb, band_end)?;
            }
            self.chs_filter_band_data(chset_index);
            if self.scalable_lsbs {
                self.chs_assemble_msbs_lsbs(chset_index);
            }
            Ok(())
        })();
        let chset = self.chset.pop().expect("temporary XLL-X channel set");
        decode_result?;

        let shift = 24usize
            .checked_sub(chset.pcm_bit_res)
            .ok_or(XllError::Invalid("XLL-X PCM resolution"))?;
        self.x_output = chset
            .band
            .msb
            .into_iter()
            .map(|samples| {
                samples
                    .into_iter()
                    .map(|sample| clip23(sample.wrapping_mul(1 << shift)))
                    .collect()
            })
            .collect();
        self.x_pcm_bit_res = chset.pcm_bit_res;
        self.x_bits_consumed = gb.position();
        Ok(())
    }

    fn parse_common_header(&mut self, gb: &mut BitReader) -> R<()> {
        if rb(gb, 32)? != DCA_SYNCWORD_XLL {
            return Err(XllError::Eagain);
        }
        let stream_ver = rb(gb, 4)? + 1;
        if stream_ver > 1 {
            return Err(XllError::Unsupported("XLL stream version"));
        }
        let header_size = rb(gb, 8)? as usize + 1;
        let frame_size_nbits = rb(gb, 5)? as usize + 1;
        self.frame_size = rb(gb, frame_size_nbits)? as usize + 1;
        self.nchsets = rb(gb, 4)? as usize + 1;
        if self.nchsets > DCA_XLL_CHSETS_MAX {
            return Err(XllError::Unsupported("too many XLL channel sets"));
        }
        let nframesegs_log2 = rb(gb, 4)? as usize;
        self.nframesegs = 1 << nframesegs_log2;
        self.nsegsamples_log2 = rb(gb, 4)? as usize;
        if self.nsegsamples_log2 == 0 {
            return Err(XllError::Invalid("too few samples per segment"));
        }
        self.nsegsamples = 1 << self.nsegsamples_log2;
        let nframesamples_log2 = self.nsegsamples_log2 + nframesegs_log2;
        self.nframesamples = 1 << nframesamples_log2;
        if self.nframesamples > 65536 {
            return Err(XllError::Invalid("too many samples per frame"));
        }
        self.seg_size_nbits = rb(gb, 5)? as usize + 1;
        self.band_crc_present = rb(gb, 2)?;
        self.scalable_lsbs = rb1(gb)?;
        self.ch_mask_nbits = rb(gb, 5)? as usize + 1;
        self.fixed_lsb_width = if self.scalable_lsbs {
            rb(gb, 4)? as usize
        } else {
            0
        };
        seek(gb, header_size * 8)
    }

    fn parse_sub_headers(&mut self, gb: &mut BitReader) -> R<()> {
        self.chset = vec![XllChSet::default(); self.nchsets];
        self.nfreqbands = 0;
        self.nchannels = 0;
        for i in 0..self.nchsets {
            let hier_ofs = self.nchannels;
            let mut c = std::mem::take(&mut self.chset[i]);
            c.hier_ofs = hier_ofs;
            self.chs_parse_header(gb, &mut c, i == 0, false)?;
            if c.nfreqbands > self.nfreqbands {
                self.nfreqbands = c.nfreqbands;
            }
            if c.hier_chset {
                self.nchannels += c.nchannels;
            }
            self.chset[i] = c;
        }
        if self.nfreqbands > 1 {
            return Err(XllError::Unsupported("XLL with 2 frequency bands"));
        }
        self.nactivechsets = self.nchsets;
        Ok(())
    }

    fn chs_parse_header(
        &mut self,
        gb: &mut BitReader,
        c: &mut XllChSet,
        is_primary: bool,
        allow_unmapped: bool,
    ) -> R<()> {
        let header_pos = gb.position();
        let header_size = rb(gb, 10)? as usize + 1;
        c.nchannels = rb(gb, 4)? as usize + 1;
        if c.nchannels > DCA_XLL_CHANNELS_MAX {
            return Err(XllError::Unsupported("too many XLL channels"));
        }
        c.residual_encode = rb(gb, c.nchannels)?;
        c.pcm_bit_res = rb(gb, 5)? as usize + 1;
        c.storage_bit_res = rb(gb, 5)? as usize + 1;
        if c.storage_bit_res != 16 && c.storage_bit_res != 20 && c.storage_bit_res != 24 {
            return Err(XllError::Unsupported("XLL storage resolution"));
        }
        if c.pcm_bit_res > c.storage_bit_res {
            return Err(XllError::Invalid("XLL pcm > storage bit res"));
        }
        c.freq = sampling_freq(rb(gb, 4)? as usize);
        if c.freq > 96000 {
            return Err(XllError::Unsupported("XLL > 96 kHz"));
        }
        if rb(gb, 2)? != 0 {
            return Err(XllError::Unsupported("XLL sampling freq modifier"));
        }
        if rb(gb, 2)? != 0 {
            return Err(XllError::Unsupported("XLL replacement set"));
        }

        // The channel-set layout depends on the asset's one_to_one_map_ch_to_spkr
        // flag (mirrors FFmpeg dca_xll.c chs_parse_header).
        if self.one_to_one && !allow_unmapped {
            // Normal one-to-one channel→speaker mapping (multichannel beds).
            c.primary_chset = rb1(gb)?;
            if c.primary_chset != is_primary {
                return Err(XllError::Invalid("first XLL chset must be primary"));
            }
            c.dmix_coeffs_present = rb1(gb)?;
            c.dmix_embedded = c.dmix_coeffs_present && rb1(gb)?;
            if c.dmix_coeffs_present && c.primary_chset {
                c.dmix_type = rb(gb, 3)? as usize;
                if c.dmix_type >= 7 {
                    return Err(XllError::Invalid("XLL primary downmix type"));
                }
            }
            c.hier_chset = rb1(gb)?;
            if !c.hier_chset && self.nchsets != 1 {
                return Err(XllError::Unsupported("XLL chset outside hierarchy"));
            }
            if c.dmix_coeffs_present {
                self.parse_dmix_coeffs(gb, c)?;
            }
            // A primary chset's embedded downmix is only used for stereo-downmix
            // requests; it is NOT undone for full multichannel output. Non-primary
            // hierarchical embedded downmix (undo_down_mix) is unsupported and
            // rejected after sub-header parsing.
            if !rb1(gb)? {
                return Err(XllError::Unsupported("disabled XLL channel mask"));
            }
            c.ch_mask = rb(gb, self.ch_mask_nbits)?;
            if c.ch_mask.count_ones() as usize != c.nchannels {
                return Err(XllError::Invalid("XLL channel mask popcount"));
            }
            let mut j = 0;
            for i in 0..self.ch_mask_nbits {
                if c.ch_mask & (1 << i) != 0 {
                    c.ch_remap[j] = i;
                    j += 1;
                }
            }
        } else {
            // Non one-to-one mapping (e.g. an Lt/Rt 2.0 set). Only the plain
            // stereo case is handled: a single 2-channel set with no custom
            // mapping coefficients, fixed to L/R.
            let mapping_coeffs_present = rb1(gb)?;
            if mapping_coeffs_present
                || (!allow_unmapped && (c.nchannels != 2 || self.nchsets != 1))
            {
                return Err(XllError::Unsupported(
                    "custom XLL channel-to-speaker mapping",
                ));
            }
            c.primary_chset = true;
            c.dmix_coeffs_present = false;
            c.dmix_embedded = false;
            c.hier_chset = allow_unmapped;
            if allow_unmapped {
                for ch in 0..c.nchannels {
                    c.ch_remap[ch] = ch;
                }
            } else {
                c.ch_mask = (1 << DCA_SPEAKER_L) | (1 << DCA_SPEAKER_R);
                c.ch_remap[0] = DCA_SPEAKER_L;
                c.ch_remap[1] = DCA_SPEAKER_R;
            }
        }

        if c.freq > 96000 {
            return Err(XllError::Unsupported("XLL extra freq bands"));
        }
        c.nfreqbands = 1;

        c.nabits = if c.storage_bit_res > 16 {
            5
        } else if c.storage_bit_res > 8 {
            4
        } else {
            3
        };
        if (self.nchsets > 1 || c.nfreqbands > 1) && c.nabits < 5 {
            c.nabits += 1;
        }

        let b = &mut c.band;
        b.decor_enabled = rb1(gb)?;
        if b.decor_enabled && c.nchannels > 1 {
            let ch_nbits = ceil_log2(c.nchannels);
            for i in 0..c.nchannels {
                b.orig_order[i] = rb(gb, ch_nbits)? as usize;
                if b.orig_order[i] >= c.nchannels {
                    return Err(XllError::Invalid("XLL original channel order"));
                }
            }
            for i in 0..c.nchannels / 2 {
                b.decor_coeff[i] = if rb1(gb)? { get_linear(gb, 7)? } else { 0 };
            }
        } else {
            for i in 0..c.nchannels {
                b.orig_order[i] = i;
            }
            for i in 0..c.nchannels / 2 {
                b.decor_coeff[i] = 0;
            }
        }

        b.highest_pred_order = 0;
        for i in 0..c.nchannels {
            b.adapt_pred_order[i] = rb(gb, 4)? as usize;
            b.highest_pred_order = b.highest_pred_order.max(b.adapt_pred_order[i]);
        }
        if b.highest_pred_order > self.nsegsamples {
            return Err(XllError::Invalid("XLL adaptive prediction order"));
        }
        for i in 0..c.nchannels {
            b.fixed_pred_order[i] = if b.adapt_pred_order[i] != 0 {
                0
            } else {
                rb(gb, 2)? as usize
            };
        }
        for i in 0..c.nchannels {
            for jj in 0..b.adapt_pred_order[i] {
                let k = get_linear(gb, 8)?;
                if k == -128 {
                    return Err(XllError::Invalid("XLL reflection coeff index"));
                }
                b.adapt_refl_coeff[i][jj] = if k < 0 {
                    -(XLL_REFL_COEFF[(-k) as usize] as i32)
                } else {
                    XLL_REFL_COEFF[k as usize] as i32
                };
            }
        }

        b.dmix_embedded = false; // band 0, dmix_embedded already rejected
        if self.scalable_lsbs {
            b.lsb_section_size = rb(gb, self.seg_size_nbits)? as usize;
            if b.lsb_section_size > self.frame_size {
                return Err(XllError::Invalid("XLL LSB section size"));
            }
            if b.lsb_section_size != 0 && self.band_crc_present > 1 {
                b.lsb_section_size += 2;
            }
            for i in 0..c.nchannels {
                b.nscalablelsbs[i] = rb(gb, 4)? as usize;
                if b.nscalablelsbs[i] != 0 && b.lsb_section_size == 0 {
                    return Err(XllError::Invalid("XLL LSB width without section"));
                }
            }
        } else {
            b.lsb_section_size = 0;
            for i in 0..c.nchannels {
                b.nscalablelsbs[i] = 0;
            }
        }
        if self.scalable_lsbs {
            for i in 0..c.nchannels {
                b.bit_width_adjust[i] = rb(gb, 4)? as usize;
            }
        } else {
            for i in 0..c.nchannels {
                b.bit_width_adjust[i] = 0;
            }
        }

        let header_end = header_pos + header_size * 8;
        if gb.position() > header_end {
            return Err(XllError::Invalid("XLL channel-set fields exceed header"));
        }
        c.header_tail_bits = header_end - gb.position();
        seek(gb, header_end)
    }

    fn parse_dmix_coeffs(&mut self, gb: &mut BitReader, c: &mut XllChSet) -> R<()> {
        let m = if c.primary_chset {
            DMIX_PRIMARY_NCH[c.dmix_type] as usize
        } else {
            c.hier_ofs
        };
        c.dmix_coeff.clear();
        c.dmix_scale.clear();
        c.dmix_scale_inv.clear();
        for _ in 0..m {
            let mut scale_inv_local = 0i32;
            if !c.primary_chset {
                let code = rb(gb, 9)? as i32;
                let sign = (code >> 8) - 1; // 0 if bit8 set, -1 otherwise
                let idx = (code & 0xff) as usize;
                if idx < FF_DCA_DMIXTABLE_OFFSET
                    || idx - FF_DCA_DMIXTABLE_OFFSET >= FF_DCA_INV_DMIXTABLE_SIZE
                {
                    return Err(XllError::Invalid("XLL downmix scale index"));
                }
                let scale = DMIXTABLE[idx] as i32;
                scale_inv_local = INV_DMIXTABLE[idx - FF_DCA_DMIXTABLE_OFFSET] as i32;
                c.dmix_scale.push((scale ^ sign) - sign);
                c.dmix_scale_inv.push((scale_inv_local ^ sign) - sign);
            }
            for _ in 0..c.nchannels {
                let code = rb(gb, 9)? as i32;
                let sign = (code >> 8) - 1;
                let idx = (code & 0xff) as usize;
                if idx >= FF_DCA_DMIXTABLE_SIZE {
                    return Err(XllError::Invalid("XLL downmix coeff index"));
                }
                let mut coeff = DMIXTABLE[idx] as i32;
                if !c.primary_chset {
                    coeff = mul16(scale_inv_local, coeff);
                }
                c.dmix_coeff.push((coeff ^ sign) - sign);
            }
        }
        Ok(())
    }

    fn parse_navi_table(&mut self, gb: &mut BitReader) -> R<()> {
        let navi_nb = self.nfreqbands * self.nframesegs * self.nchsets;
        if navi_nb > 1024 {
            return Err(XllError::Invalid("too many NAVI entries"));
        }
        self.navi.clear();
        self.navi.reserve(navi_nb);
        for band in 0..self.nfreqbands {
            for _seg in 0..self.nframesegs {
                for chs in 0..self.nchsets {
                    let mut size = 0usize;
                    if self.chset[chs].nfreqbands > band {
                        size = rb(gb, self.seg_size_nbits)? as usize;
                        if size >= self.frame_size {
                            return Err(XllError::Invalid("NAVI segment size"));
                        }
                        size += 1;
                    }
                    self.navi.push(size);
                }
            }
        }
        gb.align_bits(8);
        rb(gb, 16)?; // CRC16
        Ok(())
    }

    fn parse_band_data(&mut self, gb: &mut BitReader) -> R<()> {
        // Allocate MSB/LSB buffers for active channel sets.
        for chs in 0..self.nactivechsets {
            let nframesamples = self.nframesamples;
            let nchannels = self.chset[chs].nchannels;
            let c = &mut self.chset[chs];
            c.band.msb = vec![vec![0i32; nframesamples]; nchannels];
            c.band.lsb = if c.band.lsb_section_size != 0 {
                vec![vec![0i32; nframesamples]; nchannels]
            } else {
                Vec::new()
            };
        }

        let mut navi_pos = gb.position();
        let mut navi_idx = 0usize;
        for band in 0..self.nfreqbands {
            for seg in 0..self.nframesegs {
                for chs in 0..self.nchsets {
                    if self.chset[chs].nfreqbands > band {
                        navi_pos += self.navi[navi_idx] * 8;
                        if navi_pos > gb.remaining() + gb.position() {
                            return Err(XllError::Invalid("NAVI position"));
                        }
                        if chs < self.nactivechsets {
                            self.chs_parse_band_data(gb, chs, seg, navi_pos)?;
                        }
                        seek(gb, navi_pos)?;
                    }
                    navi_idx += 1;
                }
            }
        }
        Ok(())
    }

    fn chs_parse_band_data(
        &mut self,
        gb: &mut BitReader,
        chs: usize,
        seg: usize,
        band_data_end: usize,
    ) -> R<()> {
        let nsegsamples = self.nsegsamples;
        let nsegsamples_log2 = self.nsegsamples_log2;
        let c = &mut self.chset[chs];
        let nchannels = c.nchannels;

        // MSB coding parameters (segment 0 or when not shared with prev seg).
        if !(seg != 0 && rb1(gb)?) {
            c.seg_common = rb1(gb)?;
            let k = if c.seg_common { 1 } else { nchannels };
            for i in 0..k {
                c.rice_code_flag[i] = rb1(gb)?;
                if !c.seg_common && c.rice_code_flag[i] && rb1(gb)? {
                    c.bitalloc_hybrid_linear[i] = rb(gb, c.nabits)? as usize + 1;
                } else {
                    c.bitalloc_hybrid_linear[i] = 0;
                }
            }
            for i in 0..k {
                if seg == 0 {
                    c.bitalloc_part_a[i] = rb(gb, c.nabits)? as usize;
                    if !c.rice_code_flag[i] && c.bitalloc_part_a[i] != 0 {
                        c.bitalloc_part_a[i] += 1;
                    }
                    c.nsamples_part_a[i] = if !c.seg_common {
                        c.band.adapt_pred_order[i]
                    } else {
                        c.band.highest_pred_order
                    };
                } else {
                    c.bitalloc_part_a[i] = 0;
                    c.nsamples_part_a[i] = 0;
                }
                c.bitalloc_part_b[i] = rb(gb, c.nabits)? as usize;
                if !c.rice_code_flag[i] && c.bitalloc_part_b[i] != 0 {
                    c.bitalloc_part_b[i] += 1;
                }
            }
        }

        // Entropy codes per channel.
        for i in 0..nchannels {
            let k = if c.seg_common { 0 } else { i };
            let seg_base = seg * nsegsamples;
            let na = c.nsamples_part_a[k];
            let nb = nsegsamples - na;
            let buf = &mut c.band.msb[i];

            if !c.rice_code_flag[k] {
                for s in 0..na {
                    buf[seg_base + s] = get_linear(gb, c.bitalloc_part_a[k])?;
                }
                for s in 0..nb {
                    buf[seg_base + na + s] = get_linear(gb, c.bitalloc_part_b[k])?;
                }
            } else {
                for s in 0..na {
                    buf[seg_base + s] = get_rice(gb, c.bitalloc_part_a[k])?;
                }
                if c.bitalloc_hybrid_linear[k] != 0 {
                    let niso = rb(gb, nsegsamples_log2)? as usize;
                    for s in 0..nb {
                        buf[seg_base + na + s] = 0;
                    }
                    for _ in 0..niso {
                        let loc = rb(gb, nsegsamples_log2)? as usize;
                        if loc >= nb {
                            return Err(XllError::Invalid("isolated sample location"));
                        }
                        buf[seg_base + na + loc] = -1;
                    }
                    for s in 0..nb {
                        if buf[seg_base + na + s] != 0 {
                            buf[seg_base + na + s] = get_linear(gb, c.bitalloc_hybrid_linear[k])?;
                        } else {
                            buf[seg_base + na + s] = get_rice(gb, c.bitalloc_part_b[k])?;
                        }
                    }
                } else {
                    for s in 0..nb {
                        buf[seg_base + na + s] = get_rice(gb, c.bitalloc_part_b[k])?;
                    }
                }
            }
        }

        // LSB portion.
        if c.band.lsb_section_size != 0 {
            seek(gb, band_data_end - c.band.lsb_section_size * 8)?;
            for i in 0..nchannels {
                let w = c.band.nscalablelsbs[i];
                if w != 0 {
                    let seg_base = seg * nsegsamples;
                    let lbuf = &mut c.band.lsb[i];
                    for s in 0..nsegsamples {
                        lbuf[seg_base + s] = rb(gb, w)? as i32;
                    }
                }
            }
        }
        Ok(())
    }

    fn chs_filter_band_data(&mut self, chs: usize) {
        let nsamples = self.nframesamples;
        let c = &mut self.chset[chs];
        let b = &mut c.band;

        // Inverse adaptive or fixed prediction.
        for i in 0..c.nchannels {
            let order = b.adapt_pred_order[i];
            let buf = &mut b.msb[i];
            if order > 0 {
                let mut coeff = [0i32; DCA_XLL_PRED_ORDER_MAX];
                for j in 0..order {
                    let rc = b.adapt_refl_coeff[i][j];
                    for k in 0..(j + 1) / 2 {
                        let t1 = coeff[k];
                        let t2 = coeff[j - k - 1];
                        coeff[k] = t1 + mul16(rc, t2);
                        coeff[j - k - 1] = t2 + mul16(rc, t1);
                    }
                    coeff[j] = rc;
                }
                for j in 0..nsamples - order {
                    let mut err = 0i64;
                    for k in 0..order {
                        err += buf[j + k] as i64 * coeff[order - k - 1] as i64;
                    }
                    buf[j + order] = buf[j + order].wrapping_sub(clip23(norm16(err)));
                }
            } else {
                for _ in 0..b.fixed_pred_order[i] {
                    for k in 1..nsamples {
                        buf[k] = buf[k].wrapping_add(buf[k - 1]);
                    }
                }
            }
        }

        // Inverse pairwise channel decorrelation + reorder to original order.
        if b.decor_enabled {
            for i in 0..c.nchannels / 2 {
                let coeff = b.decor_coeff[i];
                if coeff != 0 {
                    // dst = msb[2i+1], src = msb[2i]
                    for n in 0..nsamples {
                        let s = b.msb[i * 2][n];
                        b.msb[i * 2 + 1][n] = b.msb[i * 2 + 1][n]
                            .wrapping_add((s.wrapping_mul(coeff) + (1 << 2)) >> 3);
                    }
                }
            }
            // Permute msb so that msb[orig_order[i]] = decoded[i].
            let decoded = std::mem::take(&mut b.msb);
            let mut reordered: Vec<Vec<i32>> = vec![Vec::new(); c.nchannels];
            let mut src = decoded.into_iter();
            for i in 0..c.nchannels {
                let v = src.next().unwrap();
                reordered[b.orig_order[i]] = v;
            }
            b.msb = reordered;
        }
    }

    fn chs_get_lsb_width(&self, chs: usize, ch: usize) -> usize {
        let c = &self.chset[chs];
        let adj = c.band.bit_width_adjust[ch];
        let mut shift = c.band.nscalablelsbs[ch];
        if self.fixed_lsb_width != 0 {
            shift = self.fixed_lsb_width;
        } else if shift != 0 && adj != 0 {
            shift += adj - 1;
        } else {
            shift += adj;
        }
        shift
    }

    fn chs_assemble_msbs_lsbs(&mut self, chs: usize) {
        let nsamples = self.nframesamples;
        for ch in 0..self.chset[chs].nchannels {
            let shift = self.chs_get_lsb_width(chs, ch);
            if shift == 0 {
                continue;
            }
            let c = &mut self.chset[chs];
            let has_lsb = c.band.nscalablelsbs[ch] != 0;
            let adj = c.band.bit_width_adjust[ch];
            if has_lsb {
                for n in 0..nsamples {
                    let msb = c.band.msb[ch][n];
                    let lsb = c.band.lsb[ch][n];
                    c.band.msb[ch][n] = msb.wrapping_mul(1 << shift) + (lsb << adj);
                }
            } else {
                for n in 0..nsamples {
                    c.band.msb[ch][n] = c.band.msb[ch][n].wrapping_mul(1 << shift);
                }
            }
        }
    }

    /// `find_next_hier_dmix_chset`: the next hierarchical embedded-downmix set
    /// after `chs` (if `chs` is itself part of the hierarchy).
    fn find_next_hier_dmix_chset(&self, chs: usize) -> Option<usize> {
        if !self.chset[chs].hier_chset {
            return None;
        }
        ((chs + 1)..self.nchsets).find(|&i| is_hier_dmix_chset(&self.chset[i]))
    }

    /// `undo_down_mix`: subtract channel set `o`'s embedded downmix contribution
    /// from the preceding hierarchy channel sets (band 0 only here).
    fn undo_down_mix(&mut self, o_idx: usize) {
        let nsamples = self.nframesamples;
        let o_nch = self.chset[o_idx].nchannels;
        let o_hier_ofs = self.chset[o_idx].hier_ofs;
        // Snapshot o's channel buffers (small: 2 channels).
        let o_msb: Vec<Vec<i32>> = self.chset[o_idx].band.msb.clone();
        let coeff = self.chset[o_idx].dmix_coeff.clone();

        let mut coeff_idx = 0usize;
        let mut nchannels = 0usize;
        for c_idx in 0..self.nactivechsets {
            if !self.chset[c_idx].hier_chset {
                continue;
            }
            let c_nch = self.chset[c_idx].nchannels;
            for j in 0..c_nch {
                for k in 0..o_nch {
                    let cf = coeff[coeff_idx];
                    coeff_idx += 1;
                    if cf != 0 {
                        let dst = &mut self.chset[c_idx].band.msb[j];
                        let src = &o_msb[k];
                        for n in 0..nsamples {
                            dst[n] = dst[n].wrapping_sub(rmul15(src[n], cf));
                        }
                    }
                }
            }
            nchannels += c_nch;
            if nchannels >= o_hier_ofs {
                break;
            }
        }
    }

    fn combine_residual_frame(&mut self, chs: usize, core: &CoreOutput) -> R<()> {
        let nsamples = self.nframesamples;
        let c_freq = self.chset[chs].freq;
        if c_freq != core.output_rate {
            return Err(XllError::Invalid("core/XLL sample rate mismatch"));
        }
        if nsamples != core.npcmsamples {
            return Err(XllError::Invalid("core/XLL sample count mismatch"));
        }
        let nchannels = self.chset[chs].nchannels;
        let hier_ofs = self.chset[chs].hier_ofs;
        // If this set is downmixed into by a following hierarchical dmix set, the
        // core must be un-prescaled before combining (the encoder pre-scaled the
        // embedded core downmix). dmix_scale_inv comes from that following set.
        let o_scale_inv: Option<Vec<i32>> = self
            .find_next_hier_dmix_chset(chs)
            .map(|o| self.chset[o].dmix_scale_inv.clone());

        for ch in 0..nchannels {
            if self.chset[chs].residual_encode & (1 << ch) != 0 {
                continue;
            }
            let spkr = map_core_spkr(core.ch_mask, self.chset[chs].ch_remap[ch]).ok_or(
                XllError::Invalid("residual references missing core channel"),
            )?;
            let shift = 24 - self.chset[chs].pcm_bit_res + self.chs_get_lsb_width(chs, ch);
            if shift > 24 {
                return Err(XllError::Invalid("invalid core shift"));
            }
            let round = if shift > 0 { 1 << (shift - 1) } else { 0 };
            let src = core.samples[spkr]
                .as_ref()
                .ok_or(XllError::Invalid("missing core speaker samples"))?;
            let dst = &mut self.chset[chs].band.msb[ch];
            if let Some(scale_inv) = &o_scale_inv {
                let si = scale_inv[hier_ofs + ch];
                for n in 0..nsamples {
                    dst[n] = dst[n].wrapping_add(clip23((mul16(src[n], si) + round) >> shift));
                }
            } else {
                for n in 0..nsamples {
                    dst[n] = dst[n].wrapping_add((src[n] + round) >> shift);
                }
            }
        }
        Ok(())
    }

    fn filter_frame(&mut self, core: Option<&CoreOutput>) -> R<()> {
        for o in self.output.iter_mut() {
            *o = None;
        }
        self.output_mask = 0;

        let p_freq = self.chset[0].freq;
        let p_pcm_bit_res = self.chset[0].pcm_bit_res;
        let p_storage = self.chset[0].storage_bit_res;

        for chs in 0..self.nactivechsets {
            self.chs_filter_band_data(chs);

            let full = (1u32 << self.chset[chs].nchannels) - 1;
            if self.chset[chs].residual_encode != full {
                let core = core.ok_or(XllError::Invalid("residual channels without core"))?;
                self.combine_residual_frame(chs, core)?;
            }
            if self.scalable_lsbs {
                self.chs_assemble_msbs_lsbs(chs);
            }
            self.output_mask |= self.chset[chs].ch_mask;
        }

        // Undo hierarchical embedded downmix: subtract each non-primary dmix
        // channel set's contribution from the preceding hierarchy channels.
        for o_idx in 1..self.nchsets {
            if o_idx >= self.nactivechsets {
                break;
            }
            if is_hier_dmix_chset(&self.chset[o_idx]) {
                self.undo_down_mix(o_idx);
            }
        }

        // Map output: speaker -> channel buffer, with the Lss/Rss -> Ls/Rs
        // normalization ffmpeg applies for a regular 5.1/7.1 layout.
        //
        // `output` is a 24-bit container (see its doc) and hd.rs normalizes f32 by
        // 2^23, so always upshift to 24-bit regardless of storage resolution. The
        // 16-bit-storage path used to stop at a 16-bit scale, which made 16-bit
        // DTS-HD MA masters play ~48 dB (1/256) too quiet. storage_bit_res is
        // already validated to 16/20/24 at parse time.
        let shift = match p_storage {
            16 | 20 | 24 => 24 - p_pcm_bit_res,
            _ => return Err(XllError::Invalid("XLL storage bit res")),
        };
        for chs in 0..self.nactivechsets {
            let nchannels = self.chset[chs].nchannels;
            for ch in 0..nchannels {
                let spkr = self.chset[chs].ch_remap[ch];
                let buf = std::mem::take(&mut self.chset[chs].band.msb[ch]);
                let scaled: Vec<i32> = buf
                    .iter()
                    .map(|&s| clip23(s.wrapping_mul(1 << shift)))
                    .collect();
                self.output[spkr] = Some(scaled);
            }
        }
        // Normalize side-surround to Ls/Rs slots if present.
        if self.output[DCA_SPEAKER_LSS].is_some() {
            self.output[3] = self.output[DCA_SPEAKER_LSS].take();
            self.output_mask = (self.output_mask & !(1 << DCA_SPEAKER_LSS)) | (1 << 3);
        }
        if self.output[DCA_SPEAKER_RSS].is_some() {
            self.output[4] = self.output[DCA_SPEAKER_RSS].take();
            self.output_mask = (self.output_mask & !(1 << DCA_SPEAKER_RSS)) | (1 << 4);
        }

        self.sample_rate = p_freq;
        self.pcm_bit_res = p_pcm_bit_res;
        Ok(())
    }
}

fn is_hier_dmix_chset(c: &XllChSet) -> bool {
    !c.primary_chset && c.dmix_embedded && c.hier_chset
}

/// `ff_dca_core_map_spkr` equivalent against a known core ch_mask.
fn map_core_spkr(core_mask: u32, spkr: usize) -> Option<usize> {
    use super::core::{DCA_SPEAKER_LS, DCA_SPEAKER_RS};
    const MASK_LS: u32 = 1 << DCA_SPEAKER_LS;
    const MASK_RS: u32 = 1 << DCA_SPEAKER_RS;
    if spkr < 32 && core_mask & (1 << spkr) != 0 {
        return Some(spkr);
    }
    if spkr == DCA_SPEAKER_LSS && core_mask & MASK_LS != 0 {
        return Some(DCA_SPEAKER_LS);
    }
    if spkr == DCA_SPEAKER_RSS && core_mask & MASK_RS != 0 {
        return Some(DCA_SPEAKER_RS);
    }
    None
}

fn ceil_log2(n: usize) -> usize {
    if n <= 1 {
        0
    } else {
        (usize::BITS - (n - 1).leading_zeros()) as usize
    }
}
