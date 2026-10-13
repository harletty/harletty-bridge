//! Decode the baseline corpus through the host's bridge router: see
//! README.md.

use std::path::PathBuf;
use std::process::ExitCode;

use bridge_api::{RDecodedFrame, RInputTransport};
use orender_engine::bridge_loader::{BridgeLibs, load_bridge_library};
use orender_engine::bridge_set::BridgeSet;

/// FNV-1a.
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

/// `bridge_bench`'s `pcm_hash`: samples, then each metadata payload's
/// position and its events' ids, positions and coordinates.
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

/// Every field of a decoded frame (`bridge_bench`'s `hash_frame`).
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

/// What a host asks about the stream between pushes.
fn hash_description(hash: &mut Hash, bridge: &BridgeSet) {
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

struct Run {
    pcm: Hash,
    frames_hash: Hash,
    host: Hash,
    frames: u64,
    samples: u64,
    errors: u64,
    resets: u64,
}

impl Run {
    fn push(
        &mut self,
        bridge: &mut BridgeSet,
        data: &[u8],
        transport: RInputTransport,
        data_type: u8,
    ) {
        let result = bridge.push_packet(data, transport, data_type);
        for frame in result.frames.iter() {
            self.frames += 1;
            self.samples += u64::from(frame.sample_count);
            hash_pcm(&mut self.pcm, frame);
            hash_frame(&mut self.frames_hash, frame);
            hash_frame(&mut self.host, frame);
        }
        if !result.error_message.is_empty() {
            self.errors += 1;
        }
        if result.did_reset {
            self.resets += 1;
        }
        self.host.write(&[u8::from(result.did_reset)]);
        self.host.write(result.error_message.as_bytes());
        self.host.write(&[0]);
        hash_description(&mut self.host, bridge);
    }
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1).peekable();
    let mut read = 64 * 1024;
    let mut libraries = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "--read" {
            read = match args.next().and_then(|n| n.parse::<usize>().ok()) {
                Some(n) if n > 0 => n,
                _ => {
                    eprintln!("--read takes a positive byte count");
                    return ExitCode::from(64);
                }
            };
        } else {
            libraries.push(PathBuf::from(arg));
        }
    }
    let Some(manifest) = std::env::var_os("HARLETTY_BASELINE_CORPUS") else {
        eprintln!("HARLETTY_BASELINE_CORPUS must name the corpus manifest");
        return ExitCode::from(64);
    };
    if libraries.is_empty() {
        eprintln!("usage: host-baseline [--read N] <bridge library>...");
        return ExitCode::from(64);
    }
    let libs = match libraries
        .iter()
        .map(|path| load_bridge_library(path))
        .collect::<Result<Vec<_>, _>>()
        .and_then(BridgeLibs::new)
    {
        Ok(libs) => libs,
        Err(err) => {
            eprintln!("{err:#}");
            return ExitCode::from(66);
        }
    };
    let manifest = match std::fs::read_to_string(&manifest) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("{}: {err}", PathBuf::from(&manifest).display());
            return ExitCode::from(66);
        }
    };
    for line in manifest.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [family, transport, path, ..] = fields.as_slice() else {
            continue;
        };
        if family.starts_with('#') {
            continue;
        }
        let data = match std::fs::read(path) {
            Ok(data) => data,
            Err(err) => {
                eprintln!("{path}: {err}");
                return ExitCode::from(66);
            }
        };
        let mut bridge = match BridgeSet::open(&libs) {
            Ok(bridge) => bridge,
            Err(err) => {
                eprintln!("{err:#}");
                return ExitCode::from(70);
            }
        };
        // What every host sends first.
        bridge.configure("presentation", "best");
        let mut run = Run {
            pcm: Hash::new(),
            frames_hash: Hash::new(),
            host: Hash::new(),
            frames: 0,
            samples: 0,
            errors: 0,
            resets: 0,
        };
        let iec61937 = *transport == "iec61937";
        let started = std::time::Instant::now();
        if iec61937 {
            // Fed in chunks, as a capture arrives: the parser drains each
            // burst from the front of its buffer.
            let mut parser = spdif::SpdifParser::new();
            for chunk in data.chunks(64 * 1024) {
                parser.push_bytes(chunk);
                while let Some(burst) = parser.get_next_packet() {
                    run.push(
                        &mut bridge,
                        &burst.payload,
                        RInputTransport::Iec61937,
                        burst.data_type,
                    );
                }
            }
        } else {
            for chunk in data.chunks(read) {
                run.push(&mut bridge, chunk, RInputTransport::Raw, 0);
            }
        }
        let decode_ms = started.elapsed().as_secs_f64() * 1e3;
        let mut description = Hash::new();
        hash_description(&mut description, &bridge);
        println!(
            "{}",
            serde_json::json!({
                "family": family,
                "transport": transport,
                "input": path,
                "read": if iec61937 { 0 } else { read },
                "bridges": libraries.len(),
                "frames": run.frames,
                "samples": run.samples,
                "errors": run.errors,
                "resets": run.resets,
                "pcm_hash": format!("{:016x}", run.pcm.0),
                "frame_hash": format!("{:016x}", run.frames_hash.0),
                "description_hash": format!("{:016x}", description.0),
                "host_hash": format!("{:016x}", run.host.0),
                "decode_ms": decode_ms,
            })
        );
    }
    ExitCode::SUCCESS
}
