use super::decoder_thread::{DecoderThreadConfig, spawn_decoder_thread};
use super::dts_handler::DtsDecodeHandler;
use super::dts_thread::{DtsDecoderThreadConfig, spawn_dts_decoder_thread};
use super::eac3_handler::Eac3DecodeHandler;
use super::eac3_thread::{Eac3DecoderThreadConfig, spawn_eac3_decoder_thread};
use super::handler::{DecodeHandler, FrameHandlerContext, WriterState};
use super::progress::{create_progress_bar, estimate_total_frames};
use crate::cli::command::{AudioFormat, Cli, DecodeArgs};
use crate::codec_probe::{Codec, probe_codec};
use damf::SourceCodec;
use crate::input::InputReader;
use anyhow::Result;
use indicatif::{MultiProgress, ProgressStyle};
use log::Level;
use std::sync::mpsc;
use truehd::process::{MAX_PRESENTATIONS, decode::Decoder, extract::Extractor, parse::Parser};

pub fn cmd_decode(args: &DecodeArgs, cli: &Cli, multi: Option<&MultiProgress>) -> Result<()> {
    let base_path = args.output_path.clone();
    if let Some(ref path) = base_path {
        log::info!("Output path specified: {}", path.display());
    }

    // Probe input to choose codec path.
    let mut probe_reader = InputReader::new(&args.input)?;
    let (codec, prefix) = probe_codec(&mut probe_reader, cli.codec)?;
    drop(probe_reader);
    log::info!("Detected codec: {:?}", codec);

    match codec {
        Codec::Eac3 => return cmd_decode_eac3(args, cli, multi, prefix),
        Codec::Dts => return cmd_decode_dts(args, cli, multi, prefix),
        Codec::Truehd | Codec::Auto => {}
    }

    if args.presentation > 3 {
        return Err(anyhow::anyhow!(
            "Presentation index must be 0-3, got {}",
            args.presentation
        ));
    }

    log::info!(
        "Decoding TrueHD stream: {} (strict mode: {}, presentation: {})",
        args.input.display(),
        cli.strict,
        args.presentation
    );

    let is_pipe = args.input.to_string_lossy() == "-";

    // Estimate total frames if needed
    let should_estimate = !args.no_estimate_progress && !is_pipe && multi.is_some();
    let total_frames = if should_estimate {
        Some(estimate_total_frames(&args.input)?)
    } else {
        if is_pipe {
            log::debug!("Skipping progress estimation for pipe input");
        } else if args.no_estimate_progress {
            log::debug!("Progress estimation disabled by --no-estimate-progress flag");
        }
        None
    };

    // Create progress bar
    let pb = if let Some(multi) = multi {
        Some(create_progress_bar(multi, total_frames)?)
    } else {
        None
    };

    // Setup decoder components
    let (tx, rx) = mpsc::channel();
    let pb_clone = pb.clone();
    let strict_mode = cli.strict;
    let presentation = args.presentation;

    let extractor = Extractor::default();
    let mut parser = Parser::default();
    let mut decoder = Decoder::default();

    // Configure fail level based on strict mode
    let fail_level = if strict_mode {
        Level::Warn
    } else {
        Level::Error
    };
    parser.set_fail_level(fail_level);
    decoder.set_fail_level(fail_level);

    let state = WriterState { fail_level };

    // Setup required presentations
    let mut required_presentations = [false; MAX_PRESENTATIONS];
    required_presentations[..=presentation as usize]
        .iter_mut()
        .for_each(|p| *p = true);
    parser.set_required_presentations(&required_presentations);

    // Spawn decoder thread
    let decode_thread = spawn_decoder_thread(DecoderThreadConfig {
        input_path: args.input.clone(),
        presentation,
        strict_mode,
        tx,
        pb_clone,
        extractor,
        parser,
        decoder,
        prefix,
    });

    // Handle decoded frames
    let mut handler = DecodeHandler::default();
    handler.source_codec = SourceCodec::TrueHD;
    let start_time = std::time::Instant::now();

    let effective_format = if args.presentation == 3 {
        if args.format != AudioFormat::Caf {
            log::info!(
                "Forcing CAF format for presentation 3, ignoring --format {:?}",
                args.format
            );
        }
        AudioFormat::Caf
    } else {
        args.format
    };

    while let Ok(result) = rx.recv() {
        match result {
            Ok(decoded) => {
                // Check if substream info changed and handle it before processing the frame
                if decoded.substream_info_changed {
                    // Store the current sample position as the start of the new segment
                    handler.segment_start_samples = handler.decoded_samples;

                    // Handle stream restart with actual sample rate and channel count from decoded frame
                    handler.handle_stream_restart(
                        &base_path,
                        effective_format,
                        args.no_audio,
                        decoded.sampling_frequency,
                        decoded.channel_count,
                        args.bed_conform,
                        &decoded.channel_labels,
                    )?;
                    handler.is_segmented = true; // Mark that we're now in segmented mode
                }

                let ctx = FrameHandlerContext {
                    base_path: &base_path,
                    format: effective_format,
                    no_audio: args.no_audio,
                    pb: &pb,
                    state: &state,
                    start_time,
                    bed_conform: args.bed_conform,
                    warp_mode: args.warp_mode,
                };
                handler.handle_decoded_frame(decoded, &ctx)?;
            }
            Err(e) => {
                if let Some(pb) = pb {
                    pb.finish_with_message("decode failed");
                }
                return Err(e);
            }
        }
    }

    // Finalize output
    handler.finalize()?;

    // Wait for decode thread and finalize progress
    match decode_thread.join() {
        Ok(Ok(())) => {
            finalize_progress_bar(
                &pb,
                total_frames,
                handler.decoded_samples,
                handler.final_sample_rate,
                start_time,
            );
            log::info!("Decoding completed successfully");
        }
        Ok(Err(e)) => {
            if let Some(pb) = pb {
                pb.finish_with_message("decode failed");
            }
            return Err(e);
        }
        Err(_) => {
            if let Some(pb) = pb {
                pb.finish_with_message("decode thread panicked");
            }
            return Err(anyhow::anyhow!("Decode thread panicked"));
        }
    }

    Ok(())
}

fn cmd_decode_eac3(
    args: &DecodeArgs,
    cli: &Cli,
    multi: Option<&MultiProgress>,
    prefix: Vec<u8>,
) -> Result<()> {
    log::info!(
        "Decoding EAC3 stream: {} (strict mode: {})",
        args.input.display(),
        cli.strict
    );

    let base_path = args.output_path.clone();

    // Progress bar (no frame estimation for EAC3 — fast enough).
    let pb = if let Some(multi) = multi {
        Some(create_progress_bar(multi, None)?)
    } else {
        None
    };

    let (tx, rx) = mpsc::channel();
    let pb_clone = pb.clone();
    let strict_mode = cli.strict;

    let decode_thread = spawn_eac3_decoder_thread(Eac3DecoderThreadConfig {
        input_path: args.input.clone(),
        strict_mode,
        prefix,
        tx,
        pb_clone,
    });

    let mut handler = Eac3DecodeHandler::default();
    handler.warp_mode = args.warp_mode;
    let start_time = std::time::Instant::now();

    while let Ok(result) = rx.recv() {
        match result {
            Ok(msg) => {
                handler.handle_message(msg, &base_path, args.format, args.no_audio)?;
            }
            Err(e) => {
                if let Some(pb) = pb {
                    pb.finish_with_message("decode failed");
                }
                return Err(e);
            }
        }
    }

    handler.finalize()?;

    match decode_thread.join() {
        Ok(Ok(())) => {
            finalize_progress_bar(
                &pb,
                None,
                handler.decoded_samples,
                handler.final_sample_rate,
                start_time,
            );
            log::info!("EAC3 decoding completed successfully");
        }
        Ok(Err(e)) => {
            if let Some(pb) = pb {
                pb.finish_with_message("decode failed");
            }
            return Err(e);
        }
        Err(_) => {
            if let Some(pb) = pb {
                pb.finish_with_message("decode thread panicked");
            }
            return Err(anyhow::anyhow!("EAC3 decode thread panicked"));
        }
    }

    Ok(())
}

fn cmd_decode_dts(
    args: &DecodeArgs,
    cli: &Cli,
    multi: Option<&MultiProgress>,
    prefix: Vec<u8>,
) -> Result<()> {
    log::info!(
        "Decoding DTS stream: {} (strict mode: {})",
        args.input.display(),
        cli.strict
    );

    let base_path = args.output_path.clone();

    // No frame estimation: DTS frame sizes vary with the EXSS extension, so a
    // byte-count estimate would be misleading. Spinner only, as for E-AC-3.
    let pb = if let Some(multi) = multi {
        Some(create_progress_bar(multi, None)?)
    } else {
        None
    };

    let (tx, rx) = mpsc::channel();
    let decode_thread = spawn_dts_decoder_thread(DtsDecoderThreadConfig {
        input_path: args.input.clone(),
        strict_mode: cli.strict,
        prefix,
        tx,
        pb_clone: pb.clone(),
    });

    let mut handler = DtsDecodeHandler::default();
    handler.warp_mode = args.warp_mode;
    let start_time = std::time::Instant::now();

    while let Ok(result) = rx.recv() {
        match result {
            Ok(msg) => {
                handler.handle_message(msg, &base_path, args.format, args.no_audio)?;
            }
            Err(e) => {
                if let Some(pb) = pb {
                    pb.finish_with_message("decode failed");
                }
                return Err(e);
            }
        }
    }

    handler.finalize()?;

    match decode_thread.join() {
        Ok(Ok(())) => {
            finalize_progress_bar(
                &pb,
                None,
                handler.decoded_samples,
                handler.final_sample_rate,
                start_time,
            );
            log::info!("DTS decoding completed successfully");
        }
        Ok(Err(e)) => {
            if let Some(pb) = pb {
                pb.finish_with_message("decode failed");
            }
            return Err(e);
        }
        Err(_) => {
            if let Some(pb) = pb {
                pb.finish_with_message("decode thread panicked");
            }
            return Err(anyhow::anyhow!("DTS decode thread panicked"));
        }
    }

    Ok(())
}

fn finalize_progress_bar(
    pb: &Option<indicatif::ProgressBar>,
    total_frames: Option<u64>,
    decoded_samples: u64,
    final_sample_rate: u32,
    start_time: std::time::Instant,
) {
    if let Some(pb) = pb {
        let elapsed = start_time.elapsed();
        let audio_duration_secs = decoded_samples as f64 / final_sample_rate as f64;
        let realtime_multiplier = audio_duration_secs / elapsed.as_secs_f64();
        let final_time_str = crate::timestamp::time_str(audio_duration_secs);

        if total_frames.is_some() {
            pb.set_style(
                ProgressStyle::with_template(
                    "{bar:40.cyan/blue} {pos}/{len} frames ({percent}%)\n{msg} | elapsed: {elapsed_precise}",
                )
                .unwrap_or_else(|_| ProgressStyle::default_bar()),
            );
        } else {
            pb.set_style(
                ProgressStyle::with_template(
                    "{spinner:.green} {pos} frames\n{msg} | elapsed: {elapsed_precise}",
                )
                .unwrap_or_else(|_| ProgressStyle::default_spinner()),
            );
        }

        pb.finish_with_message(format!(
            "speed: {realtime_multiplier:.1}x | timestamp: {final_time_str}"
        ));
    }
}
