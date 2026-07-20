// SPDX-License-Identifier: Apache-2.0
//
// DTS extension substream (EXSS) parser. Ported from ffmpeg's dca_exss.c.
// Locates the per-asset extension components — in particular the XLL (DTS-HD
// Master Audio lossless) offset/size within the substream — so the XLL decoder
// can be pointed at the right bytes. CRC checks are skipped.
//
// Consumed by the XLL decoder (Phase 3); allow dead_code until that lands.
#![allow(dead_code)]

use crate::bitstream::BitReader;

/// Extension masks (`enum DCAExtensionMask`, EXSS half).
pub(crate) const DCA_EXSS_CORE: u32 = 0x010;
pub(crate) const DCA_EXSS_XBR: u32 = 0x020;
pub(crate) const DCA_EXSS_XXCH: u32 = 0x040;
pub(crate) const DCA_EXSS_X96: u32 = 0x080;
pub(crate) const DCA_EXSS_LBR: u32 = 0x100;
pub(crate) const DCA_EXSS_XLL: u32 = 0x200;
pub(crate) const DCA_EXSS_RSV1: u32 = 0x400;
pub(crate) const DCA_EXSS_RSV2: u32 = 0x800;

/// `ff_dca_sampling_freqs` (dca.c) — EXSS sample-rate table (distinct from the
/// core's `ff_dca_sample_rates`).
const SAMPLING_FREQS: [u32; 16] = [
    8000, 16000, 32000, 64000, 128000, 22050, 44100, 88200, 176400, 352800, 12000, 24000, 48000,
    96000, 192000, 384000,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExssError {
    Bitstream,
    Invalid(&'static str),
    Unsupported(&'static str),
}

type R<T> = Result<T, ExssError>;

#[inline]
fn rb(gb: &mut BitReader, n: usize) -> R<u32> {
    gb.read_bits(n).ok_or(ExssError::Bitstream)
}
#[inline]
fn rb1(gb: &mut BitReader) -> R<bool> {
    gb.read_bit().ok_or(ExssError::Bitstream)
}
#[inline]
fn skip(gb: &mut BitReader, n: usize) -> R<()> {
    gb.skip_bits_long(n).ok_or(ExssError::Bitstream)
}
#[inline]
fn seek(gb: &mut BitReader, pos: usize) -> R<()> {
    if gb.seek(pos) {
        Ok(())
    } else {
        Err(ExssError::Invalid("seek past end"))
    }
}

/// EXSS sample-rate table lookup (`ff_dca_sampling_freqs`).
pub(crate) fn sampling_freq(idx: usize) -> u32 {
    SAMPLING_FREQS[idx]
}

/// `ff_dca_count_chs_for_mask`.
fn count_chs_for_mask(mask: u32) -> u32 {
    ((mask & 0xffff) | ((mask & 0xae66) << 16)).count_ones()
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ExssAsset {
    pub(crate) asset_index: u32,
    pub(crate) asset_offset: usize,
    pub(crate) asset_size: usize,

    pub(crate) pcm_bit_res: u32,
    pub(crate) max_sample_rate: u32,
    pub(crate) nchannels_total: u32,
    pub(crate) one_to_one_map_ch_to_spkr: bool,
    pub(crate) embedded_stereo: bool,
    pub(crate) embedded_6ch: bool,
    pub(crate) spkr_mask_enabled: bool,
    pub(crate) spkr_mask: u32,
    pub(crate) representation_type: u32,

    pub(crate) coding_mode: u32,
    pub(crate) extension_mask: u32,

    pub(crate) core_offset: usize,
    pub(crate) core_size: usize,
    pub(crate) xll_offset: usize,
    pub(crate) xll_size: usize,
    pub(crate) xll_sync_present: bool,
    pub(crate) xll_delay_nframes: u32,
    pub(crate) xll_sync_offset: usize,
    pub(crate) hd_stream_id: u32,
    /// Unspecified tail of the audio asset descriptor. Newer profiles can put
    /// metadata here while legacy decoders skip to `nuAssetDescriptFsize`.
    pub(crate) descriptor_tail: Vec<u8>,
    pub(crate) descriptor_tail_bits: usize,
    pub(crate) decode_in_secondary: bool,
    pub(crate) drc_rev2_present: bool,
}

#[derive(Default)]
pub(crate) struct ExssParser {
    exss_index: u32,
    exss_size_nbits: usize,
    exss_size: usize,
    static_fields_present: bool,
    frame_duration_samples: usize,
    mix_metadata_enabled: bool,
    nmixoutconfigs: usize,
    nmixoutchs: [u32; 4],
    pub(crate) nassets: usize,
    pub(crate) asset: ExssAsset,
}

impl ExssParser {
    /// Total size of this extension substream in bytes (for demuxing).
    pub(crate) fn substream_size(&self) -> usize {
        self.exss_size
    }

    /// True when the (single) audio asset carries an XLL component — i.e. this is
    /// DTS-HD Master Audio (lossless). False for HRA / lossy-extension streams,
    /// which have only a core plus an XBR/X96/etc component.
    pub(crate) fn has_xll(&self) -> bool {
        self.asset.extension_mask & DCA_EXSS_XLL != 0
    }
}

impl ExssParser {
    /// Parse an EXSS substream (must start at the 0x64582025 syncword).
    pub(crate) fn parse(data: &[u8]) -> R<Self> {
        let mut s = ExssParser::default();
        let mut gb = BitReader::new(data);

        skip(&mut gb, 32)?; // sync word
        skip(&mut gb, 8)?; // user defined bits
        s.exss_index = rb(&mut gb, 2)?;
        let wide_hdr = rb1(&mut gb)?;
        let header_size = rb(&mut gb, 8 + 4 * wide_hdr as usize)? as usize + 1;

        s.exss_size_nbits = 16 + 4 * wide_hdr as usize;
        s.exss_size = rb(&mut gb, s.exss_size_nbits)? as usize + 1;
        if s.exss_size > data.len() {
            return Err(ExssError::Invalid("packet too short for EXSS"));
        }

        s.static_fields_present = rb1(&mut gb)?;
        if s.static_fields_present {
            skip(&mut gb, 2)?; // reference clock code
            s.frame_duration_samples = 512 * (rb(&mut gb, 3)? as usize + 1);
            if rb1(&mut gb)? {
                skip(&mut gb, 36)?; // timecode
            }
            let npresents = rb(&mut gb, 3)? as usize + 1;
            if npresents > 1 {
                return Err(ExssError::Unsupported("multiple audio presentations"));
            }
            s.nassets = rb(&mut gb, 3)? as usize + 1;
            if s.nassets > 1 {
                return Err(ExssError::Unsupported("multiple audio assets"));
            }
            let mut active_exss_mask = [0u32; 8];
            for m in active_exss_mask.iter_mut().take(npresents) {
                *m = rb(&mut gb, s.exss_index as usize + 1)?;
            }
            for &m in active_exss_mask.iter().take(npresents) {
                skip(&mut gb, m.count_ones() as usize * 8)?;
            }
            s.mix_metadata_enabled = rb1(&mut gb)?;
            if s.mix_metadata_enabled {
                skip(&mut gb, 2)?; // adjustment level
                let spkr_mask_nbits = (rb(&mut gb, 2)? as usize + 1) << 2;
                s.nmixoutconfigs = rb(&mut gb, 2)? as usize + 1;
                for i in 0..s.nmixoutconfigs {
                    s.nmixoutchs[i] = count_chs_for_mask(rb(&mut gb, spkr_mask_nbits)?);
                }
            }
        } else {
            s.nassets = 1;
        }

        // Asset sizes.
        let mut offset = header_size;
        s.asset.asset_offset = offset;
        s.asset.asset_size = rb(&mut gb, s.exss_size_nbits)? as usize + 1;
        offset += s.asset.asset_size;
        if offset > s.exss_size {
            return Err(ExssError::Invalid("EXSS asset out of bounds"));
        }

        // Asset descriptor.
        s.parse_descriptor(&mut gb)?;
        s.set_exss_offsets()?;

        seek(&mut gb, header_size * 8)?;
        Ok(s)
    }

    fn parse_descriptor(&mut self, gb: &mut BitReader) -> R<()> {
        let descr_pos = gb.position();
        let descr_size = rb(gb, 9)? as usize + 1;
        let a = &mut self.asset;
        a.asset_index = rb(gb, 3)?;

        if self.static_fields_present {
            if rb1(gb)? {
                skip(gb, 4)?; // asset type descriptor
            }
            if rb1(gb)? {
                skip(gb, 24)?; // language descriptor
            }
            if rb1(gb)? {
                let text_size = rb(gb, 10)? as usize + 1;
                skip(gb, text_size * 8)?;
            }
            a.pcm_bit_res = rb(gb, 5)? + 1;
            a.max_sample_rate = SAMPLING_FREQS[rb(gb, 4)? as usize];
            a.nchannels_total = rb(gb, 8)? + 1;
            a.one_to_one_map_ch_to_spkr = rb1(gb)?;
            if a.one_to_one_map_ch_to_spkr {
                let mut spkr_mask_nbits = 0usize;
                a.embedded_stereo = a.nchannels_total > 2 && rb1(gb)?;
                a.embedded_6ch = a.nchannels_total > 6 && rb1(gb)?;
                a.spkr_mask_enabled = rb1(gb)?;
                if a.spkr_mask_enabled {
                    spkr_mask_nbits = (rb(gb, 2)? as usize + 1) << 2;
                    a.spkr_mask = rb(gb, spkr_mask_nbits)?;
                }
                let spkr_remap_nsets = rb(gb, 3)? as usize;
                if spkr_remap_nsets != 0 && spkr_mask_nbits == 0 {
                    return Err(ExssError::Invalid("remap sets without speaker mask"));
                }
                let mut nspeakers = [0u32; 8];
                for ns in nspeakers.iter_mut().take(spkr_remap_nsets) {
                    *ns = count_chs_for_mask(rb(gb, spkr_mask_nbits)?);
                }
                for &ns in nspeakers.iter().take(spkr_remap_nsets) {
                    let nch_for_remaps = rb(gb, 5)? as usize + 1;
                    for _ in 0..ns {
                        let remap_ch_mask = rb(gb, nch_for_remaps)?;
                        skip(gb, remap_ch_mask.count_ones() as usize * 5)?;
                    }
                }
            } else {
                a.embedded_stereo = false;
                a.embedded_6ch = false;
                a.spkr_mask_enabled = false;
                a.spkr_mask = 0;
                a.representation_type = rb(gb, 3)?;
            }
        }

        // DRC / DNC / mixing metadata.
        let drc_present = rb1(gb)?;
        if drc_present {
            skip(gb, 8)?;
        }
        if rb1(gb)? {
            skip(gb, 5)?; // dialog normalization
        }
        if drc_present && a.embedded_stereo {
            skip(gb, 8)?;
        }
        let mix_metadata_present = self.mix_metadata_enabled && rb1(gb)?;
        if mix_metadata_present {
            skip(gb, 1)?; // external mixing flag
            skip(gb, 6)?; // post mixing gain
            if rb(gb, 2)? == 3 {
                skip(gb, 8)?;
            } else {
                skip(gb, 3)?;
            }
            if rb1(gb)? {
                for i in 0..self.nmixoutconfigs {
                    skip(gb, 6 * self.nmixoutchs[i] as usize)?;
                }
            } else {
                skip(gb, 6 * self.nmixoutconfigs)?;
            }
            let mut nchannels_dmix = a.nchannels_total;
            if a.embedded_6ch {
                nchannels_dmix += 6;
            }
            if a.embedded_stereo {
                nchannels_dmix += 2;
            }
            for i in 0..self.nmixoutconfigs {
                if self.nmixoutchs[i] == 0 {
                    return Err(ExssError::Invalid("mix config speaker mask"));
                }
                for _ in 0..nchannels_dmix {
                    let mix_map_mask = rb(gb, self.nmixoutchs[i] as usize)?;
                    skip(gb, mix_map_mask.count_ones() as usize * 6)?;
                }
            }
        }

        // Decoder navigation data.
        a.coding_mode = rb(gb, 2)?;
        match a.coding_mode {
            0 => {
                a.extension_mask = rb(gb, 12)?;
                if a.extension_mask & DCA_EXSS_CORE != 0 {
                    a.core_size = rb(gb, 14)? as usize + 1;
                    if rb1(gb)? {
                        skip(gb, 2)?;
                    }
                }
                if a.extension_mask & DCA_EXSS_XBR != 0 {
                    rb(gb, 14)?; // xbr_size-1 (unused)
                }
                if a.extension_mask & DCA_EXSS_XXCH != 0 {
                    rb(gb, 14)?;
                }
                if a.extension_mask & DCA_EXSS_X96 != 0 {
                    rb(gb, 12)?;
                }
                if a.extension_mask & DCA_EXSS_LBR != 0 {
                    parse_lbr_parameters(gb)?;
                }
                if a.extension_mask & DCA_EXSS_XLL != 0 {
                    parse_xll_parameters(gb, a, self.exss_size_nbits)?;
                }
                if a.extension_mask & DCA_EXSS_RSV1 != 0 {
                    skip(gb, 16)?;
                }
                if a.extension_mask & DCA_EXSS_RSV2 != 0 {
                    skip(gb, 16)?;
                }
            }
            1 => {
                a.extension_mask = DCA_EXSS_XLL;
                parse_xll_parameters(gb, a, self.exss_size_nbits)?;
            }
            2 => {
                a.extension_mask = DCA_EXSS_LBR;
                parse_lbr_parameters(gb)?;
            }
            _ => {
                a.extension_mask = 0;
                skip(gb, 14)?;
                skip(gb, 8)?;
                if rb1(gb)? {
                    skip(gb, 3)?;
                }
            }
        }

        if a.extension_mask & DCA_EXSS_XLL != 0 {
            a.hd_stream_id = rb(gb, 3)?;
        }

        // Fields added after the decoder navigation block in later revisions
        // of ETSI TS 102 114. Parsing these keeps the remaining descriptor tail
        // limited to genuinely reserved/profile-specific metadata.
        if a.one_to_one_map_ch_to_spkr && self.mix_metadata_enabled && !mix_metadata_present {
            let one_to_one_mixing = rb1(gb)?;
            if one_to_one_mixing {
                let per_channel_scale = rb1(gb)?;
                for config in 0..self.nmixoutconfigs {
                    let values = if per_channel_scale {
                        self.nmixoutchs[config] as usize
                    } else {
                        1
                    };
                    skip(gb, 6 * values)?;
                }
            }
        }
        a.decode_in_secondary = rb1(gb)?;
        a.drc_rev2_present = rb1(gb)?;
        if a.drc_rev2_present {
            skip(gb, 4)?; // DRCversion_Rev2
            skip(gb, 8 * (self.frame_duration_samples / 256))?;
        }

        let descr_end = descr_pos + descr_size * 8;
        if gb.position() > descr_end {
            return Err(ExssError::Invalid("asset descriptor fields exceed size"));
        }
        a.descriptor_tail_bits = descr_end - gb.position();
        a.descriptor_tail = vec![0; a.descriptor_tail_bits.div_ceil(8)];
        for bit in 0..a.descriptor_tail_bits {
            if rb1(gb)? {
                a.descriptor_tail[bit / 8] |= 1 << (7 - bit % 8);
            }
        }
        seek(gb, descr_end)
    }

    fn set_exss_offsets(&mut self) -> R<()> {
        let a = &mut self.asset;
        let mut offs = a.asset_offset;
        let mut size = a.asset_size as isize;

        // We only retain CORE and XLL component sizes. When XLL is present, an
        // intervening XBR/XXCH/X96/LBR component would sit before it and shift the
        // XLL offset; reject rather than miscompute. When there is NO XLL (e.g.
        // DTS-HD HRA, which carries CORE + a lossy XBR extension), nothing reads
        // the XLL offset, so let the EXSS parse succeed — the caller falls back to
        // the DTS core for these.
        if a.extension_mask & DCA_EXSS_XLL != 0
            && a.extension_mask & (DCA_EXSS_XBR | DCA_EXSS_XXCH | DCA_EXSS_X96 | DCA_EXSS_LBR) != 0
        {
            return Err(ExssError::Unsupported(
                "intervening EXSS extension before XLL",
            ));
        }

        if a.extension_mask & DCA_EXSS_CORE != 0 {
            a.core_offset = offs;
            if a.core_size as isize > size {
                return Err(ExssError::Invalid("core size out of bounds"));
            }
            offs += a.core_size;
            size -= a.core_size as isize;
        }
        if a.extension_mask & DCA_EXSS_XLL != 0 {
            a.xll_offset = offs;
            if a.xll_size as isize > size {
                return Err(ExssError::Invalid("xll size out of bounds"));
            }
        }
        Ok(())
    }
}

fn parse_xll_parameters(gb: &mut BitReader, a: &mut ExssAsset, exss_size_nbits: usize) -> R<()> {
    a.xll_size = rb(gb, exss_size_nbits)? as usize + 1;
    a.xll_sync_present = rb1(gb)?;
    if a.xll_sync_present {
        skip(gb, 4)?; // PBR smoothing buffer size
        let xll_delay_nbits = rb(gb, 5)? as usize + 1;
        a.xll_delay_nframes = rb(gb, xll_delay_nbits)?;
        a.xll_sync_offset = rb(gb, exss_size_nbits)? as usize;
    } else {
        a.xll_delay_nframes = 0;
        a.xll_sync_offset = 0;
    }
    Ok(())
}

fn parse_lbr_parameters(gb: &mut BitReader) -> R<()> {
    rb(gb, 14)?; // lbr_size-1
    if rb1(gb)? {
        skip(gb, 2)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Validate EXSS parsing on the real Ex Machina DTS-HD MA stream (7.1).
    // Skips if the (uncommitted) dump is absent.
    #[test]
    fn parses_real_exss_locates_xll() {
        const DUMP: &str = "/mnt/local/SSD_B-CT4000/Dumps/Ex.Machina.2014.dtsx.eng.dts";
        if !std::path::Path::new(DUMP).exists() {
            eprintln!("skipping: 7.1 dump not present");
            return;
        }
        // The stream is [core][exss][core][exss]…; the EXSS sits immediately
        // after the first core frame (don't naive-scan — 0x64582025 can occur
        // inside core audio data).
        let bytes = std::fs::read(DUMP).unwrap();
        let core = crate::parser::parse_header(&bytes).expect("core header");
        let pos = core.frame_size;
        let sync = 0x6458_2025u32.to_be_bytes();
        assert_eq!(
            &bytes[pos..pos + 4],
            &sync,
            "EXSS not right after core frame"
        );
        let s = ExssParser::parse(&bytes[pos..]).expect("parse exss");
        let a = &s.asset;
        eprintln!(
            "EXSS: nassets={} coding_mode={} ext_mask={:#x} nch_total={} sr={} pcm_res={} \
             one2one={} spkr_mask={:#x} xll_off={} xll_size={} core_off={} core_size={}",
            s.nassets,
            a.coding_mode,
            a.extension_mask,
            a.nchannels_total,
            a.max_sample_rate,
            a.pcm_bit_res,
            a.one_to_one_map_ch_to_spkr,
            a.spkr_mask,
            a.xll_offset,
            a.xll_size,
            a.core_offset,
            a.core_size,
        );
        eprintln!(
            "  xll_sync_present={} xll_delay_nframes={} xll_sync_offset={}",
            a.xll_sync_present, a.xll_delay_nframes, a.xll_sync_offset
        );
        assert!(a.extension_mask & DCA_EXSS_XLL != 0, "XLL asset expected");
        assert!(a.xll_size > 0);
        assert_eq!(a.nchannels_total, 8, "expected 7.1 (8 channels)");

        // Demux the first N frames ([core][exss]…) and confirm the structure is
        // consistent and XLL sizes vary (PBR smoothing).
        let mut off = 0usize;
        let mut frames = 0usize;
        let mut xll_min = usize::MAX;
        let mut xll_max = 0usize;
        let mut total = 0usize;
        while frames < 400 && off + 18 < bytes.len() {
            let core = match crate::parser::parse_header(&bytes[off..]) {
                Ok(c) => c,
                Err(_) => break,
            };
            let exss_off = off + core.frame_size;
            if exss_off + 4 > bytes.len() || bytes[exss_off..exss_off + 4] != sync {
                break;
            }
            let e = ExssParser::parse(&bytes[exss_off..]).expect("exss");
            xll_min = xll_min.min(e.asset.xll_size);
            xll_max = xll_max.max(e.asset.xll_size);
            let frame_len = core.frame_size + e.exss_size;
            total += frame_len;
            off += frame_len;
            frames += 1;
        }
        eprintln!(
            "demuxed {frames} frames; xll_size min={xll_min} max={xll_max}; \
             avg frame {} bytes",
            total / frames.max(1)
        );
        assert!(frames >= 100, "demux desynced after {frames} frames");
        assert!(xll_max > xll_min, "XLL sizes should vary (PBR)");
    }
}
