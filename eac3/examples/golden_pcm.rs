// SPDX-License-Identifier: Apache-2.0

//! Black-box golden PCM capture and comparison for the core and object
//! decoders.
//!
//! Every syncframe of a raw E-AC-3 / AC-3 elementary stream goes through one
//! stateful [`PcmDecoder`] (legacy AC-3 frames through
//! `push_legacy_ac3_access_unit`, everything else through `push_access_unit`)
//! and, in a second pass, through one stateful [`ObjectPcmDecoder`]. Only the
//! public API is used, so a reimplementation of any internal stage can be held
//! against a capture made before it, sample by sample.
//!
//! Usage:
//!
//! ```text
//! # Capture: writes <golden-dir>/<name>/{core.bin,objects.bin,frames.tsv,summary.txt}
//! cargo run --release -p eac3 --example golden_pcm -- capture <input> <golden-dir> [--name N]
//!
//! # Compare a fresh decode against a capture; exits 1 when any sample
//! # differs by more than the tolerance (default 0, i.e. bit-exact).
//! cargo run --release -p eac3 --example golden_pcm -- compare <input> <golden-dir> [--name N] [--tolerance T]
//!
//! # Decode speed of the core path (in-memory, frames pre-extracted).
//! cargo run --release -p eac3 --example golden_pcm -- bench <input> [--iterations N]
//! ```
//!
//! `<name>` defaults to the input's file name. Record format of `core.bin`
//! and `objects.bin`, one record per syncframe, little endian:
//!
//! ```text
//! u32 status        0 = decoded, 1 = decode error, 2 = no object payload (objects.bin only)
//! u32 channels      0 unless status == 0
//! u32 samples       per channel
//! f32 * channels * samples, channel-major
//! ```
//!
//! Core channel order is the decoder's own: the fullband channels in
//! `fullband_channel_order`, then the LFE when present (`frames.tsv` spells
//! the order out per frame). Object records hold the (latency-aligned) core
//! channels first, then the object channels.
//!
//! `frames.tsv` also flags, per frame, the features the frame exercises:
//! AHT (from the frame's `audio_frame` fields), SPX and rematrixing (the
//! decoder's state at the end of the frame's last block).

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use eac3::{CorePcmFrame, Extractor, ObjectPcmDecoder, PcmDecoder};

const STATUS_OK: u32 = 0;
const STATUS_ERROR: u32 = 1;
const STATUS_NO_OBJECTS: u32 = 2;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let usage = "usage: golden_pcm capture <input> <golden-dir> [--name N]\n       \
                 golden_pcm compare <input> <golden-dir> [--name N] [--tolerance T]\n       \
                 golden_pcm bench <input> [--iterations N]";
    let Some(command) = args.next() else {
        eprintln!("{usage}");
        return ExitCode::from(64);
    };
    let rest: Vec<String> = args.collect();
    let result = match command.as_str() {
        "capture" => parse_paths(&rest).and_then(|(input, dir, opts)| capture(&input, &dir, &opts)),
        "compare" => parse_paths(&rest).and_then(|(input, dir, opts)| compare(&input, &dir, &opts)),
        "bench" => parse_bench(&rest).and_then(|(input, iterations)| bench(&input, iterations)),
        _ => Err(usage.to_string()),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("golden_pcm: {error}");
            ExitCode::from(2)
        }
    }
}

struct Options {
    name: Option<String>,
    tolerance: f32,
}

fn parse_paths(args: &[String]) -> Result<(PathBuf, PathBuf, Options), String> {
    let mut positional = Vec::new();
    let mut options = Options {
        name: None,
        tolerance: 0.0,
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--name" => options.name = Some(iter.next().ok_or("--name needs a value")?.clone()),
            "--tolerance" => {
                options.tolerance = iter
                    .next()
                    .ok_or("--tolerance needs a value")?
                    .parse()
                    .map_err(|e| format!("--tolerance: {e}"))?;
            }
            other => positional.push(PathBuf::from(other)),
        }
    }
    let [input, dir] = <[PathBuf; 2]>::try_from(positional)
        .map_err(|_| "expected <input> <golden-dir>".to_string())?;
    Ok((input, dir, options))
}

fn parse_bench(args: &[String]) -> Result<(PathBuf, usize), String> {
    let mut input = None;
    let mut iterations = 5usize;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--iterations" => {
                iterations = iter
                    .next()
                    .ok_or("--iterations needs a value")?
                    .parse()
                    .map_err(|e| format!("--iterations: {e}"))?;
            }
            other => input = Some(PathBuf::from(other)),
        }
    }
    Ok((input.ok_or("expected <input>")?, iterations.max(1)))
}

fn stream_name(input: &Path, options: &Options) -> String {
    options.name.clone().unwrap_or_else(|| {
        input
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "stream".into())
    })
}

/// Every syncframe of the input, with whether it is a legacy AC-3 frame.
fn read_frames(input: &Path) -> Result<Vec<(Vec<u8>, bool)>, String> {
    let mut reader = BufReader::new(
        File::open(input).map_err(|e| format!("cannot open {}: {e}", input.display()))?,
    );
    let mut extractor = Extractor::default();
    let mut frames = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buf).map_err(|e| e.to_string())?;
        if read > 0 {
            extractor.push_bytes(&buf[..read]);
        }
        loop {
            match extractor.next_frame() {
                Ok(Some(frame)) => {
                    let legacy = frame.info().bitstream_id <= 10;
                    frames.push((frame.as_bytes().to_vec(), legacy));
                }
                Ok(None) => break,
                Err(e) => {
                    eprintln!("extractor error after frame {}: {e:?}", frames.len());
                    break;
                }
            }
        }
        if read == 0 {
            break;
        }
    }
    Ok(frames)
}

/// One decoded frame: status, planar channels, and the per-frame notes.
struct Decoded {
    status: u32,
    channels: Vec<Vec<f32>>,
    note: String,
}

struct FrameFeatures {
    aht: String,
    spx: bool,
    chinspx: String,
    remat: String,
}

fn core_channels(pcm: &CorePcmFrame) -> (Vec<Vec<f32>>, String) {
    let mut channels = pcm.fullband_channels.clone();
    let mut order: Vec<String> = pcm
        .fullband_channel_order
        .iter()
        .map(|c| format!("{c:?}"))
        .collect();
    if let Some(lfe) = &pcm.lfe_channel {
        channels.push(lfe.clone());
        order.push("LFE".into());
    }
    (channels, order.join(","))
}

fn bits(flags: &[bool]) -> String {
    if flags.is_empty() {
        "-".into()
    } else {
        flags.iter().map(|&f| if f { '1' } else { '0' }).collect()
    }
}

/// Decode the whole stream through both decoders.
fn decode_stream(frames: &[(Vec<u8>, bool)]) -> (Vec<Decoded>, Vec<FrameFeatures>, Vec<Decoded>) {
    let mut core = Vec::with_capacity(frames.len());
    let mut features = Vec::with_capacity(frames.len());
    let mut decoder = PcmDecoder::new();
    for (bytes, legacy) in frames {
        let result = if *legacy {
            decoder.push_legacy_ac3_access_unit(bytes)
        } else {
            decoder.push_access_unit(bytes)
        };
        match result {
            Ok(result) => {
                let audio = &result.info.audio_frame;
                let aht = format!(
                    "{}{}{}",
                    if audio.coupling_uses_aht { "c" } else { "" },
                    if audio.channel_uses_aht.iter().any(|&a| a) {
                        format!("f{}", bits(&audio.channel_uses_aht))
                    } else {
                        String::new()
                    },
                    if audio.lfe_uses_aht { "l" } else { "" },
                );
                features.push(FrameFeatures {
                    aht: if aht.is_empty() { "-".into() } else { aht },
                    spx: decoder.last_spx_in_use(),
                    chinspx: bits(decoder.last_chinspx()),
                    remat: bits(decoder.last_rematrixing_flags()),
                });
                let (channels, order) = core_channels(&result.pcm);
                core.push(Decoded {
                    status: STATUS_OK,
                    channels,
                    note: format!(
                        "ok\t{}\t{}\t{}",
                        result.info.substreamid, result.pcm.sample_rate, order
                    ),
                });
            }
            Err(error) => {
                features.push(FrameFeatures {
                    aht: "-".into(),
                    spx: false,
                    chinspx: "-".into(),
                    remat: "-".into(),
                });
                core.push(Decoded {
                    status: STATUS_ERROR,
                    channels: Vec::new(),
                    note: format!("err:{error}\t-\t-\t-"),
                });
            }
        }
    }

    let mut objects = Vec::with_capacity(frames.len());
    let mut decoder = ObjectPcmDecoder::new();
    for (bytes, legacy) in frames {
        if *legacy {
            objects.push(Decoded {
                status: STATUS_NO_OBJECTS,
                channels: Vec::new(),
                note: "legacy".into(),
            });
            continue;
        }
        match decoder.push_access_unit(bytes) {
            Ok(Some(result)) => {
                let (mut channels, _) = core_channels(&result.pcm.core);
                let count = result.pcm.object_channels.len();
                channels.extend(result.pcm.object_channels.iter().cloned());
                objects.push(Decoded {
                    status: STATUS_OK,
                    channels,
                    note: format!("ok:{count}"),
                });
            }
            Ok(None) => objects.push(Decoded {
                status: STATUS_NO_OBJECTS,
                channels: Vec::new(),
                note: "none".into(),
            }),
            Err(error) => objects.push(Decoded {
                status: STATUS_ERROR,
                channels: Vec::new(),
                note: format!("err:{error}"),
            }),
        }
    }
    (core, features, objects)
}

/// FNV-1a over the record's status, shape and sample bits.
fn hash(decoded: &Decoded) -> u64 {
    let mut state = 0xcbf2_9ce4_8422_2325u64;
    let mut feed = |bytes: &[u8]| {
        for &b in bytes {
            state ^= u64::from(b);
            state = state.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    feed(&decoded.status.to_le_bytes());
    feed(&(decoded.channels.len() as u32).to_le_bytes());
    for channel in &decoded.channels {
        feed(&(channel.len() as u32).to_le_bytes());
        for sample in channel {
            feed(&sample.to_bits().to_le_bytes());
        }
    }
    state
}

fn write_records(path: &Path, records: &[Decoded]) -> Result<(), String> {
    let mut out = BufWriter::with_capacity(
        1 << 20,
        File::create(path).map_err(|e| format!("cannot create {}: {e}", path.display()))?,
    );
    let mut write = |bytes: &[u8]| out.write_all(bytes).map_err(|e| e.to_string());
    for record in records {
        let samples = record.channels.first().map_or(0, Vec::len);
        write(&record.status.to_le_bytes())?;
        write(&(record.channels.len() as u32).to_le_bytes())?;
        write(&(samples as u32).to_le_bytes())?;
        for channel in &record.channels {
            if channel.len() != samples {
                return Err("channels of unequal length in one frame".into());
            }
            for sample in channel {
                write(&sample.to_le_bytes())?;
            }
        }
    }
    out.flush().map_err(|e| e.to_string())
}

fn read_records(path: &Path) -> Result<Vec<Decoded>, String> {
    let data = fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut records = Vec::new();
    let mut at = 0usize;
    let word = |at: &mut usize| -> Result<u32, String> {
        let bytes = data
            .get(*at..*at + 4)
            .ok_or_else(|| format!("{} is truncated", path.display()))?;
        *at += 4;
        Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
    };
    while at < data.len() {
        let status = word(&mut at)?;
        let channels = word(&mut at)? as usize;
        let samples = word(&mut at)? as usize;
        let mut planar = Vec::with_capacity(channels);
        for _ in 0..channels {
            let mut channel = Vec::with_capacity(samples);
            for _ in 0..samples {
                channel.push(f32::from_bits(word(&mut at)?));
            }
            planar.push(channel);
        }
        records.push(Decoded {
            status,
            channels: planar,
            note: String::new(),
        });
    }
    Ok(records)
}

fn capture(input: &Path, dir: &Path, options: &Options) -> Result<bool, String> {
    let name = stream_name(input, options);
    let frames = read_frames(input)?;
    let (core, features, objects) = decode_stream(&frames);
    let out = dir.join(&name);
    fs::create_dir_all(&out).map_err(|e| format!("cannot create {}: {e}", out.display()))?;
    write_records(&out.join("core.bin"), &core)?;
    let has_objects = objects.iter().any(|o| o.status == STATUS_OK);
    if has_objects {
        write_records(&out.join("objects.bin"), &objects)?;
    }

    let mut tsv = String::from(
        "frame\tbytes\tlegacy\tstatus\tsubstream\trate\tchannels\tsamples\taht\tspx\tchinspx\tremat\tcore_hash\tobjects\tobject_hash\n",
    );
    for (index, (((bytes, legacy), decoded), f)) in
        frames.iter().zip(&core).zip(&features).enumerate()
    {
        let object = &objects[index];
        tsv.push_str(&format!(
            "{index}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:016x}\t{}\t{:016x}\n",
            bytes.len(),
            u8::from(*legacy),
            decoded.note,
            decoded.channels.first().map_or(0, Vec::len),
            f.aht,
            u8::from(f.spx),
            f.chinspx,
            f.remat,
            hash(decoded),
            object.note,
            hash(object),
        ));
    }
    fs::write(out.join("frames.tsv"), tsv).map_err(|e| e.to_string())?;

    let decoded = core.iter().filter(|d| d.status == STATUS_OK).count();
    let errors = core.len() - decoded;
    let aht = features.iter().filter(|f| f.aht != "-").count();
    let spx = features.iter().filter(|f| f.spx).count();
    let remat_mode = features.iter().filter(|f| f.remat != "-").count();
    let remat_active = features.iter().filter(|f| f.remat.contains('1')).count();
    let object_frames = objects.iter().filter(|o| o.status == STATUS_OK).count();
    let legacy = frames.iter().filter(|(_, l)| *l).count();
    let summary = format!(
        "input\t{}\nframes\t{}\nlegacy_ac3_frames\t{legacy}\ndecoded\t{decoded}\nerrors\t{errors}\n\
         aht_frames\t{aht}\nspx_frames\t{spx}\nrematrix_2_0_frames\t{remat_mode}\n\
         rematrix_active_frames\t{remat_active}\nobject_frames\t{object_frames}\n",
        input.display(),
        frames.len(),
    );
    fs::write(out.join("summary.txt"), &summary).map_err(|e| e.to_string())?;
    print!("[{name}]\n{summary}");
    Ok(true)
}

#[derive(Default)]
struct Diff {
    frames: usize,
    status_mismatch: Option<usize>,
    shape_mismatch: Option<usize>,
    first_inexact: Option<usize>,
    first_over_tolerance: Option<usize>,
    max_per_channel: Vec<f32>,
}

fn diff(golden: &[Decoded], fresh: &[Decoded], tolerance: f32) -> Diff {
    let mut d = Diff {
        frames: golden.len().max(fresh.len()),
        ..Diff::default()
    };
    if golden.len() != fresh.len() {
        d.shape_mismatch = Some(golden.len().min(fresh.len()));
    }
    for (index, (g, f)) in golden.iter().zip(fresh).enumerate() {
        if g.status != f.status {
            d.status_mismatch.get_or_insert(index);
            continue;
        }
        if g.channels.len() != f.channels.len()
            || g.channels
                .iter()
                .zip(&f.channels)
                .any(|(a, b)| a.len() != b.len())
        {
            d.shape_mismatch.get_or_insert(index);
            continue;
        }
        if d.max_per_channel.len() < g.channels.len() {
            d.max_per_channel.resize(g.channels.len(), 0.0);
        }
        for (channel, (a, b)) in g.channels.iter().zip(&f.channels).enumerate() {
            for (x, y) in a.iter().zip(b) {
                if x.to_bits() != y.to_bits() {
                    d.first_inexact.get_or_insert(index);
                    let delta = (x - y).abs();
                    let delta = if delta.is_nan() { f32::INFINITY } else { delta };
                    if delta > tolerance {
                        d.first_over_tolerance.get_or_insert(index);
                    }
                    if delta > d.max_per_channel[channel] {
                        d.max_per_channel[channel] = delta;
                    }
                }
            }
        }
    }
    d
}

fn report(label: &str, d: &Diff, tolerance: f32) -> bool {
    let fmt = |o: Option<usize>| o.map_or("none".to_string(), |i| i.to_string());
    let max = d.max_per_channel.iter().copied().fold(0.0f32, f32::max);
    let per_channel: Vec<String> = d
        .max_per_channel
        .iter()
        .map(|m| format!("{m:.3e}"))
        .collect();
    let pass = d.status_mismatch.is_none()
        && d.shape_mismatch.is_none()
        && d.first_over_tolerance.is_none();
    println!(
        "  {label}: frames={} {} max|diff|={max:.3e} (tolerance {tolerance:e}) first_inexact_frame={} \
         first_over_tolerance_frame={} status_mismatch_frame={} shape_mismatch_frame={}\n    per-channel max|diff|: [{}]",
        d.frames,
        if pass { "PASS" } else { "FAIL" },
        fmt(d.first_inexact),
        fmt(d.first_over_tolerance),
        fmt(d.status_mismatch),
        fmt(d.shape_mismatch),
        per_channel.join(", "),
    );
    pass
}

fn compare(input: &Path, dir: &Path, options: &Options) -> Result<bool, String> {
    let name = stream_name(input, options);
    let golden_dir = dir.join(&name);
    let frames = read_frames(input)?;
    let (core, _, objects) = decode_stream(&frames);
    println!("[{name}]");
    let golden_core = read_records(&golden_dir.join("core.bin"))?;
    let mut pass = report(
        "core",
        &diff(&golden_core, &core, options.tolerance),
        options.tolerance,
    );
    let objects_path = golden_dir.join("objects.bin");
    if objects_path.exists() {
        let golden_objects = read_records(&objects_path)?;
        pass &= report(
            "objects",
            &diff(&golden_objects, &objects, options.tolerance),
            options.tolerance,
        );
    } else if objects.iter().any(|o| o.status == STATUS_OK) {
        println!("  objects: FAIL (decoded objects where the capture has none)");
        pass = false;
    }
    Ok(pass)
}

fn bench(input: &Path, iterations: usize) -> Result<bool, String> {
    let frames = read_frames(input)?;
    let mut audio_seconds = 0.0f64;
    let mut best = f64::INFINITY;
    let mut total = 0.0f64;
    for iteration in 0..iterations {
        let mut decoder = PcmDecoder::new();
        let mut seconds = 0.0f64;
        let start = Instant::now();
        for (bytes, legacy) in &frames {
            let result = if *legacy {
                decoder.push_legacy_ac3_access_unit(bytes)
            } else {
                decoder.push_access_unit(bytes)
            };
            if let Ok(result) = result {
                if result.pcm.sample_rate > 0
                    && result.info.frame_type != eac3::FrameType::Dependent
                {
                    seconds +=
                        result.pcm.samples_per_channel() as f64 / f64::from(result.pcm.sample_rate);
                }
                std::hint::black_box(&result.pcm);
            }
        }
        let elapsed = start.elapsed().as_secs_f64();
        if iteration == 0 {
            audio_seconds = seconds;
        }
        best = best.min(elapsed);
        total += elapsed;
    }
    let mean = total / iterations as f64;
    println!(
        "{}: {} frames, {audio_seconds:.2} s of audio; best {:.3} ms ({:.0} frames/s, {:.0}x realtime), \
         mean {:.3} ms over {iterations} runs",
        input.display(),
        frames.len(),
        best * 1e3,
        frames.len() as f64 / best,
        audio_seconds / best,
        mean * 1e3,
    );
    Ok(true)
}
