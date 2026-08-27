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
    pending_independent_core: Option<PendingIndependentCore>,
    /// Decoded legacy AC-3 core (bsid <= 10) buffered — not yet emitted —
    /// until we see whether the next access unit is its dependent partner.
    pending_ac3_core: Option<PcmPushResult>,
    frame_count: u64,
}

/// The independent frame's core, buffered for a dependent substream that may
/// follow it.
///
/// A dependent JOC frame needs the downmix the reconstruction actually ran on;
/// the no-JOC fallback emits a plain core frame and wants the core exactly as
/// the independent frame shipped it. Those are the same audio unless the
/// independent frame carried objects, where the shipped core is held back by
/// [`eac3::JOC_LATENCY_SAMPLES`] to sit with the objects it is written beside —
/// so only that case has two forms to keep.
enum PendingIndependentCore {
    /// No objects on the frame, so nothing was delayed and one core answers
    /// both questions. This is the path every plain E-AC-3 stream takes on
    /// every frame, which is why it holds a single copy.
    Undelayed(eac3::CorePcmFrame),
    /// An object frame, where the two forms are [`eac3::JOC_LATENCY_SAMPLES`]
    /// apart and neither can stand in for the other.
    Delayed {
        joc_input: eac3::CorePcmFrame,
        aligned: eac3::CorePcmFrame,
    },
}

impl PendingIndependentCore {
    /// Split into the core to reconstruct a following dependent frame's objects
    /// from, and the core to emit if that frame turns out to carry no JOC.
    ///
    /// `push_access_unit_with_core` takes its input by value and drops it when
    /// there is no JOC to spend it on, so the fallback form has to be held back
    /// separately — before either branch is known. When the two forms are the
    /// same core that costs one copy, but only here, on a frame that actually
    /// has a dependent substream to pair with, rather than on every frame that
    /// might turn out to have one.
    fn split(self) -> (eac3::CorePcmFrame, eac3::CorePcmFrame) {
        match self {
            Self::Undelayed(core) => (core.clone(), core),
            Self::Delayed { joc_input, aligned } => (joc_input, aligned),
        }
    }
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
                return surface_decode_err(err, strict_mode, tx, pb, state, &frame.info());
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
        if let Some(pending) = state.pending_independent_core.take() {
            let (joc_input, aligned) = pending.split();
            match state
                .object_decoder
                .push_access_unit_with_core(bytes, joc_input)
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
                        pcm: aligned,
                    };
                    let _ = tx.send(Ok(Eac3FrameMessage::Core(push)));
                    return Ok(());
                }
                Err(err) => {
                    return surface_decode_err(err, strict_mode, tx, pb, state, &frame.info());
                }
            }
        }
    }

    // Try object decode first (independent frame with JOC).
    match state.object_decoder.push_access_unit(bytes) {
        Ok(Some(obj)) => {
            state.frame_count += 1;
            tick_progress(pb);
            // The core on the frame is held back to sit with the objects it
            // ships beside; a following dependent JOC substream needs the one
            // the reconstruction actually ran on, or it decodes from a core
            // that is already late and then gets delayed a second time. The
            // no-JOC fallback still wants the aligned one, so keep both. Only
            // the aligned form is copied — the decoder hands the other over.
            state.pending_independent_core =
                state.object_decoder.take_joc_input_core().map(|joc_input| {
                    PendingIndependentCore::Delayed {
                        joc_input,
                        aligned: obj.pcm.core.clone(),
                    }
                });
            let _ = tx.send(Ok(Eac3FrameMessage::Object(obj)));
            return Ok(());
        }
        Ok(None) => {
            // No JOC — fall through to core PCM decode.
        }
        Err(err) => {
            return surface_decode_err(err, strict_mode, tx, pb, state, &frame.info());
        }
    }

    match state.pcm_decoder.push_access_unit(bytes) {
        Ok(result) => {
            state.frame_count += 1;
            tick_progress(pb);
            // No objects on this frame, so no alignment delay was applied and
            // the two forms are the same core: one copy, because the result
            // itself is about to be sent away.
            state.pending_independent_core =
                Some(PendingIndependentCore::Undelayed(result.pcm.clone()));
            let _ = tx.send(Ok(Eac3FrameMessage::Core(result)));
        }
        Err(err) => {
            return surface_decode_err(err, strict_mode, tx, pb, state, &frame.info());
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
            return surface_decode_err(err, strict_mode, tx, pb, state, &frame.info());
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
                return surface_decode_err(err, strict_mode, tx, pb, state, &frame.info());
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
    state: &mut DecoderState,
    info: &eac3::FrameInfo,
) -> Result<()> {
    // The frame that failed is dropped from every decoder's point of view, so
    // this is a stream discontinuity: cross-frame state (IMDCT overlap-add
    // delay, coupling/SPX reuse flags, JOC differential state) must not be
    // carried into the frames that follow, and buffered partial pairs are
    // stale.
    state.object_decoder.reset();
    state.pcm_decoder.reset();
    state.ac3_core_decoder.reset();
    state.dependent_pcm_decoder.reset();
    state.pending_independent_core = None;
    state.pending_ac3_core = None;

    let msg = format!("{err:?}");
    if strict_mode {
        return Err(anyhow::anyhow!("EAC3 decode error: {msg}"));
    }
    log::warn!("EAC3 decode error (substituting silence): {msg}");
    state.frame_count += 1;
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
