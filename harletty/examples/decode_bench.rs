//! Time harletty's decoders on an in-memory elementary stream.
//!
//! The counterpart of `tools/ffmpeg-decode-bench`, which times libavcodec on
//! the same input; `scripts/bench-vs-ffmpeg.sh` runs both and tabulates them.
//! Both sides read the whole file into memory first, feed it to their framer
//! in 64 KiB chunks and time framing plus decoding on one thread, so the
//! numbers compare decoders, not I/O, threading or output writers.
//!
//! The decode loops mirror the CLI's decoder threads (`processor.rs`,
//! `eac3_thread.rs`, `dts_thread.rs`) minus what is not decoding: no channel
//! to a writer, no Auro-Codec stage, no DTS:X metadata parse.
//!
//! Usage:
//!     cargo run --release -p harletty --example decode_bench -- \
//!         <truehd|eac3|dts> <mode> <iterations> <input>
//!
//! Modes:
//!     truehd  `auto` (presentation 2, else the highest that decodes — what
//!             FFmpeg outputs), or a presentation index 0-3
//!     eac3    `bed` (core + dependent channel extension, as FFmpeg) or
//!             `objects` (the CLI's path: JOC reconstruction when present)
//!     dts     `auto` (the CLI's path: lossless/HD assets through the HD
//!             decoder, the core otherwise)
//!
//! Prints one JSON object on stdout.

use std::hint::black_box;
use std::process::ExitCode;
use std::time::{Duration, Instant};

const CHUNK: usize = 64 * 1024;

/// What one pass decoded, used to check both sides did the same work.
#[derive(Default, Clone, Copy, PartialEq, Debug)]
struct Tally {
    frames: u64,
    /// Samples per channel, summed over frames.
    samples: u64,
    /// Channel count of the widest frame.
    channels: usize,
    sample_rate: u32,
    errors: u64,
    /// DTS only: frames that went through the HD decoder.
    hd_frames: u64,
    /// E-AC-3 objects mode only: frames that carried JOC objects.
    object_frames: u64,
}

impl Tally {
    fn frame(&mut self, samples: usize, channels: usize, sample_rate: u32) {
        self.frames += 1;
        self.samples += samples as u64;
        self.channels = self.channels.max(channels);
        self.sample_rate = sample_rate;
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [codec, mode, iterations, input] = args.as_slice() else {
        eprintln!("usage: decode_bench <truehd|eac3|dts> <mode> <iterations> <input>");
        return ExitCode::from(64);
    };
    let iterations: usize = match iterations.parse() {
        Ok(n) if n > 0 => n,
        _ => {
            eprintln!("iterations must be a positive integer");
            return ExitCode::from(64);
        }
    };
    let data = match std::fs::read(input) {
        Ok(data) => data,
        Err(e) => {
            eprintln!("cannot read {input}: {e}");
            return ExitCode::from(66);
        }
    };

    let pass: Box<dyn Fn(&[u8]) -> Tally> = match (codec.as_str(), mode.as_str()) {
        ("truehd", "auto") => {
            let presentation = truehd_auto_presentation(&data);
            Box::new(move |d| truehd_pass(d, presentation))
        }
        ("truehd", p) => match p.parse::<usize>() {
            Ok(p) if p < truehd::process::MAX_PRESENTATIONS => Box::new(move |d| truehd_pass(d, p)),
            _ => {
                eprintln!("truehd mode must be `auto` or a presentation index 0-3");
                return ExitCode::from(64);
            }
        },
        ("eac3", "bed") => Box::new(|d| eac3_pass(d, false)),
        ("eac3", "objects") => Box::new(|d| eac3_pass(d, true)),
        ("dts", "auto") => Box::new(dts_pass),
        _ => {
            eprintln!("unknown codec/mode: {codec} {mode}");
            return ExitCode::from(64);
        }
    };

    // One untimed pass warms the caches and the allocator, and gives the
    // tally every timed pass must reproduce.
    let tally = pass(&data);
    let mut times: Vec<Duration> = (0..iterations)
        .map(|_| {
            let start = Instant::now();
            let t = black_box(pass(black_box(&data)));
            let elapsed = start.elapsed();
            assert_eq!(t, tally, "a pass decoded something else than the first");
            elapsed
        })
        .collect();
    times.sort();

    let audio_seconds = tally.samples as f64 / tally.sample_rate.max(1) as f64;
    let report = serde_json::json!({
        "decoder": "harletty",
        "codec": codec,
        "mode": mode,
        "input": input,
        "bytes": data.len(),
        "frames": tally.frames,
        "samples": tally.samples,
        "channels": tally.channels,
        "sample_rate": tally.sample_rate,
        "errors": tally.errors,
        "hd_frames": tally.hd_frames,
        "object_frames": tally.object_frames,
        "audio_seconds": audio_seconds,
        "iterations": iterations,
        "min_ms": times[0].as_secs_f64() * 1e3,
        "median_ms": times[times.len() / 2].as_secs_f64() * 1e3,
    });
    println!("{report}");
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------- TrueHD

use truehd::process::{MAX_PRESENTATIONS, decode::Decoder, extract::Extractor, parse::Parser};
use truehd::utils::errors::ExtractError;

/// FFmpeg decodes at most substream 2 (`mlpdec.c`: `FFMIN(num_substreams - 1,
/// 2)`), which is presentation 2 on a stream that has one — the 7.1 of an
/// Atmos stream. A stream with fewer substreams has its widest presentation
/// lower, so take the highest of 2, 1, 0 that the stream decodes.
fn truehd_auto_presentation(data: &[u8]) -> usize {
    (0..=2)
        .rev()
        .find(|&p| {
            let t = truehd_pass(&data[..data.len().min(1 << 20)], p);
            t.frames > 0 && t.errors == 0
        })
        .unwrap_or(0)
}

fn truehd_pass(data: &[u8], presentation: usize) -> Tally {
    let mut extractor = Extractor::default();
    let mut parser = Parser::default();
    let mut decoder = Decoder::default();
    parser.set_fail_level(log::Level::Error);
    decoder.set_fail_level(log::Level::Error);
    // As the CLI: every presentation up to the one decoded is required.
    let mut required = [false; MAX_PRESENTATIONS];
    required[..=presentation].iter_mut().for_each(|p| *p = true);
    parser.set_required_presentations(&required);

    let mut tally = Tally::default();
    let mut drain = |extractor: &mut Extractor, tally: &mut Tally| {
        loop {
            match extractor.next() {
                Some(Ok(frame)) => match parser.parse(&frame) {
                    Ok(au) => match decoder.decode_presentation(&au, presentation) {
                        Ok(decoded) => {
                            black_box(&decoded.pcm_data);
                            tally.frame(
                                decoded.sample_length,
                                decoded.channel_count,
                                decoded.sampling_frequency,
                            );
                        }
                        Err(_) => tally.errors += 1,
                    },
                    Err(_) => tally.errors += 1,
                },
                Some(Err(ExtractError::InsufficientData)) | None => break,
                Some(Err(_)) => tally.errors += 1,
            }
        }
    };
    for chunk in data.chunks(CHUNK) {
        extractor.push_bytes(chunk);
        drain(&mut extractor, &mut tally);
    }
    tally
}

// ---------------------------------------------------------------- E-AC-3

/// The CLI's E-AC-3 loop (`eac3_thread.rs`) with the emission stripped: a
/// legacy AC-3 or independent core is held until the next access unit shows
/// whether a dependent substream extends it. `objects` sends independent
/// frames through the JOC decoder first, as the CLI does; `bed` never does,
/// which is what FFmpeg decodes.
fn eac3_pass(data: &[u8], objects: bool) -> Tally {
    use eac3::{
        AccessUnitInfo, CorePcmFrame, ObjectPcmDecoder, PcmDecoder, PcmPushResult,
        inspect_access_unit, merge_core_with_decoded_dependent, merge_core_with_dependent,
    };

    /// As the CLI's `read_dependent`: a dependent expected to be a channel
    /// extension is decoded at once, a JOC one is inspected.
    enum Dependent {
        Inspected {
            info: AccessUnitInfo,
            decode_failed: bool,
        },
        Decoded(PcmPushResult),
    }

    enum Pending {
        Core(CorePcmFrame),
        /// An independent frame whose objects were already counted; only the
        /// undelayed core a JOC dependent reconstructs from is kept.
        Object(Option<CorePcmFrame>),
    }

    fn count_core(tally: &mut Tally, pcm: &CorePcmFrame) {
        black_box(&pcm.fullband_channels);
        tally.frame(
            pcm.samples_per_channel(),
            pcm.total_channels(),
            pcm.sample_rate,
        );
    }

    let mut extractor = eac3::Extractor::default();
    let mut object_decoder = ObjectPcmDecoder::new();
    let mut pcm_decoder = PcmDecoder::new();
    let mut ac3_decoder = PcmDecoder::new();
    let mut dependent_decoder = PcmDecoder::new();
    let mut pending: Option<Pending> = None;
    let mut dependents_carry_joc = false;
    let mut tally = Tally::default();

    let mut handle = |frame: &eac3::Frame, pending: &mut Option<Pending>, tally: &mut Tally| {
        let bytes = frame.as_bytes();
        // As the CLI: route on the extractor's header, and look into a
        // dependent only, once, for its JOC payload.
        let header = frame.info();
        let legacy = header.bitstream_id <= 10;
        let dependent_read = if header.stream_type == eac3::StreamType::Dependent && !legacy {
            let core_waits = matches!(pending, Some(Pending::Core(_)));
            let mut decode_failed = false;
            let mut read = None;
            if core_waits && !dependents_carry_joc {
                match dependent_decoder.push_access_unit(bytes) {
                    Ok(push) => {
                        dependents_carry_joc = push.info.joc_payload_count() > 0;
                        read = Some(Dependent::Decoded(push));
                    }
                    Err(_) => decode_failed = true,
                }
            }
            if read.is_none() {
                read = inspect_access_unit(bytes).ok().map(|info| {
                    dependents_carry_joc = info.joc_payload_count() > 0;
                    Dependent::Inspected {
                        info,
                        decode_failed,
                    }
                });
            }
            read
        } else {
            None
        };
        let dependent = dependent_read.is_some();
        if !dependent {
            if let Some(Pending::Core(core)) = pending.take() {
                object_decoder.note_non_joc_presentation();
                count_core(tally, &core);
            }
        }
        if legacy {
            match ac3_decoder.push_legacy_ac3_access_unit(bytes) {
                Ok(r) => *pending = Some(Pending::Core(r.pcm)),
                Err(_) => tally.errors += 1,
            }
            return;
        }
        if let Some(dependent_read) = dependent_read {
            match pending.take() {
                Some(Pending::Core(core)) => {
                    let info = match &dependent_read {
                        Dependent::Inspected { info, .. } => info,
                        Dependent::Decoded(push) => &push.info,
                    };
                    let joc = objects && info.joc_payload_count() > 0;
                    if joc {
                        if let Ok(Some(obj)) =
                            object_decoder.push_access_unit_with_core(bytes, core.clone())
                        {
                            count_objects(tally, &obj.pcm);
                            return;
                        }
                    }
                    object_decoder.note_non_joc_presentation();
                    let merged = match dependent_read {
                        Dependent::Decoded(push) => {
                            merge_core_with_decoded_dependent(&core, &push.pcm, &push.info)
                        }
                        Dependent::Inspected {
                            decode_failed: true,
                            ..
                        } => None,
                        Dependent::Inspected { .. } => {
                            merge_core_with_dependent(&mut dependent_decoder, &core, bytes)
                        }
                    };
                    let bed = merged.unwrap_or(core);
                    count_core(tally, &bed);
                }
                Some(Pending::Object(Some(joc_input))) => {
                    if let Ok(Some(obj)) =
                        object_decoder.push_access_unit_with_core(bytes, joc_input)
                    {
                        count_objects(tally, &obj.pcm);
                    }
                }
                Some(Pending::Object(None)) | None => {}
            }
            return;
        }
        if objects {
            match object_decoder.push_access_unit(bytes) {
                Ok(Some(obj)) => {
                    count_objects(tally, &obj.pcm);
                    *pending = Some(Pending::Object(object_decoder.take_joc_input_core()));
                    return;
                }
                Ok(None) => {}
                Err(_) => {
                    tally.errors += 1;
                    return;
                }
            }
        }
        match pcm_decoder.push_access_unit(bytes) {
            Ok(r) => *pending = Some(Pending::Core(r.pcm)),
            Err(_) => tally.errors += 1,
        }
    };

    for chunk in data.chunks(CHUNK) {
        extractor.push_bytes(chunk);
        loop {
            match extractor.next_frame() {
                Ok(Some(frame)) => handle(&frame, &mut pending, &mut tally),
                Ok(None) => break,
                Err(_) => tally.errors += 1,
            }
        }
    }
    if let Some(Pending::Core(core)) = pending.take() {
        count_core(&mut tally, &core);
    }
    tally
}

fn count_objects(tally: &mut Tally, pcm: &eac3::ObjectPcmFrame) {
    black_box(&pcm.object_channels);
    tally.frame(
        pcm.samples_per_channel(),
        pcm.core.total_channels() + pcm.object_count(),
        pcm.core.sample_rate,
    );
    tally.object_frames += 1;
}

// ---------------------------------------------------------------- DTS

/// The CLI's DTS demux (`dts_thread.rs`): a core frame, and when an EXSS
/// substream follows it carrying more than a core, the HD decoder.
fn dts_pass(data: &[u8]) -> Tally {
    use dca::{ExssKind, HdDecoder, HdError, PcmDecoder, exss_kind, exss_substream_size};

    const CORE_SYNC: [u8; 4] = 0x7FFE_8001u32.to_be_bytes();
    const SUBSTREAM_SYNC: [u8; 4] = 0x6458_2025u32.to_be_bytes();

    let mut core_decoder = PcmDecoder::new();
    let mut hd_decoder = HdDecoder::new();
    let mut buffer: Vec<u8> = Vec::with_capacity(2 * CHUNK);
    let mut tally = Tally::default();

    let mut drain = |buffer: &mut Vec<u8>, tally: &mut Tally, at_eof: bool| {
        let mut consumed = 0usize;
        loop {
            let rest = &buffer[consumed..];
            let Some(offset) = rest.windows(4).position(|w| w == CORE_SYNC) else {
                consumed += rest.len().saturating_sub(CORE_SYNC.len() - 1);
                break;
            };
            consumed += offset;
            let rest = &buffer[consumed..];
            let info = match dca::parse_header(rest) {
                Ok(info) => info,
                Err(dca::HeaderParseError::InsufficientData) => break,
                Err(_) => {
                    consumed += CORE_SYNC.len();
                    continue;
                }
            };
            let core_size = info.frame_size;
            // At the end of the input a core needs nothing after it to stand
            // alone, as in the CLI.
            let exss_testable = rest.len() >= core_size + SUBSTREAM_SYNC.len();
            if !exss_testable && !(at_eof && rest.len() >= core_size) {
                break;
            }
            let mut frame_size = core_size;
            let mut exss = None;
            if exss_testable && rest[core_size..core_size + SUBSTREAM_SYNC.len()] == SUBSTREAM_SYNC
            {
                let buffered = exss_substream_size(&rest[core_size..])
                    .filter(|&exss_size| rest.len() >= core_size + exss_size);
                match buffered {
                    Some(exss_size) => {
                        frame_size = core_size + exss_size;
                        let candidate = &rest[core_size..core_size + exss_size];
                        if exss_kind(candidate) != ExssKind::Core {
                            exss = Some(candidate);
                        }
                    }
                    None if at_eof => {}
                    None => break,
                }
            }
            let core = &rest[..core_size];
            let mut done = false;
            if let Some(exss) = exss {
                match hd_decoder.decode(core, exss) {
                    Ok(frame) => {
                        let channels =
                            frame.samples.iter().flatten().count() + frame.x_samples.len();
                        black_box(&frame.samples);
                        tally.frame(frame.bed_sample_count(), channels, frame.sample_rate);
                        tally.hd_frames += 1;
                        done = true;
                    }
                    Err(HdError::Pending) => done = true,
                    Err(_) => tally.errors += 1,
                }
            }
            if !done {
                match core_decoder.push_access_unit(core) {
                    Ok(push) => {
                        black_box(&push.pcm.fullband_channels);
                        tally.frame(
                            push.pcm.samples_per_channel(),
                            push.pcm.total_channels(),
                            push.pcm.sample_rate,
                        );
                    }
                    Err(_) => tally.errors += 1,
                }
            }
            consumed += frame_size;
        }
        buffer.drain(..consumed);
    };

    for chunk in data.chunks(CHUNK) {
        buffer.extend_from_slice(chunk);
        drain(&mut buffer, &mut tally, false);
    }
    drain(&mut buffer, &mut tally, true);
    tally
}
