use super::eac3_handler::Eac3FrameMessage;
use crate::input::InputReader;
use anyhow::Result;
use eac3::{
    AccessUnitParseError, ExtractError, Extractor, Frame, FrameType, ObjectPcmDecoder, PcmDecoder,
    PcmPushResult, inspect_access_unit, merge_core_with_dependent,
};
use indicatif::ProgressBar;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

pub struct Eac3DecoderThreadConfig {
    pub input_path: PathBuf,
    pub strict_mode: bool,
    pub prefix: Vec<u8>,
    pub tx: mpsc::Sender<Result<Eac3FrameMessage>>,
    pub pb_clone: Option<ProgressBar>,
}

/// Cross-frame decode state for one E-AC-3 stream.
///
/// Legacy AC-3 cores and dependent substreams each get a dedicated
/// [`PcmDecoder`] so their bitstream state never mixes with the independent
/// E-AC-3 stream's — mirroring the bridge's `ac3_decoder` /
/// `eac3_dependent_pcm_decoder` split.
#[derive(Default)]
struct DecoderState {
    object_decoder: ObjectPcmDecoder,
    pcm_decoder: PcmDecoder,
    ac3_core_decoder: PcmDecoder,
    dependent_pcm_decoder: PcmDecoder,
    /// Core of the last decoded (and already emitted) independent E-AC-3
    /// frame, kept in case the next frame is a JOC dependent.
    pending_independent_core: Option<eac3::CorePcmFrame>,
    /// Decoded legacy AC-3 core (bsid <= 10) buffered — not yet emitted —
    /// until we see whether the next access unit is its dependent partner.
    pending_ac3_core: Option<PcmPushResult>,
    frame_count: u64,
}

pub fn spawn_eac3_decoder_thread(
    config: Eac3DecoderThreadConfig,
) -> thread::JoinHandle<Result<()>> {
    thread::spawn(move || -> Result<()> {
        let Eac3DecoderThreadConfig {
            input_path,
            strict_mode,
            prefix,
            tx,
            pb_clone,
        } = config;

        let mut extractor = Extractor::default();
        let mut state = DecoderState::default();

        if !prefix.is_empty() {
            extractor.push_bytes(&prefix);
        }

        let mut input_reader = InputReader::new(&input_path)?;
        // When prefix already consumed the file head, we still drive process_chunks against
        // the remaining stream. For piped input, the prefix is the head we already buffered.
        let result = input_reader.process_chunks(64 * 1024, |chunk| {
            extractor.push_bytes(chunk);
            drain_frames(&mut extractor, &mut state, &pb_clone, strict_mode, &tx)
        });

        if result.is_err() {
            return result;
        }

        // Final drain after EOF.
        drain_frames(&mut extractor, &mut state, &pb_clone, strict_mode, &tx)?;

        // A trailing AC-3 core whose dependent partner never arrived (stream
        // truncated mid-pair): emit it as a standalone 5.1 bed.
        if let Some(core) = state.pending_ac3_core.take() {
            emit_standalone_ac3_core(core, &mut state.frame_count, &pb_clone, &tx);
        }

        log::info!("EAC3 decode complete: {} frames", state.frame_count);
        Ok(())
    })
}

fn drain_frames(
    extractor: &mut Extractor,
    state: &mut DecoderState,
    pb: &Option<ProgressBar>,
    strict_mode: bool,
    tx: &mpsc::Sender<Result<Eac3FrameMessage>>,
) -> Result<bool> {
    loop {
        match extractor.next_frame() {
            Ok(Some(frame)) => {
                handle_frame(&frame, state, pb, strict_mode, tx)?;
            }
            Ok(None) => return Ok(true),
            Err(ExtractError::InvalidHeader(err)) => {
                if strict_mode {
                    return Err(anyhow::anyhow!("invalid EAC3 header: {err:?}"));
                }
                log::debug!("skipping invalid EAC3 header: {err:?}");
            }
        }
    }
}

fn handle_frame(
    frame: &Frame,
    state: &mut DecoderState,
    pb: &Option<ProgressBar>,
    strict_mode: bool,
    tx: &mpsc::Sender<Result<Eac3FrameMessage>>,
) -> Result<()> {
    let bytes = frame.as_bytes();

    // `inspect_access_unit` rejects legacy AC-3 syncframes (bsid <= 10) with
    // `NotEac3` — that rejection IS the legacy detection.
    let (frame_type, is_legacy_ac3) = match inspect_access_unit(bytes) {
        Ok(info) => (Some(info.frame_type), false),
        Err(AccessUnitParseError::NotEac3) => (None, true),
        Err(_) => (None, false),
    };
    let is_dependent = matches!(frame_type, Some(FrameType::Dependent));

    // A buffered AC-3 core is only ever paired with an *immediately* following
    // dependent access unit. Any other frame type means the buffered core had
    // no partner (plain AC-3 never carries dependents) — emit it as a
    // standalone 5.1 bed before handling this unit, otherwise the core sits
    // buffered forever and the output is short.
    if !is_dependent {
        if let Some(core) = state.pending_ac3_core.take() {
            emit_standalone_ac3_core(core, &mut state.frame_count, pb, tx);
        }
    }

    // Legacy AC-3 core: decode with the dedicated AC-3 decoder and buffer it
    // until the next access unit tells us whether it has a dependent partner
    // (BD-style DD+ 7.1 delivers [AC-3 5.1 core, E-AC-3 dependent] pairs).
    if is_legacy_ac3 {
        match state.ac3_core_decoder.push_legacy_ac3_access_unit(bytes) {
            Ok(result) => {
                state.pending_ac3_core = Some(result);
                return Ok(());
            }
            Err(err) => {
                return surface_decode_err(
                    err,
                    strict_mode,
                    tx,
                    pb,
                    &mut state.frame_count,
                    &frame.info(),
                );
            }
        }
    }

    if is_dependent {
        // Pair a buffered legacy AC-3 core with this dependent substream.
        if let Some(core_result) = state.pending_ac3_core.take() {
            return handle_ac3_core_pair(core_result, frame, state, pb, strict_mode, tx);
        }

        // Dependent following an independent E-AC-3 frame: decode against the
        // previously decoded (and already emitted) independent core.
        if let Some(core) = state.pending_independent_core.take() {
            match state
                .object_decoder
                .push_access_unit_with_core(bytes, core.clone())
            {
                Ok(Some(obj)) => {
                    state.frame_count += 1;
                    tick_progress(pb);
                    let _ = tx.send(Ok(Eac3FrameMessage::Object(obj)));
                    return Ok(());
                }
                Ok(None) => {
                    // No JOC on dependent frame — emit the core we already have.
                    state.frame_count += 1;
                    tick_progress(pb);
                    let info = match inspect_access_unit(bytes) {
                        Ok(i) => i,
                        Err(_) => return Ok(()),
                    };
                    let push = PcmPushResult {
                        frames_seen: state.frame_count,
                        info,
                        pcm: core,
                    };
                    let _ = tx.send(Ok(Eac3FrameMessage::Core(push)));
                    return Ok(());
                }
                Err(err) => {
                    return surface_decode_err(
                        err,
                        strict_mode,
                        tx,
                        pb,
                        &mut state.frame_count,
                        &frame.info(),
                    );
                }
            }
        }
    }

    // Try object decode first (independent frame with JOC).
    match state.object_decoder.push_access_unit(bytes) {
        Ok(Some(obj)) => {
            state.frame_count += 1;
            tick_progress(pb);
            state.pending_independent_core = Some(obj.pcm.core.clone());
            let _ = tx.send(Ok(Eac3FrameMessage::Object(obj)));
            return Ok(());
        }
        Ok(None) => {
            // No JOC — fall through to core PCM decode.
        }
        Err(err) => {
            return surface_decode_err(
                err,
                strict_mode,
                tx,
                pb,
                &mut state.frame_count,
                &frame.info(),
            );
        }
    }

    match state.pcm_decoder.push_access_unit(bytes) {
        Ok(result) => {
            state.frame_count += 1;
            tick_progress(pb);
            state.pending_independent_core = Some(result.pcm.clone());
            let _ = tx.send(Ok(Eac3FrameMessage::Core(result)));
        }
        Err(err) => {
            return surface_decode_err(
                err,
                strict_mode,
                tx,
                pb,
                &mut state.frame_count,
                &frame.info(),
            );
        }
    }
    Ok(())
}

/// Decode a dependent substream against the buffered legacy AC-3 core it
/// belongs to. JOC dependents (DD+ Atmos with a backward-compatible AC-3 core)
/// go through the object decoder; non-JOC pairs are a plain channel-extension
/// bed, merged into a full 7.1 frame (falling back to the 5.1 core alone if
/// the merge fails — never silent).
fn handle_ac3_core_pair(
    core_result: PcmPushResult,
    frame: &Frame,
    state: &mut DecoderState,
    pb: &Option<ProgressBar>,
    strict_mode: bool,
    tx: &mpsc::Sender<Result<Eac3FrameMessage>>,
) -> Result<()> {
    let bytes = frame.as_bytes();
    let dep_info = match inspect_access_unit(bytes) {
        Ok(info) => info,
        Err(err) => {
            return surface_decode_err(
                err,
                strict_mode,
                tx,
                pb,
                &mut state.frame_count,
                &frame.info(),
            );
        }
    };

    if dep_info.joc_payload_count() > 0 {
        match state
            .object_decoder
            .push_access_unit_with_core(bytes, core_result.pcm.clone())
        {
            Ok(Some(obj)) => {
                state.frame_count += 1;
                tick_progress(pb);
                let _ = tx.send(Ok(Eac3FrameMessage::Object(obj)));
                return Ok(());
            }
            Ok(None) => {
                // No object payload after all — fall through to the channel bed.
            }
            Err(err) => {
                return surface_decode_err(
                    err,
                    strict_mode,
                    tx,
                    pb,
                    &mut state.frame_count,
                    &frame.info(),
                );
            }
        }
    }

    let bed = merge_core_with_dependent(&mut state.dependent_pcm_decoder, &core_result.pcm, bytes)
        .unwrap_or(core_result.pcm);
    state.frame_count += 1;
    tick_progress(pb);
    let push = PcmPushResult {
        frames_seen: state.frame_count,
        info: dep_info,
        pcm: bed,
    };
    let _ = tx.send(Ok(Eac3FrameMessage::Core(push)));
    Ok(())
}

/// Emit a buffered legacy AC-3 core that had no dependent partner as a plain
/// 5.1 bed.
fn emit_standalone_ac3_core(
    core: PcmPushResult,
    frame_count: &mut u64,
    pb: &Option<ProgressBar>,
    tx: &mpsc::Sender<Result<Eac3FrameMessage>>,
) {
    *frame_count += 1;
    tick_progress(pb);
    let _ = tx.send(Ok(Eac3FrameMessage::Core(core)));
}

fn surface_decode_err(
    err: AccessUnitParseError,
    strict_mode: bool,
    tx: &mpsc::Sender<Result<Eac3FrameMessage>>,
    pb: &Option<ProgressBar>,
    frame_count: &mut u64,
    info: &eac3::FrameInfo,
) -> Result<()> {
    let msg = format!("{err:?}");
    if strict_mode {
        return Err(anyhow::anyhow!("EAC3 decode error: {msg}"));
    }
    log::warn!("EAC3 decode error (substituting silence): {msg}");
    *frame_count += 1;
    tick_progress(pb);
    let _ = tx.send(Ok(Eac3FrameMessage::Silence {
        sample_count: info.samples as usize,
        sample_rate: info.sample_rate,
        channel_count: info.channels() as usize,
    }));
    Ok(())
}

fn tick_progress(pb: &Option<ProgressBar>) {
    if let Some(pb) = pb {
        pb.inc(1);
    }
}
