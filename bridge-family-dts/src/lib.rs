//! The DTS codec family of the bridge: DTS core, DTS-HD (HRA, MA), the DTS:X
//! extensions over them, and Auro-3D carried in DTS-HD MA lossless — over
//! the raw transport and IEC 61937.
//!
//! [`DtsPipeline`] holds the whole path's state, decodes what a packet
//! completes, and answers the host's questions about the stream (family,
//! label, objects, declared poses). Resetting the whole bridge is the
//! router's job: a path says when it is needed ([`AfterPush`]). As a
//! [`FamilyPipeline`], it is a bridge of its own: `PluginBridge<DtsPipeline>`.
//!
//! [`FamilyPipeline`]: bridge_common::family::FamilyPipeline
//!
//! [`AfterPush`]: bridge_common::shared::AfterPush

mod auro_pipeline;
mod dts_pipeline;
mod dts_spdif;
mod family;
mod labels;
pub mod probe;

// What every family shares, under the `crate::` paths the modules use.
use bridge_common::{frame_builders, logging, shared};

pub use dts_pipeline::{DtsPipeline, FAMILY_AURO, FAMILY_DTS};
pub use dts_spdif::accepts_data_type;
pub use family::DtsCodec;
