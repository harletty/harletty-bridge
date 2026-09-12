// SPDX-License-Identifier: Apache-2.0

//! Auro-Codec carrier detection.
//!
//! Auro-3D ships its height layer inside ordinary 24-bit PCM: the encoder
//! folds the height channels into a 5.1 or 7.1 carrier mix and borrows the
//! low bits of every carrier sample for a side channel that describes the
//! fold and carries the residuals a decoder needs to undo it. The carrier
//! plays as plain surround anywhere; only a decoder sees the side channel.
//! On disc the carrier is usually a DTS-HD MA track, so the side channel
//! survives only a bit-exact lossless decode.
//!
//! This crate reads the part of that side channel that is public: the block
//! sync, its CRC, and the ADOL configuration that names the original and
//! carrier layouts. It says *that* a stream is an Auro carrier and *what* it
//! holds; it does not reconstruct the height channels. The residual layer
//! (predictor seeds, Golomb-Rice residuals, the unmix) is not publicly
//! specified.
//!
//! Sources: the public reverse-engineering by almirus (Orua-D3, MIT;
//! MediaInfoLib PR #2531). The tables in [`layout`] are theirs.

pub mod block;
pub mod detect;
pub mod layout;

pub use block::{BlockError, BlockInfo, SyncHeader, parse_block, parse_sync};
pub use detect::{ChannelStats, Detection, Detector};
pub use layout::{ChannelConfig, Layout};
