// SPDX-License-Identifier: Apache-2.0
//! `decode` and `info` on IAMF through the built binary (`iamf` feature).
//!
//! The libiamf conformance vectors (AOMediaCodec/libiamf, `tests/`) are not
//! committed: point `HARLETTY_IAMF_VECTORS` at a directory holding them, as
//! for the bridge's IAMF tests. Each test self-skips, loudly, otherwise.
#![cfg(feature = "iamf")]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn vector(name: &str) -> Option<PathBuf> {
    let dir = std::env::var_os("HARLETTY_IAMF_VECTORS")?;
    let path = Path::new(&dir).join(name);
    path.exists().then_some(path)
}

macro_rules! vector_or_skip {
    ($name:expr) => {
        match vector($name) {
            Some(path) => path,
            None => {
                eprintln!("skipping: set HARLETTY_IAMF_VECTORS to the libiamf test vectors");
                return;
            }
        }
    };
}

fn out_base(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(test);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("out")
}

fn decode(input: &Path, base: &Path, extra: &[&str]) {
    let status = Command::new(env!("CARGO_BIN_EXE_harletty"))
        .args(["--loglevel", "error", "decode"])
        .arg(input)
        .arg("--output-path")
        .arg(base)
        .args(extra)
        .status()
        .expect("run harletty");
    assert!(status.success(), "harletty decode failed: {status}");
}

fn sibling(base: &Path, suffix: &str) -> PathBuf {
    base.with_file_name(format!(
        "{}.{suffix}",
        base.file_name().unwrap().to_string_lossy()
    ))
}

/// The interleaved samples of a 24-bit CAF and its channel count.
fn read_caf(path: &Path) -> (usize, Vec<i32>) {
    let bytes = std::fs::read(path).unwrap();
    assert_eq!(&bytes[..4], b"caff");
    let mut at = 8;
    let (mut channels, mut little_endian) = (0usize, false);
    loop {
        let kind = &bytes[at..at + 4];
        let size = i64::from_be_bytes(bytes[at + 4..at + 12].try_into().unwrap());
        let body = at + 12;
        if kind == b"desc" {
            let flags = u32::from_be_bytes(bytes[body + 12..body + 16].try_into().unwrap());
            channels = u32::from_be_bytes(bytes[body + 24..body + 28].try_into().unwrap()) as usize;
            let bits = u32::from_be_bytes(bytes[body + 28..body + 32].try_into().unwrap());
            assert_eq!(bits, 24);
            little_endian = flags & 2 != 0;
        }
        if kind == b"data" {
            let samples = bytes[body + 4..]
                .chunks_exact(3)
                .map(|b| {
                    let b = if little_endian {
                        [b[2], b[1], b[0]]
                    } else {
                        [b[0], b[1], b[2]]
                    };
                    i32::from_be_bytes([b[0], b[1], b[2], 0]) >> 8
                })
                .collect();
            return (channels, samples);
        }
        at = body + size as usize;
    }
}

/// 16-bit PCM WAV samples, interleaved, at the 24-bit scale.
fn read_wav_s16(path: &Path) -> (usize, Vec<i32>) {
    let bytes = std::fs::read(path).unwrap();
    let mut pos = 12;
    let mut channels = 0;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = &bytes[pos + 8..pos + 8 + len];
        if id == b"fmt " {
            channels = usize::from(u16::from_le_bytes([body[2], body[3]]));
            assert_eq!(u16::from_le_bytes([body[14], body[15]]), 16, "16-bit only");
        } else if id == b"data" {
            let samples = body
                .chunks_exact(2)
                .map(|b| i32::from(i16::from_le_bytes([b[0], b[1]])) << 8)
                .collect();
            return (channels, samples);
        }
        pos += 8 + len + (len & 1);
    }
    panic!("no data chunk in {}", path.display());
}

/// A position event: ID, samplePos, pos, rampLength when stated.
type PositionEvent = (u32, u64, [f64; 3], Option<u32>);

/// An event being read: its position may not be stated.
type ReadEvent = (u32, u64, Option<[f64; 3]>, Option<u32>);

/// The position events of a metadata file.
fn events(path: &Path) -> Vec<PositionEvent> {
    let text = std::fs::read_to_string(path).unwrap();
    let mut out = Vec::new();
    let mut current: Option<ReadEvent> = None;
    let mut flush = |event: Option<ReadEvent>| {
        if let Some((id, at, Some(pos), ramp)) = event {
            out.push((id, at, pos, ramp));
        }
    };
    for line in text.lines().map(str::trim) {
        if let Some(id) = line.strip_prefix("- ID: ") {
            flush(current.take());
            current = Some((id.parse().unwrap(), 0, None, None));
        } else if let Some(event) = &mut current {
            if let Some(at) = line.strip_prefix("samplePos: ") {
                event.1 = at.parse().unwrap();
            } else if let Some(ramp) = line.strip_prefix("rampLength: ") {
                event.3 = Some(ramp.parse().unwrap());
            } else if let Some(pos) = line.strip_prefix("pos: ") {
                let v: Vec<f64> = pos
                    .trim_matches(['[', ']'])
                    .split(',')
                    .map(|v| v.trim().parse().unwrap())
                    .collect();
                event.2 = Some([v[0], v[1], v[2]]);
            }
        }
    }
    flush(current);
    out
}

/// Where object `id` is at sample `at`, as the events state it: each ramps
/// from where the object is to its position over its ramp.
fn position_at(events: &[PositionEvent], id: u32, at: u64) -> [f64; 3] {
    let mut from = [0.0; 3];
    let mut current: Option<(u64, u32, [f64; 3], [f64; 3])> = None;
    let mut ramp = 0;
    for &(_, start, pos, event_ramp) in events.iter().filter(|e| e.0 == id) {
        if start > at {
            break;
        }
        ramp = event_ramp.unwrap_or(ramp);
        if let Some(previous) = current {
            from = interpolate(previous, start);
        } else {
            from = pos;
        }
        current = Some((start, ramp, from, pos));
    }
    current.map_or(from, |state| interpolate(state, at))
}

fn interpolate((start, ramp, from, to): (u64, u32, [f64; 3], [f64; 3]), at: u64) -> [f64; 3] {
    if ramp == 0 || at >= start + u64::from(ramp) {
        return to;
    }
    let along = (at - start) as f64 / f64::from(ramp);
    std::array::from_fn(|axis| from[axis] + (to[axis] - from[axis]) * along)
}

fn close(a: [f64; 3], b: [f64; 3]) -> bool {
    (0..3).all(|i| (a[i] - b[i]).abs() < 1e-6)
}

#[test]
fn a_714_bed_decodes_bit_exact_into_a_bed_only_master_set() {
    // A 7.1.4 LPCM stream with demixing parameter blocks and no temporal
    // delimiters; its second declared layout is System J, what the bed is.
    let stream = vector_or_skip!("test_000082.iamf");
    let reference = vector_or_skip!("test_000082_rendered_id_42_sub_mix_0_layout_1.wav");
    let base = out_base("iamf_714");
    decode(&stream, &base, &[]);

    let header = std::fs::read_to_string(sibling(&base, "atmos")).unwrap();
    assert!(header.contains("sourceCodec: IAMF\n"), "{header}");
    assert!(header.contains("scBedConfiguration: [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 12, 13]\n"));
    assert!(header.contains("objects: []\n"), "{header}");
    let metadata = std::fs::read_to_string(sibling(&base, "atmos.metadata")).unwrap();
    assert_eq!(metadata, "sampleRate: 48000\nevents: []\n");

    let (channels, ours) = read_caf(&sibling(&base, "atmos.audio"));
    let (reference_channels, expected) = read_wav_s16(&reference);
    assert_eq!((channels, reference_channels), (12, 12));
    assert_eq!(ours, expected);

    // Piped in, the same master set.
    let piped = out_base("iamf_714_piped");
    let mut child = Command::new(env!("CARGO_BIN_EXE_harletty"))
        .args(["--loglevel", "error", "decode", "-", "--output-path"])
        .arg(&piped)
        .stdin(Stdio::piped())
        .spawn()
        .expect("run harletty");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&std::fs::read(&stream).unwrap())
        .unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(
        std::fs::read(sibling(&piped, "atmos.audio")).unwrap(),
        std::fs::read(sibling(&base, "atmos.audio")).unwrap()
    );
}

#[test]
fn bed_conform_keeps_the_heights_as_static_objects() {
    let stream = vector_or_skip!("test_000082.iamf");
    let base = out_base("iamf_714_conform");
    decode(&stream, &base, &["--bed-conform"]);
    let header = std::fs::read_to_string(sibling(&base, "atmos")).unwrap();
    assert!(
        header.contains("scBedConfiguration: [0, 1, 2, 3, 4, 5, 6, 7]\n"),
        "{header}"
    );
    assert!(header.contains("- ID: 13\n"), "{header}");
    let events = events(&sibling(&base, "atmos.metadata"));
    let corners = [
        [-1.0, 1.0, 1.0],
        [1.0, 1.0, 1.0],
        [-1.0, -1.0, 1.0],
        [1.0, -1.0, 1.0],
    ];
    assert_eq!(events.len(), 4, "static: stated once");
    for (event, corner) in events.iter().zip(corners) {
        assert_eq!(event.2, corner);
    }
    // The same samples as the plain bed: the four heights moved from the
    // bed's end to the objects', which is the same columns here.
    let plain = out_base("iamf_714_not_conformed");
    decode(&stream, &plain, &[]);
    let (channels, conformed) = read_caf(&sibling(&base, "atmos.audio"));
    assert_eq!(channels, 12);
    assert_eq!(conformed, read_caf(&sibling(&plain, "atmos.audio")).1);
}

#[test]
fn a_moving_polar_object_is_followed_by_its_events() {
    // IAMF v2.0 base-advanced: one polar object, at the front, then
    // inter-linear to the left, the rear and the right over 1024-sample
    // units, then the front again.
    let stream = vector_or_skip!("test_000800.iamf");
    let base = out_base("iamf_object");
    decode(&stream, &base, &[]);
    let header = std::fs::read_to_string(sibling(&base, "atmos")).unwrap();
    assert!(header.contains("bedInstances: []\n"), "{header}");
    assert!(header.contains("objects:\n      - ID: 10\n"), "{header}");

    let events = events(&sibling(&base, "atmos.metadata"));
    let h = std::f64::consts::FRAC_1_SQRT_2;
    assert!(close(position_at(&events, 10, 0), [0.0, 1.0, 0.0]));
    // Halfway to the left: azimuth +45 (IAMF, positive left) is x < 0.
    let at = position_at(&events, 10, 1536);
    assert!(close(at, [-h, h, 0.0]), "{at:?}");
    assert!(close(position_at(&events, 10, 2048), [-1.0, 0.0, 0.0]));
    assert!(close(position_at(&events, 10, 3072), [0.0, -1.0, 0.0]));
    // A polar line is an arc: followed through the positions the decoder
    // evaluates every 256 samples, each a ramp over them. The first event
    // states where the object starts, and the default past the blocks is
    // where the stream puts the object at once, not a move: a jump.
    assert_eq!(events[0].3, Some(0));
    let last = events.last().unwrap();
    assert_eq!((last.1, last.3), (4096, Some(0)), "{last:?}");
    assert!(events[1..events.len() - 1].iter().all(|e| e.3 == Some(256)));
    let (channels, _) = read_caf(&sibling(&base, "atmos.audio"));
    assert_eq!(channels, 1);
}

#[test]
fn a_bed_and_static_objects_make_one_event_each() {
    // IAMF v2.0 advanced-1: a 5.1 element and four static polar objects.
    let stream = vector_or_skip!("test_000903.iamf");
    let base = out_base("iamf_mixed");
    decode(&stream, &base, &[]);
    let header = std::fs::read_to_string(sibling(&base, "atmos")).unwrap();
    // 5.1's surrounds sit at ±110°: System J renders them to its rears.
    assert!(
        header.contains("scBedConfiguration: [0, 1, 2, 3, 6, 7]\n"),
        "{header}"
    );
    assert!(header.contains("- ID: 13\n"), "{header}");
    let events = events(&sibling(&base, "atmos.metadata"));
    assert_eq!(events.len(), 4);
    // Azimuth 1, elevation 2, distance 3/127: just left of the front, close
    // to the listener.
    let d = 3.0 / 127.0;
    let (az, el) = (1f64.to_radians(), 2f64.to_radians());
    let expected = [
        d * el.cos() * (-az).sin(),
        d * el.cos() * az.cos(),
        d * el.sin(),
    ];
    for event in &events {
        assert!(
            (0..3).all(|i| (event.2[i] - expected[i]).abs() < 1e-6),
            "{event:?}"
        );
    }
    let (channels, _) = read_caf(&sibling(&base, "atmos.audio"));
    assert_eq!(channels, 10);

    let output = Command::new(env!("CARGO_BIN_EXE_harletty"))
        .args(["--loglevel", "off", "info", "--json"])
        .arg(&stream)
        .output()
        .expect("run harletty");
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["codec"], "IAMF");
    assert_eq!(report["channels"], 6);
    assert_eq!(report["sample_rate"], 48_000);
    assert_eq!(report["spatial"]["label"], "IAMF");
    assert_eq!(report["spatial"]["kind"], "iamf");
    assert_eq!(report["spatial"]["objects"], 4);
    assert_eq!(report["iamf"]["profile"], "advanced-1");
    assert_eq!(report["iamf"]["codec"], "PCM");
    assert_eq!(report["iamf"]["elements"], 5);
    assert_eq!(report["iamf"]["objects"], 4);
}
