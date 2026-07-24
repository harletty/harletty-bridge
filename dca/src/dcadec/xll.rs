// SPDX-License-Identifier: Apache-2.0
//
// DTS-HD Master Audio lossless (XLL) decoder. Ported from ffmpeg's dca_xll.c.
// Supports the single-frequency-band path (freq <= 96 kHz, the common BluRay
// 7.1 case) with residual-coded channels combined against
// the fixed-point core output. Two frequency bands and embedded hierarchical
// downmix are not supported (rejected) — they don't occur in 48 kHz 7.1 MA.

use super::core::DCA_SPEAKER_L;
use super::core::DCA_SPEAKER_LSS;
use super::core::DCA_SPEAKER_R;
use super::core::DCA_SPEAKER_RSS;
use super::exss::{ExssAsset, sampling_freq};
use super::synth::CoreOutput;
use super::tables::{DMIX_PRIMARY_NCH, DMIXTABLE, INV_DMIXTABLE, XLL_REFL_COEFF};
use crate::bitstream::BitReader;

const DCA_XLL_CHANNELS_MAX: usize = 8;
const DCA_XLL_CHSETS_MAX: usize = 3;
const DCA_XLL_PRED_ORDER_MAX: usize = 16;
/// XLL-X (DTS:X spatial extension) end-of-frame syncword.
const DCA_SYNCWORD_XLL_X: u32 = 0x0200_0850;
const DCA_SYNCWORD_XLL_X_ALT_D0: u32 = 0xF140_00D0;
const DCA_SYNCWORD_XLL_X_ALT_D1: u32 = 0xF140_00D1;
const DCA_SYNCWORD_XLL_X_ALT_D3: u32 = 0xF140_00D3;
const DCA_SYNCWORD_XLL: u32 = 0x41A2_9547;
const XLL_X_ALT_FRAME_SAMPLES: usize = 512;
const XLL_X_ALT_MAX_SEGMENTS: usize = 8;
const XLL_X_ALT_MAX_INTERSTITIAL: usize = 20;
const XLL_X_ALT_OUTER_SUFFIX: [u8; 6] = [0x03, 0x34, 0x38, 0x8c, 0x4f, 0x00];
const XLL_X_ALT_INNER_SUFFIX: [u8; 6] = [0x02, 0x34, 0x38, 0x8c, 0x4f, 0x00];
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

#[derive(Clone, Copy)]
struct AlternateGeometry {
    segments: usize,
    navigation_size_bits: usize,
}

#[derive(Clone, Copy)]
struct AlternateHeader {
    offset: usize,
    size: usize,
    channels: usize,
}

#[derive(Clone, Copy)]
enum AlternateProfile {
    D0,
    D1,
    D3,
}

struct AlternateLayout {
    headers: [AlternateHeader; 2],
    geometries: [AlternateGeometry; 2],
}

/// Allocation-free kind string for a failed XLL-X decode (the audio path
/// never formats errors per frame; the bridge logs this rate-limited).
fn xll_x_error_kind(error: &XllError) -> &'static str {
    match error {
        XllError::Eagain => "eagain",
        XllError::Bitstream => "bitstream",
        XllError::Invalid(what) | XllError::Unsupported(what) => what,
    }
}

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

fn crc16_ccitt(data: &[u8]) -> u16 {
    let mut crc = 0xffffu16;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn alternate_geometry_at(control: &[u8], bit_offset: usize) -> Option<AlternateGeometry> {
    let mut bits = BitReader::with_offset(control, bit_offset);
    let segment_log2 = rb(&mut bits, 4).ok()?;
    let segment_samples_log2 = rb(&mut bits, 4).ok()?;
    let navigation_size_bits = rb(&mut bits, 5).ok()? as usize + 1;
    let segments = 1usize.checked_shl(segment_log2)?;
    let segment_samples = 1usize.checked_shl(segment_samples_log2)?;
    if segments <= XLL_X_ALT_MAX_SEGMENTS
        && segments.checked_mul(segment_samples) == Some(XLL_X_ALT_FRAME_SAMPLES)
        && (4..=20).contains(&navigation_size_bits)
    {
        Some(AlternateGeometry {
            segments,
            navigation_size_bits,
        })
    } else {
        None
    }
}

fn alternate_unique_geometry(
    control: &[u8],
    first_bit: usize,
    last_bit: usize,
) -> R<(usize, AlternateGeometry)> {
    let mut selected = None;
    for bit_offset in first_bit..=last_bit {
        if let Some(geometry) = alternate_geometry_at(control, bit_offset) {
            if selected.is_some() {
                return Err(XllError::Invalid("ambiguous alternate XLL control"));
            }
            selected = Some((bit_offset, geometry));
        }
    }
    selected.ok_or(XllError::Invalid("missing alternate XLL geometry"))
}

fn alternate_header_at(payload: &[u8], byte_offset: usize) -> Option<AlternateHeader> {
    let mut bits = BitReader::with_offset(payload, byte_offset.checked_mul(8)?);
    let header_size = rb(&mut bits, 10).ok()? as usize + 1;
    let channels = rb(&mut bits, 4).ok()? as usize + 1;
    if channels > DCA_XLL_CHANNELS_MAX || rb(&mut bits, channels).is_err() {
        return None;
    }
    let pcm_resolution = rb(&mut bits, 5).ok()? as usize + 1;
    let storage_resolution = rb(&mut bits, 5).ok()? as usize + 1;
    let frequency_index = rb(&mut bits, 4).ok()?;
    let frequency_modifier = rb(&mut bits, 2).ok()?;
    let replacement_set = rb(&mut bits, 2).ok()?;
    let header_end = byte_offset.checked_add(header_size)?;
    if header_end > payload.len()
        || pcm_resolution > storage_resolution
        || !matches!(storage_resolution, 16 | 20 | 24)
        || frequency_index != 12
        || frequency_modifier != 0
        || replacement_set != 0
        || crc16_ccitt(payload.get(byte_offset..header_end)?) != 0
    {
        return None;
    }
    Some(AlternateHeader {
        offset: byte_offset,
        size: header_size,
        channels,
    })
}

fn alternate_second_header(
    payload: &[u8],
    control: &[u8],
    common_bit: usize,
    offset_bias: usize,
) -> R<AlternateHeader> {
    let field_width = common_bit
        .checked_sub(14)
        .ok_or(XllError::Invalid("alternate XLL size field"))?;
    let mut bits = BitReader::with_offset(control, 9);
    let encoded_span = rb(&mut bits, field_width)? as usize;
    let nominal = encoded_span
        .checked_mul(2)
        .and_then(|span| span.checked_add(offset_bias))
        .ok_or(XllError::Invalid("alternate XLL header offset overflow"))?;
    let mut selected = None;
    for offset in [Some(nominal), nominal.checked_sub(1)]
        .into_iter()
        .flatten()
    {
        let Some(header) = alternate_header_at(payload, offset) else {
            continue;
        };
        if header.channels != 4 {
            continue;
        }
        if selected.is_some() {
            return Err(XllError::Invalid("ambiguous alternate XLL header"));
        }
        selected = Some(header);
    }
    selected.ok_or(XllError::Invalid("missing alternate XLL second header"))
}

fn alternate_inner_control(payload: &[u8], second_header_offset: usize) -> R<&[u8]> {
    let search_start = second_header_offset.saturating_sub(24);
    let window = payload
        .get(search_start..second_header_offset)
        .ok_or(XllError::Invalid("short alternate XLL interstitial"))?;
    let prefix_offset = window
        .windows(XLL_X_ALT_INNER_SUFFIX.len())
        .rposition(|bytes| bytes == XLL_X_ALT_INNER_SUFFIX)
        .ok_or(XllError::Invalid("missing alternate XLL inner suffix"))?;
    let control_start = prefix_offset + XLL_X_ALT_INNER_SUFFIX.len();
    let control = window
        .get(control_start..)
        .ok_or(XllError::Invalid("short alternate XLL inner control"))?;
    if !(8..=9).contains(&control.len()) {
        return Err(XllError::Invalid("alternate XLL inner control size"));
    }
    Ok(control)
}

fn alternate_outer_layout(payload: &[u8], profile: AlternateProfile) -> R<(usize, usize, usize)> {
    const MAX_PREFIX_BYTES: usize = 96;

    let minimum_prefix = match profile {
        AlternateProfile::D0 => 48,
        AlternateProfile::D1 | AlternateProfile::D3 => 49,
    };
    let search_end = payload.len().min(MAX_PREFIX_BYTES);
    let mut prefix_end = None;
    for candidate in minimum_prefix..search_end {
        let Some(suffix_end) = candidate.checked_add(XLL_X_ALT_OUTER_SUFFIX.len()) else {
            break;
        };
        if suffix_end > payload.len() {
            break;
        }
        if payload.get(candidate..suffix_end) != Some(&XLL_X_ALT_OUTER_SUFFIX)
            || crc16_ccitt(&payload[..candidate]) != 0
        {
            continue;
        }
        if prefix_end.replace(candidate).is_some() {
            return Err(XllError::Invalid("ambiguous alternate XLL prefix"));
        }
    }
    let prefix_end = prefix_end.ok_or(XllError::Invalid("alternate XLL prefix CRC"))?;
    let control_start = prefix_end
        .checked_add(XLL_X_ALT_OUTER_SUFFIX.len())
        .ok_or(XllError::Invalid("alternate XLL control offset overflow"))?;
    let control_size = match payload.get(control_start) {
        Some(0xb2) => 7,
        Some(0xc2..=0xc6) => 8,
        _ => {
            return Err(match profile {
                AlternateProfile::D0 => XllError::Invalid("alternate D0 control tag"),
                AlternateProfile::D1 => XllError::Invalid("alternate D1 control tag"),
                AlternateProfile::D3 => XllError::Invalid("alternate D3 control tag"),
            });
        }
    };
    Ok((control_start, control_size, control_start + 12))
}

fn alternate_layout(payload: &[u8]) -> R<AlternateLayout> {
    let syncword = payload
        .get(..4)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u32::from_be_bytes)
        .ok_or(XllError::Invalid("short alternate XLL payload"))?;
    let profile = match syncword {
        DCA_SYNCWORD_XLL_X_ALT_D0 => AlternateProfile::D0,
        DCA_SYNCWORD_XLL_X_ALT_D1 => AlternateProfile::D1,
        DCA_SYNCWORD_XLL_X_ALT_D3 => AlternateProfile::D3,
        _ => return Err(XllError::Invalid("unknown alternate XLL profile")),
    };
    let (control_start, control_size, offset_bias) = alternate_outer_layout(payload, profile)?;
    let first_header_offset = control_start
        .checked_add(control_size)
        .ok_or(XllError::Invalid("alternate XLL header offset overflow"))?;
    let outer_control = payload
        .get(control_start..first_header_offset)
        .ok_or(XllError::Invalid("short alternate XLL outer control"))?;
    let first_geometry_end = match profile {
        AlternateProfile::D3 => 31,
        AlternateProfile::D0 | AlternateProfile::D1 => 25,
    };
    let (common_bit, first_geometry) =
        alternate_unique_geometry(outer_control, 18, first_geometry_end)?;
    let first_header = alternate_header_at(payload, first_header_offset)
        .ok_or(XllError::Invalid("invalid alternate XLL first header"))?;
    let expected_first_channels = match profile {
        AlternateProfile::D0 => 1,
        AlternateProfile::D1 => 2,
        AlternateProfile::D3 => 4,
    };
    if first_header.channels != expected_first_channels {
        return Err(XllError::Invalid(
            "unexpected alternate XLL first channel count",
        ));
    }
    let second_header = alternate_second_header(payload, outer_control, common_bit, offset_bias)?;
    let inner_control = alternate_inner_control(payload, second_header.offset)?;
    let second_geometry_start = match profile {
        AlternateProfile::D0 => 18,
        AlternateProfile::D1 | AlternateProfile::D3 => 19,
    };
    let (_, second_geometry) = alternate_unique_geometry(inner_control, second_geometry_start, 26)?;
    Ok(AlternateLayout {
        headers: [first_header, second_header],
        geometries: [first_geometry, second_geometry],
    })
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

#[derive(Clone, Copy)]
enum ChsMappingSyntax {
    Asset {
        is_primary: bool,
        allow_unmapped: bool,
    },
    AlternatePrefix(usize),
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
    pub(crate) x_syncword_present: bool,
    pub(crate) x_imax_syncword_present: bool,
    /// Raw DTS:X extension payload (syncword + data) for the current frame.
    pub(crate) x_payload: Vec<u8>,
    /// Byte offset of `x_payload` within the XLL frame.
    pub(crate) x_payload_offset: usize,
    /// Decoded, speaker-unmapped waveforms from the bare extension channel
    /// sets. These stay separate from the lossless bed; presentation is a
    /// bridge concern and requires independently established semantics.
    pub(crate) x_output: Vec<Vec<i32>>,
    pub(crate) x_pcm_bit_res: usize,
    pub(crate) x_bits_consumed: usize,
    /// Allocation-free failure kind of the current XLL-X decode attempt.
    pub(crate) x_decode_error: Option<&'static str>,
    pub(crate) x_header_tail_bits: usize,
    x_descriptor_offset: Option<usize>,
    x_descriptor_size: Option<usize>,
    pub(crate) x_descriptor_navigation_used: bool,
}

#[derive(Clone, Copy)]
struct AlternateDecoderState {
    frame_size: usize,
    nchsets: usize,
    nframesegs: usize,
    nsegsamples_log2: usize,
    nsegsamples: usize,
    nframesamples: usize,
    seg_size_nbits: usize,
    band_crc_present: u32,
    scalable_lsbs: bool,
    ch_mask_nbits: usize,
    fixed_lsb_width: usize,
    nfreqbands: usize,
    nchannels: usize,
    nactivechsets: usize,
    one_to_one: bool,
}

impl AlternateDecoderState {
    fn capture(decoder: &XllDecoder) -> Self {
        Self {
            frame_size: decoder.frame_size,
            nchsets: decoder.nchsets,
            nframesegs: decoder.nframesegs,
            nsegsamples_log2: decoder.nsegsamples_log2,
            nsegsamples: decoder.nsegsamples,
            nframesamples: decoder.nframesamples,
            seg_size_nbits: decoder.seg_size_nbits,
            band_crc_present: decoder.band_crc_present,
            scalable_lsbs: decoder.scalable_lsbs,
            ch_mask_nbits: decoder.ch_mask_nbits,
            fixed_lsb_width: decoder.fixed_lsb_width,
            nfreqbands: decoder.nfreqbands,
            nchannels: decoder.nchannels,
            nactivechsets: decoder.nactivechsets,
            one_to_one: decoder.one_to_one,
        }
    }

    fn restore(self, decoder: &mut XllDecoder) {
        decoder.frame_size = self.frame_size;
        decoder.nchsets = self.nchsets;
        decoder.nframesegs = self.nframesegs;
        decoder.nsegsamples_log2 = self.nsegsamples_log2;
        decoder.nsegsamples = self.nsegsamples;
        decoder.nframesamples = self.nframesamples;
        decoder.seg_size_nbits = self.seg_size_nbits;
        decoder.band_crc_present = self.band_crc_present;
        decoder.scalable_lsbs = self.scalable_lsbs;
        decoder.ch_mask_nbits = self.ch_mask_nbits;
        decoder.fixed_lsb_width = self.fixed_lsb_width;
        decoder.nfreqbands = self.nfreqbands;
        decoder.nchannels = self.nchannels;
        decoder.nactivechsets = self.nactivechsets;
        decoder.one_to_one = self.one_to_one;
    }
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
        self.x_descriptor_offset = asset.xll_x_offset;
        self.x_descriptor_size = asset.xll_x_size;
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
        self.x_descriptor_navigation_used = false;

        let frame_end = self.frame_size.min(data.len());
        let frame_bits = frame_end * 8;
        let aligned = (gb.position() + 31) & !31; // FFALIGN(get_bits_count, 32)
        if frame_bits <= aligned {
            return;
        }
        let mut start = aligned / 8;

        // The profile-specific asset-descriptor word provides the XLL-X size
        // and its DWORD offset. Prefer that navigation when its
        // markers, bounds and syncword all agree; retain the legacy aligned
        // band-end probe as a fallback for other profiles.
        if let (Some(offset), Some(size)) = (self.x_descriptor_offset, self.x_descriptor_size) {
            if let Some(hinted_start) = frame_end.checked_sub(size) {
                if hinted_start == offset
                    && data.get(hinted_start..hinted_start + 4)
                        == Some(&DCA_SYNCWORD_XLL_X.to_be_bytes())
                {
                    start = hinted_start;
                    self.x_descriptor_navigation_used = true;
                }
            }
        }
        if !gb.seek(start * 8) {
            return;
        }
        match gb.show_bits(32) {
            Some(DCA_SYNCWORD_XLL_X) => self.x_syncword_present = true,
            Some(
                DCA_SYNCWORD_XLL_X_ALT_D0 | DCA_SYNCWORD_XLL_X_ALT_D1 | DCA_SYNCWORD_XLL_X_ALT_D3,
            ) => self.x_imax_syncword_present = true,
            _ => return,
        }
        if frame_end > start {
            self.x_payload_offset = start;
            self.x_payload.extend_from_slice(&data[start..frame_end]);
        }
    }

    fn decode_x_extension_audio(&mut self) {
        if !self.x_syncword_present && !self.x_imax_syncword_present {
            return;
        }
        let payload = std::mem::take(&mut self.x_payload);
        let result = if self.x_syncword_present {
            self.try_decode_x_extension_audio(&payload)
        } else {
            self.try_decode_alternate_x_extension_audio(&payload)
        };
        self.x_payload = payload;
        if let Err(error) = result {
            self.x_output.clear();
            self.x_decode_error = Some(xll_x_error_kind(&error));
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
        let chset = self
            .chset
            .pop()
            .ok_or(XllError::Invalid("missing temporary XLL-X channel set"))?;
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

    /// Decode the two bounded channel sets carried by the alternate extension
    /// profiles. Their controls provide the per-set segment geometry; neither
    /// set inherits the lossless bed's common XLL geometry.
    fn try_decode_alternate_x_extension_audio(&mut self, payload: &[u8]) -> R<()> {
        let layout = alternate_layout(payload)?;
        let first = self.decode_alternate_channel_set(
            payload,
            layout.headers[0],
            layout.geometries[0],
            layout.headers[1].offset,
        )?;
        let second = self.decode_alternate_channel_set(
            payload,
            layout.headers[1],
            layout.geometries[1],
            payload.len(),
        )?;
        if first.0.pcm_bit_res != second.0.pcm_bit_res {
            return Err(XllError::Unsupported("mixed alternate XLL PCM resolutions"));
        }
        let pcm_bit_res = first.0.pcm_bit_res;
        let shift = 24usize
            .checked_sub(pcm_bit_res)
            .ok_or(XllError::Invalid("alternate XLL PCM resolution"))?;
        let scale = 1i32
            .checked_shl(shift as u32)
            .ok_or(XllError::Invalid("alternate XLL PCM scale"))?;

        self.x_output.clear();
        self.x_output
            .reserve(first.0.nchannels + second.0.nchannels);
        for mut samples in first.0.band.msb.into_iter().chain(second.0.band.msb) {
            for sample in &mut samples {
                *sample = clip23(sample.wrapping_mul(scale));
            }
            self.x_output.push(samples);
        }
        self.x_pcm_bit_res = pcm_bit_res;
        self.x_bits_consumed = second.1;
        // This legacy diagnostic describes the single standard-profile header;
        // do not overload it with one of the alternate profile's two headers.
        self.x_header_tail_bits = 0;
        Ok(())
    }

    fn decode_alternate_channel_set(
        &mut self,
        payload: &[u8],
        header: AlternateHeader,
        geometry: AlternateGeometry,
        boundary: usize,
    ) -> R<(XllChSet, usize)> {
        let saved = AlternateDecoderState::capture(self);
        let result = self.decode_alternate_channel_set_inner(payload, header, geometry, boundary);
        saved.restore(self);
        result
    }

    fn decode_alternate_channel_set_inner(
        &mut self,
        payload: &[u8],
        header: AlternateHeader,
        geometry: AlternateGeometry,
        boundary: usize,
    ) -> R<(XllChSet, usize)> {
        let header_end = header
            .offset
            .checked_add(header.size)
            .ok_or(XllError::Invalid("alternate XLL header size overflow"))?;
        if boundary > payload.len() || header_end > boundary {
            return Err(XllError::Invalid("alternate XLL channel-set bounds"));
        }
        let boundary_bits = boundary
            .checked_mul(8)
            .ok_or(XllError::Invalid("alternate XLL boundary overflow"))?;
        if !geometry.segments.is_power_of_two() || XLL_X_ALT_FRAME_SAMPLES % geometry.segments != 0
        {
            return Err(XllError::Invalid("alternate XLL segment count"));
        }
        let segment_samples = XLL_X_ALT_FRAME_SAMPLES / geometry.segments;
        if !segment_samples.is_power_of_two() {
            return Err(XllError::Invalid("alternate XLL segment samples"));
        }

        self.frame_size = payload.len();
        self.nchsets = 1;
        self.nactivechsets = 1;
        self.nframesegs = geometry.segments;
        self.nsegsamples = segment_samples;
        self.nsegsamples_log2 = segment_samples.ilog2() as usize;
        self.nframesamples = XLL_X_ALT_FRAME_SAMPLES;
        self.seg_size_nbits = geometry.navigation_size_bits;
        self.band_crc_present = 0;
        self.scalable_lsbs = false;
        self.ch_mask_nbits = 1;
        self.fixed_lsb_width = 0;
        self.nfreqbands = 1;
        self.nchannels = header.channels;
        self.one_to_one = false;

        let mut bits = BitReader::with_offset(payload, header.offset * 8);
        let mut channel_set = XllChSet::default();
        self.chs_parse_header_with_mapping(
            &mut bits,
            &mut channel_set,
            ChsMappingSyntax::AlternatePrefix(2),
        )?;
        let navi_start = header
            .offset
            .checked_add(header.size)
            .ok_or(XllError::Invalid("alternate XLL NAVI offset overflow"))?;
        if bits.position() != navi_start * 8
            || channel_set.nchannels != header.channels
            || channel_set.residual_encode != (1u32 << header.channels) - 1
        {
            return Err(XllError::Invalid("alternate XLL channel-set header"));
        }

        let navi_payload_bits = geometry
            .segments
            .checked_mul(geometry.navigation_size_bits)
            .ok_or(XllError::Invalid("alternate XLL NAVI size overflow"))?;
        let navi_size = navi_payload_bits.div_ceil(8) + 2;
        let navi_end = navi_start
            .checked_add(navi_size)
            .filter(|&end| end <= boundary)
            .ok_or(XllError::Invalid("short alternate XLL NAVI"))?;
        if crc16_ccitt(&payload[navi_start..navi_end]) != 0 {
            return Err(XllError::Invalid("alternate XLL NAVI CRC"));
        }
        let mut navigation = [0usize; XLL_X_ALT_MAX_SEGMENTS];
        let mut navi_reader = BitReader::with_offset(payload, navi_start * 8);
        let mut audio_bytes = 0usize;
        for slot in navigation.iter_mut().take(geometry.segments) {
            *slot = rb(&mut navi_reader, geometry.navigation_size_bits)? as usize + 1;
            audio_bytes = audio_bytes
                .checked_add(*slot)
                .ok_or(XllError::Invalid("alternate XLL audio size overflow"))?;
        }
        let audio_end = navi_end
            .checked_add(audio_bytes)
            .filter(|&end| end <= boundary)
            .ok_or(XllError::Invalid("alternate XLL audio exceeds boundary"))?;
        if boundary - audio_end > XLL_X_ALT_MAX_INTERSTITIAL {
            return Err(XllError::Invalid("alternate XLL interstitial too large"));
        }

        channel_set.band.msb = vec![vec![0i32; XLL_X_ALT_FRAME_SAMPLES]; header.channels];
        channel_set.band.lsb = if channel_set.band.lsb_section_size != 0 {
            vec![vec![0i32; XLL_X_ALT_FRAME_SAMPLES]; header.channels]
        } else {
            Vec::new()
        };
        self.chset.push(channel_set);
        let channel_set_index = self.chset.len() - 1;
        let decode_result = (|| {
            seek(&mut bits, navi_end * 8)?;
            let mut band_end = bits.position();
            for (segment, &segment_bytes) in navigation.iter().take(geometry.segments).enumerate() {
                band_end = band_end
                    .checked_add(
                        segment_bytes
                            .checked_mul(8)
                            .ok_or(XllError::Invalid("alternate XLL band size overflow"))?,
                    )
                    .filter(|&end| end <= boundary_bits)
                    .ok_or(XllError::Invalid("alternate XLL band exceeds boundary"))?;
                self.chs_parse_band_data(&mut bits, channel_set_index, segment, band_end)?;
                if bits.position() > band_end || band_end - bits.position() > 32 {
                    return Err(XllError::Invalid("alternate XLL band trailing bits"));
                }
                seek(&mut bits, band_end)?;
            }
            self.chs_filter_band_data(channel_set_index);
            Ok(())
        })();
        let channel_set = self
            .chset
            .pop()
            .ok_or(XllError::Invalid("missing alternate XLL channel set"))?;
        decode_result?;
        Ok((channel_set, audio_end * 8))
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
        self.chs_parse_header_with_mapping(
            gb,
            c,
            ChsMappingSyntax::Asset {
                is_primary,
                allow_unmapped,
            },
        )
    }

    fn chs_parse_header_with_mapping(
        &mut self,
        gb: &mut BitReader,
        c: &mut XllChSet,
        mapping: ChsMappingSyntax,
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
        match mapping {
            ChsMappingSyntax::Asset {
                is_primary,
                allow_unmapped,
            } => {
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
            }
            ChsMappingSyntax::AlternatePrefix(prefix_bits) => {
                rb(gb, prefix_bits)?;
                c.primary_chset = true;
                c.dmix_coeffs_present = false;
                c.dmix_embedded = false;
                c.hier_chset = true;
                for ch in 0..c.nchannels {
                    c.ch_remap[ch] = ch;
                }
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
            let mut order_mask = 0u32;
            for i in 0..c.nchannels {
                b.orig_order[i] = rb(gb, ch_nbits)? as usize;
                if b.orig_order[i] >= c.nchannels {
                    return Err(XllError::Invalid("XLL original channel order"));
                }
                let order_bit = 1u32 << b.orig_order[i];
                if order_mask & order_bit != 0 {
                    return Err(XllError::Invalid("duplicate XLL original channel order"));
                }
                order_mask |= order_bit;
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
            let lsb_bits = c
                .band
                .lsb_section_size
                .checked_mul(8)
                .filter(|&bits| bits <= band_data_end)
                .ok_or(XllError::Invalid("lsb section exceeds band data"))?;
            seek(gb, band_data_end - lsb_bits)?;
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

#[cfg(test)]
mod alt_extension_tests {
    use super::*;
    use crate::parser::parse_header;
    use crate::{HdDecoder, exss_substream_size};
    use std::io::{BufReader, Read};

    const D0_CORPUS_PATH_ENV: &str = "HARLETTY_D0_CORPUS";
    const D1_CORPUS_PATH_ENV: &str = "HARLETTY_D1_CORPUS";

    fn exss_size_from_prefix(data: &[u8]) -> Option<usize> {
        let mut bits = BitReader::new(data);
        if rb(&mut bits, 32).ok()? != crate::SYNCWORD_SUBSTREAM {
            return None;
        }
        rb(&mut bits, 8).ok()?;
        rb(&mut bits, 2).ok()?;
        let wide_header = rb(&mut bits, 1).ok()? as usize;
        rb(&mut bits, 8 + 4 * wide_header).ok()?;
        Some(rb(&mut bits, 16 + 4 * wide_header).ok()? as usize + 1)
    }

    fn extension_payload(path: &str, target_frame: usize) -> Option<Vec<u8>> {
        const HEADER_BYTES: usize = 18;
        const EXSS_PREFIX_BYTES: usize = 16;

        let input = std::fs::File::open(path).ok()?;
        let mut input = BufReader::with_capacity(1024 * 1024, input);
        let mut decoder = HdDecoder::new();
        let mut core = Vec::new();
        let mut exss = Vec::new();
        let mut header = [0u8; HEADER_BYTES];
        for frame in 0..=target_frame {
            input.read_exact(&mut header).ok()?;
            let info = parse_header(&header).ok()?;
            core.resize(info.frame_size, 0);
            core[..HEADER_BYTES].copy_from_slice(&header);
            input.read_exact(&mut core[HEADER_BYTES..]).ok()?;

            exss.resize(EXSS_PREFIX_BYTES, 0);
            input.read_exact(&mut exss).ok()?;
            let exss_size = exss_size_from_prefix(&exss)?;
            exss.resize(exss_size, 0);
            input.read_exact(&mut exss[EXSS_PREFIX_BYTES..]).ok()?;
            let decoded = decoder.decode(&core, &exss).ok()?;
            if frame == target_frame {
                return Some(decoded.x_payload);
            }
        }
        None
    }

    type RuntimeExtensionResult = (usize, bool, Option<&'static str>);

    fn extension_payloads(
        path: &str,
        max_frames: usize,
        max_bytes: u64,
    ) -> Option<(Vec<Vec<u8>>, Vec<RuntimeExtensionResult>)> {
        let mut input = std::fs::File::open(path).ok()?;
        let mut bytes = Vec::with_capacity(max_bytes.min(usize::MAX as u64) as usize);
        input
            .by_ref()
            .take(max_bytes)
            .read_to_end(&mut bytes)
            .ok()?;
        let mut decoder = HdDecoder::new();
        let mut payloads = Vec::new();
        let mut runtime_results = Vec::new();
        let mut offset = 0usize;
        while offset + 18 < bytes.len() && payloads.len() < max_frames {
            let Ok(core) = parse_header(&bytes[offset..]) else {
                break;
            };
            let exss_offset = offset + core.frame_size;
            let Some(exss_len) = bytes
                .get(exss_offset..)
                .and_then(exss_substream_size)
                .filter(|length| exss_offset + length <= bytes.len())
            else {
                break;
            };
            if let Ok(decoded) = decoder.decode(
                &bytes[offset..exss_offset],
                &bytes[exss_offset..exss_offset + exss_len],
            ) {
                runtime_results.push((
                    decoded.x_samples.len(),
                    decoded
                        .x_samples
                        .iter()
                        .all(|channel| channel.len() == XLL_X_ALT_FRAME_SAMPLES),
                    decoded.x_decode_error,
                ));
                payloads.push(decoded.x_payload);
            }
            offset = exss_offset + exss_len;
        }
        Some((payloads, runtime_results))
    }

    fn extension_payload_from_env(variable: &str, target_frame: usize) -> Option<Vec<u8>> {
        let path = std::env::var(variable).ok()?;
        extension_payload(&path, target_frame)
    }

    fn extension_payloads_from_env(
        variable: &str,
        max_frames: usize,
        max_bytes: u64,
    ) -> Option<(Vec<Vec<u8>>, Vec<RuntimeExtensionResult>)> {
        let path = std::env::var(variable).ok()?;
        extension_payloads(&path, max_frames, max_bytes)
    }

    fn alternate_d1_header_candidates(payload: &[u8]) -> Option<[(usize, usize, usize); 2]> {
        if payload.get(..4) != Some(&DCA_SYNCWORD_XLL_X_ALT_D1.to_be_bytes()) {
            return None;
        }
        let layout = alternate_layout(payload).ok()?;
        Some(
            layout
                .headers
                .map(|header| (header.offset, header.size, header.channels)),
        )
    }

    fn alternate_d0_header_candidates(payload: &[u8]) -> Option<[(usize, usize, usize); 2]> {
        if payload.get(..4) != Some(&DCA_SYNCWORD_XLL_X_ALT_D0.to_be_bytes()) {
            return None;
        }
        let layout = alternate_layout(payload).ok()?;
        Some(
            layout
                .headers
                .map(|header| (header.offset, header.size, header.channels)),
        )
    }

    fn common_geometry_at(control: &[u8], bit_offset: usize) -> Option<(usize, usize)> {
        alternate_geometry_at(control, bit_offset)
            .map(|geometry| (geometry.segments, geometry.navigation_size_bits))
    }

    fn common_geometry_candidates(control: &[u8]) -> Vec<(usize, usize)> {
        common_geometry_candidates_between(control, 19, 26)
    }

    fn common_geometry_candidates_between(
        control: &[u8],
        first_offset: usize,
        last_offset: usize,
    ) -> Vec<(usize, usize)> {
        (first_offset..=last_offset)
            .filter_map(|offset| common_geometry_at(control, offset))
            .collect()
    }

    fn alternate_d1_first_geometries(payload: &[u8]) -> Vec<(usize, usize)> {
        const CONTROL_OFFSET: usize = 55;
        let control_size = match payload.get(CONTROL_OFFSET) {
            Some(0xb2) => 7,
            Some(0xc3..=0xc5) => 8,
            _ => return Vec::new(),
        };
        payload
            .get(CONTROL_OFFSET..CONTROL_OFFSET + control_size)
            .map(|control| common_geometry_candidates_between(control, 18, 25))
            .unwrap_or_default()
    }

    fn alternate_d1_second_geometries(
        payload: &[u8],
        second_header_offset: usize,
    ) -> Vec<(usize, usize)> {
        alternate_second_control(payload, second_header_offset)
            .map(common_geometry_candidates)
            .unwrap_or_default()
    }

    fn alternate_second_control(payload: &[u8], second_header_offset: usize) -> Option<&[u8]> {
        alternate_inner_control(payload, second_header_offset).ok()
    }

    #[test]
    fn alternate_layout_rejects_short_and_unknown_payloads() {
        for syncword in [
            DCA_SYNCWORD_XLL_X_ALT_D0,
            DCA_SYNCWORD_XLL_X_ALT_D1,
            DCA_SYNCWORD_XLL_X_ALT_D3,
        ] {
            let mut payload = [0u8; 96];
            payload[..4].copy_from_slice(&syncword.to_be_bytes());
            for length in 0..payload.len() {
                assert!(alternate_layout(&payload[..length]).is_err());
            }
        }
        assert!(alternate_layout(&[0u8; 128]).is_err());
    }

    #[test]
    fn alternate_d1_outer_prefix_accepts_known_lengths() {
        let prefixes: [&[u8]; 2] = [
            &[
                0xf1, 0x40, 0x00, 0xd1, 0x30, 0x28, 0x4b, 0x00, 0xe0, 0x00, 0x39, 0x00, 0x01, 0x41,
                0x9e, 0xfe, 0xc3, 0x10, 0x28, 0x33, 0xdf, 0xe3, 0xe2, 0x00, 0x03, 0x00, 0x08, 0x81,
                0xf4, 0xe2, 0x1a, 0xc4, 0x04, 0x7f, 0x40, 0x25, 0xbf, 0xa0, 0x49, 0xbf, 0xa8, 0x81,
                0xbf, 0xb1, 0x01, 0xbf, 0xa0, 0xd6, 0xe3,
            ],
            &[
                0xf1, 0x40, 0x00, 0xd1, 0x30, 0x28, 0x4b, 0x00, 0xe1, 0x00, 0x39, 0x40, 0x00, 0x83,
                0x3d, 0xfc, 0x5e, 0x7a, 0x30, 0xf5, 0x50, 0x83, 0x3d, 0xff, 0x66, 0x72, 0x30, 0x53,
                0xd0, 0x03, 0x00, 0x08, 0x81, 0xf4, 0xe2, 0x1a, 0xc4, 0x04, 0x7f, 0x40, 0x25, 0xbf,
                0xa0, 0x49, 0xbf, 0xa8, 0x81, 0xbf, 0xb1, 0x01, 0xbf, 0xa0, 0x09, 0x98,
            ],
        ];
        for prefix in prefixes {
            assert_eq!(crc16_ccitt(prefix), 0);
            let mut payload = prefix.to_vec();
            payload.extend_from_slice(&XLL_X_ALT_OUTER_SUFFIX);
            payload.push(0xb2);
            assert_eq!(
                alternate_outer_layout(&payload, AlternateProfile::D1),
                Ok((
                    prefix.len() + XLL_X_ALT_OUTER_SUFFIX.len(),
                    7,
                    prefix.len() + 18
                ))
            );
        }
    }

    #[test]
    fn alternate_d3_outer_prefix_is_crc_delimited() {
        let prefix = [
            0xf1, 0x40, 0x00, 0xd3, 0x30, 0x28, 0x4b, 0x00, 0xe1, 0x00, 0x39, 0x40, 0x0e, 0x90,
            0x03, 0xb4, 0x00, 0x08, 0x33, 0xdf, 0xc5, 0xe7, 0xa3, 0x0f, 0x55, 0x08, 0x33, 0xdf,
            0xf6, 0x67, 0x23, 0x05, 0x3d, 0x08, 0x33, 0xdf, 0xc5, 0x1e, 0x21, 0x0f, 0x42, 0x0c,
            0xf7, 0xfd, 0xc7, 0x88, 0x83, 0xd0, 0x03, 0x00, 0x08, 0x81, 0xf4, 0xe2, 0x1a, 0xc4,
            0x04, 0x7f, 0x40, 0x25, 0xbf, 0xa0, 0x49, 0xbf, 0xa8, 0x81, 0xbf, 0xb1, 0x01, 0xbf,
            0xa0, 0xd8, 0x72,
        ];
        assert_eq!(prefix.len(), 73);
        assert_eq!(crc16_ccitt(&prefix), 0);
        let mut payload = prefix.to_vec();
        payload.extend_from_slice(&XLL_X_ALT_OUTER_SUFFIX);
        payload.push(0xc6);
        assert_eq!(
            alternate_outer_layout(&payload, AlternateProfile::D3),
            Ok((79, 8, 91))
        );
    }

    #[test]
    fn alternate_extended_controls_have_expected_geometries() {
        let d1_inner = [0xd6, 0x40, 0x0c, 0x06, 0x16, 0x70, 0x00, 0xee, 0x4d];
        assert_eq!(common_geometry_at(&d1_inner, 26), Some((2, 12)));
        assert_eq!(
            common_geometry_candidates_between(&d1_inner, 19, 26),
            [(2, 12)]
        );

        let d0_outer = [0xc5, 0x40, 0x70, 0x18, 0x48, 0x00, 0x31, 0x45];
        assert_eq!(common_geometry_at(&d0_outer, 24), Some((2, 10)));
        assert_eq!(
            common_geometry_candidates_between(&d0_outer, 18, 25),
            [(2, 10)]
        );
    }

    #[test]
    fn alternate_runtime_rejects_truncated_and_corrupt_payload() {
        let Some(payload) = extension_payload_from_env(D0_CORPUS_PATH_ENV, 187) else {
            eprintln!("skipping: alternate-extension corpus not present");
            return;
        };
        let layout = alternate_layout(&payload).expect("valid corpus layout");
        let mut decoder = XllDecoder::new();
        decoder
            .try_decode_alternate_x_extension_audio(&payload)
            .expect("decode complete alternate payload");
        assert_eq!(decoder.x_output.len(), 5);
        assert!(
            decoder
                .x_output
                .iter()
                .all(|channel| channel.len() == XLL_X_ALT_FRAME_SAMPLES)
        );

        let required_bytes = decoder.x_bits_consumed.div_ceil(8);
        let tail_start = required_bytes.saturating_sub(64);
        for length in (0..96).chain(tail_start..required_bytes) {
            let mut decoder = XllDecoder::new();
            assert!(
                decoder
                    .try_decode_alternate_x_extension_audio(&payload[..length])
                    .is_err(),
                "accepted truncated alternate payload at {length} bytes"
            );
            assert!(decoder.x_output.is_empty());
        }

        for byte_offset in [
            48,
            layout.headers[0].offset,
            layout.headers[0].offset + layout.headers[0].size,
            layout.headers[1].offset,
            layout.headers[1].offset + layout.headers[1].size,
        ] {
            let mut corrupt = payload.clone();
            corrupt[byte_offset] ^= 1;
            let mut decoder = XllDecoder::new();
            assert!(
                decoder
                    .try_decode_alternate_x_extension_audio(&corrupt)
                    .is_err()
            );
            assert!(decoder.x_output.is_empty());
        }
    }

    fn direct_single_segment_candidates(
        payload: &[u8],
        header_offset: usize,
        boundary: usize,
        expected_channels: usize,
    ) -> Vec<(usize, bool, u32, usize, usize)> {
        let mut candidates = Vec::new();
        for seg_size_nbits in 4usize..=20 {
            for scalable_lsbs in [false, true] {
                for band_crc_present in 0u32..=2 {
                    for trailing_bytes in 0usize..=16 {
                        let Some(band_end) = boundary.checked_sub(trailing_bytes) else {
                            continue;
                        };
                        if band_end <= header_offset {
                            continue;
                        }
                        let mut decoder = XllDecoder::new();
                        decoder.frame_size = payload.len();
                        decoder.nchsets = 1;
                        decoder.nactivechsets = 1;
                        decoder.nframesegs = 1;
                        decoder.nsegsamples = 512;
                        decoder.nsegsamples_log2 = 9;
                        decoder.nframesamples = 512;
                        decoder.seg_size_nbits = seg_size_nbits;
                        decoder.band_crc_present = band_crc_present;
                        decoder.scalable_lsbs = scalable_lsbs;

                        let mut bits = BitReader::with_offset(payload, header_offset * 8);
                        let mut channel_set = XllChSet::default();
                        if decoder
                            .chs_parse_header(&mut bits, &mut channel_set, true, true)
                            .is_err()
                            || channel_set.nchannels != expected_channels
                        {
                            continue;
                        }
                        channel_set.band.msb =
                            vec![vec![0i32; decoder.nframesamples]; expected_channels];
                        channel_set.band.lsb = if channel_set.band.lsb_section_size != 0 {
                            vec![vec![0i32; decoder.nframesamples]; expected_channels]
                        } else {
                            Vec::new()
                        };
                        decoder.chset.push(channel_set);
                        if decoder
                            .chs_parse_band_data(&mut bits, 0, 0, band_end * 8)
                            .is_err()
                            || bits.position() > band_end * 8
                        {
                            continue;
                        }
                        let trailing_bits = band_end * 8 - bits.position();
                        if trailing_bits > 32 {
                            continue;
                        }
                        candidates.push((
                            seg_size_nbits,
                            scalable_lsbs,
                            band_crc_present,
                            trailing_bytes,
                            trailing_bits,
                        ));
                    }
                }
            }
        }
        candidates
    }

    fn immediate_navi_candidates(
        payload: &[u8],
        header_offset: usize,
        header_size: usize,
        boundary: usize,
        max_trailer: usize,
        expected_geometry: Option<(usize, usize)>,
    ) -> Vec<(usize, usize, usize, Vec<usize>)> {
        let mut candidates = Vec::new();
        let Some(navi_start) = header_offset.checked_add(header_size) else {
            return candidates;
        };
        if navi_start > boundary || boundary > payload.len() {
            return candidates;
        }
        for segments in 1usize..=8 {
            for size_bits in 4usize..=20 {
                if expected_geometry.is_some_and(|geometry| geometry != (segments, size_bits)) {
                    continue;
                }
                let navi_size = (segments * size_bits).div_ceil(8) + 2;
                let Some(navi_end) = navi_start.checked_add(navi_size) else {
                    continue;
                };
                if navi_end > boundary || crc16_ccitt(&payload[navi_start..navi_end]) != 0 {
                    continue;
                }
                let mut bits = BitReader::with_offset(payload, navi_start * 8);
                let mut sizes = Vec::with_capacity(segments);
                let mut audio_bytes = 0usize;
                for _ in 0..segments {
                    let Ok(size) = rb(&mut bits, size_bits) else {
                        sizes.clear();
                        continue;
                    };
                    let size = size as usize + 1;
                    audio_bytes += size;
                    sizes.push(size);
                }
                let Some(audio_end) = navi_end.checked_add(audio_bytes) else {
                    continue;
                };
                if sizes.len() == segments
                    && audio_end <= boundary
                    && boundary - audio_end <= max_trailer
                {
                    candidates.push((segments, size_bits, boundary - audio_end, sizes));
                }
            }
        }
        candidates
    }

    #[derive(Debug)]
    struct DecodedChannelSetCandidate {
        segments: usize,
        navigation_size_bits: usize,
        header_size_bits: usize,
        channel_mask_bits: usize,
        channel_mask: u32,
        scalable_lsbs: bool,
        band_crc_present: u32,
        navigation: Vec<usize>,
        trailing_bits: Vec<usize>,
        pcm_bit_res: usize,
        header_tail_bits: usize,
        peaks: Vec<i32>,
        checksums: Vec<i64>,
    }

    fn decode_channel_set_candidates(
        payload: &[u8],
        header_offset: usize,
        header_size: usize,
        boundary: usize,
        expected_channels: usize,
        one_to_one: bool,
        is_primary: bool,
        allow_unmapped: bool,
        frame_samples: usize,
        alternate_prefix_bits: Option<usize>,
        common_from_navigation: bool,
        expected_geometry: Option<(usize, usize)>,
    ) -> Vec<DecodedChannelSetCandidate> {
        let mut candidates = Vec::new();
        // The first D1 channel set leaves 14--18 bytes between the final
        // NAVI-sized band and the following channel-set header. Keep this
        // corpus probe wide enough to cover that wrapper-owned interstitial
        // data; the NAVI itself must still pass CRC16 before decoding.
        for (segments, navigation_size_bits, _, navigation) in immediate_navi_candidates(
            payload,
            header_offset,
            header_size,
            boundary,
            20,
            expected_geometry,
        ) {
            if !segments.is_power_of_two() || frame_samples % segments != 0 {
                continue;
            }
            let segment_samples = frame_samples / segments;
            if !segment_samples.is_power_of_two() {
                continue;
            }
            let navi_start = header_offset + header_size;
            let navi_size = (segments * navigation_size_bits).div_ceil(8) + 2;
            let data_start = navi_start + navi_size;

            let max_channel_mask_bits = if one_to_one && alternate_prefix_bits.is_none() {
                32
            } else {
                1
            };
            for channel_mask_bits in 1usize..=max_channel_mask_bits {
                let header_size_bits = if common_from_navigation {
                    navigation_size_bits..=navigation_size_bits
                } else {
                    4usize..=20
                };
                for header_size_bits in header_size_bits {
                    for scalable_lsbs in [false, true] {
                        if common_from_navigation && scalable_lsbs {
                            continue;
                        }
                        let max_band_crc = if common_from_navigation { 0 } else { 3 };
                        for band_crc_present in 0u32..=max_band_crc {
                            let mut decoder = XllDecoder::new();
                            decoder.frame_size = payload.len();
                            decoder.nchsets = 1;
                            decoder.nactivechsets = 1;
                            decoder.nframesegs = segments;
                            decoder.nsegsamples = segment_samples;
                            decoder.nsegsamples_log2 = segment_samples.ilog2() as usize;
                            decoder.nframesamples = frame_samples;
                            decoder.seg_size_nbits = header_size_bits;
                            decoder.band_crc_present = band_crc_present;
                            decoder.scalable_lsbs = scalable_lsbs;
                            decoder.ch_mask_nbits = channel_mask_bits;
                            decoder.one_to_one = one_to_one;

                            let mut bits = BitReader::with_offset(payload, header_offset * 8);
                            let mut channel_set = XllChSet::default();
                            let header_result = match alternate_prefix_bits {
                                Some(prefix_bits) => decoder.chs_parse_header_with_mapping(
                                    &mut bits,
                                    &mut channel_set,
                                    ChsMappingSyntax::AlternatePrefix(prefix_bits),
                                ),
                                None => decoder.chs_parse_header(
                                    &mut bits,
                                    &mut channel_set,
                                    is_primary,
                                    allow_unmapped,
                                ),
                            };
                            if header_result.is_err()
                                || bits.position() != navi_start * 8
                                || channel_set.nchannels != expected_channels
                                || channel_set.residual_encode != (1 << expected_channels) - 1
                            {
                                continue;
                            }
                            channel_set.band.msb =
                                vec![vec![0i32; frame_samples]; expected_channels];
                            channel_set.band.lsb = if channel_set.band.lsb_section_size != 0 {
                                vec![vec![0i32; frame_samples]; expected_channels]
                            } else {
                                Vec::new()
                            };
                            let channel_mask = channel_set.ch_mask;
                            let pcm_bit_res = channel_set.pcm_bit_res;
                            let header_tail_bits = channel_set.header_tail_bits;
                            decoder.chset.push(channel_set);
                            if seek(&mut bits, data_start * 8).is_err() {
                                continue;
                            }

                            let mut band_end = bits.position();
                            let mut trailing_bits = Vec::with_capacity(segments);
                            let mut valid = true;
                            for (segment, &segment_bytes) in navigation.iter().enumerate() {
                                let Some(next_band_end) = band_end.checked_add(segment_bytes * 8)
                                else {
                                    valid = false;
                                    break;
                                };
                                if decoder
                                    .chs_parse_band_data(&mut bits, 0, segment, next_band_end)
                                    .is_err()
                                    || bits.position() > next_band_end
                                {
                                    valid = false;
                                    break;
                                }
                                let unused = next_band_end - bits.position();
                                if unused > 32 || seek(&mut bits, next_band_end).is_err() {
                                    valid = false;
                                    break;
                                }
                                trailing_bits.push(unused);
                                band_end = next_band_end;
                            }
                            if !valid {
                                continue;
                            }

                            decoder.chs_filter_band_data(0);
                            if scalable_lsbs {
                                decoder.chs_assemble_msbs_lsbs(0);
                            }
                            let peaks = decoder.chset[0]
                                .band
                                .msb
                                .iter()
                                .map(|channel| {
                                    channel
                                        .iter()
                                        .map(|&sample| sample.saturating_abs())
                                        .max()
                                        .unwrap_or_default()
                                })
                                .collect();
                            let checksums = decoder.chset[0]
                                .band
                                .msb
                                .iter()
                                .map(|channel| channel.iter().map(|&sample| sample as i64).sum())
                                .collect();
                            candidates.push(DecodedChannelSetCandidate {
                                segments,
                                navigation_size_bits,
                                header_size_bits,
                                channel_mask_bits,
                                channel_mask,
                                scalable_lsbs,
                                band_crc_present,
                                navigation: navigation.clone(),
                                trailing_bits,
                                pcm_bit_res,
                                header_tail_bits,
                                peaks,
                                checksums,
                            });
                        }
                    }
                }
            }
        }
        candidates
    }

    #[test]
    fn alternate_first_channel_set_is_not_a_direct_xll_segment() {
        let Some(d0) = extension_payload_from_env(D0_CORPUS_PATH_ENV, 185) else {
            eprintln!("skipping: alternate-extension corpus not present");
            return;
        };
        let Some(d1) = extension_payload_from_env(D1_CORPUS_PATH_ENV, 190) else {
            eprintln!("skipping: alternate-extension corpus not present");
            return;
        };

        let d0_candidates = direct_single_segment_candidates(&d0, 62, 131, 1);
        let d1_candidates = direct_single_segment_candidates(&d1, 63, 764, 2);
        eprintln!("D0 corpus direct-segment candidates: {d0_candidates:?}");
        eprintln!("D1 corpus direct-segment candidates: {d1_candidates:?}");
        assert!(d0_candidates.is_empty());
        assert!(d1_candidates.is_empty());
    }

    #[test]
    fn alternate_d1_terminal_quartet_has_crc_valid_xll_navigation() {
        let Some(d1) = extension_payload_from_env(D1_CORPUS_PATH_ENV, 190) else {
            eprintln!("skipping: alternate-extension corpus not present");
            return;
        };
        let d1_active =
            extension_payload_from_env(D1_CORPUS_PATH_ENV, 192).expect("adjacent corpus frame");

        let d1_candidates = immediate_navi_candidates(&d1, 764, 16, d1.len(), 16, None);
        let d1_active_candidates = immediate_navi_candidates(
            &d1_active,
            1300,
            18,
            d1_active.len(),
            16,
            None,
        );
        eprintln!("D1 corpus terminal-quartet NAVI: {d1_candidates:?}");
        eprintln!(
            "D1 corpus active terminal-quartet NAVI: {d1_active_candidates:?}"
        );
        assert!(!d1_candidates.is_empty());
        assert!(!d1_active_candidates.is_empty());
    }

    #[test]
    fn alternate_d1_terminal_quartet_reaches_xll_pcm() {
        let Some(d1) = extension_payload_from_env(D1_CORPUS_PATH_ENV, 192) else {
            eprintln!("skipping: alternate-extension corpus not present");
            return;
        };
        let candidates = decode_channel_set_candidates(
            &d1,
            1300,
            18,
            d1.len(),
            4,
            true,
            false,
            false,
            512,
            Some(2),
            true,
            Some((2, 8)),
        );
        let decoded = candidates.iter().find(|candidate| {
            candidate.segments == 2
                && candidate.navigation_size_bits == 8
                && candidate.header_size_bits == 8
                && candidate.channel_mask_bits == 1
                && candidate.channel_mask == 0
                && !candidate.scalable_lsbs
                && candidate.band_crc_present == 0
                && candidate.navigation == [158, 161]
                && candidate.trailing_bits == [4, 3]
                && candidate.pcm_bit_res == 20
                && candidate.header_tail_bits == 59
                && candidate.peaks == [0, 0, 7, 8]
                && candidate.checksums == [0, 0, 14, -43]
        });
        assert!(decoded.is_some(), "D1 quartet did not reach stable XLL PCM");
    }

    #[test]
    fn alternate_d1_prefix_mode_decodes_active_channel_set_run() {
        let Some((payloads, runtime_results)) = extension_payloads_from_env(
            D1_CORPUS_PATH_ENV,
            9_000,
            64 * 1024 * 1024,
        )
        else {
            eprintln!("skipping: alternate-extension corpus not present");
            return;
        };
        assert!(
            runtime_results
                .iter()
                .all(|result| *result == (6, true, None)),
            "D1 runtime decoder did not produce six complete sources"
        );
        let mut decoded_frames = [0usize; 2];
        let mut self_consistent_frames = [0usize; 2];
        let mut configuration_counts =
            std::array::from_fn::<_, 2, _>(|_| std::collections::HashMap::new());
        let mut geometry_counts =
            std::array::from_fn::<_, 2, _>(|_| std::collections::HashMap::new());
        let mut second_control_candidate_counts = std::collections::HashMap::new();
        let mut first_control_candidate_counts = std::collections::HashMap::new();
        for payload in payloads.iter().skip(191) {
            let Some(
                [
                    (first_offset, first_header_size, first_channels),
                    (second_offset, second_header_size, second_channels),
                ],
            ) = alternate_d1_header_candidates(payload)
            else {
                continue;
            };
            let first_geometries = alternate_d1_first_geometries(payload);
            *first_control_candidate_counts
                .entry(first_geometries.len())
                .or_insert(0usize) += 1;
            let second_geometries = alternate_d1_second_geometries(payload, second_offset);
            *second_control_candidate_counts
                .entry(second_geometries.len())
                .or_insert(0usize) += 1;
            assert_eq!(first_geometries.len(), 1, "ambiguous D1 first control");
            assert_eq!(second_geometries.len(), 1, "ambiguous D1 second control");
            let channel_sets = [
                (
                    first_offset,
                    first_header_size,
                    first_channels,
                    second_offset,
                ),
                (
                    second_offset,
                    second_header_size,
                    second_channels,
                    payload.len(),
                ),
            ];
            for (set, (offset, header_size, channels, boundary)) in
                channel_sets.into_iter().enumerate()
            {
                let mut candidates = Vec::new();
                let expected_geometries = if set == 0 {
                    first_geometries.clone()
                } else {
                    second_geometries.clone()
                };
                for expected_geometry in expected_geometries {
                    candidates.extend(
                        decode_channel_set_candidates(
                            payload,
                            offset,
                            header_size,
                            boundary,
                            channels,
                            false,
                            false,
                            false,
                            512,
                            Some(2),
                            true,
                            Some(expected_geometry),
                        )
                        .into_iter()
                        .map(|candidate| (512, candidate)),
                    );
                }
                if !candidates.is_empty() {
                    decoded_frames[set] += 1;
                }
                if candidates.iter().any(|(_, candidate)| {
                    candidate.header_size_bits == candidate.navigation_size_bits
                        && !candidate.scalable_lsbs
                        && candidate.band_crc_present == 0
                }) {
                    self_consistent_frames[set] += 1;
                }
                let geometries = candidates
                    .iter()
                    .map(|(_, candidate)| (candidate.segments, candidate.navigation_size_bits))
                    .collect::<std::collections::HashSet<_>>();
                for geometry in geometries {
                    *geometry_counts[set].entry(geometry).or_insert(0usize) += 1;
                }
                let configurations = candidates
                    .iter()
                    .map(|(frame_samples, candidate)| {
                        (
                            *frame_samples,
                            candidate.segments,
                            candidate.navigation_size_bits,
                            candidate.header_size_bits,
                            candidate.scalable_lsbs,
                            candidate.band_crc_present,
                        )
                    })
                    .collect::<std::collections::HashSet<_>>();
                for configuration in configurations {
                    *configuration_counts[set]
                        .entry(configuration)
                        .or_insert(0usize) += 1;
                }
            }
        }
        for (set, counts) in configuration_counts.into_iter().enumerate() {
            let mut counts = counts.into_iter().collect::<Vec<_>>();
            counts.sort_unstable_by(|a, b| b.1.cmp(&a.1));
            let mut geometries = std::mem::take(&mut geometry_counts[set])
                .into_iter()
                .collect::<Vec<_>>();
            geometries.sort_unstable_by(|a, b| b.1.cmp(&a.1));
            eprintln!(
                "D1 alternate-prefix set {set}: {}/{} active-run frames decoded, {} self-consistent; geometries: {:?}; top configurations: {:?}",
                decoded_frames[set],
                payloads.len().saturating_sub(191),
                self_consistent_frames[set],
                &geometries[..geometries.len().min(8)],
                &counts[..counts.len().min(12)]
            );
            assert_eq!(
                self_consistent_frames[set],
                payloads.len().saturating_sub(191),
                "not every active D1 channel set decoded"
            );
        }
        eprintln!("D1 first-control geometry candidate counts: {first_control_candidate_counts:?}");
        eprintln!(
            "D1 second-control geometry candidate counts: {second_control_candidate_counts:?}"
        );
    }

    #[test]
    fn alternate_d1_c3_mode_decodes_both_channel_sets() {
        let Some(payload) = extension_payload_from_env(D1_CORPUS_PATH_ENV, 16_277) else {
            eprintln!("skipping: alternate-extension corpus not present");
            return;
        };
        assert_eq!(payload.get(55), Some(&0xc3));
        let layout = alternate_layout(&payload).expect("valid D1 c3 layout");
        assert_eq!(layout.headers.map(|header| header.channels), [2, 4]);

        let mut decoder = XllDecoder::new();
        decoder
            .try_decode_alternate_x_extension_audio(&payload)
            .expect("decode complete D1 c3 payload");
        assert_eq!(decoder.x_output.len(), 6);
        assert!(
            decoder
                .x_output
                .iter()
                .all(|samples| samples.len() == XLL_X_ALT_FRAME_SAMPLES)
        );
    }

    #[test]
    fn alternate_d0_prefix_mode_decodes_active_channel_set_run() {
        let Some((payloads, runtime_results)) = extension_payloads_from_env(
            D0_CORPUS_PATH_ENV,
            9_000,
            64 * 1024 * 1024,
        )
        else {
            eprintln!("skipping: alternate-extension corpus not present");
            return;
        };
        assert!(
            payloads
                .iter()
                .zip(&runtime_results)
                .filter(|(payload, _)| payload.len() > 116)
                .all(|(_, result)| *result == (5, true, None)),
            "D0 runtime decoder did not produce five complete sources"
        );
        let mut active_frames = 0usize;
        let mut decoded_frames = [0usize; 2];
        let mut self_consistent_frames = [0usize; 2];
        let mut configuration_counts =
            std::array::from_fn::<_, 2, _>(|_| std::collections::HashMap::new());
        let mut geometry_counts =
            std::array::from_fn::<_, 2, _>(|_| std::collections::HashMap::new());
        let mut control_candidate_counts =
            std::array::from_fn::<_, 2, _>(|_| std::collections::HashMap::new());
        for payload in payloads.iter().filter(|payload| payload.len() > 116) {
            active_frames += 1;
            let Some(
                [
                    (first_offset, first_header_size, first_channels),
                    (second_offset, second_header_size, second_channels),
                ],
            ) = alternate_d0_header_candidates(payload)
            else {
                continue;
            };
            let channel_sets = [
                (
                    first_offset,
                    first_header_size,
                    first_channels,
                    second_offset,
                ),
                (
                    second_offset,
                    second_header_size,
                    second_channels,
                    payload.len(),
                ),
            ];
            for (set, (offset, header_size, channels, boundary)) in
                channel_sets.into_iter().enumerate()
            {
                let control = if set == 0 {
                    payload.get(54..first_offset)
                } else {
                    alternate_second_control(payload, second_offset)
                };
                let expected_geometries = control
                    .map(|control| common_geometry_candidates_between(control, 18, 25))
                    .unwrap_or_default();
                *control_candidate_counts[set]
                    .entry(expected_geometries.len())
                    .or_insert(0usize) += 1;
                assert_eq!(
                    expected_geometries.len(),
                    1,
                    "ambiguous D0 channel-set control"
                );
                let mut candidates = Vec::new();
                for expected_geometry in expected_geometries {
                    candidates.extend(decode_channel_set_candidates(
                        payload,
                        offset,
                        header_size,
                        boundary,
                        channels,
                        false,
                        false,
                        false,
                        512,
                        Some(2),
                        true,
                        Some(expected_geometry),
                    ));
                }
                if !candidates.is_empty() {
                    decoded_frames[set] += 1;
                }
                if candidates.iter().any(|candidate| {
                    candidate.header_size_bits == candidate.navigation_size_bits
                        && !candidate.scalable_lsbs
                        && candidate.band_crc_present == 0
                }) {
                    self_consistent_frames[set] += 1;
                }
                let geometries = candidates
                    .iter()
                    .map(|candidate| (candidate.segments, candidate.navigation_size_bits))
                    .collect::<std::collections::HashSet<_>>();
                for geometry in geometries {
                    *geometry_counts[set].entry(geometry).or_insert(0usize) += 1;
                }
                let configurations = candidates
                    .iter()
                    .map(|candidate| {
                        (
                            candidate.segments,
                            candidate.navigation_size_bits,
                            candidate.header_size_bits,
                            candidate.scalable_lsbs,
                            candidate.band_crc_present,
                        )
                    })
                    .collect::<std::collections::HashSet<_>>();
                for configuration in configurations {
                    *configuration_counts[set]
                        .entry(configuration)
                        .or_insert(0usize) += 1;
                }
            }
        }
        for (set, counts) in configuration_counts.into_iter().enumerate() {
            let mut counts = counts.into_iter().collect::<Vec<_>>();
            counts.sort_unstable_by(|a, b| b.1.cmp(&a.1));
            let mut geometries = std::mem::take(&mut geometry_counts[set])
                .into_iter()
                .collect::<Vec<_>>();
            geometries.sort_unstable_by(|a, b| b.1.cmp(&a.1));
            eprintln!(
                "D0 alternate-prefix set {set}: {}/{} active frames decoded, {} self-consistent; geometries: {:?}; top configurations: {:?}",
                decoded_frames[set],
                active_frames,
                self_consistent_frames[set],
                &geometries[..geometries.len().min(8)],
                &counts[..counts.len().min(12)]
            );
            eprintln!(
                "D0 alternate-prefix set {set} control geometry candidate counts: {:?}",
                control_candidate_counts[set]
            );
            assert_eq!(
                self_consistent_frames[set], active_frames,
                "not every active D0 channel set decoded"
            );
        }
    }
}
