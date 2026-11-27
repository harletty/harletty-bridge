// SPDX-License-Identifier: Apache-2.0
//! Auro-3D in PCM input, through the built binary: a synthetic 7.1 carrier
//! (blocks of the side channel announcing a `7.1_5H_1T` layout, built by a
//! writer that mirrors the reader's bit map as `auro/tests/synthetic.rs`
//! does) wrapped as WAV, as ffmpeg writes it into a pipe.
//!
//! The DTS path has no committed Auro-3D fixture; its equivalence with this
//! path is checked on a real DTS-HD MA carrier decoded by ffmpeg (see
//! `docs/harletty-cli.md`).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use auro::block::{SYNC_SAMPLES, bit_location, block_crc};
use serde_json::Value;

const BLOCK: usize = 1000;
const BLOCKS: usize = 20;
const FRAMES: usize = BLOCK * BLOCKS;
/// The 7.1 channel mask: FL FR FC LFE BL BR SL SR.
const MASK_7_1: u32 = 0x63F;

/// Deterministic 24-bit "audio": a linear congruential generator, so the
/// low bits are as random as a real mix's before the encoder borrows them.
struct Lcg(u32);

impl Lcg {
    fn next(&mut self) -> i32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((self.0 >> 8) as i32) << 8 >> 8
    }
}

fn set_bit(samples: &mut [i32], idx: usize, bit: u32, value: u32) {
    let mask = 1i32 << bit;
    if value & 1 != 0 {
        samples[idx] |= mask;
    } else {
        samples[idx] &= !mask;
    }
}

struct Writer<'a> {
    samples: &'a mut [i32],
    pos: usize,
}

impl Writer<'_> {
    fn put(&mut self, bits: u32, value: u32) {
        for i in (0..bits).rev() {
            let (idx, b) = bit_location(self.pos, 3, true);
            set_bit(self.samples, idx, b, (value >> i) & 1);
            self.pos += 1;
        }
    }
}

/// One block of `BLOCK` samples borrowing three bits, announcing channel
/// configuration 62 (`7.1_5H_1T` over a 7.1 carrier).
fn block(seed: u32) -> Vec<i32> {
    let mut lcg = Lcg(seed);
    let mut s: Vec<i32> = (0..BLOCK).map(|_| lcg.next()).collect();
    for v in s.iter_mut().take(SYNC_SAMPLES) {
        *v &= !0b111;
        *v |= 1;
    }
    let size_code = 0x3Du32; // 1000 samples
    for i in 0..8 {
        set_bit(&mut s, i, 2, (size_code >> (7 - i)) & 1);
    }
    for i in 8..12 {
        set_bit(&mut s, i, 2, 0);
    }
    let width_code = 14 - 3u32;
    for i in 12..16 {
        set_bit(&mut s, i, 2, (width_code >> (15 - i)) & 1);
    }
    let mut w = Writer {
        samples: &mut s,
        pos: 48,
    };
    w.put(8, 1);
    w.put(8, 10);
    w.put(1, 0);
    w.put(1, 0);
    w.put(2, 0);
    w.put(4, 0);
    w.put(8, 3);
    w.put(8, 1); // one ADOL block
    w.put(8, 5);
    w.put(8, 0); // stream slot
    w.put(8, 0xFF);
    w.put(8, 0xFF);
    w.put(8, 0xFF);
    for _ in 0..4 {
        w.put(8, 0);
    }
    w.put(8, 1); // tag
    w.put(8, 0x1E);
    w.put(8, 62); // channel configuration
    w.put(8, 0);
    // Reserved bits.
    let mut pos = 17 * 3 - 1;
    while pos < 3 * BLOCK {
        let (idx, b) = bit_location(pos, 3, false);
        set_bit(&mut s, idx, b, 0);
        pos += SYNC_SAMPLES * 3;
    }
    let crc = block_crc(&s);
    for i in 0..16 {
        set_bit(&mut s, i, 1, u32::from((crc >> (15 - i)) & 1));
    }
    s
}

/// The eight channels of a carrier, a block run in each.
fn carrier() -> Vec<Vec<i32>> {
    (0..8u32)
        .map(|channel| {
            (0..BLOCKS as u32)
                .flat_map(|n| block(1000 * channel + n))
                .collect()
        })
        .collect()
}

/// Noise on every channel: a plain 5.1 track.
fn noise(channels: usize) -> Vec<Vec<i32>> {
    (0..channels)
        .map(|channel| {
            let mut lcg = Lcg(7 + channel as u32);
            (0..FRAMES).map(|_| lcg.next()).collect()
        })
        .collect()
}

#[derive(Clone, Copy)]
enum Header {
    /// Sizes as a seekable writer leaves them.
    Sized,
    /// Sizes as ffmpeg leaves them in a pipe.
    Placeholder,
}

/// `channels` as a 24-bit WAV; extensible with a mask when `mask` is given.
fn wav(channels: &[Vec<i32>], bits: u16, mask: Option<u32>, header: Header) -> Vec<u8> {
    let count = channels.len() as u16;
    let width = usize::from(bits / 8);
    let mut data: Vec<u8> = Vec::new();
    for frame in 0..channels[0].len() {
        for channel in channels {
            // 16-bit input is the top of the 24 bits, 32-bit input their
            // top as well.
            let value = if bits >= 24 {
                channel[frame] << (bits - 24)
            } else {
                channel[frame] >> (24 - bits)
            };
            data.extend(&value.to_le_bytes()[..width]);
        }
    }
    let mut fmt = Vec::new();
    let tag: u16 = if mask.is_some() { 0xFFFE } else { 1 };
    fmt.extend(tag.to_le_bytes());
    fmt.extend(count.to_le_bytes());
    fmt.extend(48_000u32.to_le_bytes());
    let align = count * (bits / 8);
    fmt.extend((48_000 * u32::from(align)).to_le_bytes());
    fmt.extend(align.to_le_bytes());
    fmt.extend(bits.to_le_bytes());
    if let Some(mask) = mask {
        fmt.extend(22u16.to_le_bytes());
        fmt.extend(bits.to_le_bytes());
        fmt.extend(mask.to_le_bytes());
        fmt.extend([
            1, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xAA, 0, 0x38, 0x9B, 0x71,
        ]);
    }
    let (riff_size, data_size) = match header {
        Header::Sized => (
            4 + 8 + fmt.len() as u32 + 8 + data.len() as u32,
            data.len() as u32,
        ),
        Header::Placeholder => (u32::MAX, u32::MAX),
    };
    let mut file = b"RIFF".to_vec();
    file.extend(riff_size.to_le_bytes());
    file.extend(b"WAVEfmt ");
    file.extend((fmt.len() as u32).to_le_bytes());
    file.extend(fmt);
    file.extend(b"data");
    file.extend(data_size.to_le_bytes());
    file.extend(data);
    file
}

fn harletty(args: &[&str], stdin: Option<Vec<u8>>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_harletty"))
        .args(["--loglevel", "off"])
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run harletty");
    let writer = stdin.map(|bytes| {
        let mut pipe = child.stdin.take().expect("stdin");
        // The reader may stop before the end (info does): a broken pipe is
        // not a failure of the test.
        std::thread::spawn(move || {
            let _ = pipe.write_all(&bytes);
        })
    });
    let output = child.wait_with_output().expect("wait for harletty");
    if let Some(writer) = writer {
        writer.join().expect("stdin writer");
    }
    output
}

fn info(args: &[&str], stdin: Vec<u8>) -> Value {
    let output = harletty(&[&["info", "--json"], args, &["-"]].concat(), Some(stdin));
    assert!(
        output.status.success(),
        "info failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("one JSON object on stdout")
}

fn out_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("pcm_auro_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn info_finds_the_carrier_in_a_piped_wav_and_stops() {
    let wav = wav(&carrier(), 24, Some(MASK_7_1), Header::Placeholder);
    // Detected by its header, then with the codec named.
    for args in [&[][..], &["--codec", "wav"][..]] {
        let report = info(args, wav.clone());
        assert_eq!(report["schema"], 1);
        assert_eq!(report["codec"], "PCM");
        assert_eq!(report["channels"], 8);
        assert_eq!(report["sample_rate"], 48_000);
        assert_eq!(report["spatial"]["label"], "Auro-3D-13.1");
        assert_eq!(report["spatial"]["kind"], "auro");
        assert_eq!(report["auro"]["carrier"], "7.1");
        assert_eq!(report["auro"]["original"], "7.1_5H_1T");
        // Three blocks latch it: a few chunks, not the 20 blocks of the file.
        assert!(report["frames_seen"].as_u64().unwrap() < FRAMES as u64);
    }
}

#[test]
fn info_calls_a_plain_track_plain_after_two_seconds() {
    // Four seconds of noise.
    let channels: Vec<Vec<i32>> = noise(6)
        .into_iter()
        .map(|c| c.iter().cycle().take(4 * 48_000).copied().collect())
        .collect();
    let report = info(&[], wav(&channels, 24, None, Header::Placeholder));
    assert_eq!(report["codec"], "PCM");
    assert_eq!(report["channels"], 6);
    assert_eq!(report["spatial"], Value::Null);
    assert!(report.get("auro").is_none());
    let seconds = report["seconds_seen"].as_f64().unwrap();
    assert!((2.0..2.2).contains(&seconds), "{seconds} s");
}

#[test]
fn other_bit_depths_are_read_and_are_plain() {
    for bits in [16, 32] {
        let report = info(&[], wav(&noise(8), bits, Some(MASK_7_1), Header::Sized));
        assert_eq!(report["codec"], "PCM", "{bits}-bit");
        assert_eq!(report["channels"], 8);
        assert_eq!(report["spatial"], Value::Null);
    }
}

#[test]
fn decode_writes_the_master_set_of_a_carrier() {
    let carrier = carrier();
    for (name, header, mask) in [
        ("placeholder", Header::Placeholder, Some(MASK_7_1)),
        // No mask: the default order of eight channels is the same.
        ("sized", Header::Sized, None),
    ] {
        let dir = out_dir(name);
        let base = dir.join("out");
        let output = harletty(
            &[
                "decode",
                "--bed-conform",
                "--output-path",
                base.to_str().unwrap(),
                "-",
            ],
            Some(wav(&carrier, 24, mask, header)),
        );
        assert!(
            output.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let presentation = std::fs::read_to_string(dir.join("out.atmos")).unwrap();
        assert!(
            presentation.contains("    sourceCodec: Auro-3D-13.1\n"),
            "{presentation}"
        );
        // A 7.1 bed, and the four corner heights, the centre height and the
        // top as objects.
        assert!(presentation.contains("scBedConfiguration: [0, 1, 2, 3, 4, 5, 6, 7]"));
        assert_eq!(
            presentation.matches("      - ID: 1").count(),
            6,
            "{presentation}"
        );
        assert!(dir.join("out.atmos.metadata").is_file());
        // Every sample of the input comes out: 14 channels of 24 bits, and
        // a CAF header.
        let audio = std::fs::metadata(dir.join("out.atmos.audio"))
            .unwrap()
            .len();
        let payload = (FRAMES * 14 * 3) as u64;
        assert!(
            (payload..payload + 256).contains(&audio),
            "{name}: {audio} bytes"
        );
    }
}

#[test]
fn decode_refuses_pcm_without_auro_3d() {
    for (channels, bits) in [(6, 24), (8, 24), (2, 24), (6, 16)] {
        let dir = out_dir("plain");
        let base = dir.join("out");
        let output = harletty(
            &["decode", "--output-path", base.to_str().unwrap(), "-"],
            Some(wav(&noise(channels), bits, None, Header::Placeholder)),
        );
        assert_eq!(output.status.code(), Some(1), "{channels} ch, {bits}-bit");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("no Auro-3D found in the PCM input"),
            "{channels} ch, {bits}-bit: {stderr}"
        );
        assert!(!dir.join("out.atmos").exists());
    }
}
