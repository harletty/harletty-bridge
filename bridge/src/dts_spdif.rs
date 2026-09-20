//! Minimal IEC 61937 (S/PDIF) handling for DTS.
//!
//! For types I/II/III the burst carries the DTS frame directly: we only
//! normalise the 16-bit word order — IEC 61937 may transmit DTS byte-swapped —
//! and hand the bytes to the DTS pipeline, which demuxes `[core][exss]` frames
//! itself (the same path as the raw transport).
//!
//! Type IV (DTS-HD) is not direct: the frame is wrapped in a 12-byte header, so
//! the payload has to be unwrapped before the pipeline can see a sync word.

use std::borrow::Cow;

/// IEC 61937 data types carrying DTS: type I (0x0B), II (0x0C), III (0x0D) and
/// the DTS-HD substream (type IV, 0x11).
pub(crate) fn accepts_data_type(data_type: u8) -> bool {
    matches!(data_type, 0x0B | 0x0C | 0x0D | 0x11)
}

/// IEC 61937 data type of the DTS-HD substream burst.
pub(crate) const DTSHD_DATA_TYPE: u8 = 0x11;

/// Start code opening a DTS-HD burst payload, followed by the frame length as a
/// big-endian `u16`. Matches `dtshd_start_code` in ffmpeg's spdif muxer, which
/// is what mpv emits.
const DTSHD_START_CODE: [u8; 10] = [0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFE, 0xFE];
const DTSHD_HEADER_LEN: usize = DTSHD_START_CODE.len() + 2;

/// Normalise an IEC 61937 DTS burst payload to native byte order.
///
/// DTS sync words are `0x7FFE8001` (core) / `0x64582025` (extension substream);
/// IEC 61937 may carry them with each 16-bit word byte-swapped (`0xFE7F0180` /
/// `0x58642520`), which this undoes. A DTS-HD burst opens on its start code
/// rather than a sync word, so that swapped form is recognised too — otherwise
/// the payload stays swapped and nothing downstream matches. Returns the input
/// untouched when it is already native.
pub(crate) fn normalise_payload(data: &[u8]) -> Cow<'_, [u8]> {
    let swapped = data.len() >= 2
        && ((data[0] == 0xFE && data[1] == 0x7F)
            || (data[0] == 0x58 && data[1] == 0x64)
            || (data[0] == 0x00 && data[1] == 0x01));
    if !swapped {
        return Cow::Borrowed(data);
    }
    let mut out = data.to_vec();
    for pair in out.chunks_exact_mut(2) {
        pair.swap(0, 1);
    }
    Cow::Owned(out)
}

/// Strip the DTS-HD burst wrapper, returning the DTS frame it carries.
///
/// The payload is `[start code][length: u16 big-endian][frame]`, and the burst
/// is padded out to the IEC 61937 period, so the declared length — not the
/// payload size — bounds the frame. Returns `None` when the wrapper is absent,
/// letting the caller pass the payload through unchanged rather than guess.
///
/// Expects `data` already in native byte order.
pub(crate) fn unwrap_hd_payload(data: &[u8]) -> Option<&[u8]> {
    if data.len() < DTSHD_HEADER_LEN || data[..DTSHD_START_CODE.len()] != DTSHD_START_CODE {
        return None;
    }
    let declared = u16::from_be_bytes([data[10], data[11]]) as usize;
    let frame = &data[DTSHD_HEADER_LEN..];
    Some(&frame[..declared.min(frame.len())])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_dts_types_only() {
        for t in [0x0B, 0x0C, 0x0D, 0x11] {
            assert!(accepts_data_type(t));
        }
        for t in [0x15, 0x16, 0x00] {
            assert!(!accepts_data_type(t));
        }
    }

    #[test]
    fn deswaps_byte_swapped_core_syncword() {
        // Swapped core sync 0xFE 7F 01 80 -> native 0x7F FE 80 01.
        let out = normalise_payload(&[0xFE, 0x7F, 0x01, 0x80]);
        assert_eq!(&out[..], &[0x7F, 0xFE, 0x80, 0x01]);
        // Native payload is passed through untouched.
        let native = [0x7F, 0xFE, 0x80, 0x01];
        assert_eq!(&normalise_payload(&native)[..], &native);
    }

    /// A DTS-HD burst opens on its start code, not a sync word, so the swap
    /// detection has to recognise it or the payload stays byte-swapped.
    #[test]
    fn deswaps_byte_swapped_hd_start_code() {
        let mut swapped = vec![0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFE, 0xFE];
        swapped.extend_from_slice(&[0xF0, 0x17]);
        let out = normalise_payload(&swapped);
        assert_eq!(&out[..DTSHD_START_CODE.len()], &DTSHD_START_CODE);
    }

    #[test]
    fn unwraps_hd_payload_to_the_declared_length() {
        let frame: Vec<u8> = (0..64u8).collect();
        let mut payload = DTSHD_START_CODE.to_vec();
        payload.extend_from_slice(&(frame.len() as u16).to_be_bytes());
        payload.extend_from_slice(&frame);
        // Burst padding beyond the declared frame must be dropped.
        payload.extend_from_slice(&[0u8; 32]);
        assert_eq!(unwrap_hd_payload(&payload), Some(&frame[..]));
    }

    #[test]
    fn leaves_a_plain_dts_burst_alone() {
        let core = [0x7F, 0xFE, 0x80, 0x01, 0x00, 0x11, 0x22, 0x33];
        assert_eq!(unwrap_hd_payload(&core), None);
    }

    /// A truncated burst must clamp to what arrived instead of panicking.
    #[test]
    fn clamps_a_declared_length_past_the_payload() {
        let mut payload = DTSHD_START_CODE.to_vec();
        payload.extend_from_slice(&4096u16.to_be_bytes());
        payload.extend_from_slice(&[0xAA; 16]);
        assert_eq!(unwrap_hd_payload(&payload), Some(&[0xAA; 16][..]));
    }
}
