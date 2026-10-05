//! Time the bridge on an in-memory raw stream, as the renderer drives it.
//!
//! The counterpart, for what only the bridge decodes, of
//! `harletty/examples/decode_bench.rs`: today IAMF, whose decoder (iamf-rs)
//! lives in the bridge alone. The whole file is read into memory and pushed
//! through [`FormatBridge::push_packet`] in 64 KiB raw chunks, on one thread,
//! the way `tools/ffmpeg-decode-bench` feeds libavcodec. What is timed is what
//! the renderer pays: framing, decoding, rendering every mix to the bridge's
//! output layout (7.1.4 for IAMF) and the 24-bit PCM conversion.
//!
//! Usage:
//!   bridge_bench [--iec61937] <codec> <iterations> <input>
//!
//!   codec       only names the result (`iamf`); the bridge detects the stream
//!   --iec61937  the input is an IEC 61937 burst stream (`ffmpeg -f spdif`),
//!               split into bursts before timing and pushed one burst per
//!               call with its data type, the way the live S/PDIF input does
//!
//! Build with the codec's feature: `--features iamf` for IAMF. Prints one
//! JSON object on stdout, in `decode_bench`'s shape, plus two FNV-1a hashes
//! taken on the untimed pass, so two builds can be checked for the same
//! output: `pcm_hash` over every decoded sample and object position, and
//! `host_hash` over everything a host reads from the bridge — every frame
//! and metadata field, `did_reset` and errors, and the stream description
//! (`source_family`, `source_label`, `has_objects`, poses, tags) after
//! every push.

use std::hint::black_box;
use std::process::ExitCode;
use std::time::Instant;

use abi_stable::std_types::{RSlice, RStr};
use bridge_api::{FormatBridgeBox, RDecodedFrame, RInputTransport, RLogLevel};

const CHUNK: usize = 64 * 1024;

#[derive(Default, PartialEq, Eq, Debug)]
struct Tally {
    frames: u64,
    /// Per channel, summed over frames.
    samples: u64,
    /// Widest frame.
    channels: u32,
    sample_rate: u32,
    /// Frames carrying object metadata.
    object_frames: u64,
    errors: u64,
}

/// FNV-1a over what a pass decoded.
struct Hash(u64);

impl Hash {
    fn new() -> Self {
        Hash(0xcbf2_9ce4_8422_2325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn len(&mut self, len: usize) {
        self.write(&(len as u64).to_le_bytes());
    }

    fn str(&mut self, s: &str) {
        self.len(s.len());
        self.write(s.as_bytes());
    }
}

/// What one pass pushes: 64 KiB raw chunks, or IEC 61937 bursts.
enum Input {
    Raw(Vec<u8>),
    Iec61937(Vec<spdif::Iec61937Packet>),
}

/// The two hashes of the untimed pass.
struct Hashes {
    pcm: Hash,
    host: Hash,
}

fn pass(input: &Input, mut hashes: Option<&mut Hashes>) -> Tally {
    let mut bridge = harletty_bridge::new_bridge(false);
    let mut t = Tally::default();
    let mut push = |bridge: &mut FormatBridgeBox, data: &[u8], transport, data_type| {
        let result = bridge.push_packet(RSlice::from_slice(data), transport, data_type);
        if !result.error_message.is_empty() {
            t.errors += 1;
        }
        for frame in result.frames.iter() {
            t.frames += 1;
            t.samples += u64::from(frame.sample_count);
            t.channels = t.channels.max(frame.channel_count);
            t.sample_rate = frame.sampling_frequency;
            if !frame.metadata.is_empty() {
                t.object_frames += 1;
            }
            black_box(frame.pcm.first());
        }
        if let Some(hashes) = hashes.as_deref_mut() {
            for frame in result.frames.iter() {
                hash_pcm(&mut hashes.pcm, frame);
                hash_frame(&mut hashes.host, frame);
            }
            let host = &mut hashes.host;
            host.write(&[u8::from(result.did_reset)]);
            host.write(result.error_message.as_bytes());
            host.write(&[0]);
            hash_description(host, bridge);
        }
    };
    match input {
        Input::Raw(data) => {
            for chunk in data.chunks(CHUNK) {
                push(&mut bridge, chunk, RInputTransport::Raw, 0);
            }
        }
        Input::Iec61937(bursts) => {
            for burst in bursts {
                push(
                    &mut bridge,
                    &burst.payload,
                    RInputTransport::Iec61937,
                    burst.data_type,
                );
            }
        }
    }
    t
}

/// The historical `pcm_hash`: samples, then each metadata payload's position
/// and its events' ids, positions and coordinates.
fn hash_pcm(hash: &mut Hash, frame: &RDecodedFrame) {
    for sample in frame.pcm.iter() {
        hash.write(&sample.to_le_bytes());
    }
    for metadata in frame.metadata.iter() {
        hash.write(&metadata.sample_pos.to_le_bytes());
        for event in metadata.events.iter() {
            hash.write(&event.id.to_le_bytes());
            hash.write(&event.sample_pos.to_le_bytes());
            for axis in event.pos {
                hash.write(&axis.to_le_bytes());
            }
        }
    }
}

/// Every field of a decoded frame. Variable-length lists are prefixed with
/// their length so that moving an element from one list to the next changes
/// the hash.
fn hash_frame(hash: &mut Hash, frame: &RDecodedFrame) {
    hash.write(&frame.sampling_frequency.to_le_bytes());
    hash.write(&frame.sample_count.to_le_bytes());
    hash.write(&frame.channel_count.to_le_bytes());
    hash.len(frame.pcm.len());
    for sample in frame.pcm.iter() {
        hash.write(&sample.to_le_bytes());
    }
    hash.len(frame.channel_labels.len());
    for label in frame.channel_labels.iter() {
        hash.write(&[*label as u8]);
    }
    hash.write(&frame.drc_gain.to_le_bytes());
    hash.write(&frame.drc_ramp_duration.to_le_bytes());
    match frame.dialogue_level.into_option() {
        Some(level) => hash.write(&[1, level as u8]),
        None => hash.write(&[0]),
    }
    hash.write(&[u8::from(frame.is_new_segment)]);
    hash.len(frame.metadata.len());
    for metadata in frame.metadata.iter() {
        hash.write(&metadata.sample_pos.to_le_bytes());
        hash.write(&metadata.ramp_duration.to_le_bytes());
        hash.len(metadata.events.len());
        for event in metadata.events.iter() {
            hash.write(&event.id.to_le_bytes());
            hash.write(&event.sample_pos.to_le_bytes());
            hash.write(&[u8::from(event.has_pos)]);
            for axis in event.pos.iter().chain(event.size.iter()) {
                hash.write(&axis.to_le_bytes());
            }
            hash.write(&[event.gain_db as u8]);
            hash.write(&event.ramp_duration.to_le_bytes());
        }
        hash.len(metadata.object_channels.len());
        for declared in metadata.object_channels.iter() {
            hash.write(&declared.id.to_le_bytes());
            hash.write(&declared.channel.to_le_bytes());
        }
        hash.len(metadata.channel_gains.len());
        for gain in metadata.channel_gains.iter() {
            hash.write(&gain.channel.to_le_bytes());
            hash.write(&[gain.gain_db as u8]);
        }
        hash.len(metadata.name_updates.len());
        for update in metadata.name_updates.iter() {
            hash.write(&update.id.to_le_bytes());
            hash.str(update.name.as_str());
        }
    }
}

/// What a host asks the bridge about the stream between pushes.
fn hash_description(hash: &mut Hash, bridge: &FormatBridgeBox) {
    hash.write(&[u8::from(bridge.is_ready()), u8::from(bridge.has_objects())]);
    hash.str(bridge.source_family().as_str());
    hash.str(bridge.source_label().as_str());
    let poses = bridge.fixed_channel_poses();
    hash.len(poses.len());
    for pose in poses.iter() {
        hash.write(&[pose.label as u8]);
        hash.write(&pose.azimuth_deg.to_le_bytes());
        hash.write(&pose.elevation_deg.to_le_bytes());
    }
    let tags = bridge.channel_tags();
    hash.len(tags.len());
    for tag in tags.iter() {
        hash.str(tag.kind.as_str());
        hash.str(tag.language.as_str());
        hash.str(tag.label.as_str());
        hash.len(tag.channels.len());
        for channel in tag.channels.iter() {
            hash.write(&channel.to_le_bytes());
        }
    }
}

/// What a host at its default level does with the bridge's diagnostics:
/// warnings and errors are shown, the rest is dropped before formatting
/// reaches a terminal.
extern "C" fn log_sink(level: RLogLevel, _target: RStr<'_>, message: RStr<'_>) {
    if matches!(level, RLogLevel::Error | RLogLevel::Warn) {
        eprintln!("{message}");
    }
}

fn main() -> ExitCode {
    harletty_bridge::set_log_sink(log_sink);
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let iec61937 = args.first().is_some_and(|arg| arg == "--iec61937");
    if iec61937 {
        args.remove(0);
    }
    let [codec, iterations, input] = args.as_slice() else {
        eprintln!("usage: bridge_bench [--iec61937] <codec> <iterations> <input>");
        return ExitCode::from(64);
    };
    let Ok(iterations) = iterations.parse::<usize>() else {
        eprintln!("iterations must be a positive integer");
        return ExitCode::from(64);
    };
    if iterations == 0 {
        eprintln!("iterations must be a positive integer");
        return ExitCode::from(64);
    }
    let data = match std::fs::read(input) {
        Ok(data) => data,
        Err(err) => {
            eprintln!("{input}: {err}");
            return ExitCode::from(66);
        }
    };

    let bytes = data.len();
    let input_data = if iec61937 {
        // Fed in chunks, as a capture arrives: the parser drains each burst
        // from the front of its buffer, which is quadratic over a whole file.
        let mut parser = spdif::SpdifParser::new();
        let mut bursts = Vec::new();
        for chunk in data.chunks(CHUNK) {
            parser.push_bytes(chunk);
            while let Some(burst) = parser.get_next_packet() {
                bursts.push(burst);
            }
        }
        if bursts.is_empty() {
            eprintln!("{input}: no IEC 61937 burst");
            return ExitCode::from(65);
        }
        Input::Iec61937(bursts)
    } else {
        Input::Raw(data)
    };

    // One untimed pass, as on the FFmpeg side.
    let mut hashes = Hashes {
        pcm: Hash::new(),
        host: Hash::new(),
    };
    let tally = pass(&input_data, Some(&mut hashes));
    let mut times = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        let t = pass(&input_data, None);
        times.push(start.elapsed().as_secs_f64() * 1e3);
        if t != tally {
            eprintln!("a pass decoded something else than the first");
            return ExitCode::from(70);
        }
    }
    times.sort_by(f64::total_cmp);

    let audio_seconds = if tally.sample_rate == 0 {
        0.0
    } else {
        tally.samples as f64 / f64::from(tally.sample_rate)
    };
    println!(
        "{}",
        serde_json::json!({
            "decoder": "harletty",
            "codec": codec,
            "mode": if iec61937 { "bridge-iec61937" } else { "bridge" },
            "input": input,
            "bytes": bytes,
            "frames": tally.frames,
            "samples": tally.samples,
            "channels": tally.channels,
            "sample_rate": tally.sample_rate,
            "object_frames": tally.object_frames,
            "errors": tally.errors,
            "pcm_hash": format!("{:016x}", hashes.pcm.0),
            "host_hash": format!("{:016x}", hashes.host.0),
            "audio_seconds": audio_seconds,
            "iterations": iterations,
            "min_ms": times[0],
            "median_ms": times[iterations / 2],
        })
    );
    ExitCode::SUCCESS
}
