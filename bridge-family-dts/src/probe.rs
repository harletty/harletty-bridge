//! Where a DTS stream starts in raw bytes (`BridgeLib::probe`, Omniphony's
//! `docs/multi-bridge.md`, "ABI: bridge_api 0.6"):
//!
//! - **Core frame**: the sync word, a header the decoder's own parser takes,
//!   and the next frame's sync word (a core, or the extension substream of a
//!   DTS-HD frame) at the declared frame size: within one frame and four
//!   bytes, [`CORE_MAX_PROBE`] at most. The header CRC is optional in the
//!   format and not relied on.
//! - **Extension substream with no core before it** (a stream read from its
//!   substream, or one that has no core): the substream sync word, the header
//!   size and frame size fields, and the header CRC: within the header,
//!   [`SUBSTREAM_MAX_PROBE`] at most. The frame itself may be longer than
//!   anything a host buffers.
//!
//! Like the decoder, it reads the 16-bit big-endian form only.

use bridge_api::RProbe;
use bridge_common::probe::{Start, scan_raw, sync_prefix};
use dca::parse_header;
use dca::parser::CORE_FRAME_HEADER_SIZE;

/// The most bytes a core start needs: the largest frame (14-bit size) and
/// the next frame's sync word.
pub const CORE_MAX_PROBE: usize = (1 << 14) + 4;
/// The most bytes a substream start needs: its largest header (12-bit size).
pub const SUBSTREAM_MAX_PROBE: usize = 1 << 12;

const CORE_SYNC: [u8; 4] = 0x7FFE_8001u32.to_be_bytes();
const SUBSTREAM_SYNC: [u8; 4] = 0x6458_2025u32.to_be_bytes();
/// The substream header bytes that hold its size fields: 75 bits at most.
const SUBSTREAM_FIELDS: usize = 10;

/// `BridgeLib::probe` for the raw transport.
pub fn probe_raw(data: &[u8]) -> RProbe {
    scan_raw(data, start)
}

fn start(s: &[u8]) -> Start {
    match s.first() {
        Some(&byte) if byte == CORE_SYNC[0] => core_start(s),
        Some(&byte) if byte == SUBSTREAM_SYNC[0] => substream_start(s),
        _ => Start::Reject,
    }
}

/// A core frame at `s`, followed by the next frame's sync word.
fn core_start(s: &[u8]) -> Start {
    if let Some(start) = sync_prefix(s, &CORE_SYNC) {
        return start;
    }
    if s.len() < CORE_FRAME_HEADER_SIZE {
        return Start::Need(CORE_FRAME_HEADER_SIZE);
    }
    // Every field the parser reads fits in the header's minimum size.
    let Ok(info) = parse_header(s) else {
        return Start::Reject;
    };
    let frame_size = info.frame_size;
    let end = frame_size + CORE_SYNC.len();
    if s.len() < end {
        return Start::Need(end);
    }
    let next = &s[frame_size..end];
    if next == CORE_SYNC || next == SUBSTREAM_SYNC {
        Start::Claim
    } else {
        Start::Reject
    }
}

/// An extension substream header at `s` whose CRC checks.
fn substream_start(s: &[u8]) -> Start {
    if let Some(start) = sync_prefix(s, &SUBSTREAM_SYNC) {
        return start;
    }
    if s.len() < SUBSTREAM_FIELDS {
        return Start::Need(SUBSTREAM_FIELDS);
    }
    // After the sync word: 8 user-defined bits, the 2-bit substream index,
    // then whether the size fields are the wide ones.
    let bits = u64::from_be_bytes([s[4], s[5], s[6], s[7], s[8], s[9], 0, 0]);
    let field = |at: u32, width: u32| ((bits >> (64 - at - width)) & ((1 << width) - 1)) as usize;
    let wide = field(10, 1) == 1;
    let (header_bits, size_bits) = if wide { (12, 20) } else { (8, 16) };
    let header_size = field(11, header_bits) + 1;
    let frame_size = field(11 + header_bits, size_bits) + 1;
    // The CRC covers the header after the user-defined bits, and is its
    // last two bytes: the header holds at least the fields read above and
    // the CRC.
    if header_size < SUBSTREAM_FIELDS + 2 || frame_size < header_size {
        return Start::Reject;
    }
    if s.len() < header_size {
        return Start::Need(header_size);
    }
    if crc16_ccitt(&s[5..header_size]) == 0 {
        Start::Claim
    } else {
        Start::Reject
    }
}

/// CRC-16-CCITT, polynomial 0x1021, MSB first, initial value 0xFFFF: the
/// substream header CRC, zero over a header that carries a valid one.
fn crc16_ccitt(data: &[u8]) -> u16 {
    static TABLE: [u16; 256] = {
        let mut table = [0u16; 256];
        let mut i = 0;
        while i < 256 {
            let mut crc = (i as u16) << 8;
            let mut bit = 0;
            while bit < 8 {
                crc = if crc & 0x8000 != 0 {
                    (crc << 1) ^ 0x1021
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
    data.iter().fold(0xFFFFu16, |crc, &byte| {
        (crc << 8) ^ TABLE[usize::from((crc >> 8) as u8 ^ byte)]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bridge_api::RProbeVerdict;
    use bridge_common::probe::{replay, replay_split};

    const DTS: &[u8] = include_bytes!("../../harletty/tests/fixtures/dts_core_tone_10f.dts");
    const EAC3: &[u8] = include_bytes!("../../harletty/tests/fixtures/joc_atmos_1s.eac3");

    fn assert_claimed_however_cut(stream: &[u8], start: usize, bound: usize) {
        for first in start.saturating_sub(16).max(1)..=start + bound + 1 {
            let replay = replay_split(probe_raw, stream, first);
            assert_eq!(replay.claim, Some(start), "split at {first}");
        }
        for read in [1, 2, 3, 5, 7, 64, 997, 4096] {
            let replay = replay(probe_raw, stream, read);
            assert_eq!(replay.claim, Some(start), "reads of {read}");
            assert!(replay.most_needed <= bound, "reads of {read}: {replay:?}");
        }
    }

    fn frame_size(frame: &[u8]) -> usize {
        parse_header(frame).unwrap().frame_size
    }

    #[test]
    fn a_core_stream_is_claimed_at_its_first_frame() {
        assert_eq!(probe_raw(DTS), RProbe::claim(0));
        let size = frame_size(DTS);
        assert_claimed_however_cut(DTS, 0, size + 4);
        // Decided once the next frame's sync word is there, not before.
        assert_eq!(
            probe_raw(&DTS[..size + 3]),
            RProbe::pending(0, (size + 4) as u32)
        );
        let mut late = vec![0x7Fu8; 77];
        late.extend_from_slice(DTS);
        assert_claimed_however_cut(&late, 77, size + 4);
        // Read from inside its first frame: the stream starts at the second.
        assert_eq!(probe_raw(&DTS[9..]), RProbe::claim((size - 9) as u32));
    }

    #[test]
    fn a_core_frame_without_a_sync_word_after_it_is_not_claimed() {
        let size = frame_size(DTS);
        let mut alone = DTS[..size].to_vec();
        alone.extend_from_slice(&[0u8; 8]);
        assert_eq!(probe_raw(&alone).verdict, RProbeVerdict::None);
    }

    /// A substream header built as an encoder writes one: narrow fields, a
    /// header of `header_size` bytes ending in its CRC.
    fn substream(header_size: usize, frame_size: usize) -> Vec<u8> {
        let mut s = vec![0u8; frame_size];
        s[..4].copy_from_slice(&SUBSTREAM_SYNC);
        // user bits 0x00; index 0, narrow; header size − 1 on 8 bits, frame
        // size − 1 on 16.
        let fields: u64 = ((header_size as u64 - 1) << (64 - 11 - 8))
            | ((frame_size as u64 - 1) << (64 - 19 - 16));
        s[4..10].copy_from_slice(&fields.to_be_bytes()[..6]);
        let crc = crc16_ccitt(&s[5..header_size - 2]);
        s[header_size - 2..header_size].copy_from_slice(&crc.to_be_bytes());
        s
    }

    #[test]
    fn a_coreless_substream_is_claimed_on_its_header_crc() {
        let header = substream(48, 3000);
        assert_eq!(crc16_ccitt(&header[5..48]), 0);
        assert_eq!(probe_raw(&header[..48]), RProbe::claim(0));
        assert_eq!(probe_raw(&header[..47]), RProbe::pending(0, 48));
        assert_claimed_however_cut(&header, 0, 48);
        let mut bad = header.clone();
        bad[20] ^= 0x04;
        assert_ne!(probe_raw(&bad).verdict, RProbeVerdict::Claim);
    }

    #[test]
    fn other_families_streams_are_not_claimed() {
        assert_eq!(probe_raw(EAC3).verdict, RProbeVerdict::None);
        let iamf = [0xF8, 0x06, b'i', b'a', b'm', b'f', 0x00, 0x00, 0x00, 0x00];
        assert_eq!(probe_raw(&iamf).verdict, RProbeVerdict::None);
        assert_eq!(
            probe_raw(truehd_like()).verdict,
            RProbeVerdict::None,
            "a TrueHD major sync"
        );
    }

    fn truehd_like() -> &'static [u8] {
        &[
            0x10, 0x80, 0, 0, 0xF8, 0x72, 0x6F, 0xBA, 0x00, 0x27, 0x80, 0x4F, 0xB7, 0x52,
        ]
    }

    #[test]
    fn undecidable_bytes_cost_a_bounded_number_of_looks() {
        let mut noise = Vec::with_capacity(1 << 20);
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
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
                replay.shown <= 4 * noise.len() + CORE_MAX_PROBE,
                "reads of {read}: {} bytes shown for {}",
                replay.shown,
                noise.len()
            );
        }
    }
}
