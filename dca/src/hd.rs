// SPDX-License-Identifier: Apache-2.0
//
// High-level DTS-HD Master Audio decode: ties the core (residual base), EXSS
// parser, and XLL lossless decoder together to produce the lossless 5.1/7.1
// bed. One DTS-HD frame = a core access unit + the following EXSS substream.

use crate::dcadec::core::CoreDecoder;
use crate::dcadec::exss::ExssParser;
use crate::dcadec::synth::SynthState;
use crate::dcadec::xll::{XllDecoder, XllError};
use crate::parser::{parse_header, ParseError};

const PCM_SCALE: f32 = 8_388_608.0; // 2^23

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

/// One decoded DTS-HD frame: lossless PCM indexed by DCA speaker, plus the
/// active-speaker mask.
pub struct HdFrame {
    pub sample_rate: u32,
    pub output_mask: u32,
    /// `samples[spkr]` = Some(f32 PCM in [-1, 1]) for active speakers.
    pub samples: Vec<Option<Vec<f32>>>,
    /// DTS:X end-of-frame extension present (`0x02000850`).
    pub x_present: bool,
    /// DTS:X IMAX variant present.
    pub x_imax: bool,
    /// Raw DTS:X extension payload (syncword + data) for diagnostics. Empty when
    /// no extension is present.
    pub x_payload: Vec<u8>,
    /// Byte offset of `x_payload` within the XLL frame.
    pub x_payload_offset: usize,
    /// Four decoded DTS:X extension waveforms. The XLL channel-set header does
    /// not assign these channels to speakers, so they are deliberately not
    /// mixed into `samples` or `output_mask` yet.
    pub x_samples: Vec<Vec<f32>>,
    pub x_pcm_bit_res: usize,
    /// Bit position reached after decoding the four extension waveforms,
    /// relative to `x_payload`.
    pub x_bits_consumed: usize,
    /// Diagnostic failure from the optional extension decode. A failure here
    /// never invalidates the lossless 7.1 bed.
    pub x_decode_error: Option<String>,
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

    /// Decode one DTS-HD frame from its core access unit + EXSS substream bytes.
    pub fn decode(&mut self, core_au: &[u8], exss: &[u8]) -> Result<HdFrame, HdError> {
        // 1) Core (residual base), fixed-point, indexed by speaker.
        let info = parse_header(core_au)?;
        self.core
            .decode_frame(&info, core_au)
            .map_err(|_| HdError::Core)?;
        let core_out = self.synth.synthesize_fixed_by_speaker(&mut self.core);

        // 2) EXSS → locate XLL asset.
        let exssp = ExssParser::parse(exss).map_err(|_| HdError::Exss)?;

        // 3) XLL lossless decode, combined with the core residual.
        match self.xll.decode(exss, &exssp.asset, Some(&core_out)) {
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
            exss_descriptor_tail: exssp.asset.descriptor_tail.clone(),
            exss_descriptor_tail_bits: exssp.asset.descriptor_tail_bits,
            x_descriptor_offset: exssp.asset.xll_x_offset,
            x_descriptor_size: exssp.asset.xll_x_size,
            x_descriptor_navigation_used: self.xll.x_descriptor_navigation_used,
        })
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
        // Ex Machina (2014): 24-bit DTS-HD MA 7.1.
        check_xll_lossless(
            "/mnt/local/SSD_B-CT4000/Dumps/Ex.Machina.2014.dtsx.eng.dts",
            "/home/user/dev/spatial-renderer/dumps/dtsx_ref8.f32",
            8,
        );
    }

    #[test]
    fn xll_5_1_16bit_matches_ffmpeg_lossless() {
        // Dark Crystal (1982): 16-bit DTS-HD MA 5.1. Regression guard for the
        // 16-bit-storage output-scale bug (the bed was ~48 dB / 256x too quiet).
        check_xll_lossless(
            "/home/user/dev/spatial-renderer/dumps/darkcrystal_dts51_16b.dts",
            "/home/user/dev/spatial-renderer/dumps/darkcrystal_ref6.f32",
            6,
        );
    }

    #[test]
    fn xll_x_decodes_four_unmapped_waveforms() {
        use std::io::Read;

        const DUMP: &str = "/mnt/local/SSD_A-CT4000/DTS:X-Dumps/Ex.Machina.2014.dtsx.eng.dts";
        if !std::path::Path::new(DUMP).exists() {
            eprintln!("skipping: spatial-layer corpus not present ({DUMP})");
            return;
        }
        let mut bytes = Vec::new();
        std::fs::File::open(DUMP)
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
