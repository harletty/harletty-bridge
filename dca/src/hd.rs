// SPDX-License-Identifier: Apache-2.0
//
// High-level DTS-HD decode. One DTS-HD frame = a core access unit + the
// following EXSS substream. Two carriers come out of it:
//
// - DTS-HD Master Audio: the core (residual base), the EXSS parser and the
//   XLL lossless decoder together produce the lossless 5.1/7.1 bed, and the
//   XLL-X extension its DTS:X waveforms.
// - A lossy carrier (DTS-HD High Resolution Audio): the core plus the XXCH
//   component produce the 7.1 bed, and the DTS:X extension that follows the
//   asset — a bare channel set in the core syntax — its four height feeds.
//   Both are decoded by the core decoder.

use crate::dcadec::core::{CoreDecoder, CoreError};
use crate::dcadec::exss::ExssParser;
use crate::dcadec::synth::SynthState;
use crate::dcadec::xll::{DCA_SYNCWORD_XLL_X, XllDecoder, XllError, crc16_ccitt};
use crate::dcadec::xmeta::FIXED_HEIGHT_COUNT;
use crate::parser::{ParseError, parse_header};

const PCM_SCALE: f32 = 8_388_608.0; // 2^23

/// The bytes of a lossy carrier's DTS:X extension before its channel set:
/// the 22-byte wrapper the metadata reader parses (the fold matrix and its
/// CRC16), four bytes constant across the corpus, a CRC16-protected
/// navigation (two constant bytes, then the size of everything after it —
/// the set and the substream's padding), and one constant byte.
const LOSSY_EXTENSION_WRAPPER: usize = 22;
const LOSSY_EXTENSION_SET_OFFSET: usize = 33;
const LOSSY_EXTENSION_CONSTANT: [u8; 4] = [0x75, 0x9a, 0x19, 0x08];
const LOSSY_EXTENSION_NAVIGATION_HEAD: [u8; 2] = [0x00, 0x40];
const LOSSY_EXTENSION_SET_MARKER: u8 = 0x02;

/// The bare channel set inside a lossy carrier's DTS:X extension `blob`
/// (marker included), once the container around it reads as expected.
fn lossy_extension_set(blob: &[u8]) -> Result<&[u8], &'static str> {
    let nav = blob
        .get(LOSSY_EXTENSION_WRAPPER..LOSSY_EXTENSION_SET_OFFSET)
        .ok_or("short lossy extension")?;
    if nav[..4] != LOSSY_EXTENSION_CONSTANT
        || nav[4..6] != LOSSY_EXTENSION_NAVIGATION_HEAD
        || nav[10] != LOSSY_EXTENSION_SET_MARKER
    {
        return Err("lossy extension container");
    }
    if crc16_ccitt(&nav[4..10]) != 0 {
        return Err("lossy extension navigation crc");
    }
    let size = u16::from_be_bytes([nav[6], nav[7]]) as usize;
    if size != blob.len() - LOSSY_EXTENSION_SET_OFFSET {
        return Err("lossy extension size");
    }
    Ok(&blob[LOSSY_EXTENSION_SET_OFFSET..])
}

fn core_error_kind(error: &CoreError) -> &'static str {
    match error {
        CoreError::Bitstream => "bitstream",
        CoreError::Invalid(kind) | CoreError::Unsupported(kind) => kind,
    }
}

/// What an EXSS substream carries for [`HdDecoder`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExssKind {
    /// An XLL asset: DTS-HD Master Audio, decoded losslessly.
    Lossless,
    /// No XLL, but an XXCH component and/or a DTS:X extension after the
    /// asset: a lossy carrier the decoder reconstructs beyond the core.
    Lossy,
    /// Nothing the decoder reads beyond the core (XBR-only HRA, or an
    /// unparseable substream): callers decode the DTS core alone.
    Core,
}

/// Classify the EXSS at `data` (which must begin at the 0x64582025 syncword).
pub fn exss_kind(data: &[u8]) -> ExssKind {
    match ExssParser::parse(data) {
        Ok(p) if p.has_xll() => ExssKind::Lossless,
        Ok(p) if p.has_xxch() || p.extension_after_asset(data).is_some() => ExssKind::Lossy,
        _ => ExssKind::Core,
    }
}

#[derive(Debug)]
pub enum HdError {
    Header(ParseError),
    Core,
    Exss,
    Xll(String),
    /// XLL is buffering across substreams (PBR); no output this frame.
    Pending,
}

impl From<ParseError> for HdError {
    fn from(e: ParseError) -> Self {
        HdError::Header(e)
    }
}

/// One decoded DTS-HD frame: PCM indexed by DCA speaker (lossless for a
/// Master Audio carrier, core + XXCH for a lossy one), plus the
/// active-speaker mask.
#[derive(Default)]
pub struct HdFrame {
    pub sample_rate: u32,
    pub output_mask: u32,
    /// `samples[spkr]` = Some(f32 PCM in [-1, 1]) for active speakers.
    pub samples: Vec<Option<Vec<f32>>>,
    /// Whether `samples` are the lossless reconstruction of a Master Audio
    /// carrier (false: a lossy carrier's core + XXCH).
    pub lossless: bool,
    /// Why a lossy carrier's XXCH channels are missing from `samples` this
    /// frame (the bed then is the core alone), if they are.
    pub xxch_decode_error: Option<&'static str>,
    /// DTS:X end-of-frame extension present (`0x02000850`).
    pub x_present: bool,
    /// DTS:X IMAX variant present.
    pub x_imax: bool,
    /// Raw DTS:X extension payload (syncword + data) for diagnostics. Empty when
    /// no extension is present.
    pub x_payload: Vec<u8>,
    /// Byte offset of `x_payload` within the XLL frame.
    pub x_payload_offset: usize,
    /// Decoded, speaker-unmapped extension waveforms. The standard profile has
    /// four; alternate profiles can carry five or six across two channel sets.
    /// They are deliberately not mixed into `samples` or `output_mask` here.
    pub x_samples: Vec<Vec<f32>>,
    pub x_pcm_bit_res: usize,
    /// Bit position reached after decoding the extension waveforms, relative
    /// to `x_payload`.
    pub x_bits_consumed: usize,
    /// Diagnostic failure kind from the optional extension decode
    /// (allocation-free). A failure here never invalidates the lossless 7.1
    /// bed.
    pub x_decode_error: Option<&'static str>,
    /// Unparsed tail of the extension channel-set header, including byte
    /// alignment and its mandatory CRC16.
    pub x_header_tail_bits: usize,
    /// XLL frame geometry inherited by the extension channel set.
    pub xll_frame_segments: usize,
    pub xll_segment_samples: usize,
    pub xll_segment_size_bits: usize,
    pub xll_band_crc_present: u32,
    pub xll_scalable_lsbs: bool,
    /// Unspecified tail of the EXSS asset descriptor, retained for spatial
    /// metadata research. Only the first `exss_descriptor_tail_bits` are valid.
    pub exss_descriptor_tail: Vec<u8>,
    pub exss_descriptor_tail_bits: usize,
    /// Parsed profile-specific navigation for the XLL-X block.
    pub x_descriptor_offset: Option<usize>,
    pub x_descriptor_size: Option<usize>,
    /// True when the decoder used the descriptor navigation rather than only
    /// locating the syncword after the decoded XLL band data.
    pub x_descriptor_navigation_used: bool,
}

impl HdFrame {
    /// Samples per channel in the lossless bed, taken from the first active
    /// speaker, or 0 when no speaker is active.
    ///
    /// Every bed and extension channel in a well-formed frame has this length;
    /// consumers use it to validate a frame before indexing into it.
    pub fn bed_sample_count(&self) -> usize {
        self.samples
            .iter()
            .find_map(|channel| channel.as_ref().map(Vec::len))
            .unwrap_or(0)
    }
}

/// Size in bytes of the EXSS substream starting at `data` (which must begin at
/// the 0x64582025 syncword), for demuxing `[core][exss]` DTS-HD frames.
pub fn exss_substream_size(data: &[u8]) -> Option<usize> {
    ExssParser::parse(data).ok().map(|p| p.substream_size())
}

/// True when the EXSS at `data` carries an XLL (DTS-HD MA, lossless) asset, i.e.
/// [`HdDecoder`] can reconstruct the lossless bed. DTS-HD HRA and other lossy
/// extensions parse but expose no XLL asset; callers should decode the DTS core
/// instead for those.
pub fn exss_has_xll(data: &[u8]) -> bool {
    ExssParser::parse(data)
        .map(|p| p.has_xll())
        .unwrap_or(false)
}

#[derive(Default)]
pub struct HdDecoder {
    core: CoreDecoder,
    synth: SynthState,
    xll: XllDecoder,
}

impl HdDecoder {
    pub fn new() -> Self {
        HdDecoder {
            core: CoreDecoder::default(),
            synth: SynthState::default(),
            xll: XllDecoder::new(),
        }
    }

    pub fn reset(&mut self) {
        self.core.reset();
        self.synth.reset();
        self.xll.reset();
    }

    /// The lossless 24-bit samples of the last decoded frame, by DCA speaker
    /// index, as integers — before the float conversion `decode` hands out.
    /// Active speakers only. This is the tap for anything that reads the low
    /// bits of the PCM (an Auro-Codec carrier keeps its side channel there),
    /// which a float round trip would not preserve.
    pub fn lossless_samples(&self) -> impl Iterator<Item = (usize, &[i32])> {
        self.xll
            .output
            .iter()
            .enumerate()
            .filter_map(|(speaker, channel)| channel.as_deref().map(|v| (speaker, v)))
    }

    /// Decode one DTS-HD frame from its core access unit + EXSS substream bytes.
    pub fn decode(&mut self, core_au: &[u8], exss: &[u8]) -> Result<HdFrame, HdError> {
        // 1) Core bitstream decode (the residual base).
        let info = parse_header(core_au)?;
        self.core
            .decode_frame(&info, core_au)
            .map_err(|_| HdError::Core)?;

        // 2) EXSS → locate the XLL asset, or take the lossy route without one.
        let mut exssp = ExssParser::parse(exss).map_err(|_| HdError::Exss)?;
        if !exssp.has_xll() {
            return Ok(self.decode_lossy(exss, &exssp));
        }

        // 3) Parse XLL before synthesizing the core: the core must be rendered at
        // the XLL's rate for the residual to line up.
        match self.xll.parse(exss, &exssp.asset) {
            Ok(()) => {}
            Err(XllError::Eagain) => return Err(HdError::Pending),
            Err(e) => return Err(HdError::Xll(format!("{e:?}"))),
        }

        // 4) A 96 kHz channel set over a 48 kHz core is rendered through the
        // 64-band bank so both sides carry the same rate and sample count. There
        // is no X96 payload involved — this is pure oversampling of the core.
        let x96_synth =
            self.xll.primary_freq() == Some(96_000) && self.core.sample_rate() == 48_000;
        let core_out = self
            .synth
            .synthesize_fixed_by_speaker(&mut self.core, x96_synth);

        // 5) Combine the lossless XLL bands with the core residual.
        match self.xll.filter(Some(&core_out)) {
            Ok(()) => {}
            Err(XllError::Eagain) => return Err(HdError::Pending),
            Err(e) => return Err(HdError::Xll(format!("{e:?}"))),
        }

        // 4) Convert 24-bit lossless ints to f32 by speaker.
        let samples = self
            .xll
            .output
            .iter()
            .map(|opt| {
                opt.as_ref()
                    .map(|v| v.iter().map(|&s| s as f32 / PCM_SCALE).collect())
            })
            .collect();
        let x_samples = std::mem::take(&mut self.xll.x_output)
            .into_iter()
            .map(|channel| {
                channel
                    .into_iter()
                    .map(|sample| sample as f32 / PCM_SCALE)
                    .collect()
            })
            .collect();

        Ok(HdFrame {
            sample_rate: self.xll.sample_rate,
            output_mask: self.xll.output_mask,
            samples,
            x_present: self.xll.x_syncword_present,
            x_imax: self.xll.x_imax_syncword_present,
            x_payload: std::mem::take(&mut self.xll.x_payload),
            x_payload_offset: self.xll.x_payload_offset,
            x_samples,
            x_pcm_bit_res: self.xll.x_pcm_bit_res,
            x_bits_consumed: self.xll.x_bits_consumed,
            x_decode_error: self.xll.x_decode_error.take(),
            x_header_tail_bits: self.xll.x_header_tail_bits,
            xll_frame_segments: self.xll.nframesegs,
            xll_segment_samples: self.xll.nsegsamples,
            xll_segment_size_bits: self.xll.seg_size_nbits,
            xll_band_crc_present: self.xll.band_crc_present,
            xll_scalable_lsbs: self.xll.scalable_lsbs,
            exss_descriptor_tail: std::mem::take(&mut exssp.asset.descriptor_tail),
            exss_descriptor_tail_bits: exssp.asset.descriptor_tail_bits,
            x_descriptor_offset: exssp.asset.xll_x_offset,
            x_descriptor_size: exssp.asset.xll_x_size,
            x_descriptor_navigation_used: self.xll.x_descriptor_navigation_used,
            lossless: true,
            xxch_decode_error: None,
        })
    }

    /// The lossy route: the core (already decoded) plus the asset's XXCH
    /// channels make the bed; the DTS:X extension after the asset, when it
    /// is the standard profile's, makes the four height feeds. A failing
    /// component degrades to the bed without it, recorded in the frame,
    /// never to a dropped frame.
    fn decode_lossy(&mut self, exss: &[u8], exssp: &ExssParser) -> HdFrame {
        // The lossless decoder's output is not this frame's: a reader of the
        // integer tap must see nothing.
        for slot in &mut self.xll.output {
            *slot = None;
        }

        let asset = &exssp.asset;
        let mut xxch_decode_error = None;
        if exssp.has_xxch() {
            match exss.get(asset.xxch_offset..asset.xxch_offset + asset.xxch_size) {
                Some(bytes) => {
                    if let Err(e) = self.core.decode_xxch(bytes) {
                        xxch_decode_error = Some(core_error_kind(&e));
                    }
                }
                None => xxch_decode_error = Some("xxch component bounds"),
            }
        }

        let mut x_present = false;
        let mut x_imax = false;
        let mut x_payload = Vec::new();
        let mut x_payload_offset = 0;
        let mut x_bits_consumed = 0;
        let mut x_decode_error = None;
        if let Some(start) = exssp.extension_after_asset(exss) {
            let end = exssp.substream_size().min(exss.len());
            let blob = &exss[start..end];
            let marker = u32::from_be_bytes([blob[0], blob[1], blob[2], blob[3]]);
            x_present = marker == DCA_SYNCWORD_XLL_X;
            x_imax = !x_present;
            x_payload = blob.to_vec();
            x_payload_offset = start;
            if x_present {
                match lossy_extension_set(blob) {
                    Ok(set) => match self.core.decode_extension_set(set, FIXED_HEIGHT_COUNT) {
                        Ok(consumed) => x_bits_consumed = consumed * 8,
                        Err(e) => x_decode_error = Some(core_error_kind(&e)),
                    },
                    Err(kind) => x_decode_error = Some(kind),
                }
            } else {
                x_decode_error = Some("alternate profile on a lossy carrier");
            }
        }

        let core_out = self
            .synth
            .synthesize_fixed_by_speaker(&mut self.core, false);
        let samples = core_out
            .samples
            .iter()
            .map(|opt| {
                opt.as_ref()
                    .map(|v| v.iter().map(|&s| s as f32 / PCM_SCALE).collect())
            })
            .collect();
        let x_samples = core_out
            .extension
            .into_iter()
            .map(|channel| {
                channel
                    .into_iter()
                    .map(|sample| sample as f32 / PCM_SCALE)
                    .collect()
            })
            .collect();

        HdFrame {
            sample_rate: core_out.output_rate,
            output_mask: core_out.ch_mask,
            samples,
            lossless: false,
            xxch_decode_error,
            x_present,
            x_imax,
            x_payload,
            x_payload_offset,
            x_samples,
            x_pcm_bit_res: 24,
            x_bits_consumed,
            x_decode_error,
            ..HdFrame::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dcadec::exss::ExssParser;

    // Decode `dump` (a `[core][exss]` DTS-HD MA elementary stream) through the full
    // HD decoder and assert the lossless PCM matches ffmpeg's f32 reference
    // (`refpath`, interleaved `ch` channels). Each ffmpeg channel is auto-matched to
    // its best-fitting decoded speaker, so channel order doesn't matter; a wrong
    // output scale (e.g. the 16-bit-storage bug) blows up the RMSE far past 1e-5.
    fn check_xll_lossless(dump: &str, refpath: &str, ch: usize) {
        if !std::path::Path::new(dump).exists() || !std::path::Path::new(refpath).exists() {
            eprintln!("skipping: corpus not present ({dump})");
            return;
        }
        let bytes = std::fs::read(dump).unwrap();
        let rbytes = std::fs::read(refpath).unwrap();
        let reference: Vec<f32> = rbytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();

        let mut dec = HdDecoder::new();
        // Per-speaker concatenated output.
        let mut spk: Vec<Vec<f32>> = vec![Vec::new(); 32];
        let mut off = 0usize;
        let mut frames = 0usize;
        let mut pending = 0usize;
        let mut active_mask = 0u32;
        while frames < 200 && off + 18 < bytes.len() {
            let core = match parse_header(&bytes[off..]) {
                Ok(c) => c,
                Err(_) => break,
            };
            let exss_off = off + core.frame_size;
            let exssp = match ExssParser::parse(&bytes[exss_off..]) {
                Ok(p) => p,
                Err(_) => break,
            };
            let exss_len = exssp.substream_size();
            let exss_bytes = &bytes[exss_off..exss_off + exss_len];
            match dec.decode(&bytes[off..exss_off], exss_bytes) {
                Ok(f) => {
                    active_mask = f.output_mask;
                    for s in 0..32 {
                        if let Some(v) = &f.samples[s] {
                            spk[s].extend_from_slice(v);
                        }
                    }
                    frames += 1;
                }
                Err(HdError::Pending) => {
                    pending += 1;
                    // Stop the contiguous comparison at the first PBR gap.
                    break;
                }
                Err(e) => panic!("decode error at frame {frames}: {e:?}"),
            }
            off += core.frame_size + exss_len;
        }

        eprintln!("decoded {frames} XLL frames (pending={pending}), output_mask={active_mask:#x}");
        assert!(frames >= 20, "too few XLL frames decoded ({frames})");

        let nsamp = spk
            .iter()
            .filter(|v| !v.is_empty())
            .map(|v| v.len())
            .min()
            .unwrap();
        let ref_n = reference.len() / ch;
        let cmp = nsamp.min(ref_n);
        assert!(cmp > 5000, "too little to compare ({cmp})");

        // Auto-match each ffmpeg channel to the best-fitting decoded speaker;
        // lossless ⇒ exact match (rmse ~0).
        let active: Vec<usize> = (0..32).filter(|&s| !spk[s].is_empty()).collect();
        eprintln!("active speakers: {active:?}");
        let mut worst = 0f64;
        for rc in 0..ch {
            let mut best = f64::INFINITY;
            let mut best_spk = 0usize;
            let mut best_max = 0f32;
            for &s in &active {
                let mut sq = 0f64;
                let mut mx = 0f32;
                for i in 0..cmp {
                    let d = (spk[s][i] - reference[i * ch + rc]).abs();
                    sq += (d as f64) * (d as f64);
                    mx = mx.max(d);
                }
                let rmse = (sq / cmp as f64).sqrt();
                if rmse < best {
                    best = rmse;
                    best_spk = s;
                    best_max = mx;
                }
            }
            eprintln!("ref ch{rc} -> speaker {best_spk}, rmse={best:.3e} maxabs={best_max:.3e}");
            worst = worst.max(best);
        }
        assert!(worst < 1e-5, "not lossless (worst rmse {worst:.3e})");
    }

    #[test]
    fn xll_7_1_matches_ffmpeg_lossless() {
        let Ok(dump) = std::env::var("HARLETTY_DTSX_STANDARD_CORPUS") else {
            eprintln!("skipping: HARLETTY_DTSX_STANDARD_CORPUS is not set");
            return;
        };
        let Ok(reference) = std::env::var("HARLETTY_DTSX_STANDARD_REFERENCE") else {
            eprintln!("skipping: HARLETTY_DTSX_STANDARD_REFERENCE is not set");
            return;
        };
        check_xll_lossless(&dump, &reference, 8);
    }

    #[test]
    fn xll_5_1_16bit_matches_ffmpeg_lossless() {
        // Regression guard for the 16-bit-storage output-scale bug (the bed
        // was ~48 dB / 256x too quiet).
        let Ok(dump) = std::env::var("HARLETTY_DTSHD_16BIT_CORPUS") else {
            eprintln!("skipping: HARLETTY_DTSHD_16BIT_CORPUS is not set");
            return;
        };
        let Ok(reference) = std::env::var("HARLETTY_DTSHD_16BIT_REFERENCE") else {
            eprintln!("skipping: HARLETTY_DTSHD_16BIT_REFERENCE is not set");
            return;
        };
        check_xll_lossless(&dump, &reference, 6);
    }

    #[test]
    fn xll_x_decodes_four_unmapped_waveforms() {
        use std::io::Read;

        let Ok(dump) = std::env::var("HARLETTY_DTSX_STANDARD_CORPUS") else {
            eprintln!("skipping: HARLETTY_DTSX_STANDARD_CORPUS is not set");
            return;
        };
        if !std::path::Path::new(&dump).is_file() {
            eprintln!("skipping: configured spatial-layer corpus is not readable");
            return;
        }
        let mut bytes = Vec::new();
        std::fs::File::open(dump)
            .unwrap()
            .take(2 * 1024 * 1024)
            .read_to_end(&mut bytes)
            .unwrap();

        let mut decoder = HdDecoder::new();
        let mut offset = 0usize;
        let mut frames = 0usize;
        while frames < 100 && offset + 18 < bytes.len() {
            let core = parse_header(&bytes[offset..]).unwrap();
            let exss_offset = offset + core.frame_size;
            let exss = ExssParser::parse(&bytes[exss_offset..]).unwrap();
            let exss_size = exss.substream_size();
            let frame = decoder
                .decode(
                    &bytes[offset..exss_offset],
                    &bytes[exss_offset..exss_offset + exss_size],
                )
                .unwrap();
            assert!(frame.x_present);
            assert_eq!(frame.x_samples.len(), 4);
            assert!(frame.x_samples.iter().all(|channel| channel.len() == 512));
            assert_eq!(frame.x_decode_error, None);
            assert!(frame.x_descriptor_navigation_used);
            assert_eq!(frame.x_descriptor_offset, Some(frame.x_payload_offset));
            assert_eq!(frame.x_descriptor_size, Some(frame.x_payload.len()));
            frames += 1;
            offset += core.frame_size + exss_size;
        }
        assert_eq!(frames, 100);
    }
}

#[cfg(test)]
mod lossy_carrier_tests {
    use super::*;
    use crate::dcadec::xmeta::{BedFold, SourceRole, XMetadata};

    /// Every `[core][exss]` frame of a raw DTS-HD dump, decoded in order.
    fn decode_dump(
        dump: &str,
        mut each: impl FnMut(&[u8], &[u8], Result<HdFrame, HdError>),
    ) -> usize {
        let bytes = std::fs::read(dump).unwrap();
        let mut dec = HdDecoder::new();
        let mut pos = 0usize;
        let mut frames = 0usize;
        while pos + 16 <= bytes.len() {
            let Ok(info) = parse_header(&bytes[pos..]) else {
                break;
            };
            let exss_start = pos + info.frame_size;
            if exss_start + 4 > bytes.len()
                || bytes[exss_start..exss_start + 4] != crate::SYNCWORD_SUBSTREAM.to_be_bytes()
            {
                break;
            }
            let Some(es) = exss_substream_size(&bytes[exss_start..]) else {
                break;
            };
            let exss = &bytes[exss_start..exss_start + es];
            each(
                &bytes[pos..exss_start],
                exss,
                dec.decode(&bytes[pos..exss_start], exss),
            );
            frames += 1;
            pos = exss_start + es;
        }
        frames
    }

    /// A lossy carrier (DTS-HD HRA: core + XXCH, DTS:X extension after the
    /// asset): every frame yields the 7.1 bed and the four height feeds, and
    /// its wrapper states their fold.
    #[test]
    fn lossy_carrier_decodes_the_bed_and_the_height_quartet() {
        let Ok(dump) = std::env::var("HARLETTY_LOSSY_X_CORPUS") else {
            eprintln!("skipping: HARLETTY_LOSSY_X_CORPUS is not set");
            return;
        };
        let mut decoded = 0usize;
        let frames = decode_dump(&dump, |_, exss, result| {
            assert_eq!(exss_kind(exss), ExssKind::Lossy);
            let frame = result.expect("lossy frame");
            assert!(!frame.lossless);
            assert_eq!(frame.xxch_decode_error, None);
            assert_eq!(frame.x_decode_error, None);
            assert!(frame.x_present && !frame.x_imax);
            // C, L, R, Ls, Rs, LFE, Lsr, Rsr.
            assert_eq!(frame.output_mask, 0x1bf);
            let n = frame.bed_sample_count();
            assert_eq!(n, 512);
            assert_eq!(frame.samples.iter().filter(|s| s.is_some()).count(), 8);
            assert_eq!(frame.x_samples.len(), 4);
            assert!(frame.x_samples.iter().all(|feed| feed.len() == n));
            assert_eq!(frame.x_pcm_bit_res, 24);
            let metadata = XMetadata::parse(&frame.x_payload, 4).expect("wrapper");
            for feed in 0..4 {
                let source = metadata.source(feed).expect("height");
                assert!(matches!(source.role, SourceRole::Height(_)));
                assert!(matches!(source.fold, BedFold::Known(_)));
            }
            decoded += 1;
        });
        assert!(frames >= 100, "only {frames} frames in the corpus");
        assert_eq!(decoded, frames);
    }

    /// The 7.1 bed of a lossy carrier matches ffmpeg's decode (interleaved
    /// f32, ffmpeg 7.1 order) on every full-band speaker. The LFE is
    /// compared loosely: this decoder interpolates it with the fixed-point
    /// filter ffmpeg reserves for lossless reconstruction, whose passband
    /// differs from the one ffmpeg's float output uses.
    #[test]
    fn lossy_carrier_bed_matches_ffmpeg() {
        let (Ok(dump), Ok(reference)) = (
            std::env::var("HARLETTY_LOSSY_X_CORPUS"),
            std::env::var("HARLETTY_LOSSY_X_REFERENCE"),
        ) else {
            eprintln!("skipping: HARLETTY_LOSSY_X_CORPUS / HARLETTY_LOSSY_X_REFERENCE are not set");
            return;
        };
        let reference: Vec<f32> = std::fs::read(reference)
            .unwrap()
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        // ffmpeg 7.1 order -> DCA speaker.
        const ORDER: [usize; 8] = [1, 2, 0, 5, 7, 8, 3, 4];
        let mut sq_err = [0f64; 8];
        let mut sq_ref = [0f64; 8];
        let mut dot = [0f64; 8];
        let mut sq_ours = [0f64; 8];
        let mut sample_pos = 0usize;
        decode_dump(&dump, |_, _, result| {
            let frame = result.expect("lossy frame");
            let n = frame.bed_sample_count();
            for (k, &spkr) in ORDER.iter().enumerate() {
                let ours = frame.samples[spkr].as_ref().expect("bed speaker");
                for (s, &v) in ours.iter().enumerate() {
                    let Some(&r) = reference.get((sample_pos + s) * 8 + k) else {
                        return;
                    };
                    let (v, r) = (v as f64, r as f64);
                    sq_err[k] += (v - r) * (v - r);
                    sq_ref[k] += r * r;
                    sq_ours[k] += v * v;
                    dot[k] += v * r;
                }
            }
            sample_pos += n;
        });
        assert!(sample_pos > 48_000);
        for k in 0..8 {
            let rmse = (sq_err[k] / sample_pos as f64).sqrt();
            if k == 3 {
                let correlation = dot[k] / (sq_ours[k] * sq_ref[k]).sqrt();
                assert!(correlation > 0.98, "LFE correlation {correlation}");
                let ratio = (sq_ours[k] / sq_ref[k]).sqrt();
                assert!((0.8..1.3).contains(&ratio), "LFE level ratio {ratio}");
            } else {
                assert!(rmse < 1e-5, "speaker {k}: rmse {rmse}");
            }
        }
    }

    /// The container around the bare set is read exactly, never guessed.
    #[test]
    fn lossy_extension_container_is_checked() {
        let mut blob = vec![0u8; 40];
        blob[..4].copy_from_slice(&DCA_SYNCWORD_XLL_X.to_be_bytes());
        assert_eq!(
            lossy_extension_set(&blob[..30]),
            Err("short lossy extension")
        );
        assert_eq!(lossy_extension_set(&blob), Err("lossy extension container"));
        blob[22..26].copy_from_slice(&LOSSY_EXTENSION_CONSTANT);
        blob[26..28].copy_from_slice(&LOSSY_EXTENSION_NAVIGATION_HEAD);
        blob[32] = LOSSY_EXTENSION_SET_MARKER;
        let size = (blob.len() - LOSSY_EXTENSION_SET_OFFSET) as u16;
        blob[28..30].copy_from_slice(&size.to_be_bytes());
        let crc = crc16_ccitt(&blob[26..30]);
        blob[30..32].copy_from_slice(&crc.to_be_bytes());
        assert_eq!(lossy_extension_set(&blob).map(<[u8]>::len), Ok(7));
        blob[29] ^= 1;
        assert_eq!(
            lossy_extension_set(&blob),
            Err("lossy extension navigation crc")
        );
    }
}
