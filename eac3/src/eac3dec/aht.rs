// SPDX-License-Identifier: Apache-2.0

//! Adaptive Hybrid Transform (AHT) mantissa decoding for E-AC-3
//! (ETSI TS 102 366 Annex E, clause E.2.4).
//!
//! Being rewritten from the specification; see CONTRACTS.md (F1, F2).

use super::bitstream::BitReader;
use super::syncframe::ParseError;

/// Decode one channel's AHT payload (block 0 of the frame) into per-block
/// transform coefficient mantissas for bins `start..end`.
#[allow(unused_variables)]
pub(super) fn decode_pre_mantissas(
    reader: &mut BitReader<'_>,
    hebap: &[u8],
    start: usize,
    end: usize,
    pre_mantissas: &mut [[i32; 6]],
) -> Result<(), ParseError> {
    unimplemented!("clean-room: see CONTRACTS.md")
}
