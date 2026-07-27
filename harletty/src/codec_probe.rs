use crate::input::InputReader;
use anyhow::{Result, anyhow};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Codec {
    Auto,
    Truehd,
    Eac3,
    Dts,
}

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
             found in first {} bytes; pass --codec explicitly",
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
fn detect_codec(buffer: &[u8]) -> Option<Codec> {
    SYNC_CANDIDATES
        .iter()
        .filter_map(|candidate| {
            find_pattern(buffer, candidate.pattern).map(|offset| (candidate, offset))
        })
        .min_by_key(|(candidate, offset)| sort_key(*offset, candidate.pattern.len()))
        .map(|(candidate, _)| candidate.codec)
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

    #[test]
    fn empty_buffer_detects_nothing() {
        assert_eq!(detect_codec(&[]), None);
    }
}
