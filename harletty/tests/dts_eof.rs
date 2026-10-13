//! Regression: the offline DTS demux must decode the final frame of a file.
//!
//! The demux holds a core frame back until four more bytes arrive, to test
//! whether an EXSS substream follows it. The last frame of a file has nothing
//! after it, so it was never decoded: a 751-frame core stream came out as 750
//! frames where libavcodec gives 751.
//!
//! The fixture is 10 core-only frames (stereo, 48 kHz, 428 bytes each) of a
//! synthetic 440 Hz tone, made with libavcodec's DTS encoder:
//!
//!   ffmpeg -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=0.1" \
//!       -ac 2 -c:a dca -strict -2 -b:a 320k -f dts dts_core_tone_10f.dts

use std::path::{Path, PathBuf};
use std::process::Command;

const FRAMES: u64 = 10;
const FRAME_BYTES: usize = 428;
const SAMPLES_PER_FRAME: u64 = 512;
const CHANNELS: u64 = 2;
/// `--format pcm` writes 24-bit little-endian samples.
const BYTES_PER_SAMPLE: u64 = 3;

fn fixture() -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dts_core_tone_10f.dts");
    std::fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// Decode `stream` to raw PCM and return the number of frames it produced.
fn decoded_frames(name: &str, stream: &[u8]) -> u64 {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dts_eof");
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join(format!("{name}.dts"));
    std::fs::write(&input, stream).unwrap();
    let out_base: PathBuf = dir.join(name);
    let output = out_base.with_extension("pcm");
    let _ = std::fs::remove_file(&output);

    let status = Command::new(env!("CARGO_BIN_EXE_harletty"))
        .args(["--loglevel", "error", "decode", "--format", "pcm"])
        .arg(&input)
        .arg("--output-path")
        .arg(&out_base)
        .status()
        .expect("failed to run the harletty binary");
    assert!(status.success(), "harletty decode failed: {status}");

    let bytes = std::fs::metadata(&output)
        .unwrap_or_else(|e| panic!("reading {}: {e}", output.display()))
        .len();
    let frame_bytes = SAMPLES_PER_FRAME * CHANNELS * BYTES_PER_SAMPLE;
    assert_eq!(bytes % frame_bytes, 0, "{name}: output is not whole frames");
    bytes / frame_bytes
}

#[test]
fn stream_ending_exactly_on_a_core_frame_decodes_every_frame() {
    let stream = fixture();
    assert_eq!(stream.len(), FRAMES as usize * FRAME_BYTES);
    assert_eq!(decoded_frames("exact", &stream), FRAMES);
}

#[test]
fn trailing_bytes_too_short_for_an_exss_sync_do_not_hide_the_last_frame() {
    let mut stream = fixture();
    stream.extend_from_slice(&[0, 0, 0]);
    assert_eq!(decoded_frames("short_tail", &stream), FRAMES);
}

#[test]
fn truncated_final_frame_is_dropped() {
    let mut stream = fixture();
    stream.truncate(stream.len() - FRAME_BYTES / 2);
    assert_eq!(decoded_frames("truncated", &stream), FRAMES - 1);
}
