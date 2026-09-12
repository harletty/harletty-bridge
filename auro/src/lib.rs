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
//! This crate reads the whole side channel. [`block`] and [`detect`] find
//! the blocks, check their CRC and read the ADOL configuration that names
//! the original and carrier layouts. [`stream`], [`rice`] and [`unmix`]
//! then read what each carrier folded — the seeds, gains, codebook and
//! Golomb-Rice residuals — and restore the folded streams sample by sample,
//! so that a 7.1 carrier gives back its 7.1 bed and its height layer.
//! [`decode`] runs the whole chain on one carrier channel.
//!
//! The layout layer follows the public description by almirus (Orua-D3,
//! MIT; MediaInfoLib PR #2531). The residual layer was worked out here by
//! analysis; it is checked against a published original/encoded pair.

pub mod block;
pub mod decode;
pub mod detect;
pub mod layout;
pub mod rice;
pub mod stream;
pub mod unfold;
pub mod unmix;

pub use block::{BlockError, BlockInfo, SyncHeader, parse_block, parse_sync};
pub use decode::{ChannelDecoder, DecodeError, Decoded, StreamId};
pub use detect::{ChannelStats, Detection, Detector};
pub use layout::{ChannelConfig, Layout, Streams};
pub use stream::{StreamBlock, StreamError};
pub use unfold::Unfolder;
