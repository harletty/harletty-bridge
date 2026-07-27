use crate::input::InputReader;
use anyhow::{Result, anyhow, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Codec {
    Auto,
    Truehd,
    Eac3,
}

const PROBE_BUFFER_SIZE: usize = 8 * 1024;

const TRUEHD_SYNC_BE: [u8; 4] = [0xF8, 0x72, 0x6F, 0xBA];
const TRUEHD_SYNC_FBB_BE: [u8; 4] = [0xF8, 0x72, 0x6F, 0xBB];
const EAC3_SYNC_BE: [u8; 2] = [0x0B, 0x77];

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

    let truehd_offset = find_pattern(&buffer, &TRUEHD_SYNC_BE)
        .or_else(|| find_pattern(&buffer, &TRUEHD_SYNC_FBB_BE));
    let eac3_offset = find_pattern(&buffer, &EAC3_SYNC_BE);

    let codec = match (truehd_offset, eac3_offset) {
        (Some(t), Some(e)) if t <= e => Codec::Truehd,
        (Some(_), Some(_)) => Codec::Eac3,
        (Some(_), None) => Codec::Truehd,
        (None, Some(_)) => Codec::Eac3,
        (None, None) => bail!(
            "no TrueHD (0xF8726FBA) or EAC3 (0x0B77) sync word found in first {} bytes; \
             pass --codec explicitly",
            buffer.len()
        ),
    };

    // Only pipes need the consumed prefix replayed; file paths are re-opened from 0.
    let prefix = if reader.is_pipe() { buffer } else { Vec::new() };
    Ok((codec, prefix))
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
}
