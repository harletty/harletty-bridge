use crate::input::InputReader;
use anyhow::{Result, anyhow};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Codec {
    Auto,
    Truehd,
    Eac3,
    Dts,
    /// A standalone IAMF OBU stream (an IA sequence: its header, the
    /// descriptors, then temporal units). Decoded only by a build with the
    /// `iamf` feature.
    Iamf,
}

/// What `decode` and `info` say when asked for IAMF by a build without it.
pub const NO_IAMF: &str = "this build has no IAMF decoder; build with --features iamf";

const PROBE_BUFFER_SIZE: usize = 8 * 1024;

const TRUEHD_SYNC_BE: [u8; 4] = [0xF8, 0x72, 0x6F, 0xBA];
const TRUEHD_SYNC_FBB_BE: [u8; 4] = [0xF8, 0x72, 0x6F, 0xBB];
const EAC3_SYNC_BE: [u8; 2] = [0x0B, 0x77];
/// DTS core substream. Same constants the bridge's DTS pipeline scans for.
const DTS_CORE_SYNC_BE: [u8; 4] = [0x7F, 0xFE, 0x80, 0x01];
/// DTS extension substream — what a DTS-HD MA / DTS:X stream opens with when
/// there is no backward-compatible core ahead of it.
const DTS_SUBSTREAM_SYNC_BE: [u8; 4] = [0x64, 0x58, 0x20, 0x25];

/// A sync word that identifies a codec, in the order candidates are weighed.
struct SyncCandidate {
    codec: Codec,
    pattern: &'static [u8],
}

const SYNC_CANDIDATES: &[SyncCandidate] = &[
    SyncCandidate { codec: Codec::Truehd, pattern: &TRUEHD_SYNC_BE },
    SyncCandidate { codec: Codec::Truehd, pattern: &TRUEHD_SYNC_FBB_BE },
    SyncCandidate { codec: Codec::Dts, pattern: &DTS_CORE_SYNC_BE },
    SyncCandidate { codec: Codec::Dts, pattern: &DTS_SUBSTREAM_SYNC_BE },
    SyncCandidate { codec: Codec::Eac3, pattern: &EAC3_SYNC_BE },
];

/// An IA sequence header OBU (IAMF §3.5): OBU type 31 in the top five bits
/// of the header byte, a leb128 size (and an extension, when the header says
/// so), then the `iamf` code, the primary and the additional profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IamfSequenceHeader {
    /// Where the OBU starts.
    pub offset: usize,
    pub primary_profile: u8,
    pub additional_profile: u8,
}

/// The name of an IAMF profile number (§3.5), as Atmos Ranker shows it
/// after `IAMF `.
pub fn iamf_profile_name(profile: u8) -> &'static str {
    match profile {
        0 => "simple",
        1 => "base",
        2 => "base-enhanced",
        3 => "base-advanced",
        4 => "advanced-1",
        5 => "advanced-2",
        _ => "unknown",
    }
}

/// Bytes of a sequence header OBU without an extension: header byte, a
/// one-byte size, `iamf` and the two profiles. What it weighs against the
/// sync words of the other codecs.
const IAMF_SEQUENCE_HEADER_LEN: usize = 8;

/// The first IA sequence header in `buffer`.
pub fn find_iamf_sequence_header(buffer: &[u8]) -> Option<IamfSequenceHeader> {
    (0..buffer.len()).find_map(|offset| iamf_sequence_header_at(buffer, offset))
}

fn iamf_sequence_header_at(buffer: &[u8], offset: usize) -> Option<IamfSequenceHeader> {
    let header = *buffer.get(offset)?;
    // Type 31; a sequence header is never trimmed.
    if header >> 3 != 31 || header & 0x02 != 0 {
        return None;
    }
    let mut at = offset + 1;
    let size = read_leb128(buffer, &mut at)?;
    if header & 0x01 != 0 {
        let extension = read_leb128(buffer, &mut at)?;
        at = at.checked_add(usize::try_from(extension).ok()?)?;
    }
    if size < 6 || buffer.get(at..at + 4)? != b"iamf" {
        return None;
    }
    Some(IamfSequenceHeader {
        offset,
        primary_profile: *buffer.get(at + 4)?,
        additional_profile: *buffer.get(at + 5)?,
    })
}

/// A leb128 of at most eight bytes (IAMF §2.4) at `*at`, advancing past it.
fn read_leb128(buffer: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for i in 0..8 {
        let byte = *buffer.get(*at)?;
        *at += 1;
        value |= u64::from(byte & 0x7F) << (7 * i);
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

pub fn probe_codec(reader: &mut InputReader, hint: Codec) -> Result<(Codec, Vec<u8>)> {
    if hint != Codec::Auto {
        return Ok((hint, Vec::new()));
    }

    let mut buffer = vec![0u8; PROBE_BUFFER_SIZE];
    let mut filled = 0usize;
    while filled < buffer.len() {
        let n = reader.read_chunk(&mut buffer[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    buffer.truncate(filled);

    let codec = detect_codec(&buffer).ok_or_else(|| {
        anyhow!(
            "no TrueHD (0xF8726FBA), EAC3 (0x0B77) or DTS (0x7FFE8001 / 0x64582025) sync word \
             nor IA sequence header found in first {} bytes; pass --codec explicitly",
            buffer.len()
        )
    })?;

    // Only pipes need the consumed prefix replayed; file paths are re-opened from 0.
    let prefix = if reader.is_pipe() { buffer } else { Vec::new() };
    Ok((codec, prefix))
}

/// Picks the codec whose sync word appears earliest in the probe buffer.
///
/// Ties go to the longer pattern. That matters because E-AC-3's sync word is
/// only two bytes (0x0B77) and turns up by chance in dense binary payloads,
/// while every other candidate here is four bytes; without the tie-break a
/// coincidental 0x0B77 could outrank a real DTS or TrueHD sync sitting at the
/// same offset. Streams in practice start on a sync word, so the earliest match
/// is the true one.
///
/// An IA sequence header weighs in the same way, as an eight-byte pattern:
/// its first byte is TrueHD's, but not the three after it.
fn detect_codec(buffer: &[u8]) -> Option<Codec> {
    let iamf = find_iamf_sequence_header(buffer).map(|header| {
        (
            Codec::Iamf,
            sort_key(header.offset, IAMF_SEQUENCE_HEADER_LEN),
        )
    });
    SYNC_CANDIDATES
        .iter()
        .filter_map(|candidate| {
            find_pattern(buffer, candidate.pattern)
                .map(|offset| (candidate.codec, sort_key(offset, candidate.pattern.len())))
        })
        .chain(iamf)
        .min_by_key(|(_, key)| *key)
        .map(|(codec, _)| codec)
}

/// Earliest offset first, then longest pattern first.
fn sort_key(offset: usize, pattern_len: usize) -> (usize, std::cmp::Reverse<usize>) {
    (offset, std::cmp::Reverse(pattern_len))
}

fn find_pattern(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

pub fn describe_codec(codec: Codec) -> &'static str {
    match codec {
        Codec::Auto => "auto",
        Codec::Truehd => "TrueHD",
        Codec::Eac3 => "EAC3",
        Codec::Dts => "DTS",
        Codec::Iamf => "IAMF",
    }
}

#[allow(dead_code)]
pub(crate) fn ensure_resolved(codec: Codec) -> Result<Codec> {
    if codec == Codec::Auto {
        Err(anyhow!("codec auto-detection did not resolve to a concrete codec"))
    } else {
        Ok(codec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_truehd_first() {
        let mut buf = vec![0u8; 64];
        buf[10..14].copy_from_slice(&TRUEHD_SYNC_BE);
        buf[40..42].copy_from_slice(&EAC3_SYNC_BE);
        assert_eq!(
            find_pattern(&buf, &TRUEHD_SYNC_BE),
            Some(10)
        );
        assert_eq!(find_pattern(&buf, &EAC3_SYNC_BE), Some(40));
    }

    #[test]
    fn finds_eac3_first() {
        let mut buf = vec![0u8; 64];
        buf[5..7].copy_from_slice(&EAC3_SYNC_BE);
        buf[20..24].copy_from_slice(&TRUEHD_SYNC_BE);
        assert_eq!(find_pattern(&buf, &EAC3_SYNC_BE), Some(5));
        assert_eq!(find_pattern(&buf, &TRUEHD_SYNC_BE), Some(20));
    }

    #[test]
    fn returns_none_when_absent() {
        let buf = vec![0u8; 128];
        assert_eq!(find_pattern(&buf, &TRUEHD_SYNC_BE), None);
        assert_eq!(find_pattern(&buf, &EAC3_SYNC_BE), None);
    }

    #[test]
    fn detects_each_codec_alone() {
        for (pattern, expected) in [
            (&TRUEHD_SYNC_BE[..], Codec::Truehd),
            (&TRUEHD_SYNC_FBB_BE[..], Codec::Truehd),
            (&DTS_CORE_SYNC_BE[..], Codec::Dts),
            (&DTS_SUBSTREAM_SYNC_BE[..], Codec::Dts),
            (&EAC3_SYNC_BE[..], Codec::Eac3),
        ] {
            let mut buf = vec![0u8; 64];
            buf[8..8 + pattern.len()].copy_from_slice(pattern);
            assert_eq!(detect_codec(&buf), Some(expected), "pattern {pattern:02X?}");
        }
    }

    #[test]
    fn earliest_sync_word_wins() {
        let mut buf = vec![0u8; 64];
        buf[4..8].copy_from_slice(&DTS_CORE_SYNC_BE);
        buf[20..24].copy_from_slice(&TRUEHD_SYNC_BE);
        assert_eq!(detect_codec(&buf), Some(Codec::Dts));

        let mut buf = vec![0u8; 64];
        buf[4..8].copy_from_slice(&TRUEHD_SYNC_BE);
        buf[20..24].copy_from_slice(&DTS_CORE_SYNC_BE);
        assert_eq!(detect_codec(&buf), Some(Codec::Truehd));
    }

    /// A stray two-byte 0x0B77 must not outrank a four-byte sync at the same
    /// offset — the case the tie-break in `detect_codec` exists for.
    #[test]
    fn longer_sync_word_wins_a_tie() {
        // 0x64582025 does not contain 0x0B77, so plant the collision by hand:
        // put the DTS substream sync at 8 and an EAC3 sync at the same offset
        // is impossible, so use the next best thing — EAC3 immediately after,
        // and assert the 4-byte match at the *earlier* offset still wins.
        let mut buf = vec![0u8; 64];
        buf[8..12].copy_from_slice(&DTS_SUBSTREAM_SYNC_BE);
        buf[12..14].copy_from_slice(&EAC3_SYNC_BE);
        assert_eq!(detect_codec(&buf), Some(Codec::Dts));

        // Two patterns cannot literally both match at one offset here, so the
        // tie-break is asserted on the ordering key directly.
        assert!(
            sort_key(8, 4) < sort_key(8, 2),
            "at equal offset a 4-byte sync must sort before a 2-byte one"
        );
    }

    /// The head of a standalone IAMF stream as harlettizer writes it: the
    /// sequence header (`iamf`, advanced-2 twice), then a codec config.
    const IAMF_HEAD: [u8; 12] = [0xF8, 0x06, b'i', b'a', b'm', b'f', 5, 5, 0x00, 0x02, 0, 0];

    #[test]
    fn detects_an_iamf_sequence_header() {
        assert_eq!(detect_codec(&IAMF_HEAD), Some(Codec::Iamf));
        assert_eq!(
            find_iamf_sequence_header(&IAMF_HEAD),
            Some(IamfSequenceHeader {
                offset: 0,
                primary_profile: 5,
                additional_profile: 5
            })
        );
        // Redundant copy flag, two-byte size: still one.
        let mut redundant = vec![0xFC, 0x86, 0x00];
        redundant.extend_from_slice(b"iamf\x00\x01");
        assert_eq!(
            find_iamf_sequence_header(&redundant)
                .map(|h| (h.primary_profile, h.additional_profile)),
            Some((0, 1))
        );
        // TrueHD's sync shares only its first byte: not a header.
        assert_eq!(detect_codec(&TRUEHD_SYNC_BE), Some(Codec::Truehd));
        assert_eq!(find_iamf_sequence_header(&TRUEHD_SYNC_BE), None);
    }

    /// FLAC or PCM payloads may hold any byte pattern: the stream's own
    /// header, at its first byte, is the earliest.
    #[test]
    fn an_iamf_stream_wins_over_a_sync_word_in_its_payload() {
        let mut buf = IAMF_HEAD.to_vec();
        buf.extend_from_slice(&[0x0B, 0x77, 0x7F, 0xFE, 0x80, 0x01]);
        assert_eq!(detect_codec(&buf), Some(Codec::Iamf));
    }

    #[test]
    fn empty_buffer_detects_nothing() {
        assert_eq!(detect_codec(&[]), None);
    }
}
