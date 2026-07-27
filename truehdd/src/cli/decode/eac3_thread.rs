use super::eac3_handler::Eac3FrameMessage;
use crate::input::InputReader;
use anyhow::Result;
use eac3::{
    AccessUnitInfo, AccessUnitParseError, ExtractError, Extractor, Frame, FrameType,
    ObjectPcmDecoder, PcmDecoder, PcmPushResult, inspect_access_unit,
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
        let mut object_decoder = ObjectPcmDecoder::default();
        let mut pcm_decoder = PcmDecoder::default();
        let mut frame_count: u64 = 0;
        let mut pending_independent_core: Option<eac3::CorePcmFrame> = None;

        if !prefix.is_empty() {
            extractor.push_bytes(&prefix);
        }

        let mut input_reader = InputReader::new(&input_path)?;
        // When prefix already consumed the file head, we still drive process_chunks against
        // the remaining stream. For piped input, the prefix is the head we already buffered.
        let result = input_reader.process_chunks(64 * 1024, |chunk| {
            extractor.push_bytes(chunk);
            drain_frames(
                &mut extractor,
                &mut object_decoder,
                &mut pcm_decoder,
                &mut pending_independent_core,
                &mut frame_count,
                &pb_clone,
                strict_mode,
                &tx,
            )
        });

        if result.is_err() {
            return result;
        }

        // Final drain after EOF.
        drain_frames(
            &mut extractor,
            &mut object_decoder,
            &mut pcm_decoder,
            &mut pending_independent_core,
            &mut frame_count,
            &pb_clone,
            strict_mode,
            &tx,
        )?;

        log::info!("EAC3 decode complete: {frame_count} frames");
        Ok(())
    })
}

#[allow(clippy::too_many_arguments)]
fn drain_frames(
    extractor: &mut Extractor,
    object_decoder: &mut ObjectPcmDecoder,
    pcm_decoder: &mut PcmDecoder,
    pending_independent_core: &mut Option<eac3::CorePcmFrame>,
    frame_count: &mut u64,
    pb: &Option<ProgressBar>,
    strict_mode: bool,
    tx: &mpsc::Sender<Result<Eac3FrameMessage>>,
) -> Result<bool> {
    loop {
        match extractor.next_frame() {
            Ok(Some(frame)) => {
                handle_frame(
                    &frame,
                    object_decoder,
                    pcm_decoder,
                    pending_independent_core,
                    frame_count,
                    pb,
                    strict_mode,
                    tx,
                )?;
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

#[allow(clippy::too_many_arguments)]
fn handle_frame(
    frame: &Frame,
    object_decoder: &mut ObjectPcmDecoder,
    pcm_decoder: &mut PcmDecoder,
    pending_independent_core: &mut Option<eac3::CorePcmFrame>,
    frame_count: &mut u64,
    pb: &Option<ProgressBar>,
    strict_mode: bool,
    tx: &mpsc::Sender<Result<Eac3FrameMessage>>,
) -> Result<()> {
    let bytes = frame.as_bytes();

    let frame_type = inspect_access_unit(bytes)
        .ok()
        .map(|info: AccessUnitInfo| info.frame_type);

    // Dependent substream: decode against the previously decoded independent core.
    if matches!(frame_type, Some(FrameType::Dependent)) {
        if let Some(core) = pending_independent_core.take() {
            match object_decoder.push_access_unit_with_core(bytes, core.clone()) {
                Ok(Some(obj)) => {
                    *frame_count += 1;
                    tick_progress(pb);
                    let _ = tx.send(Ok(Eac3FrameMessage::Object(obj)));
                    return Ok(());
                }
                Ok(None) => {
                    // No JOC on dependent frame — emit the core we already have.
                    *frame_count += 1;
                    tick_progress(pb);
                    let info = match inspect_access_unit(bytes) {
                        Ok(i) => i,
                        Err(_) => return Ok(()),
                    };
                    let push = PcmPushResult {
                        frames_seen: *frame_count,
                        info,
                        pcm: core,
                    };
                    let _ = tx.send(Ok(Eac3FrameMessage::Core(push)));
                    return Ok(());
                }
                Err(err) => return surface_decode_err(err, strict_mode, tx, pb, frame_count, &frame.info()),
            }
        }
    }

    // Try object decode first (independent frame with JOC).
    match object_decoder.push_access_unit(bytes) {
        Ok(Some(obj)) => {
            *frame_count += 1;
            tick_progress(pb);
            *pending_independent_core = Some(obj.pcm.core.clone());
            let _ = tx.send(Ok(Eac3FrameMessage::Object(obj)));
            return Ok(());
        }
        Ok(None) => {
            // No JOC — fall through to core PCM decode.
        }
        Err(err) => {
            return surface_decode_err(err, strict_mode, tx, pb, frame_count, &frame.info());
        }
    }

    match pcm_decoder.push_access_unit(bytes) {
        Ok(result) => {
            *frame_count += 1;
            tick_progress(pb);
            *pending_independent_core = Some(result.pcm.clone());
            let _ = tx.send(Ok(Eac3FrameMessage::Core(result)));
        }
        Err(err) => return surface_decode_err(err, strict_mode, tx, pb, frame_count, &frame.info()),
    }
    Ok(())
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
