use anyhow::Result;
use indicatif::ProgressBar;
use std::sync::mpsc;
use truehd::process::decode::DecodedAccessUnit;
use truehd::process::{decode::Decoder, extract::Extractor, parse::Parser};
use truehd::structs::channel::ChannelLabel;

/// Shape of the last access unit that came through cleanly.
///
/// Used to size the silence that stands in for one we could not decode: a
/// skipped access unit is a hole in the timeline, and nothing in a frame we
/// failed to parse tells us how long it was meant to be. TrueHD access units
/// run at a fixed 1/1200 s for a given sampling rate, so the previous one is
/// an exact answer in practice rather than an approximation.
#[derive(Clone)]
pub struct AccessUnitShape {
    sampling_frequency: u32,
    sample_length: usize,
    channel_count: usize,
    channel_labels: Vec<ChannelLabel>,
}

impl AccessUnitShape {
    pub fn sampling_frequency(&self) -> u32 {
        self.sampling_frequency
    }

    fn of(decoded: &DecodedAccessUnit) -> Self {
        Self {
            sampling_frequency: decoded.sampling_frequency,
            sample_length: decoded.sample_length,
            channel_count: decoded.channel_count,
            channel_labels: decoded.channel_labels.clone(),
        }
    }

    fn to_silence(&self) -> DecodedAccessUnit {
        DecodedAccessUnit {
            sampling_frequency: self.sampling_frequency,
            sample_length: self.sample_length,
            channel_count: self.channel_count,
            pcm_data: [[0; 16]; 160],
            channel_labels: self.channel_labels.clone(),
            // No object metadata for a span we never decoded: the last
            // positions stay in force, which is what a renderer does anyway
            // between events.
            oamd: Vec::new(),
            is_duplicate: false,
            substream_info_changed: false,
        }
    }
}

pub struct ProcessFramesContext<'a> {
    pub extractor: &'a mut Extractor,
    pub parser: &'a mut Parser,
    pub decoder: &'a mut Decoder,
    pub frames_processed: &'a mut u64,
    pub frame_count: &'a mut u64,
    pub total_samples: &'a mut u64,
    pub presentation: u8,
    pub strict_mode: bool,
    pub tx: &'a mpsc::Sender<Result<truehd::process::decode::DecodedAccessUnit>>,
    pub pb_clone: &'a Option<ProgressBar>,
    pub current_substream_info: &'a mut Option<u8>,
    pub current_extended_substream_info: &'a mut Option<u8>,
    pub recovering_until_major_sync: &'a mut bool,
    /// Shape of the last cleanly decoded access unit, or `None` before the
    /// first one.
    pub last_shape: &'a mut Option<AccessUnitShape>,
    /// Access units replaced with silence, and the samples that cost.
    pub gap_access_units: &'a mut u64,
    pub gap_samples: &'a mut u64,
}

/// Send silence in place of an access unit that could not be decoded.
///
/// Dropping it instead shortens the output by its duration, which is how a
/// handful of corrupt frames turns into audio that drifts against the picture
/// for the rest of the programme — the loss is permanent and accumulates at
/// every stitch point. Returns `false` if the receiver is gone.
fn emit_gap(ctx: &mut ProcessFramesContext) -> bool {
    let Some(shape) = ctx.last_shape.as_ref() else {
        // Nothing decoded yet, so there is no timeline to hold open and no
        // channel layout to write. The stream has not started.
        return true;
    };

    let silence = shape.to_silence();
    *ctx.gap_access_units += 1;
    *ctx.gap_samples += silence.sample_length as u64;
    *ctx.total_samples += silence.sample_length as u64;

    ctx.tx.send(Ok(silence)).is_ok()
}

pub fn process_frames(ctx: &mut ProcessFramesContext) -> Result<bool> {
    loop {
        match ctx.extractor.next() {
            Some(Ok(frame)) => {
                *ctx.frames_processed += 1;
                if let Some(pb) = ctx.pb_clone {
                    pb.set_position(*ctx.frames_processed);
                }
                *ctx.frame_count += 1;

                if *ctx.recovering_until_major_sync {
                    if !frame.is_major_sync() {
                        if !emit_gap(ctx) {
                            return Ok(true);
                        }
                        continue;
                    }

                    log::info!(
                        "Major sync found at frame {}; resuming after parse recovery",
                        *ctx.frame_count
                    );
                    *ctx.recovering_until_major_sync = false;
                }

                match ctx.parser.parse(&frame) {
                    Ok(access_unit) => {
                        // Check for substream_info changes after parsing
                        let mut substream_info_changed = false;
                        if let Some(major_sync) = &access_unit.major_sync_info {
                            // Check if substream_info has changed
                            match *ctx.current_substream_info {
                                Some(current) if current != major_sync.substream_info => {
                                    log::info!(
                                        "substream_info changed: {:#02X} -> {:#02X}",
                                        current,
                                        major_sync.substream_info
                                    );
                                    substream_info_changed = true;
                                }
                                None => {
                                    // First time seeing substream_info
                                    *ctx.current_substream_info = Some(major_sync.substream_info);
                                }
                                _ => {} // No change
                            }

                            // Check if extended_substream_info has changed
                            match *ctx.current_extended_substream_info {
                                Some(current) if current != major_sync.extended_substream_info => {
                                    log::info!(
                                        "extended_substream_info changed: {:#02X} -> {:#02X}",
                                        current,
                                        major_sync.extended_substream_info
                                    );
                                    substream_info_changed = true;
                                }
                                None => {
                                    // First time seeing extended_substream_info
                                    *ctx.current_extended_substream_info =
                                        Some(major_sync.extended_substream_info);
                                }
                                _ => {} // No change
                            }

                            // Update stored values
                            *ctx.current_substream_info = Some(major_sync.substream_info);
                            *ctx.current_extended_substream_info =
                                Some(major_sync.extended_substream_info);
                        }

                        match ctx
                            .decoder
                            .decode_presentation(&access_unit, ctx.presentation as usize)
                        {
                            Ok(mut decoded) => {
                                // Set the substream_info_changed flag if we detected a change
                                if substream_info_changed {
                                    decoded.substream_info_changed = true;
                                }

                                *ctx.last_shape = Some(AccessUnitShape::of(&decoded));
                                *ctx.total_samples += decoded.sample_length as u64;
                                if ctx.tx.send(Ok(decoded)).is_err() {
                                    return Ok(true);
                                }
                            }
                            Err(e) => {
                                log::error!("Decode error at frame {}: {e}", *ctx.frame_count);
                                if ctx.strict_mode {
                                    let _ = ctx.tx.send(Err(e));
                                    return Ok(true);
                                }

                                if !emit_gap(ctx) {
                                    return Ok(true);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        log::error!("Parse error at frame {}: {e}", *ctx.frame_count);
                        if ctx.strict_mode {
                            let _ = ctx.tx.send(Err(e));
                            return Ok(true);
                        }

                        ctx.parser.reset_for_next_major_sync();
                        ctx.decoder.reset_for_next_major_sync();
                        *ctx.current_substream_info = None;
                        *ctx.current_extended_substream_info = None;
                        *ctx.recovering_until_major_sync = true;

                        if !emit_gap(ctx) {
                            return Ok(true);
                        }
                    }
                }
            }
            Some(Err(ref e))
                if matches!(e, truehd::utils::errors::ExtractError::InsufficientData) =>
            {
                break;
            }
            Some(Err(_extract_error)) => {
                if let Some(pb) = ctx.pb_clone {
                    pb.set_message("processing (some extraction errors)");
                }
            }
            None => {
                break;
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use truehd::process::EXAMPLE_DATA;

    struct Run {
        access_units: Vec<DecodedAccessUnit>,
        total_samples: u64,
        gap_access_units: u64,
        gap_samples: u64,
    }

    /// `EXAMPLE_DATA` is one major sync plus one continuation access unit, 40
    /// samples each. Repeating it gives a stream with the sync cadence a
    /// recovery needs in order to resume.
    fn stream(copies: usize) -> Vec<u8> {
        EXAMPLE_DATA.repeat(copies)
    }

    fn run(bytes: &[u8]) -> Run {
        let (tx, rx) = mpsc::channel();
        let mut extractor = Extractor::default();
        extractor.push_bytes(bytes);

        let mut parser = Parser::default();
        let mut decoder = Decoder::default();
        let (mut frames_processed, mut frame_count, mut total_samples) = (0u64, 0u64, 0u64);
        let (mut substream_info, mut extended_substream_info) = (None, None);
        let mut recovering = false;
        let mut last_shape = None;
        let (mut gap_access_units, mut gap_samples) = (0u64, 0u64);

        let mut ctx = ProcessFramesContext {
            extractor: &mut extractor,
            parser: &mut parser,
            decoder: &mut decoder,
            frames_processed: &mut frames_processed,
            frame_count: &mut frame_count,
            total_samples: &mut total_samples,
            presentation: 3,
            strict_mode: false,
            tx: &tx,
            pb_clone: &None,
            current_substream_info: &mut substream_info,
            current_extended_substream_info: &mut extended_substream_info,
            recovering_until_major_sync: &mut recovering,
            last_shape: &mut last_shape,
            gap_access_units: &mut gap_access_units,
            gap_samples: &mut gap_samples,
        };

        process_frames(&mut ctx).expect("processing must not abort in non-strict mode");
        drop(tx);

        Run {
            access_units: rx.into_iter().map(|r| r.expect("no error sent")).collect(),
            total_samples,
            gap_access_units,
            gap_samples,
        }
    }

    #[test]
    fn clean_stream_fills_no_gaps() {
        let run = run(&stream(4));

        assert_eq!(run.access_units.len(), 8);
        assert_eq!(run.total_samples, 320);
        assert_eq!(run.gap_access_units, 0, "nothing to recover from");
    }

    /// An access unit we cannot decode has to leave silence behind, not
    /// nothing. Dropping it shortens the output by its duration, so the rest of
    /// the programme slides earlier against the picture — and the error is
    /// permanent, accumulating at every corrupt stitch point. This is the drift
    /// reported upstream as truehdd/truehdd#19 and #25.
    #[test]
    fn undecodable_access_unit_becomes_silence_of_the_same_length() {
        let clean = run(&stream(4));

        // Scramble the payload of the second copy's continuation access unit,
        // past the length field so framing still holds.
        let mut damaged = stream(4);
        let frame_len = EXAMPLE_DATA.len() / 2;
        for byte in damaged
            .iter_mut()
            .skip(EXAMPLE_DATA.len() + frame_len + 12)
            .take(16)
        {
            *byte ^= 0xFF;
        }

        let run = run(&damaged);

        // Checked before the "did it actually break" guard below, so that
        // removing the gap fill fails here with the sample count rather than
        // on a counter the fill itself maintains.
        assert_eq!(
            run.total_samples,
            clean.total_samples,
            "recovery lost {} samples against the intact stream",
            clean.total_samples as i64 - run.total_samples as i64
        );
        assert_eq!(
            run.access_units.len(),
            clean.access_units.len(),
            "an access unit went missing from the timeline"
        );
        assert!(
            run.gap_access_units > 0,
            "the corruption no longer trips a parse failure, so this test is \
             not exercising recovery any more"
        );
        assert_eq!(run.gap_samples, 40 * run.gap_access_units);

        // The stand-in has to look like its neighbours, or the writer sees a
        // format change mid-file.
        let silent: Vec<_> = run
            .access_units
            .iter()
            .filter(|au| {
                au.pcm_data
                    .iter()
                    .take(au.sample_length)
                    .all(|s| s == &[0; 16])
            })
            .collect();
        assert_eq!(silent.len() as u64, run.gap_access_units);
        for au in silent {
            assert_eq!(au.sample_length, 40);
            assert_eq!(au.channel_count, clean.access_units[0].channel_count);
            assert_eq!(
                au.sampling_frequency,
                clean.access_units[0].sampling_frequency
            );
            assert!(!au.is_duplicate, "silence must not be discarded downstream");
        }
    }
}
