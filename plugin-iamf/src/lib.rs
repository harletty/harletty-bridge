//! IAMF (AOMedia Immersive Audio Model and Formats) as an Omniphony bridge
//! plugin of its own (`libharletty_iamf_bridge.so`,
//! `harletty_iamf_bridge.dll`, `libharletty_iamf_bridge.dylib`): an OBU
//! stream over the raw transport. IAMF has no IEC 61937 burst type. Its Opus
//! decoding links libopus.
//!
//! What it decodes and how it answers the host is the family's
//! (`bridge-family-iamf`, a `FamilyPipeline`); the bridge around it and the root
//! module are `bridge-common`'s, the same for every plugin.

use abi_stable::export_root_module;
use bridge_api::{BridgeHostLogSink, BridgeLibRef, FormatBridgeBox};
use bridge_common::plugin::{self, Plugin};
use bridge_family_iamf::IamfPipeline;

/// This plugin: the family it decodes and what it is called.
pub struct Iamf;

impl Plugin for Iamf {
    type Family = IamfPipeline;
    const NAME: &'static str = env!("CARGO_PKG_NAME");
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");
}

/// Plugin entry point: export the root module so the host can load it.
#[export_root_module]
fn get_library() -> BridgeLibRef {
    library()
}

/// The root module, for in-process callers (tests) rather than loading the
/// library.
pub fn library() -> BridgeLibRef {
    plugin::root_module::<Iamf>()
}

/// What this library was built from, as logged when a host loads it.
pub fn build_id() -> String {
    plugin::build_id::<Iamf>()
}

/// A bridge as the host gets one, for in-process callers.
pub fn new_bridge(strict: bool) -> FormatBridgeBox {
    plugin::new_bridge::<Iamf>(strict)
}

/// Install a host log sink, for in-process callers.
pub fn set_log_sink(sink: BridgeHostLogSink) {
    plugin::set_log_sink::<Iamf>(sink);
}
