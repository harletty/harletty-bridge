//! Stream an entire raw .eac3 file through a single stateful decoder.
//!
//! Unlike `corpus_compare` which instantiates a fresh decoder per `.bin`, this
//! example feeds every access unit into one decoder instance — the same model
//! as the live PipeWire chain, which is the only configuration that exposes
//! the class of bugs documented in `docs/eac3-coupling-debug.md` (bug #7).
//!
//! Output formats:
//! - stdout: TSV one row per frame (frame_idx, byte_offset, byte_len, status, joc_count, oamd_count, max_abs).
//! - sidecar (`--out` flag): interleaved float32 PCM, channel order matching FFmpeg's 5.1(side):
//!   FL FR FC LFE SL SR followed by any dynamic-object channels (object mode only).
//!
//! Usage:
//!     cargo run --release --example corpus_stream -p eac3 -- \
//!         <input.eac3> --mode pcm --out /tmp/harletty.f32
//!
//! Modes:
//!     pcm     core PCM only (PcmDecoder)
//!     object  core PCM + JOC objects (ObjectPcmDecoder)

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use eac3::{
    BedChannel, CorePcmFrame, Extractor, ObjectPcmDecoder, PcmDecoder,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Pcm,
    Object,
}

struct Args {
    input: PathBuf,
    mode: Mode,
    output: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut iter = std::env::args().skip(1);
    let input = iter
        .next()
        .ok_or("missing positional <input.eac3>")?
        .into();
    let mut mode = Mode::Pcm;
    let mut output: Option<PathBuf> = None;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--mode" => {
                let value = iter.next().ok_or("--mode requires a value (pcm|object)")?;
                mode = match value.as_str() {
                    "pcm" => Mode::Pcm,
                    "object" | "obj" => Mode::Object,
                    other => return Err(format!("unknown --mode value: {other}")),
                };
            }
            "--out" => {
                output = Some(iter.next().ok_or("--out requires a path")?.into());
            }
            other => return Err(format!("unexpected argument: {other}")),
        }
    }

    Ok(Args { input, mode, output })
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            eprintln!("usage: corpus_stream <input.eac3> [--mode pcm|object] [--out <path>]");
            return ExitCode::from(64);
        }
    };

    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> std::io::Result<()> {
    let file = File::open(&args.input)?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);

    let mut writer: Option<BufWriter<File>> = match &args.output {
        Some(path) => Some(BufWriter::with_capacity(
            1 << 20,
            File::create(path)?,
        )),
        None => None,
    };

    let mut extractor = Extractor::default();
    let mut buf = [0u8; 64 * 1024];

    let header = [
        "frame", "byte_off", "bytes", "status", "joc", "oamd", "obj_count", "max_abs",
    ];
    println!("{}", header.join("\t"));

    let mut pcm_decoder = PcmDecoder::new();
    let mut obj_decoder = ObjectPcmDecoder::new();

    let mut frame_idx: u64 = 0;
    let mut byte_off: u64 = 0;
    let mut error_count: u64 = 0;
    // Output alignment: when a frame errors, emit zero PCM the same shape
    // as the most recent successful frame so downstream PCM comparators
    // don't desync. Defaults to typical E-AC3 5.1 (6 ch * 1536 samples).
    let mut last_core_samples: usize = 1536;
    let mut last_object_count: usize = 0;

    loop {
        let read = reader.read(&mut buf)?;
        let eof = read == 0;
        if !eof {
            extractor.push_bytes(&buf[..read]);
        }

        loop {
            let frame = match extractor.next_frame() {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(e) => {
                    eprintln!("extractor error at frame {frame_idx}: {e:?}");
                    error_count += 1;
                    break;
                }
            };

            let bytes = frame.as_bytes();
            let frame_len = bytes.len();
            let (status, joc_count, oamd_count, obj_count, max_abs) = match args.mode {
                Mode::Pcm => decode_one_pcm(
                    &mut pcm_decoder,
                    bytes,
                    writer.as_mut(),
                    &mut last_core_samples,
                ),
                Mode::Object => decode_one_object(
                    &mut obj_decoder,
                    bytes,
                    writer.as_mut(),
                    &mut last_core_samples,
                    &mut last_object_count,
                ),
            };

            if status.starts_with("err") {
                error_count += 1;
            }

            println!(
                "{frame_idx}\t{byte_off}\t{frame_len}\t{status}\t{joc_count}\t{oamd_count}\t{obj_count}\t{max_abs}"
            );

            frame_idx += 1;
            byte_off += frame_len as u64;
        }

        if eof {
            break;
        }
    }

    if let Some(mut w) = writer {
        w.flush()?;
    }

    eprintln!(
        "[corpus_stream] {} frames, {} errors, mode={}",
        frame_idx,
        error_count,
        match args.mode { Mode::Pcm => "pcm", Mode::Object => "object" }
    );
    // Errors are surfaced in the per-frame TSV (status column starting with
    // "err"). Don't propagate as a process-exit failure: downstream tools
    // (compare_pcm, the cargo regression test) consume the PCM dump and
    // would rather see the diagnostic than have the pipeline abort.
    Ok(())
}

fn decode_one_pcm(
    decoder: &mut PcmDecoder,
    bytes: &[u8],
    writer: Option<&mut BufWriter<File>>,
    last_core_samples: &mut usize,
) -> (String, usize, usize, usize, String) {
    match decoder.push_access_unit(bytes) {
        Ok(result) => {
            let pcm = &result.pcm;
            let max_abs = max_abs_core(pcm);
            *last_core_samples = pcm.samples_per_channel().max(1);
            if let Some(w) = writer {
                if let Err(e) = write_core_interleaved(w, pcm) {
                    return (format!("err:write:{e}"), 0, 0, 0, "-".into());
                }
            }
            let joc = result.info.joc_payload_count();
            let oamd = result.info.oamd_payload_count();
            ("ok".into(), joc, oamd, 0, format!("{max_abs:.5}"))
        }
        Err(e) => {
            if let Some(w) = writer {
                let _ = write_silence(w, *last_core_samples, 6);
            }
            (format!("err:{e}"), 0, 0, 0, "-".into())
        }
    }
}

fn decode_one_object(
    decoder: &mut ObjectPcmDecoder,
    bytes: &[u8],
    writer: Option<&mut BufWriter<File>>,
    last_core_samples: &mut usize,
    last_object_count: &mut usize,
) -> (String, usize, usize, usize, String) {
    match decoder.push_access_unit(bytes) {
        Ok(Some(result)) => {
            let frame = &result.pcm;
            let mut max_abs = max_abs_core(&frame.core);
            for ch in &frame.object_channels {
                for &s in ch {
                    if s.abs() > max_abs {
                        max_abs = s.abs();
                    }
                }
            }
            *last_core_samples = frame.core.samples_per_channel().max(1);
            *last_object_count = frame.object_channels.len();
            if let Some(w) = writer {
                if let Err(e) = write_core_interleaved(w, &frame.core) {
                    return (format!("err:write:{e}"), 0, 0, 0, "-".into());
                }
                if let Err(e) = write_object_interleaved(w, &frame.object_channels) {
                    return (format!("err:write:{e}"), 0, 0, 0, "-".into());
                }
            }
            let joc = result.info.joc_payload_count();
            let oamd = result.info.oamd_payload_count();
            let obj_count = frame.object_channels.len();
            ("ok".into(), joc, oamd, obj_count, format!("{max_abs:.5}"))
        }
        Ok(None) => {
            if let Some(w) = writer {
                let _ = write_silence(w, *last_core_samples, 6 + *last_object_count);
            }
            ("skip:no-joc".into(), 0, 0, 0, "-".into())
        }
        Err(e) => {
            if let Some(w) = writer {
                let _ = write_silence(w, *last_core_samples, 6 + *last_object_count);
            }
            (format!("err:{e}"), 0, 0, 0, "-".into())
        }
    }
}

fn write_silence(
    writer: &mut BufWriter<File>,
    samples_per_channel: usize,
    total_channels: usize,
) -> std::io::Result<()> {
    let zero = 0.0f32.to_le_bytes();
    for _ in 0..(samples_per_channel * total_channels) {
        writer.write_all(&zero)?;
    }
    Ok(())
}

fn max_abs_core(frame: &CorePcmFrame) -> f32 {
    let mut m = 0.0_f32;
    for ch in &frame.fullband_channels {
        for &s in ch {
            if s.abs() > m {
                m = s.abs();
            }
        }
    }
    if let Some(lfe) = &frame.lfe_channel {
        for &s in lfe {
            if s.abs() > m {
                m = s.abs();
            }
        }
    }
    m
}

/// Write the core PCM frame interleaved in FFmpeg's 5.1(side) channel order:
/// FL FR FC LFE SL SR. Missing channels (e.g. no LFE) are written as zeros.
fn write_core_interleaved(
    writer: &mut BufWriter<File>,
    frame: &CorePcmFrame,
) -> std::io::Result<()> {
    let nsamples = frame.samples_per_channel();
    if nsamples == 0 {
        return Ok(());
    }

    // Build per-target-channel slice references in the 5.1(side) order.
    let order = [
        BedChannel::FrontLeft,
        BedChannel::FrontRight,
        BedChannel::Center,
        BedChannel::LowFrequencyEffects,
        BedChannel::SurroundLeft,
        BedChannel::SurroundRight,
    ];

    let mut slots: [Option<&[f32]>; 6] = [None; 6];
    for (idx, src_ch) in frame.fullband_channel_order.iter().enumerate() {
        if let Some(pos) = order.iter().position(|c| c == src_ch) {
            slots[pos] = frame.fullband_channels.get(idx).map(Vec::as_slice);
        }
    }
    if let Some(lfe) = frame.lfe_channel.as_deref() {
        slots[3] = Some(lfe);
    }

    let zero = vec![0.0f32; nsamples];
    let resolved: [&[f32]; 6] = [
        slots[0].unwrap_or(&zero),
        slots[1].unwrap_or(&zero),
        slots[2].unwrap_or(&zero),
        slots[3].unwrap_or(&zero),
        slots[4].unwrap_or(&zero),
        slots[5].unwrap_or(&zero),
    ];

    for sample_idx in 0..nsamples {
        for ch in &resolved {
            let v = ch.get(sample_idx).copied().unwrap_or(0.0);
            writer.write_all(&v.to_le_bytes())?;
        }
    }
    Ok(())
}

fn write_object_interleaved(
    writer: &mut BufWriter<File>,
    objects: &[Vec<f32>],
) -> std::io::Result<()> {
    let nsamples = objects.first().map(Vec::len).unwrap_or(0);
    if nsamples == 0 || objects.is_empty() {
        return Ok(());
    }
    for sample_idx in 0..nsamples {
        for ch in objects {
            let v = ch.get(sample_idx).copied().unwrap_or(0.0);
            writer.write_all(&v.to_le_bytes())?;
        }
    }
    Ok(())
}

