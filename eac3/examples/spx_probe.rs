//! Detect whether an E-AC-3 stream activates Spectral Extension (SPX).
//!
//! Streams every access unit of `<input.eac3>` through a stateful
//! PcmDecoder and tallies the `spxinu` / `chinspx[ch]` flags after each
//! frame. Useful when picking content for validating SPX-related
//! decoder changes (e.g. the `fix/eac3-spx-coordinate-parse` branch),
//! since not all DDP+ tracks exercise SPX — Dune Part Two's main bed
//! never activates it.
//!
//! Usage:
//!     # From an MKV via ffmpeg:
//!     ffmpeg -i film.mkv -map 0:a:0 -c:a copy -f eac3 - 2>/dev/null \
//!         | cargo run --release --example spx_probe -p eac3 -- -
//!
//!     # From a pre-extracted .eac3 file:
//!     cargo run --release --example spx_probe -p eac3 -- film.eac3
//!
//! Output:
//!     spx_probe: frames=N spx_active=K (P.PP%) channels=[ch0:K0, ch1:K1, ...]

use std::fs::File;
use std::io::{BufReader, Read};
use std::process::ExitCode;

use eac3::{Extractor, PcmDecoder};

fn main() -> ExitCode {
    let path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: spx_probe <input.eac3 | ->");
            return ExitCode::from(64);
        }
    };

    let mut reader: Box<dyn Read> = if path == "-" {
        Box::new(BufReader::with_capacity(64 * 1024, std::io::stdin()))
    } else {
        match File::open(&path) {
            Ok(f) => Box::new(BufReader::with_capacity(64 * 1024, f)),
            Err(e) => {
                eprintln!("open {path}: {e}");
                return ExitCode::FAILURE;
            }
        }
    };

    let mut decoder = PcmDecoder::new();
    let mut extractor = Extractor::default();
    let mut buf = [0u8; 64 * 1024];

    let mut frame_count = 0u64;
    let mut spx_active_frames = 0u64;
    let mut channel_active_counts: Vec<u64> = Vec::new();
    let mut decode_errors = 0u64;

    loop {
        let n = match reader.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("read: {e}");
                return ExitCode::FAILURE;
            }
        };
        let eof = n == 0;
        if !eof {
            extractor.push_bytes(&buf[..n]);
        }
        loop {
            let frame = match extractor.next_frame() {
                Ok(Some(f)) => f,
                Ok(None) => break,
                Err(_) => {
                    decode_errors += 1;
                    break;
                }
            };
            match decoder.push_access_unit(frame.as_bytes()) {
                Ok(_) => {
                    frame_count += 1;
                    if decoder.last_spx_in_use() {
                        spx_active_frames += 1;
                    }
                    let chinspx = decoder.last_chinspx();
                    if channel_active_counts.len() < chinspx.len() {
                        channel_active_counts.resize(chinspx.len(), 0);
                    }
                    for (i, in_spx) in chinspx.iter().enumerate() {
                        if *in_spx {
                            channel_active_counts[i] += 1;
                        }
                    }
                }
                Err(_) => decode_errors += 1,
            }
        }
        if eof {
            break;
        }
    }

    let pct = if frame_count > 0 {
        (spx_active_frames as f64) * 100.0 / (frame_count as f64)
    } else {
        0.0
    };
    let ch_summary = channel_active_counts
        .iter()
        .enumerate()
        .map(|(i, count)| format!("ch{i}:{count}"))
        .collect::<Vec<_>>()
        .join(", ");
    println!(
        "spx_probe: frames={frame_count} spx_active={spx_active_frames} ({pct:.2}%) decode_errors={decode_errors} channels=[{ch_summary}]"
    );
    ExitCode::SUCCESS
}
