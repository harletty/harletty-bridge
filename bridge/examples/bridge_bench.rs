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
//!   bridge_bench <codec> <iterations> <input>
//!
//!   codec   only names the result (`iamf`); the bridge detects the stream
//!
//! Build with the codec's feature: `--features iamf` for IAMF. Prints one
//! JSON object on stdout, in `decode_bench`'s shape.

use std::hint::black_box;
use std::process::ExitCode;
use std::time::Instant;

use abi_stable::std_types::RSlice;
use bridge_api::RInputTransport;

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

fn pass(data: &[u8]) -> Tally {
    let mut bridge = harletty_bridge::new_bridge(false);
    let mut t = Tally::default();
    for chunk in data.chunks(CHUNK) {
        let result = bridge.push_packet(RSlice::from_slice(chunk), RInputTransport::Raw, 0);
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
    }
    t
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [codec, iterations, input] = args.as_slice() else {
        eprintln!("usage: bridge_bench <codec> <iterations> <input>");
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

    // One untimed pass, as on the FFmpeg side.
    let tally = pass(&data);
    let mut times = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        let t = pass(&data);
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
            "mode": "bridge",
            "input": input,
            "bytes": data.len(),
            "frames": tally.frames,
            "samples": tally.samples,
            "channels": tally.channels,
            "sample_rate": tally.sample_rate,
            "object_frames": tally.object_frames,
            "errors": tally.errors,
            "audio_seconds": audio_seconds,
            "iterations": iterations,
            "min_ms": times[0],
            "median_ms": times[iterations / 2],
        })
    );
    ExitCode::SUCCESS
}
