//! The Dolby codec family of the bridge: TrueHD (raw, and MAT over
//! IEC 61937) and E-AC-3 / AC-3 with their JOC objects (raw and IEC 61937).
//!
//! [`DolbyPipeline`] holds the whole family's state, decodes what a packet
//! completes, and answers the host's questions about the stream (label,
//! objects, the `presentation` key, DRC). Resetting the whole bridge is the
//! router's job: a path says when it is needed ([`AfterPush`]).
//!
//! [`AfterPush`]: bridge_common::shared::AfterPush

mod ac3_native;
mod dolby;
mod eac3_pipeline;
mod eac3_spdif;
mod labels;
mod mat;
mod metadata;
mod perf;
mod truehd_pipeline;

// What every family shares, under the `crate::` paths the modules use.
use bridge_common::{frame_builders, logging, shared};

pub use dolby::{DolbyPipeline, FAMILY_DOLBY};
