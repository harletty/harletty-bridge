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
        self.core.decode_frame(&info, core_au).map_err(|_| HdError::Core)?;
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
            .map(|opt| opt.as_ref().map(|v| v.iter().map(|&s| s as f32 / PCM_SCALE).collect()))
            .collect();

        Ok(HdFrame {
            sample_rate: self.xll.sample_rate,
            output_mask: self.xll.output_mask,
            samples,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dcadec::exss::ExssParser;

    const DUMP: &str = "/home/user/dev/spatial-renderer/dumps/Ex.Machina.2014.dtsx.eng.dts";
    const REF: &str = "/home/user/dev/spatial-renderer/dumps/dtsx_ref8.f32";
    const CH: usize = 8;

    #[test]
    fn xll_7_1_matches_ffmpeg_lossless() {
        if !std::path::Path::new(DUMP).exists() || !std::path::Path::new(REF).exists() {
            eprintln!("skipping: 7.1 corpus not present");
            return;
        }
        let bytes = std::fs::read(DUMP).unwrap();
        let rbytes = std::fs::read(REF).unwrap();
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
                    let n = f
                        .samples
                        .iter()
                        .find_map(|o| o.as_ref().map(|v| v.len()))
                        .unwrap_or(0);
                    for s in 0..32 {
                        if let Some(v) = &f.samples[s] {
                            spk[s].extend_from_slice(v);
                        } else {
                            // keep arrays aligned in length for active speakers only
                            let _ = n;
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

        let nsamp = spk.iter().filter(|v| !v.is_empty()).map(|v| v.len()).min().unwrap();
        let ref_n = reference.len() / CH;
        let cmp = nsamp.min(ref_n);
        assert!(cmp > 5000, "too little to compare ({cmp})");

        // Auto-match each ffmpeg channel to the best-fitting decoded speaker;
        // lossless ⇒ exact match (rmse ~0).
        let active: Vec<usize> = (0..32).filter(|&s| !spk[s].is_empty()).collect();
        eprintln!("active speakers: {active:?}");
        let mut worst = 0f64;
        for rc in 0..CH {
            let mut best = f64::INFINITY;
            let mut best_spk = 0usize;
            let mut best_max = 0f32;
            for &s in &active {
                let mut sq = 0f64;
                let mut mx = 0f32;
                for i in 0..cmp {
                    let d = (spk[s][i] - reference[i * CH + rc]).abs();
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
}
