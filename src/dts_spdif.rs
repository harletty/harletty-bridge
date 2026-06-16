//! Minimal IEC 61937 (S/PDIF) handling for DTS.
//!
//! Unlike E-AC3 (which needs syncframe re-framing), a DTS burst carries the DTS
//! frame directly. We only normalise the 16-bit word order — IEC 61937 may
//! transmit DTS byte-swapped — and hand the bytes to the DTS pipeline, which
//! demuxes `[core][exss]` frames itself (the same path as the raw transport).

use std::borrow::Cow;

/// IEC 61937 data types carrying DTS: type I (0x0B), II (0x0C), III (0x0D) and
/// the DTS-HD substream (type IV, 0x11).
pub(crate) fn accepts_data_type(data_type: u8) -> bool {
    matches!(data_type, 0x0B | 0x0C | 0x0D | 0x11)
}

/// Normalise an IEC 61937 DTS burst payload to native byte order. DTS sync words
/// are `0x7FFE8001` (core) / `0x64582025` (extension substream); IEC 61937 may
/// carry them with each 16-bit word byte-swapped (`0xFE7F0180` / `0x58642520`),
/// which this undoes. Returns the input untouched when it is already native.
pub(crate) fn normalise_payload(data: &[u8]) -> Cow<'_, [u8]> {
    let swapped = data.len() >= 2
        && ((data[0] == 0xFE && data[1] == 0x7F) || (data[0] == 0x58 && data[1] == 0x64));
    if !swapped {
        return Cow::Borrowed(data);
    }
    let mut out = data.to_vec();
    for pair in out.chunks_exact_mut(2) {
        pair.swap(0, 1);
    }
    Cow::Owned(out)
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
}
