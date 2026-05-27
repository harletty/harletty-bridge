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

    let harletty = match load_f32(&args.harletty) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("load harletty: {e}");
            return ExitCode::FAILURE;
        }
    };
    let ffmpeg = match load_f32(&args.ffmpeg) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("load ffmpeg: {e}");
            return ExitCode::FAILURE;
        }
    };
    let cavern = match args.cavern.as_deref().map(load_f32).transpose() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("load cavern: {e}");
            return ExitCode::FAILURE;
        }
    };

    print_diff(
        "harletty vs ffmpeg",
        &harletty,
        &ffmpeg,
        args.channels,
        args.tolerance,
    );
    if let Some(cav) = cavern.as_deref() {
        print_diff(
            "harletty vs cavern",
            &harletty,
            cav,
            args.channels,
            args.tolerance,
        );
        print_diff(
            "ffmpeg vs cavern",
            &ffmpeg,
            cav,
            args.channels,
            args.tolerance,
        );
    }

    ExitCode::SUCCESS
}

fn load_f32(path: &Path) -> std::io::Result<Vec<f32>> {
    let file = File::open(path)?;
    let len = file.metadata()?.len() as usize;
    if !len.is_multiple_of(4) {
        return Err(std::io::Error::other(format!(
            "{}: size {} not a multiple of 4 bytes",
            path.display(),
            len
        )));
    }
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let nsamples = len / 4;
    let mut out = Vec::with_capacity(nsamples);
    let mut buf = [0u8; 4];
    for _ in 0..nsamples {
        reader.read_exact(&mut buf)?;
        out.push(f32::from_le_bytes(buf));
    }
    Ok(out)
}

fn print_diff(label: &str, a: &[f32], b: &[f32], channels: usize, tolerance: f32) {
    if channels == 0 {
        eprintln!("{label}: channels=0, skipped");
        return;
    }
    let aligned_samples = a.len().min(b.len()) / channels * channels;
    let nframes = aligned_samples / channels;
    let mut sum_sq = vec![0.0_f64; channels];
    let mut max_abs_diff = 0.0_f32;
    let mut max_abs_idx = 0usize;
    let mut first_div_idx: Option<usize> = None;

    for i in 0..aligned_samples {
        let diff = a[i] - b[i];
        let abs = diff.abs();
        let ch = i % channels;
        sum_sq[ch] += (diff as f64) * (diff as f64);
        if abs > max_abs_diff {
            max_abs_diff = abs;
            max_abs_idx = i;
        }
        if first_div_idx.is_none() && abs > tolerance {
            first_div_idx = Some(i);
        }
    }

    let a_excess = a.len().saturating_sub(aligned_samples);
    let b_excess = b.len().saturating_sub(aligned_samples);

    println!("=== {label} ===");
    println!("aligned samples: {aligned_samples} ({nframes} per-channel frames, {channels} ch)");
    println!("trailing unmatched: a+{a_excess} b+{b_excess}");
    println!("tolerance: {tolerance:.3e}");
    println!(
        "max |a-b|: {:.6e}  at sample {} (ch {}, frame {})",
        max_abs_diff,
        max_abs_idx,
        max_abs_idx % channels,
        max_abs_idx / channels
    );
    match first_div_idx {
        Some(idx) => println!(
            "first divergence above tolerance: sample {} (ch {}, frame {})",
            idx,
            idx % channels,
            idx / channels
        ),
        None => println!("no sample exceeds tolerance"),
    }
    print!("per-channel RMSE:");
    for ch in 0..channels {
        let denom = (nframes as f64).max(1.0);
        let rmse = (sum_sq[ch] / denom).sqrt();
        print!(" ch{ch}={rmse:.6e}");
    }
    println!();
    println!();
}
