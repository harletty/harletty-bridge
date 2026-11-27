//! Demux a raw DTS byte stream and decode each frame.
//!
//! DTS elementary streams are a run of `[core]` or `[core][exss]` frames. When
//! an EXSS substream follows the core and carries an XLL asset, the lossless HD
//! decoder reconstructs the 5.1/7.1 bed plus any DTS:X extension feeds;
//! otherwise only the core is decodable (DTS-HD HRA layers lossy detail we do
//! not decode, so its core is rendered and the extension dropped).
//!
//! The demux mirrors `bridge/src/dts_pipeline.rs`, which does the same job for
//! the realtime path. Kept separate on purpose: that one is a streaming
//! push-model driven by an audio callback, this one owns a file and can block.

use super::auro_stage::AuroStage;
use super::dts_handler::DtsFrameMessage;
use crate::input::InputReader;
use anyhow::Result;
use dca::{
    ExssKind, HdDecoder, HdError, HdFrame, PcmDecoder, XMetadata, XPresentation, exss_kind,
    exss_substream_size, parse_header,
};
use indicatif::ProgressBar;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

const CORE_SYNC: [u8; 4] = 0x7FFE_8001u32.to_be_bytes();
const SUBSTREAM_SYNC: [u8; 4] = 0x6458_2025u32.to_be_bytes();

pub struct DtsDecoderThreadConfig {
    pub input_path: PathBuf,
    pub strict_mode: bool,
    pub prefix: Vec<u8>,
    pub tx: mpsc::Sender<Result<DtsFrameMessage>>,
    pub pb_clone: Option<ProgressBar>,
}

/// Where decoded frames go: the single-threaded path's channel, or the
/// bounded one the parallel path writes through.
pub trait FrameSink {
    fn deliver(&self, message: Result<DtsFrameMessage>);
}

impl FrameSink for mpsc::Sender<Result<DtsFrameMessage>> {
    fn deliver(&self, message: Result<DtsFrameMessage>) {
        // The receiver hangs up only when it stops for good.
        let _ = self.send(message);
    }
}

pub fn spawn_dts_decoder_thread(config: DtsDecoderThreadConfig) -> thread::JoinHandle<Result<()>> {
    thread::spawn(move || -> Result<()> {
        let DtsDecoderThreadConfig {
            input_path,
            strict_mode,
            prefix,
            tx,
            pb_clone,
        } = config;

        let mut buffer = prefix;
        let mut hd = HdDecoder::new();
        let mut downstream = Downstream::new(strict_mode);
        let mut decode = |core: &[u8], exss: Option<&[u8]>| {
            let outcome = decode_hd(&mut hd, core, exss);
            downstream.take(outcome, core, &pb_clone, &tx)
        };

        let mut input_reader = InputReader::new(&input_path)?;
        input_reader.process_chunks(64 * 1024, |chunk| {
            buffer.extend_from_slice(chunk);
            drain_frames(&mut buffer, strict_mode, false, &mut decode)?;
            Ok(true)
        })?;

        // The final frame has nothing after it, so it is only decodable once
        // the demux knows no EXSS can follow.
        drain_frames(&mut buffer, strict_mode, true, &mut decode)?;
        downstream.finish(&tx);
        Ok(())
    })
}

/// Frame every complete frame buffered in `buffer` and hand it to `frame`,
/// as its core bytes and the EXSS substream the HD decoder reads (`None`: the
/// core alone), then drop those bytes, leaving any partial tail.
///
/// `at_eof` says no more input will arrive: a core frame that ends the buffer
/// (or is followed by fewer bytes than an EXSS syncword) is then complete and
/// decoded core-only, instead of being held back to look for an EXSS.
pub(super) fn drain_frames(
    buffer: &mut Vec<u8>,
    strict_mode: bool,
    at_eof: bool,
    frame: &mut impl FnMut(&[u8], Option<&[u8]>) -> Result<()>,
) -> Result<()> {
    let mut consumed = 0usize;
    loop {
        let rest = &buffer[consumed..];
        let Some(sync_offset) = find(rest, &CORE_SYNC) else {
            // Keep back three bytes in case a syncword straddles the boundary.
            consumed += rest.len().saturating_sub(CORE_SYNC.len() - 1);
            break;
        };
        consumed += sync_offset;
        let rest = &buffer[consumed..];

        let info = match parse_header(rest) {
            Ok(info) => info,
            Err(dca::HeaderParseError::InsufficientData) => break,
            Err(err) => {
                if strict_mode {
                    return Err(anyhow::anyhow!("invalid DTS header: {err:?}"));
                }
                log::debug!("skipping invalid DTS header: {err:?}");
                consumed += CORE_SYNC.len(); // resync past this candidate
                continue;
            }
        };

        let core_size = info.frame_size;
        // Need the core frame plus four bytes to test for a trailing EXSS,
        // unless the input has ended: then whatever follows the core is too
        // short to be one, and the core stands alone.
        let exss_testable = rest.len() >= core_size + SUBSTREAM_SYNC.len();
        if !exss_testable && !(at_eof && rest.len() >= core_size) {
            break;
        }

        let mut frame_size = core_size;
        let mut exss: Option<&[u8]> = None;
        if exss_testable && rest[core_size..core_size + SUBSTREAM_SYNC.len()] == SUBSTREAM_SYNC {
            let buffered = exss_substream_size(&rest[core_size..])
                .filter(|&exss_size| rest.len() >= core_size + exss_size);
            match buffered {
                Some(exss_size) => {
                    frame_size = core_size + exss_size;
                    let candidate = &rest[core_size..core_size + exss_size];
                    // Nothing beyond the core to reconstruct (an XBR-only lossy
                    // extension) means the core, which every such stream still
                    // carries; a lossless asset or a lossy carrier's channel
                    // sets go through the HD decoder.
                    if exss_kind(candidate) != ExssKind::Core {
                        exss = Some(candidate);
                    }
                }
                // A file cut inside the EXSS: it will never complete, but the
                // core before it is whole and still plays.
                None if at_eof => {}
                None => break, // EXSS not fully buffered yet
            }
        }

        frame(&rest[..core_size], exss)?;
        consumed += frame_size;
    }

    buffer.drain(..consumed);
    Ok(())
}

/// What the HD decoder made of a frame: the part of a frame's decode that
/// depends on nothing but the HD decoder's own state.
pub(super) enum Outcome {
    Decoded {
        frame: Box<HdFrame>,
        presentation: Option<XPresentation>,
        metadata: Option<XMetadata>,
        /// Whether the decoder had integer output for it: false for a lossy
        /// carrier.
        lossless: bool,
    },
    /// Peak-bitrate buffering: this packet yields no frame, which is normal
    /// and not an error.
    Pending,
    /// The HD decoder could not decode the frame: its core plays instead.
    Failed(HdError),
    /// No EXSS for the HD decoder: the core alone.
    CoreOnly,
}

/// Decodes a frame in the HD decoder, when it has an EXSS for it.
pub(super) fn decode_hd(hd: &mut HdDecoder, core: &[u8], exss: Option<&[u8]>) -> Outcome {
    let Some(exss) = exss else {
        return Outcome::CoreOnly;
    };
    match hd.decode(core, exss) {
        Ok(frame) => {
            let presentation = XPresentation::detect(&frame);
            // Unreadable metadata is reported by the handler once; the
            // frame still plays with its bed as authored.
            let metadata =
                presentation.and_then(|p| XMetadata::parse(&frame.x_payload, p.feed_count()).ok());
            let lossless = hd.lossless_samples().next().is_some();
            Outcome::Decoded {
                frame: Box::new(frame),
                presentation,
                metadata,
                lossless,
            }
        }
        Err(HdError::Pending) => Outcome::Pending,
        Err(err) => Outcome::Failed(err),
    }
}

/// What a frame's decode does past the HD decoder, in stream order: the
/// core-only decoder for a frame without one, the Auro-3D stage, the count.
/// Each depends on the frames before it.
pub(super) struct Downstream {
    core: PcmDecoder,
    /// Auro-Codec detection and unfolding over the lossless output; owns
    /// the frames it holds back while deciding.
    auro: AuroStage,
    frame_count: u64,
    strict_mode: bool,
}

impl Downstream {
    pub(super) fn new(strict_mode: bool) -> Self {
        Self {
            core: PcmDecoder::new(),
            auro: AuroStage::new(),
            frame_count: 0,
            strict_mode,
        }
    }

    /// Takes the frame whose core is `core` on from `outcome`.
    pub(super) fn take(
        &mut self,
        outcome: Outcome,
        core: &[u8],
        pb: &Option<ProgressBar>,
        tx: &impl FrameSink,
    ) -> Result<()> {
        match outcome {
            Outcome::Decoded {
                frame,
                presentation,
                metadata,
                lossless,
            } => {
                self.frame_count += 1;
                tick(pb);
                if !lossless {
                    // A lossy carrier: no side channel to read, the frame
                    // goes out as it is.
                    self.auro.not_a_carrier(tx);
                    tx.deliver(Ok(DtsFrameMessage::Hd {
                        frame,
                        presentation,
                        metadata,
                    }));
                } else {
                    // The stage decides whether the frame goes out as it is
                    // or unfolded.
                    self.auro.frame(frame, presentation, metadata, tx);
                }
                Ok(())
            }
            Outcome::Pending => Ok(()),
            Outcome::Failed(err) => {
                if self.strict_mode {
                    return Err(anyhow::anyhow!("DTS-HD decode error: {err:?}"));
                }
                log::warn!("DTS-HD decode error, falling back to the core: {err:?}");
                self.core_only(core, pb, tx)
            }
            Outcome::CoreOnly => self.core_only(core, pb, tx),
        }
    }

    fn core_only(
        &mut self,
        core: &[u8],
        pb: &Option<ProgressBar>,
        tx: &impl FrameSink,
    ) -> Result<()> {
        match self.core.push_access_unit(core) {
            Ok(push) => {
                self.frame_count += 1;
                tick(pb);
                // A lossy core frame cannot carry the side channel.
                self.auro.not_a_carrier(tx);
                tx.deliver(Ok(DtsFrameMessage::Core(Box::new(push))));
            }
            Err(err) => {
                if self.strict_mode {
                    return Err(anyhow::anyhow!("DTS core decode error: {err:?}"));
                }
                log::warn!("DTS core decode error (dropping frame): {err:?}");
            }
        }
        Ok(())
    }

    /// Input is over: releases what the Auro stage held back.
    pub(super) fn finish(&mut self, tx: &impl FrameSink) {
        self.auro.finish(tx);
        log::info!("DTS decode complete: {} frames", self.frame_count);
    }
}

fn find(data: &[u8], needle: &[u8; 4]) -> Option<usize> {
    data.windows(needle.len()).position(|w| w == needle)
}

fn tick(pb: &Option<ProgressBar>) {
    if let Some(pb) = pb {
        pb.inc(1);
    }
}
