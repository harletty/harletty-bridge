// SPDX-License-Identifier: Apache-2.0
//
// Classify XLL-X profiles and decoded extension-source activity over an initial
// bounded portion of one or more raw `[core][EXSS]` elementary streams.
// Research tool only; it is not used by the realtime decoder.
//
// Usage:
//   cargo run -p dca --release --example xll_corpus_probe -- \
//     [--max-mb 128] <track.dts> [track.dts ...]

use std::collections::BTreeMap;
use std::io::Read;

use dca::{HdDecoder, HdError, exss_substream_size, parse_header};

#[derive(Default)]
struct SourceEnergy {
    square_sum: f64,
    samples: u64,
    nonzero_samples: u64,
}

fn profile_name(payload: &[u8], standard: bool, alternate: bool) -> &'static str {
    match payload.get(..4) {
        Some([0xf1, 0x40, 0x00, 0xd0]) => "D0",
        Some([0xf1, 0x40, 0x00, 0xd1]) => "D1",
        Some([0xf1, 0x40, 0x00, 0xd3]) => "D3",
        _ if standard => "standard",
        _ if alternate => "alternate-unknown",
        _ => "none",
    }
}

fn probe(path: &str, max_mb: usize) -> Result<(), String> {
    let mut input = std::fs::File::open(path).map_err(|error| format!("open: {error}"))?;
    let limit = max_mb
        .checked_mul(1024 * 1024)
        .ok_or_else(|| "max-mb overflow".to_string())?;
    let mut bytes = vec![0u8; limit];
    let read = input
        .read(&mut bytes)
        .map_err(|error| format!("read: {error}"))?;
    bytes.truncate(read);

    let mut decoder = HdDecoder::new();
    let mut offset = 0usize;
    let mut frames = 0u64;
    let mut decoded = 0u64;
    let mut pending = 0u64;
    let mut profiles = BTreeMap::<&'static str, u64>::new();
    let mut source_counts = BTreeMap::<usize, u64>::new();
    let mut pcm_resolutions = BTreeMap::<usize, u64>::new();
    let mut errors = BTreeMap::<&'static str, u64>::new();
    let mut energy = Vec::<SourceEnergy>::new();
    let mut sample_rate = 0u32;

    while offset + 18 <= bytes.len() {
        let header = match parse_header(&bytes[offset..]) {
            Ok(header) => header,
            Err(_) => break,
        };
        let exss_offset = match offset.checked_add(header.frame_size) {
            Some(value) => value,
            None => break,
        };
        let Some(exss) = bytes.get(exss_offset..) else {
            break;
        };
        let Some(exss_size) = exss_substream_size(exss) else {
            break;
        };
        let Some(frame_end) = exss_offset.checked_add(exss_size) else {
            break;
        };
        let Some(core) = bytes.get(offset..exss_offset) else {
            break;
        };
        let Some(exss) = bytes.get(exss_offset..frame_end) else {
            break;
        };

        match decoder.decode(core, exss) {
            Ok(frame) => {
                frames += 1;
                if frame.x_present || frame.x_imax {
                    decoded += 1;
                    *profiles
                        .entry(profile_name(
                            &frame.x_payload,
                            frame.x_present,
                            frame.x_imax,
                        ))
                        .or_default() += 1;
                    *source_counts.entry(frame.x_samples.len()).or_default() += 1;
                    *pcm_resolutions.entry(frame.x_pcm_bit_res).or_default() += 1;
                    if let Some(error) = frame.x_decode_error {
                        *errors.entry(error).or_default() += 1;
                    }
                    sample_rate = frame.sample_rate;
                    energy.resize_with(frame.x_samples.len(), SourceEnergy::default);
                    for (stats, channel) in energy.iter_mut().zip(&frame.x_samples) {
                        for &sample in channel {
                            let value = sample as f64;
                            stats.square_sum += value * value;
                            stats.samples += 1;
                            stats.nonzero_samples += u64::from(sample != 0.0);
                        }
                    }
                }
            }
            Err(HdError::Pending) => pending += 1,
            Err(error) => return Err(format!("decode at byte {offset}: {error:?}")),
        }
        offset = frame_end;
    }

    let sources = energy
        .iter()
        .enumerate()
        .map(|(index, stats)| {
            let rms = (stats.square_sum / stats.samples.max(1) as f64).sqrt();
            let active = 100.0 * stats.nonzero_samples as f64 / stats.samples.max(1) as f64;
            format!("X{index}={rms:.6}/{active:.1}%")
        })
        .collect::<Vec<_>>()
        .join(",");
    println!(
        "{path}\tread={read}\tframes={frames}\tpending={pending}\text={decoded}\trate={sample_rate}\tprofiles={profiles:?}\tsources={source_counts:?}\tpcm={pcm_resolutions:?}\terrors={errors:?}\tactivity={sources}"
    );
    Ok(())
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut max_mb = 128usize;
    let mut paths = Vec::new();
    while let Some(argument) = args.next() {
        if argument == "--max-mb" {
            max_mb = args
                .next()
                .expect("--max-mb requires a value")
                .parse()
                .expect("invalid --max-mb value");
        } else {
            paths.push(argument);
        }
    }
    if paths.is_empty() {
        eprintln!("usage: xll_corpus_probe [--max-mb 128] <track.dts> [track.dts ...]");
        std::process::exit(2);
    }
    let mut failed = false;
    for path in paths {
        if let Err(error) = probe(&path, max_mb) {
            eprintln!("{path}: {error}");
            failed = true;
        }
    }
    if failed {
        std::process::exit(1);
    }
}
