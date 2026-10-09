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

/// The OBUs of a standalone stream, each with its bytes.
fn obus(data: &[u8]) -> Vec<(iamf_obu::ObuType, &[u8])> {
    let mut reader = iamf_obu::ByteReader::new(data);
    let mut out = Vec::new();
    while !reader.is_empty() {
        let start = reader.position();
        let obu = iamf_obu::Obu::parse(&mut reader).expect("a well-formed vector");
        out.push((obu.header.obu_type, &data[start..reader.position()]));
    }
    out
}

/// The object vector cut into what a splice needs: its descriptors, and
/// its temporal units, each the parameter blocks before an audio frame and
/// that frame (the vector has no temporal delimiters and one substream).
fn descriptors_and_units(obus: &[(iamf_obu::ObuType, &[u8])]) -> (Vec<u8>, Vec<Vec<Vec<u8>>>) {
    let descriptor = |kind: &iamf_obu::ObuType| {
        matches!(
            kind,
            iamf_obu::ObuType::SequenceHeader
                | iamf_obu::ObuType::CodecConfig
                | iamf_obu::ObuType::AudioElement
                | iamf_obu::ObuType::MixPresentation
        )
    };
    let first_unit = obus
        .iter()
        .position(|(kind, _)| !descriptor(kind))
        .expect("a temporal unit");
    let descriptors: Vec<u8> = obus[..first_unit]
        .iter()
        .flat_map(|(_, bytes)| bytes.iter().copied())
        .collect();
    let is_frame = |kind: &iamf_obu::ObuType| {
        matches!(
            kind,
            iamf_obu::ObuType::AudioFrame | iamf_obu::ObuType::AudioFrameId(_)
        )
    };
    let mut units: Vec<Vec<Vec<u8>>> = vec![Vec::new()];
    let mut closed = false;
    for &(kind, bytes) in &obus[first_unit..] {
        if closed {
            units.push(Vec::new());
        }
        units.last_mut().unwrap().push(bytes.to_vec());
        closed = is_frame(&kind);
    }
    assert!(units.len() > 6, "{} units", units.len());
    (descriptors, units)
}

fn is_parameter_block(obu: &[u8]) -> bool {
    obu[0] >> 3 == 3
}

fn leb128(mut v: u32) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return out;
        }
        out.push(byte | 0x80);
    }
}

/// An OBU of the vector (no trimming, no extension header) with its
/// payload replaced, trimmed at its start and end as an audio frame says.
fn rebuilt(obu: &[u8], trim: Option<(u32, u32)>, payload: &[u8]) -> Vec<u8> {
    assert_eq!(obu[0] & 0b11, 0, "an OBU without trimming or extension");
    let mut out = vec![obu[0]];
    let mut fields = Vec::new();
    if let Some((start, end)) = trim {
        out[0] |= 0b10;
        fields.extend(leb128(end));
        fields.extend(leb128(start));
    }
    out.extend(leb128((fields.len() + payload.len()) as u32));
    out.extend(fields);
    out.extend_from_slice(payload);
    out
}

/// The payload of an OBU of the vector (no trimming, no extension header).
fn payload_of(obu: &[u8]) -> &[u8] {
    assert_eq!(obu[0] & 0b11, 0, "an OBU without trimming or extension");
    let mut i = 1;
    while obu[i] & 0x80 != 0 {
        i += 1;
    }
    &obu[i + 1..]
}

/// The audio frame of a unit of the vector, trimmed by `start` samples at
/// its start and `end` at its end.
fn trimmed(unit: &[Vec<u8>], start: u32, end: u32) -> Vec<Vec<u8>> {
    unit.iter()
        .map(|obu| {
            if is_parameter_block(obu) {
                obu.clone()
            } else {
                rebuilt(obu, Some((start, end)), payload_of(obu))
            }
        })
        .collect()
}

/// Two IA sequences in one stream: the object vector's descriptors over
/// `first`, then the same descriptors again over `second`, whose parameter
/// blocks are dropped, so its object sits at the default, the front.
fn two_sequences(descriptors: &[u8], first: &[Vec<Vec<u8>>], second: &[Vec<Vec<u8>>]) -> Vec<u8> {
    let mut spliced = descriptors.to_vec();
    for unit in first {
        for obu in unit {
            spliced.extend_from_slice(obu);
        }
    }
    spliced.extend_from_slice(descriptors);
    for unit in second {
        for obu in unit {
            if !is_parameter_block(obu) {
                spliced.extend_from_slice(obu);
            }
        }
    }
    spliced
}

/// The object is at the front from `from` on, put there by a jump at
/// `from`, and nothing moves it after.
fn assert_front_from(events: &[PositionEvent], from: u64) {
    let at = position_at(events, 10, from);
    assert!(close(at, [0.0, 1.0, 0.0]), "{at:?}");
    let at = position_at(events, 10, from + 2000);
    assert!(close(at, [0.0, 1.0, 0.0]), "{at:?}");
    let jump = events
        .iter()
        .find(|e| e.0 == 10 && e.1 == from)
        .expect("an event at the second sequence's start");
    assert_eq!(jump.3, Some(0), "{jump:?}");
    assert!(
        events.iter().all(|e| e.1 <= from || e.0 != 10),
        "{events:?}"
    );
}

#[test]
fn a_new_sequence_puts_the_objects_where_it_evaluates_them() {
    // The first three units of the object vector (the object at the
    // front, then on its way to the left), then the same descriptors again
    // as a second IA sequence whose units carry no position block: the
    // object is at the second sequence's default, the front, from its
    // first sample on, though nothing moves it there.
    let stream = vector_or_skip!("test_000800.iamf");
    let data = std::fs::read(&stream).unwrap();
    let (descriptors, units) = descriptors_and_units(&obus(&data));
    let spliced = two_sequences(&descriptors, &units[..3], &units[3..6]);
    let base = out_base("iamf_two_sequences");
    let input = sibling(&base, "iamf");
    std::fs::write(&input, &spliced).unwrap();
    decode(&input, &base, &[]);

    let events = events(&sibling(&base, "atmos.metadata"));
    assert!(close(position_at(&events, 10, 2048), [-1.0, 0.0, 0.0]));
    // The second sequence starts at 3072.
    assert_front_from(&events, 3072);
}

#[test]
fn a_new_sequence_trimmed_away_at_first_still_puts_the_objects_where_it_evaluates_them() {
    // The same, the second sequence's first unit wholly trimmed away (a
    // codec's pre-skip longer than a unit): it has no sample and no
    // position, so the one after it, at 3072, is where the object is put
    // at the front.
    let stream = vector_or_skip!("test_000800.iamf");
    let data = std::fs::read(&stream).unwrap();
    let (descriptors, units) = descriptors_and_units(&obus(&data));
    let mut second = units[3..6].to_vec();
    second[0] = trimmed(&second[0], 1024, 0);
    let spliced = two_sequences(&descriptors, &units[..3], &second);
    let base = out_base("iamf_two_sequences_preskip");
    let input = sibling(&base, "iamf");
    std::fs::write(&input, &spliced).unwrap();
    decode(&input, &base, &[]);

    let events = events(&sibling(&base, "atmos.metadata"));
    assert!(close(position_at(&events, 10, 2048), [-1.0, 0.0, 0.0]));
    assert_front_from(&events, 3072);
}

#[test]
fn a_new_sequence_cuts_the_move_before_it_where_it_takes_over() {
    // The first sequence's third block brought back to the front (azimuth
    // 0 instead of 180: an arc from the left, where the second block left
    // the object, back to where the second sequence will put it), and its
    // frame trimmed by 512 samples at its end: the second sequence starts
    // at 2560, before the arc gets there. The arc ends where the stream
    // last evaluated it, at 2304, and the object jumps to the front at
    // 2560: no event ramps on into the second sequence's samples.
    let stream = vector_or_skip!("test_000800.iamf");
    let data = std::fs::read(&stream).unwrap();
    let (descriptors, units) = descriptors_and_units(&obus(&data));
    let mut first = units[..3].to_vec();
    first[2] = trimmed(&first[2], 0, 512);
    let block = first[2]
        .iter_mut()
        .find(|obu| is_parameter_block(obu))
        .expect("the third position block");
    let mut payload = payload_of(block).to_vec();
    // parameter_id, animation_type, azimuth, elevation, distance.
    assert_eq!(payload, [0x01, 0x03, 0x5a, 0x00, 0x7f]);
    payload[2] = 0;
    *block = rebuilt(block, None, &payload);
    let spliced = two_sequences(&descriptors, &first, &units[3..6]);
    let base = out_base("iamf_two_sequences_cut");
    let input = sibling(&base, "iamf");
    std::fs::write(&input, &spliced).unwrap();
    decode(&input, &base, &[]);

    let events = events(&sibling(&base, "atmos.metadata"));
    assert!(close(position_at(&events, 10, 2048), [-1.0, 0.0, 0.0]));
    // 256 samples into the arc from +90 to 0: azimuth 67.5.
    let az = 67.5f64.to_radians();
    let at = position_at(&events, 10, 2304);
    assert!(close(at, [-az.sin(), az.cos(), 0.0]), "{at:?}");
    assert_front_from(&events, 2560);
    for &(id, start, _, ramp) in &events {
        if id == 10 && start < 2560 {
            let end = start + u64::from(ramp.unwrap_or(0));
            assert!(end <= 2560, "{events:?}");
        }
    }
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
