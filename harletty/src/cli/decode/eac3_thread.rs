use super::eac3_handler::Eac3FrameMessage;
use crate::input::InputReader;
use anyhow::Result;
use eac3::{
    AccessUnitParseError, ExtractError, Extractor, Frame, ObjectPcmDecoder, PcmDecoder,
    PcmPushResult, StreamType, inspect_access_unit, merge_core_with_decoded_dependent,
    merge_core_with_dependent,
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
    /// The last independent E-AC-3 frame, buffered until the next access unit
    /// shows whether a dependent substream pairs with it.
    pending_independent: Option<PendingIndependent>,
    /// Decoded legacy AC-3 core (bsid <= 10) buffered — not yet emitted —
    /// until we see whether the next access unit is its dependent partner.
    pending_ac3_core: Option<PcmPushResult>,
    /// Whether the last dependent substream looked at carried JOC: a guess at
    /// what the next one is, which [`read_dependent`] acts on and corrects.
    dependents_carry_joc: bool,
    frame_count: u64,
}

/// A dependent substream and how it was learned what it carries.
///
/// Its JOC payload sits in a block's skip field, so finding out takes a walk
/// over every audio block either way: an inspection, which stops there, or a
/// decode, which also has the channels a plain extension is merged for.
enum Dependent {
    /// Inspected only. `decode_failed` says a decode was tried first and
    /// rejected the frame, so the merge has nothing to add and must not
    /// push it through the decoder a second time.
    Inspected {
        info: eac3::AccessUnitInfo,
        decode_failed: bool,
    },
    /// Decoded: its channels and its description from the one walk.
    Decoded(PcmPushResult),
}

impl Dependent {
    fn info(&self) -> &eac3::AccessUnitInfo {
        match self {
            Dependent::Inspected { info, .. } => info,
            Dependent::Decoded(push) => &push.info,
        }
    }
}

/// Learn what a dependent substream carries, reading its blocks once where
/// that is enough.
///
/// A channel extension is decoded and merged onto the core that waits for
/// it; a JOC dependent is inspected and handed to the object decoder, which
/// has no use for its channels. Which it is shows only in the frame, so the
/// one before it stands in: streams do not switch between the two from one
/// frame to the next. A dependent expected to be an extension, with a core
/// to merge it onto, is decoded straight away - the inspection used to come
/// first and the decode walked the same blocks again. When the guess is
/// wrong (the first JOC dependent of a stream) the object decoder gets the
/// frame as it always did, and the next dependent is inspected.
///
/// `None` is a frame the inspection rejects, which is then not handled as a
/// dependent at all, as before.
fn read_dependent(bytes: &[u8], state: &mut DecoderState) -> Option<Dependent> {
    let core_waits = state.pending_ac3_core.is_some()
        || matches!(
            state.pending_independent,
            Some(PendingIndependent::UnsentCore(_))
        );
    let mut decode_failed = false;
    if core_waits && !state.dependents_carry_joc {
        match state.dependent_pcm_decoder.push_access_unit(bytes) {
            Ok(push) => {
                state.dependents_carry_joc = push.info.joc_payload_count() > 0;
                return Some(Dependent::Decoded(push));
            }
            Err(_) => decode_failed = true,
        }
    }
    let info = inspect_access_unit(bytes).ok()?;
    state.dependents_carry_joc = info.joc_payload_count() > 0;
    Some(Dependent::Inspected {
        info,
        decode_failed,
    })
}

/// An E-AC-3 program larger than 5.1 is carried as an independent frame (the
/// 5.1 core) immediately followed by a dependent frame (the channel extension
/// and/or the JOC objects), and the pair describes ONE span of audio — so a
/// plain independent frame must not be emitted until the next access unit
/// shows it stands alone, exactly as `pending_ac3_core` already holds a legacy
/// AC-3 core back. The exception is an independent frame that itself carried
/// JOC: it is a complete object presentation and is emitted at once; only its
/// undelayed core is kept, as reconstruction input in case a JOC dependent
/// follows.
enum PendingIndependent {
    /// A plain independent frame, not yet emitted. Standalone → sent as-is; a
    /// JOC dependent consumes its core as reconstruction input; a non-JOC
    /// dependent merges its extension channels onto that core.
    UnsentCore(PcmPushResult),
    /// An independent frame with JOC, already emitted. Only the undelayed core
    /// its own reconstruction ran on is kept — a following JOC dependent needs
    /// exactly that form, not the [`eac3::JOC_LATENCY_SAMPLES`]-delayed one
    /// that shipped inside the emitted frame.
    EmittedObject { joc_input: eac3::CorePcmFrame },
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

        // A trailing buffered frame whose dependent partner never arrived
        // (stream truncated mid-pair): emit it as a standalone bed.
        flush_pending_independent(&mut state, &pb_clone, &tx);
        if let Some(core) = state.pending_ac3_core.take() {
            // AC-3 never carries JOC, so a core emitted on its own is one
            // more presentation the object decoder did not see.
            state.object_decoder.note_non_joc_presentation();
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
    let header = frame.info();

    // The extractor has already read the header, and routing needs nothing
    // more: a legacy AC-3 syncframe is `bsid <= 10`, a dependent substream
    // says so in `strmtyp`. Only a dependent is looked into, once, for its
    // JOC payload - and one whose inspection fails is handled as the
    // independent frame it is not, as it always has been: its decode fails
    // and stands in silence after the buffered frame is flushed.
    let is_legacy_ac3 = header.bitstream_id <= 10;
    let dependent = if header.stream_type == StreamType::Dependent && !is_legacy_ac3 {
        read_dependent(bytes, state)
    } else {
        None
    };
    let is_dependent = dependent.is_some();

    // A buffered frame is only ever paired with an *immediately* following
    // dependent access unit. Any other frame type means the buffered frame had
    // no partner — emit it as a standalone bed before handling this unit,
    // otherwise it sits buffered forever and the output is short. (At most one
    // of the two buffers is occupied: both are flushed here before either is
    // refilled below.)
    if !is_dependent {
        flush_pending_independent(state, pb, tx);
        if let Some(core) = state.pending_ac3_core.take() {
            // As above: a standalone AC-3 core is a resolved no-JOC
            // presentation.
            state.object_decoder.note_non_joc_presentation();
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

    if let Some(dependent) = dependent {
        // Pair a buffered legacy AC-3 core with this dependent substream.
        if let Some(core_result) = state.pending_ac3_core.take() {
            return handle_core_pair(core_result, frame, dependent, state, pb, strict_mode, tx);
        }

        // Pair the buffered independent E-AC-3 frame with this dependent
        // substream. The pair emits exactly one message.
        if let Some(pending) = state.pending_independent.take() {
            return match pending {
                PendingIndependent::UnsentCore(core_msg) => {
                    handle_core_pair(core_msg, frame, dependent, state, pb, strict_mode, tx)
                }
                PendingIndependent::EmittedObject { joc_input } => {
                    handle_emitted_object_pair(joc_input, frame, state, pb, strict_mode, tx)
                }
            };
        }

        // A dependent with nothing to pair with (orphaned by a decode error or
        // a stream cut): its extension channels cannot stand alone — skip it
        // rather than emit a bed with only the extension's channel layout.
        log::warn!("skipping E-AC-3 dependent substream with no frame to pair with");
        return Ok(());
    }

    // Try object decode first (independent frame with JOC).
    match state.object_decoder.push_access_unit(bytes) {
        Ok(Some(obj)) => {
            state.frame_count += 1;
            tick_progress(pb);
            // A following dependent JOC substream needs the core the
            // reconstruction actually ran on, or it decodes from a core that
            // is already late and then gets delayed a second time. The decoder
            // hands it over by value — no copy.
            state.pending_independent = state
                .object_decoder
                .take_joc_input_core()
                .map(|joc_input| PendingIndependent::EmittedObject { joc_input });
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
            // Not sent yet: the next access unit decides whether this frame
            // stands alone (sent as-is) or is one half of a pair — in which
            // case the pair's single message is built from it. No copy is made
            // either way.
            state.pending_independent = Some(PendingIndependent::UnsentCore(result));
        }
        Err(err) => {
            return surface_decode_err(err, strict_mode, tx, pb, state, &frame.info());
        }
    }
    Ok(())
}

/// Decode a dependent substream against the buffered, not-yet-emitted core it
/// belongs to — a legacy AC-3 core (BD-style pairs) or an independent E-AC-3
/// frame's core. JOC dependents (DD+ Atmos with a backward-compatible core)
/// go through the object decoder; non-JOC pairs are a plain channel-extension
/// bed, merged into a full 7.1 frame (falling back to the 5.1 core alone if
/// the merge fails — never silent). Exactly one message is emitted per pair.
fn handle_core_pair(
    core_result: PcmPushResult,
    frame: &Frame,
    dependent: Dependent,
    state: &mut DecoderState,
    pb: &Option<ProgressBar>,
    strict_mode: bool,
    tx: &mpsc::Sender<Result<Eac3FrameMessage>>,
) -> Result<()> {
    let bytes = frame.as_bytes();

    if dependent.info().joc_payload_count() > 0 {
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

    // Resolved the other way: the pair is complete and neither half produced
    // objects, so this presentation carried no JOC either.
    state.object_decoder.note_non_joc_presentation();
    let (merged, dep_info) = match dependent {
        Dependent::Decoded(push) => (
            merge_core_with_decoded_dependent(&core_result.pcm, &push.pcm, &push.info),
            push.info,
        ),
        Dependent::Inspected {
            info,
            decode_failed: true,
        } => (None, info),
        Dependent::Inspected {
            info,
            decode_failed: false,
        } => (
            merge_core_with_dependent(&mut state.dependent_pcm_decoder, &core_result.pcm, bytes),
            info,
        ),
    };
    let bed = merged.unwrap_or(core_result.pcm);
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

/// Decode a dependent substream that follows an independent frame which
/// carried JOC and was therefore already emitted. Only a JOC dependent
/// produces output here: reconstruction runs on the undelayed core the
/// independent's own JOC consumed. A dependent without JOC has only
/// channel-extension audio to offer, and the frame's bed is already sent —
/// merging would write the bed twice, so the extension is dropped instead.
fn handle_emitted_object_pair(
    joc_input: eac3::CorePcmFrame,
    frame: &Frame,
    state: &mut DecoderState,
    pb: &Option<ProgressBar>,
    strict_mode: bool,
    tx: &mpsc::Sender<Result<Eac3FrameMessage>>,
) -> Result<()> {
    match state
        .object_decoder
        .push_access_unit_with_core(frame.as_bytes(), joc_input)
    {
        Ok(Some(obj)) => {
            state.frame_count += 1;
            tick_progress(pb);
            let _ = tx.send(Ok(Eac3FrameMessage::Object(obj)));
            Ok(())
        }
        Ok(None) => {
            log::warn!(
                "dropping E-AC-3 dependent substream without JOC after an already-emitted object frame"
            );
            Ok(())
        }
        Err(err) => surface_decode_err(err, strict_mode, tx, pb, state, &frame.info()),
    }
}

/// Send a buffered independent E-AC-3 frame that turned out to have no
/// dependent partner. An `EmittedObject` (or empty) buffer holds nothing left
/// to send — clearing it is all that happens.
fn flush_pending_independent(
    state: &mut DecoderState,
    pb: &Option<ProgressBar>,
    tx: &mpsc::Sender<Result<Eac3FrameMessage>>,
) {
    if let Some(PendingIndependent::UnsentCore(msg)) = state.pending_independent.take() {
        // Resolved: this independent had no dependent partner and carried no
        // JOC, so a whole presentation has gone by without the object decoder
        // running. Its filter banks would otherwise carry the programme from
        // before the gap into whatever JOC frame comes next.
        state.object_decoder.note_non_joc_presentation();
        state.frame_count += 1;
        tick_progress(pb);
        let _ = tx.send(Ok(Eac3FrameMessage::Core(msg)));
    }
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
    state.pending_independent = None;
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
