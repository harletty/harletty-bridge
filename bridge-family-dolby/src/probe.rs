//! Where a Dolby stream starts in raw bytes (`BridgeLib::probe`, Omniphony's
//! `docs/multi-bridge.md`, "ABI: bridge_api 0.6"):
//!
//! - **TrueHD / MLP**: an access unit whose major sync (FBA or FBB, four
//!   bytes in) has a valid `major_sync_info` checksum, the one the decoder's
//!   own extractor checks. For FBA the checksum follows the optional
//!   `extra_channel_meaning` extension (2 × (n + 1) bytes, n on 4 bits), so
//!   a start is decided within [`TRUEHD_MAX_PROBE`] bytes.
//! - **E-AC-3 / AC-3**: the sync word, a header the decoder's framing takes
//!   (bit stream id, frame size and sample rate codes), and the frame CRC
//!   over the whole frame: within one frame, [`EAC3_MAX_PROBE`] bytes at
//!   most.
//!
//! The byte-swapped E-AC-3 sync word is not claimed: the raw framing does
//! not read that order.

use bridge_api::RProbe;
use bridge_common::probe::{Start, scan_raw, sync_prefix};
use eac3::{HeaderParseError, parse_header, parse_legacy_ac3_header};
use truehd::utils::crc::{CRC_MAJOR_SYNC_INFO_ALG, Crc16};

/// The most bytes a TrueHD start needs: the access-unit header, the major
/// sync info with the longest `extra_channel_meaning` extension, its CRC.
pub const TRUEHD_MAX_PROBE: usize = 4 + 28 + 30 + 2;
/// The most bytes an E-AC-3 start needs: its largest frame (2048 words).
pub const EAC3_MAX_PROBE: usize = 4096;

/// The access-unit header in front of a major sync.
const AU_HEADER: usize = 4;
/// The major sync word but its last byte, which says FBA (TrueHD) or FBB
/// (MLP).
const MAJOR_SYNC_PREFIX: [u8; 3] = [0xF8, 0x72, 0x6F];
const FORMAT_FBA: u8 = 0xBA;
const FORMAT_FBB: u8 = 0xBB;
/// The E-AC-3 / AC-3 sync word.
const EAC3_SYNC: [u8; 2] = [0x0B, 0x77];
/// The fewest header bytes the E-AC-3 framing reads.
const EAC3_HEADER: usize = 7;

static MAJOR_SYNC_CRC: Crc16 = Crc16::new(&CRC_MAJOR_SYNC_INFO_ALG);

/// `BridgeLib::probe` for the raw transport.
pub fn probe_raw(data: &[u8]) -> RProbe {
    scan_raw(data, start)
}

/// A Dolby stream start at the first byte of `s`: TrueHD's or E-AC-3's.
fn start(s: &[u8]) -> Start {
    match (truehd_start(s), eac3_start(s)) {
        (Start::Claim, _) | (_, Start::Claim) => Start::Claim,
        (Start::Need(a), Start::Need(b)) => Start::Need(a.min(b)),
        (Start::Need(n), Start::Reject) | (Start::Reject, Start::Need(n)) => Start::Need(n),
        (Start::Reject, Start::Reject) => Start::Reject,
    }
}

/// A TrueHD (or MLP) access unit with a valid major sync at `s`.
fn truehd_start(s: &[u8]) -> Start {
    let sync = s.get(AU_HEADER..).unwrap_or_default();
    let shown = sync.len().min(MAJOR_SYNC_PREFIX.len());
    if sync[..shown] != MAJOR_SYNC_PREFIX[..shown] {
        return Start::Reject;
    }
    let Some(&format) = s.get(AU_HEADER + 3) else {
        return Start::Need(AU_HEADER + 4);
    };
    // The major sync info's length before its CRC, as the decoder's
    // extractor reads it: FBB has no extension.
    let info_len = match format {
        FORMAT_FBB => 26,
        FORMAT_FBA => {
            let (Some(&flags), Some(&extension)) = (s.get(29), s.get(30)) else {
                return Start::Need(31);
            };
            if flags & 0x01 == 0 {
                26
            } else {
                28 + usize::from((extension >> 3) & 0x1E)
            }
        }
        _ => return Start::Reject,
    };
    let end = AU_HEADER + info_len + 2;
    if s.len() < end {
        return Start::Need(end);
    }
    // An access unit that cannot hold its header, major sync, CRC and one
    // substream directory entry is not one.
    let access_unit_len = usize::from(u16::from_be_bytes([s[0], s[1]]) & 0x0FFF) << 1;
    if access_unit_len < info_len + 8 {
        return Start::Reject;
    }
    let info = &s[AU_HEADER..AU_HEADER + info_len];
    let crc = u16::from_be_bytes([s[AU_HEADER + info_len], s[AU_HEADER + info_len + 1]]);
    if MAJOR_SYNC_CRC.update(MAJOR_SYNC_CRC.init, info) == crc {
        Start::Claim
    } else {
        Start::Reject
    }
}

/// An E-AC-3 or AC-3 syncframe at `s` whose CRC checks.
fn eac3_start(s: &[u8]) -> Start {
    if let Some(start) = sync_prefix(s, &EAC3_SYNC) {
        return start;
    }
    if s.len() < EAC3_HEADER {
        return Start::Need(EAC3_HEADER);
    }
    // The raw framing's own header rules: E-AC-3 (bsid 11..=16), else a
    // legacy AC-3 frame (bsid up to 10).
    let frame_size = match parse_header(s) {
        Ok(info) => info.frame_size,
        Err(HeaderParseError::UnsupportedBitstreamId(bsid)) if bsid <= 10 => {
            match parse_legacy_ac3_header(s) {
                Ok(info) => info.frame_size,
                Err(_) => return Start::Reject,
            }
        }
        Err(_) => return Start::Reject,
    };
    if frame_size < EAC3_HEADER + 2 {
        return Start::Reject;
    }
    if s.len() < frame_size {
        return Start::Need(frame_size);
    }
    // crc1 (AC-3) and crc2 leave the CRC of everything after the sync word
    // at zero.
    if crc16_ansi(&s[2..frame_size]) == 0 {
        Start::Claim
    } else {
        Start::Reject
    }
}

/// CRC-16, polynomial 0x8005, MSB first, initial value 0: the E-AC-3 / AC-3
/// frame CRC.
fn crc16_ansi(data: &[u8]) -> u16 {
    static TABLE: [u16; 256] = {
        let mut table = [0u16; 256];
        let mut i = 0;
        while i < 256 {
            let mut crc = (i as u16) << 8;
            let mut bit = 0;
            while bit < 8 {
                crc = if crc & 0x8000 != 0 {
                    (crc << 1) ^ 0x8005
                } else {
                    crc << 1
                };
                bit += 1;
            }
            table[i] = crc;
            i += 1;
        }
        table
    };
    data.iter().fold(0u16, |crc, &byte| {
        (crc << 8) ^ TABLE[usize::from((crc >> 8) as u8 ^ byte)]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bridge_api::RProbeVerdict;
    use bridge_common::probe::{replay, replay_split};
    use truehd::process::{EXAMPLE_DATA, EXAMPLE_DATA_FBB};

    const EAC3: &[u8] = include_bytes!("../../harletty/tests/fixtures/joc_atmos_1s.eac3");
    const DTS: &[u8] = include_bytes!("../../harletty/tests/fixtures/dts_core_tone_10f.dts");

    /// Every way a host can cut the opening of `stream`: one read of every
    /// length up to `bound` then the rest, and fixed reads of a few sizes.
    /// Each one claims `start`, having read no more than `bound` past it.
    fn assert_claimed_however_cut(stream: &[u8], start: usize, bound: usize) {
        for first in start.saturating_sub(16).max(1)..=start + bound + 1 {
            let replay = replay_split(probe_raw, stream, first);
            assert_eq!(replay.claim, Some(start), "split at {first}");
        }
        for read in [1, 2, 3, 5, 7, 64, 997, 4096] {
            let replay = replay(probe_raw, stream, read);
            assert_eq!(replay.claim, Some(start), "reads of {read}");
            assert!(replay.most_needed <= bound, "reads of {read}: {replay:?}");
            assert!(
                replay.read - start <= bound + read,
                "reads of {read}: claimed after {} bytes",
                replay.read
            );
        }
    }

    #[test]
    fn a_truehd_stream_is_claimed_at_its_first_access_unit() {
        // The example opens with the 16-byte timestamp some files carry in
        // front of the first access unit: the stream starts after it.
        assert_eq!(&EXAMPLE_DATA[20..24], &[0xF8, 0x72, 0x6F, 0xBA]);
        assert_eq!(probe_raw(EXAMPLE_DATA), RProbe::claim(16));
        assert_claimed_however_cut(EXAMPLE_DATA, 16, TRUEHD_MAX_PROBE);
        // An FBB (MLP) major sync too.
        assert_eq!(probe_raw(EXAMPLE_DATA_FBB).verdict, RProbeVerdict::Claim);
        // Behind bytes that belong to no stream.
        let mut late = vec![0x5Au8; 1001];
        late.extend_from_slice(&EXAMPLE_DATA[16..]);
        assert_claimed_however_cut(&late, 1001, TRUEHD_MAX_PROBE);
    }

    /// A major sync with the longest extra_channel_meaning extension, n = 15:
    /// its checksum sits at 62, and it is decided at 64 bytes, not before.
    #[test]
    fn the_longest_major_sync_is_claimed_at_64_bytes() {
        let mut au = vec![0u8; 256];
        // check nibble 0, access_unit_length 128 words, input timing 0.
        au[0] = 0x00;
        au[1] = 0x80;
        au[4..8].copy_from_slice(&[0xF8, 0x72, 0x6F, 0xBA]);
        au[29] = 0x01; // extra_channel_meaning_present
        au[30] = 0xF0; // 15: 2 × 16 bytes of extension
        let info_len = 28 + 30;
        let crc = MAJOR_SYNC_CRC.update(MAJOR_SYNC_CRC.init, &au[4..4 + info_len]);
        au[4 + info_len..6 + info_len].copy_from_slice(&crc.to_be_bytes());
        assert_eq!(probe_raw(&au), RProbe::claim(0));
        for len in 1..TRUEHD_MAX_PROBE {
            let answer = probe_raw(&au[..len]);
            assert_eq!(answer.verdict, RProbeVerdict::Pending, "{len} bytes");
            assert_eq!(answer.offset, 0);
            assert!(answer.needed as usize <= TRUEHD_MAX_PROBE);
        }
        assert_eq!(probe_raw(&au[..TRUEHD_MAX_PROBE]), RProbe::claim(0));
        let replay = replay(probe_raw, &au, 1);
        assert_eq!((replay.claim, replay.read), (Some(0), TRUEHD_MAX_PROBE));
        assert_claimed_however_cut(&au, 0, TRUEHD_MAX_PROBE);
        // A checksum off by one bit is no start.
        au[62] ^= 0x01;
        assert_ne!(probe_raw(&au).verdict, RProbeVerdict::Claim);
        assert_ne!(probe_raw(&au).offset, 0);
    }

    #[test]
    fn an_eac3_stream_is_claimed_at_its_first_frame() {
        assert_eq!(probe_raw(EAC3), RProbe::claim(0));
        assert_claimed_however_cut(EAC3, 0, EAC3_MAX_PROBE);
        let mut late = vec![0x00u8; 333];
        late.extend_from_slice(EAC3);
        assert_claimed_however_cut(&late, 333, EAC3_MAX_PROBE);
        // Read from inside its first frame: the stream starts at the second.
        let first = ((usize::from(EAC3[2] & 0x07) << 8) | usize::from(EAC3[3])) * 2 + 2;
        assert_eq!(probe_raw(&EAC3[100..]), RProbe::claim((first - 100) as u32));
        assert_claimed_however_cut(&EAC3[100..], first - 100, EAC3_MAX_PROBE);
    }

    #[test]
    fn a_corrupt_eac3_frame_is_not_claimed() {
        let size = ((usize::from(EAC3[2] & 0x07) << 8) | usize::from(EAC3[3])) * 2 + 2;
        let mut frame = EAC3[..size].to_vec();
        frame[size / 2] ^= 0x10;
        let answer = probe_raw(&frame);
        assert_ne!(answer.verdict, RProbeVerdict::Claim);
        assert!(answer.offset > 0, "{answer:?}");
    }

    #[test]
    fn other_families_streams_are_not_claimed() {
        let answer = probe_raw(DTS);
        assert_ne!(answer.verdict, RProbeVerdict::Claim);
        // Nothing but the tail, which a TrueHD access unit could still start.
        assert!(
            answer.offset as usize >= DTS.len() - AU_HEADER,
            "{answer:?}"
        );
        let iamf = [0xF8, 0x06, b'i', b'a', b'm', b'f', 0x00, 0x00, 0x00, 0x00];
        assert_ne!(probe_raw(&iamf).verdict, RProbeVerdict::Claim);
    }

    /// Bytes no family's stream starts in are each shown a bounded number
    /// of times, however they are read.
    #[test]
    fn undecidable_bytes_cost_a_bounded_number_of_looks() {
        let mut noise = Vec::with_capacity(1 << 20);
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        while noise.len() < 1 << 20 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            noise.push(seed as u8);
        }
        for read in [1, 13, 4096] {
            let replay = replay(probe_raw, &noise, read);
            assert_eq!(replay.claim, None);
            assert!(
                replay.shown <= 8 * noise.len() + EAC3_MAX_PROBE,
                "reads of {read}: {} bytes shown for {}",
                replay.shown,
                noise.len()
            );
        }
    }
}
