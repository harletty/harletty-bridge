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

use super::dts_handler::DtsFrameMessage;
use crate::input::InputReader;
use anyhow::Result;
use dca::{HdDecoder, HdError, PcmDecoder, XPresentation, exss_has_xll, exss_substream_size,
    parse_header};
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

struct DtsDecodeState {
    buffer: Vec<u8>,
    core: PcmDecoder,
    hd: HdDecoder,
    frame_count: u64,
    strict_mode: bool,
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

        let mut state = DtsDecodeState {
            buffer: prefix,
            core: PcmDecoder::new(),
            hd: HdDecoder::new(),
            frame_count: 0,
            strict_mode,
        };

        let mut input_reader = InputReader::new(&input_path)?;
        input_reader.process_chunks(64 * 1024, |chunk| {
            state.buffer.extend_from_slice(chunk);
            drain_frames(&mut state, &pb_clone, &tx)?;
            Ok(true)
        })?;

        // A trailing frame can still be complete after EOF.
        drain_frames(&mut state, &pb_clone, &tx)?;

        log::info!("DTS decode complete: {} frames", state.frame_count);
        Ok(())
    })
}

/// Consume every complete frame currently buffered, leaving any partial tail.
fn drain_frames(
    state: &mut DtsDecodeState,
    pb: &Option<ProgressBar>,
    tx: &mpsc::Sender<Result<DtsFrameMessage>>,
) -> Result<()> {
    let mut consumed = 0usize;
    loop {
        let rest = &state.buffer[consumed..];
        let Some(sync_offset) = find(rest, &CORE_SYNC) else {
            // Keep back three bytes in case a syncword straddles the boundary.
            consumed += rest.len().saturating_sub(CORE_SYNC.len() - 1);
            break;
        };
        consumed += sync_offset;
        let rest = &state.buffer[consumed..];

        let info = match parse_header(rest) {
            Ok(info) => info,
            Err(dca::HeaderParseError::InsufficientData) => break,
            Err(err) => {
                if state.strict_mode {
                    return Err(anyhow::anyhow!("invalid DTS header: {err:?}"));
                }
                log::debug!("skipping invalid DTS header: {err:?}");
                consumed += CORE_SYNC.len(); // resync past this candidate
                continue;
            }
        };

        let core_size = info.frame_size;
        // Need the core frame plus four bytes to test for a trailing EXSS.
        if rest.len() < core_size + 4 {
            break;
        }

        let mut frame_size = core_size;
        let mut exss: Option<&[u8]> = None;
        if rest[core_size..core_size + 4] == SUBSTREAM_SYNC {
            let Some(exss_size) = exss_substream_size(&rest[core_size..]) else {
                break; // EXSS header not fully buffered yet
            };
            if rest.len() < core_size + exss_size {
                break;
            }
            frame_size = core_size + exss_size;
            let candidate = &rest[core_size..core_size + exss_size];
            // No XLL asset means nothing lossless to reconstruct; fall back to
            // the core, which every such stream still carries.
            if exss_has_xll(candidate) {
                exss = Some(candidate);
            }
        }

        // Borrow the decoders and the buffer as disjoint fields: `rest` still
        // points into `state.buffer`, so `state` cannot be passed whole here.
        let DtsDecodeState {
            core: core_decoder,
            hd: hd_decoder,
            frame_count,
            strict_mode,
            ..
        } = state;
        decode_one(
            DecodeTargets {
                core_decoder,
                hd_decoder,
                frame_count,
                strict_mode: *strict_mode,
            },
            &rest[..core_size],
            exss,
            pb,
            tx,
        )?;
        consumed += frame_size;
    }

    state.buffer.drain(..consumed);
    Ok(())
}

/// The mutable slice of decode state one frame needs, borrowed apart from the
/// input buffer the frame bytes point into.
struct DecodeTargets<'a> {
    core_decoder: &'a mut PcmDecoder,
    hd_decoder: &'a mut HdDecoder,
    frame_count: &'a mut u64,
    strict_mode: bool,
}

fn decode_one(
    targets: DecodeTargets<'_>,
    core: &[u8],
    exss: Option<&[u8]>,
    pb: &Option<ProgressBar>,
    tx: &mpsc::Sender<Result<DtsFrameMessage>>,
) -> Result<()> {
    let DecodeTargets {
        core_decoder,
        hd_decoder,
        frame_count,
        strict_mode,
    } = targets;
    if let Some(exss) = exss {
        match hd_decoder.decode(core, exss) {
            Ok(frame) => {
                let presentation = XPresentation::detect(&frame);
                *frame_count += 1;
                tick(pb);
                let _ = tx.send(Ok(DtsFrameMessage::Hd {
                    frame: Box::new(frame),
                    presentation,
                }));
                return Ok(());
            }
            // Peak-bitrate buffering: this packet yields no frame, which is
            // normal and not an error.
            Err(HdError::Pending) => return Ok(()),
            Err(err) => {
                if strict_mode {
                    return Err(anyhow::anyhow!("DTS-HD decode error: {err:?}"));
                }
                log::warn!("DTS-HD decode error, falling back to the core: {err:?}");
            }
        }
    }

    match core_decoder.push_access_unit(core) {
        Ok(push) => {
            *frame_count += 1;
            tick(pb);
            let _ = tx.send(Ok(DtsFrameMessage::Core(Box::new(push))));
        }
        Err(err) => {
            if strict_mode {
                return Err(anyhow::anyhow!("DTS core decode error: {err:?}"));
            }
            log::warn!("DTS core decode error (dropping frame): {err:?}");
        }
    }
    Ok(())
}

fn find(data: &[u8], needle: &[u8; 4]) -> Option<usize> {
    data.windows(needle.len()).position(|w| w == needle)
}

fn tick(pb: &Option<ProgressBar>) {
    if let Some(pb) = pb {
        pb.inc(1);
    }
}
