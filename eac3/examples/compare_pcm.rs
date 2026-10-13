//! Compare two or three interleaved float32 PCM streams.
//!
//! Designed for the harletty / FFmpeg / Cavern E-AC3 regression matrix:
//! - `--harletty` is the path under test
//! - `--ffmpeg` is the FFmpeg reference (always present)
//! - `--cavern` is the Cavern reference (optional)
//!
//! All inputs must share the same channel count and sample order
//! (FFmpeg 5.1(side): FL FR FC LFE SL SR for the bed). Streams are aligned
//! from sample 0; any trailing samples past the shortest input are ignored
//! and reported.
//!
//! Per pairing, the report contains:
//! - aligned sample count
//! - per-channel RMSE
//! - global max-abs diff
//! - first-divergence sample index above the tolerance
//!
//! Streaming: reads ~16384 samples per channel at a time from each file,
//! computes diffs incrementally, never materialises the full vectors.
//! Each pairing is a single pass over both files. A full two-hour witness
//! produces 11.5 GB f32 files, which would OOM a 60 GB box if loaded
//! whole for all three pairings (34 GB Vec<f32> peak).
//!
//! Usage:
//!     cargo run --release --example compare_pcm -p eac3 -- \
//!         --harletty path/to/harletty.f32 \
//!         --ffmpeg path/to/ffmpeg.f32 \
//!         [--cavern path/to/cavern.f32] \
//!         --channels 6 [--tolerance 1e-3]

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const CHUNK_SAMPLES_PER_CHANNEL: usize = 16_384;

struct Args {
    harletty: PathBuf,
    ffmpeg: PathBuf,
    cavern: Option<PathBuf>,
    channels: usize,
    tolerance: f32,
}

fn parse_args() -> Result<Args, String> {
    let mut iter = std::env::args().skip(1);
    let mut harletty: Option<PathBuf> = None;
    let mut ffmpeg: Option<PathBuf> = None;
    let mut cavern: Option<PathBuf> = None;
    let mut channels: Option<usize> = None;
    let mut tolerance: f32 = 1.0e-3;

    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--harletty" => harletty = Some(iter.next().ok_or("--harletty needs a path")?.into()),
            "--ffmpeg" => ffmpeg = Some(iter.next().ok_or("--ffmpeg needs a path")?.into()),
            "--cavern" => cavern = Some(iter.next().ok_or("--cavern needs a path")?.into()),
            "--channels" => {
                channels = Some(
                    iter.next()
                        .ok_or("--channels needs a value")?
                        .parse()
                        .map_err(|e| format!("--channels: {e}"))?,
                );
            }
            "--tolerance" => {
                tolerance = iter
                    .next()
                    .ok_or("--tolerance needs a value")?
                    .parse()
                    .map_err(|e| format!("--tolerance: {e}"))?;
            }
            other => return Err(format!("unexpected argument: {other}")),
        }
    }

    Ok(Args {
        harletty: harletty.ok_or("--harletty is required")?,
        ffmpeg: ffmpeg.ok_or("--ffmpeg is required")?,
        cavern,
        channels: channels.ok_or("--channels is required")?,
        tolerance,
    })
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            eprintln!(
                "usage: compare_pcm --harletty <f32> --ffmpeg <f32> [--cavern <f32>] --channels <N> [--tolerance <f>]"
            );
            return ExitCode::from(64);
        }
    };

    if args.channels == 0 {
        eprintln!("--channels must be > 0");
        return ExitCode::FAILURE;
    }

    let pairings: &[(&str, &Path, &Path)] = match args.cavern.as_deref() {
        Some(cav) => &[
            ("harletty vs ffmpeg", &args.harletty, &args.ffmpeg),
            ("harletty vs cavern", &args.harletty, cav),
            ("ffmpeg vs cavern", &args.ffmpeg, cav),
        ],
        None => &[("harletty vs ffmpeg", &args.harletty, &args.ffmpeg)],
    };

    for (label, a, b) in pairings {
        match stream_diff(a, b, args.channels, args.tolerance) {
            Ok(report) => print_report(label, &report, args.channels, args.tolerance),
            Err(e) => {
                eprintln!("[{label}] {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    ExitCode::SUCCESS
}

#[derive(Debug)]
struct Report {
    aligned_samples: usize,
    a_excess: usize,
    b_excess: usize,
    max_abs_diff: f32,
    max_abs_idx: usize,
    first_div_idx: Option<usize>,
    sum_sq_per_channel: Vec<f64>,
}

fn stream_diff(
    a_path: &Path,
    b_path: &Path,
    channels: usize,
    tolerance: f32,
) -> std::io::Result<Report> {
    let a_file = File::open(a_path)?;
    let b_file = File::open(b_path)?;
    let a_len = a_file.metadata()?.len() as usize;
    let b_len = b_file.metadata()?.len() as usize;
    if !a_len.is_multiple_of(4) {
        return Err(std::io::Error::other(format!(
            "{}: size {} not multiple of 4",
            a_path.display(),
            a_len
        )));
    }
    if !b_len.is_multiple_of(4) {
        return Err(std::io::Error::other(format!(
            "{}: size {} not multiple of 4",
            b_path.display(),
            b_len
        )));
    }
    let a_samples_total = a_len / 4;
    let b_samples_total = b_len / 4;
    let aligned_samples = a_samples_total.min(b_samples_total) / channels * channels;

    let mut a_reader = BufReader::with_capacity(1 << 20, a_file);
    let mut b_reader = BufReader::with_capacity(1 << 20, b_file);

    let chunk_samples = CHUNK_SAMPLES_PER_CHANNEL * channels;
    let mut a_chunk = vec![0u8; chunk_samples * 4];
    let mut b_chunk = vec![0u8; chunk_samples * 4];

    let mut sum_sq = vec![0.0_f64; channels];
    let mut max_abs_diff = 0.0_f32;
    let mut max_abs_idx = 0usize;
    let mut first_div_idx: Option<usize> = None;
    let mut consumed: usize = 0;

    while consumed < aligned_samples {
        let remaining = aligned_samples - consumed;
        let this_chunk = remaining.min(chunk_samples);
        let bytes = this_chunk * 4;
        a_reader.read_exact(&mut a_chunk[..bytes])?;
        b_reader.read_exact(&mut b_chunk[..bytes])?;

        for i in 0..this_chunk {
            let off = i * 4;
            let av = f32::from_le_bytes([
                a_chunk[off],
                a_chunk[off + 1],
                a_chunk[off + 2],
                a_chunk[off + 3],
            ]);
            let bv = f32::from_le_bytes([
                b_chunk[off],
                b_chunk[off + 1],
                b_chunk[off + 2],
                b_chunk[off + 3],
            ]);
            let diff = av - bv;
            let abs = diff.abs();
            let ch = i % channels;
            sum_sq[ch] += (diff as f64) * (diff as f64);
            if abs > max_abs_diff {
                max_abs_diff = abs;
                max_abs_idx = consumed + i;
            }
            if first_div_idx.is_none() && abs > tolerance {
                first_div_idx = Some(consumed + i);
            }
        }
        consumed += this_chunk;
    }

    Ok(Report {
        aligned_samples,
        a_excess: a_samples_total.saturating_sub(aligned_samples),
        b_excess: b_samples_total.saturating_sub(aligned_samples),
        max_abs_diff,
        max_abs_idx,
        first_div_idx,
        sum_sq_per_channel: sum_sq,
    })
}

fn print_report(label: &str, r: &Report, channels: usize, tolerance: f32) {
    let nframes = r.aligned_samples / channels;
    println!("=== {label} ===");
    println!(
        "aligned samples: {} ({} per-channel frames, {} ch)",
        r.aligned_samples, nframes, channels
    );
    println!("trailing unmatched: a+{} b+{}", r.a_excess, r.b_excess);
    println!("tolerance: {tolerance:.3e}");
    println!(
        "max |a-b|: {:.6e}  at sample {} (ch {}, frame {})",
        r.max_abs_diff,
        r.max_abs_idx,
        r.max_abs_idx % channels,
        r.max_abs_idx / channels
    );
    match r.first_div_idx {
        Some(idx) => println!(
            "first divergence above tolerance: sample {} (ch {}, frame {})",
            idx,
            idx % channels,
            idx / channels
        ),
        None => println!("no sample exceeds tolerance"),
    }
    print!("per-channel RMSE:");
    let denom = (nframes as f64).max(1.0);
    for ch in 0..channels {
        let rmse = (r.sum_sq_per_channel[ch] / denom).sqrt();
        print!(" ch{ch}={rmse:.6e}");
    }
    println!();
    println!();
}
