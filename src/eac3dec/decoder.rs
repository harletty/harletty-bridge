// SPDX-License-Identifier: Apache-2.0

use super::metadata::MetadataParseState;
use super::syncframe::{
    AccessUnitInfo, AuxDataDecodeState, ParseError, inspect_access_unit_with_metadata_state,
};

#[derive(Debug, Clone, PartialEq)]
/// Result returned by [`Decoder::push_access_unit`].
///
/// The `info` field contains the parsed summary for the current access unit and `frames_seen`
/// reflects the decoder's state after accepting that frame.
pub struct PushResult {
    pub frames_seen: u64,
    pub info: AccessUnitInfo,
}

#[derive(Debug)]
/// Stateful access-unit inspector.
///
/// Use this type when you want frame validation and metadata extraction but do not need decoded
/// PCM yet. The decoder preserves cross-frame object metadata state, so frames must be pushed in
/// the original stream order.
pub struct Decoder {
    frames_seen: u64,
    aux_state: AuxDataDecodeState,
    metadata_state: MetadataParseState,
    debug_log_level: log::Level,
}

impl Default for Decoder {
    fn default() -> Self {
        Self {
            frames_seen: 0,
            aux_state: AuxDataDecodeState::default(),
            metadata_state: MetadataParseState::default(),
            debug_log_level: log::Level::Debug,
        }
    }
}

impl Decoder {
    /// Create a fresh decoder with empty cross-frame state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Clear all accumulated state.
    ///
    /// Call this after seeks, packet loss, or any discontinuity that breaks frame order.
    pub fn reset(&mut self) {
        self.frames_seen = 0;
        self.aux_state.reset();
        self.metadata_state.reset();
    }

    /// Number of access units accepted since the last reset.
    pub fn frames_seen(&self) -> u64 {
        self.frames_seen
    }

    /// Configure the metadata / aux diagnostic log level for this decoder instance.
    pub fn set_debug_log_level(&mut self, level: log::Level) {
        self.debug_log_level = level;
    }

    fn apply_debug_log_level(&self) {
        super::metadata::set_metadata_log_level(self.debug_log_level);
        super::syncframe::set_aux_log_level(self.debug_log_level);
    }

    /// Parse one complete E-AC-3 access unit.
    ///
    /// The input must contain exactly one frame. Short buffers and trailing bytes are reported as
    /// errors so the caller can keep access-unit boundaries explicit.
    pub fn push_access_unit(&mut self, access_unit: &[u8]) -> Result<PushResult, ParseError> {
        self.apply_debug_log_level();
        let info = inspect_access_unit_with_metadata_state(
            access_unit,
            &mut self.metadata_state,
            Some(&mut self.aux_state),
        )?;

        if access_unit.len() < info.frame_size {
            return Err(ParseError::TruncatedFrame {
                expected: info.frame_size,
                available: access_unit.len(),
            });
        }
        if access_unit.len() != info.frame_size {
            return Err(ParseError::TrailingData {
                expected: info.frame_size,
                provided: access_unit.len(),
            });
        }

        self.frames_seen += 1;
        Ok(PushResult {
            frames_seen: self.frames_seen,
            info,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Decoder;

    fn push_bits(bits: &mut Vec<bool>, value: u32, width: usize) {
        for bit in (0..width).rev() {
            bits.push(((value >> bit) & 1) != 0);
        }
    }

    fn build_minimal_eac3_frame(frame_size: usize) -> Vec<u8> {
        let mut bits = Vec::new();
        let frmsiz = ((frame_size / 2) - 1) as u32;

        push_bits(&mut bits, 0x0B77, 16);
        push_bits(&mut bits, 0, 2);
        push_bits(&mut bits, 0, 3);
        push_bits(&mut bits, frmsiz, 11);
        push_bits(&mut bits, 0, 2);
        push_bits(&mut bits, 3, 2);
        push_bits(&mut bits, 7, 3);
        push_bits(&mut bits, 1, 1);
        push_bits(&mut bits, 16, 5);
        push_bits(&mut bits, 0, 5);
        push_bits(&mut bits, 0, 1);
        push_bits(&mut bits, 0, 1);
        push_bits(&mut bits, 0, 1);
        push_bits(&mut bits, 0, 1);

        let mut bytes = vec![0u8; frame_size];
        for (index, bit) in bits.iter().copied().enumerate() {
            if bit {
                bytes[index >> 3] |= 1 << (7 - (index & 7));
            }
        }
        bytes
    }

    #[test]
    fn push_access_unit_tracks_state() {
        let frame = build_minimal_eac3_frame(32);
        let mut decoder = Decoder::new();
        let result = decoder
            .push_access_unit(&frame)
            .expect("frame should be accepted");
        assert_eq!(result.frames_seen, 1);
        assert_eq!(decoder.frames_seen(), 1);
    }

    #[test]
    fn push_access_unit_rejects_trailing_data() {
        let mut frame = build_minimal_eac3_frame(32);
        frame.extend_from_slice(&[0u8; 4]);

        let mut decoder = Decoder::new();
        let err = decoder
            .push_access_unit(&frame)
            .expect_err("trailing bytes should be rejected");
        assert_eq!(err.to_string(), "trailing-data expected=32 provided=36");
    }
}
