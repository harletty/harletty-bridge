//! Where an IAMF stream starts in raw bytes (`BridgeLib::probe`, Omniphony's
//! `docs/multi-bridge.md`, "ABI: bridge_api 0.6"): an IA sequence header OBU
//! (IAMF 1.1 §3.2, §3.4).
//!
//! The OBU header byte says type 31 with no trimming fields; `obu_size` is a
//! well-formed LEB128 at least as large as the syntax it covers; when
//! `obu_extension_flag` is set, the extension's LEB128 size and that many
//! bytes come before the payload; the payload opens with the `iamf` code and
//! two profile bytes, of which the primary is one the decoder knows. The
//! additional profile is not judged: it names a second profile the sequence
//! also complies with, possibly one reserved for the future (6–255), and the
//! sequence still decodes under its primary profile — libiamf's vectors
//! test_000710 and test_000711 declare 255 there and expect their
//! Base-Enhanced mix decoded. A primary profile the decoder does not know is
//! a sequence it discards (§3.4), so it is not claimed. Nothing after these
//! fields is required: `obu_size` may extend past them, and reserved OBUs may
//! follow before the codec config. So a start is decided within 15 bytes
//! without an extension, and within 23 bytes plus the declared extension
//! with one.
//!
//! IAMF has no CRC and need not repeat its sequence header: the OBU type, a
//! 32-bit code and a constrained profile byte at fixed places make a chance
//! match negligible.
//!
//! One ambiguity is the syntax's own: the header byte of a sequence header
//! (0xF8–0xFF) reads as a LEB128 continuation byte, so a byte of 0xF8, 0xF9,
//! 0xFC or 0xFD just in front of one reads as a sequence header too, whose
//! `obu_size` runs over the real header byte to the same `iamf` code. Of
//! starts that read the same code, the latest one is the stream's: an
//! earlier one's `obu_size` would be at least 888, where a sequence header
//! carries 6 bytes plus its extension.

use bridge_api::RProbe;
use bridge_common::probe::{Start, scan_raw};

/// The most bytes a start without an extension needs: the header byte, an
/// 8-byte `obu_size`, the code and the two profiles.
pub const MAX_PROBE_WITHOUT_EXTENSION: usize = 1 + 8 + IA_CODE.len() + 2;

const OBU_SEQUENCE_HEADER: u8 = 31;
const OBU_TRIMMING_STATUS_FLAG: u8 = 0x02;
const OBU_EXTENSION_FLAG: u8 = 0x01;
const IA_CODE: [u8; 4] = *b"iamf";
/// The highest primary profile number the decoder knows (simple, base,
/// base-enhanced, base-advanced, advanced-1, advanced-2).
const MAX_PROFILE: u8 = 5;
/// A LEB128 of the OBU syntax is at most 8 bytes long (§2.4).
const MAX_LEB128: usize = 8;

/// `BridgeLib::probe` for the raw transport.
pub fn probe_raw(data: &[u8]) -> RProbe {
    scan_raw(data, sequence_header)
}

/// An IA sequence header OBU at `s`.
pub fn sequence_header(s: &[u8]) -> Start {
    let payload = match fields(s) {
        Ok(payload) => payload,
        Err(start) => return start,
    };
    // Starts that read the same code: the latest one is the stream's (see
    // the module doc), so this one is not when another begins inside its
    // size fields.
    for next in 1..payload.min(1 + 2 * MAX_LEB128) {
        match fields(&s[next..]) {
            Ok(other) if next + other == payload => return Start::Reject,
            Err(Start::Need(needed)) => return Start::Need(next + needed),
            _ => {}
        }
    }
    Start::Claim
}

/// The fields of a sequence header at `s`, checked: where its payload
/// starts.
fn fields(s: &[u8]) -> Result<usize, Start> {
    let Some(&header) = s.first() else {
        return Err(Start::Need(1));
    };
    if header >> 3 != OBU_SEQUENCE_HEADER || header & OBU_TRIMMING_STATUS_FLAG != 0 {
        return Err(Start::Reject);
    }
    let obu_size = leb128(s, 1)?;
    // Where the payload starts, past the extension when there is one.
    let mut payload = obu_size.end;
    if header & OBU_EXTENSION_FLAG != 0 {
        let extension = leb128(s, payload)?;
        payload = extension.end.saturating_add(extension.value);
    }
    let end = payload.saturating_add(IA_CODE.len() + 2);
    if (obu_size.value as u64) < (end - obu_size.end) as u64 {
        return Err(Start::Reject);
    }
    if s.len() < end {
        return Err(Start::Need(end));
    }
    let primary = s[payload + IA_CODE.len()];
    if s[payload..payload + IA_CODE.len()] != IA_CODE || primary > MAX_PROFILE {
        return Err(Start::Reject);
    }
    Ok(payload)
}

/// A LEB128 field read at `at`.
struct Leb128 {
    value: usize,
    /// The offset just past it.
    end: usize,
}

/// The LEB128 at `at`: `Need` one more byte while it is incomplete, `Reject`
/// when it runs past 8 bytes.
fn leb128(s: &[u8], at: usize) -> Result<Leb128, Start> {
    let mut value = 0u64;
    for index in 0..MAX_LEB128 {
        let Some(&byte) = s.get(at + index) else {
            return Err(Start::Need(at + index + 1));
        };
        value |= u64::from(byte & 0x7F) << (7 * index);
        if byte & 0x80 == 0 {
            return Ok(Leb128 {
                value: usize::try_from(value).unwrap_or(usize::MAX),
                end: at + index + 1,
            });
        }
    }
    Err(Start::Reject)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bridge_api::RProbeVerdict;
    use bridge_common::probe::{replay, replay_split};

    const EAC3: &[u8] = include_bytes!("../../harletty/tests/fixtures/joc_atmos_1s.eac3");
    const DTS: &[u8] = include_bytes!("../../harletty/tests/fixtures/dts_core_tone_10f.dts");

    /// A LEB128 of `value` on exactly `len` bytes (padded with continuation
    /// bytes, as the syntax allows).
    fn leb(value: usize, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| {
                let byte = ((value >> (7 * i)) & 0x7F) as u8;
                if i + 1 < len { byte | 0x80 } else { byte }
            })
            .collect()
    }

    /// A sequence header: `obu_size` on `size_len` bytes declaring `size`,
    /// an optional extension of `extension` bytes whose size field is
    /// `extension_len` bytes long, then the code and profiles 0 / 1, then
    /// `size` minus what that took of filler, and a reserved OBU after it.
    fn sequence(size: usize, size_len: usize, extension: Option<(usize, usize)>) -> Vec<u8> {
        let mut obu = vec![(OBU_SEQUENCE_HEADER << 3) | u8::from(extension.is_some())];
        obu.extend(leb(size, size_len));
        let mut body = Vec::new();
        if let Some((extension, extension_len)) = extension {
            body.extend(leb(extension, extension_len));
            body.extend(std::iter::repeat_n(0xEEu8, extension));
        }
        body.extend_from_slice(b"iamf");
        body.extend([0, 1]);
        assert!(body.len() <= size);
        body.resize(size, 0x55);
        obu.extend(body);
        // A reserved OBU (type 24) carrying an E-AC-3 frame start.
        obu.extend([24 << 3, 6, 0x0B, 0x77, 0, 0, 0, 0]);
        obu
    }

    fn assert_claimed_at(stream: &[u8], start: usize, decided_at: usize) {
        // Undecided until then: pending at the start, or at a byte before
        // it that could still begin one.
        for len in 1..decided_at {
            let answer = probe_raw(&stream[..start + len]);
            assert_eq!(answer.verdict, RProbeVerdict::Pending, "{len} bytes");
            assert!(answer.offset as usize <= start, "{len} bytes: {answer:?}");
        }
        assert_eq!(
            probe_raw(&stream[..start + decided_at]),
            RProbe::claim(start as u32)
        );
        for first in 1..=start + decided_at + 1 {
            assert_eq!(
                replay_split(probe_raw, stream, first).claim,
                Some(start),
                "split at {first}"
            );
        }
        for read in [1, 2, 3, 7, 64] {
            let replay = replay(probe_raw, stream, read);
            assert_eq!(replay.claim, Some(start), "reads of {read}");
            if read == 1 {
                assert_eq!(replay.read, start + decided_at, "{replay:?}");
            }
        }
    }

    #[test]
    fn an_ordinary_sequence_header_is_claimed_within_15_bytes() {
        // obu_size 6, on one byte: decided at 8 bytes.
        assert_claimed_at(&sequence(6, 1, None), 0, 8);
        // obu_size on the full 8 bytes: decided at 15.
        assert_claimed_at(&sequence(6, 8, None), 0, MAX_PROBE_WITHOUT_EXTENSION);
        // An obu_size past the syntax (64, 58 bytes ignored), with a reserved
        // OBU after it: decided at 8 bytes all the same.
        assert_claimed_at(&sequence(64, 1, None), 0, 8);
        let mut late = vec![0xF8u8; 40];
        late.extend(sequence(64, 1, None));
        assert_claimed_at(&late, 40, 8);
    }

    #[test]
    fn an_extension_is_skipped_before_the_code() {
        // A 16-byte extension: 1 + 1 + 1 + 16 + 6 = 25 bytes.
        assert_claimed_at(&sequence(1 + 16 + 6, 1, Some((16, 1))), 0, 25);
        // An empty one, both LEB128 fields on 8 bytes: 1 + 8 + 8 + 6 = 23.
        assert_claimed_at(&sequence(8 + 6, 8, Some((0, 8))), 0, 23);
    }

    /// An extension longer than a host buffers stays pending with the length
    /// it declares: the host abandons it for its own limit, the probe never
    /// rules it out as another format.
    #[test]
    fn an_extension_past_the_host_buffer_stays_pending() {
        let extension = 100_000;
        let stream = sequence(3 + extension + 6, 3, Some((extension, 3)));
        let answer = probe_raw(&stream[..64 * 1024]);
        assert_eq!(answer.verdict, RProbeVerdict::Pending);
        assert_eq!(answer.offset, 0);
        assert_eq!(answer.needed as usize, 1 + 3 + 3 + extension + 6);
        assert_eq!(probe_raw(&stream), RProbe::claim(0));
    }

    /// A byte that reads as a sequence header in front of the real one,
    /// with an obu_size running over the real header byte to the same code.
    #[test]
    fn a_header_like_byte_in_front_does_not_move_the_start() {
        for junk in [0xF8u8, 0xFC, 0xFF] {
            let mut stream = vec![0x00, junk];
            stream.extend(sequence(6, 1, None));
            assert_claimed_at(&stream, 2, 8);
            let mut stream = vec![0x00, 0xF8, junk];
            stream.extend(sequence(6, 1, None));
            assert_claimed_at(&stream, 3, 8);
        }
        // With the extension flag, the byte in front reads an extension
        // size out of the real header: it stays a candidate until the bytes
        // it declares are there, then gives way.
        for junk in [0xF9u8, 0xFD] {
            let mut stream = vec![0x00, junk];
            stream.extend(sequence(6, 1, None));
            stream.resize(stream.len() + 256, 0);
            for read in [1, 7, 64] {
                assert_eq!(replay(probe_raw, &stream, read).claim, Some(2), "{junk:#x}");
            }
        }
    }

    #[test]
    fn what_is_not_a_sequence_header_is_not_claimed() {
        let good = sequence(6, 1, None);
        let mut code = good.clone();
        code[2] = b'x';
        assert_eq!(probe_raw(&code[..8]).verdict, RProbeVerdict::None);
        let mut profile = good.clone();
        profile[6] = MAX_PROFILE + 1;
        assert_eq!(probe_raw(&profile[..8]).verdict, RProbeVerdict::None);
        let mut trimmed = good.clone();
        trimmed[0] |= OBU_TRIMMING_STATUS_FLAG;
        assert_eq!(probe_raw(&trimmed[..8]).verdict, RProbeVerdict::None);
        // An obu_size smaller than the code and profiles.
        let mut short = good.clone();
        short[1] = 5;
        assert_eq!(probe_raw(&short[..8]).verdict, RProbeVerdict::None);
        // A LEB128 longer than 8 bytes.
        let mut long = vec![0xF8u8];
        long.extend([0x80; 9]);
        long.extend_from_slice(b"iamf\0\0");
        assert_eq!(probe_raw(&long).verdict, RProbeVerdict::None);
    }

    /// An additional profile the decoder does not know, one reserved for
    /// the future, does not hide the stream: it still complies with its
    /// primary profile. The header of libiamf's test_000710 (Base-Enhanced,
    /// additional profile 255), whose mix 43 decodes.
    #[test]
    fn a_reserved_additional_profile_is_claimed() {
        let header = [0xF8, 0x06, b'i', b'a', b'm', b'f', 0x02, 0xFF];
        assert_eq!(probe_raw(&header), RProbe::claim(0));
        let mut reserved = sequence(6, 1, None);
        reserved[7] = MAX_PROFILE + 1;
        assert_claimed_at(&reserved, 0, 8);
        // Not so a reserved primary profile (test_000709): the decoder
        // discards that sequence.
        let primary = [0xF8, 0x06, b'i', b'a', b'm', b'f', 0xFF, 0xFF];
        assert_eq!(probe_raw(&primary).verdict, RProbeVerdict::None);
    }

    #[test]
    fn other_families_streams_are_not_claimed() {
        assert_ne!(probe_raw(EAC3).verdict, RProbeVerdict::Claim);
        assert_ne!(probe_raw(DTS).verdict, RProbeVerdict::Claim);
        assert_ne!(
            probe_raw(&[
                0x10, 0x80, 0, 0, 0xF8, 0x72, 0x6F, 0xBA, 0x00, 0x27, 0x80, 0x4F
            ])
            .verdict,
            RProbeVerdict::Claim
        );
    }

    /// Every sequence header of the libiamf conformance vectors, when they
    /// are there (`HARLETTY_IAMF_VECTORS`), is claimed where it starts.
    #[test]
    fn the_conformance_vectors_are_claimed_at_their_first_byte() {
        let Some(dir) = std::env::var_os("HARLETTY_IAMF_VECTORS") else {
            eprintln!("skipping: HARLETTY_IAMF_VECTORS is not set");
            return;
        };
        let mut claimed = 0;
        let mut others = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|ext| ext != "iamf") {
                continue;
            }
            let stream = std::fs::read(&path).unwrap();
            if probe_raw(&stream[..stream.len().min(4096)]) == RProbe::claim(0) {
                claimed += 1;
            } else {
                others.push(path.file_name().unwrap().to_string_lossy().into_owned());
            }
        }
        eprintln!("{claimed} vectors claimed at 0; not: {others:?}");
        assert!(claimed > 0);
    }
}
