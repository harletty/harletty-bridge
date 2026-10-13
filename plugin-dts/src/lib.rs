//! The DTS family as an Omniphony bridge plugin of its own
//! (`libharletty_dts_bridge.so`, `harletty_dts_bridge.dll`,
//! `libharletty_dts_bridge.dylib`): DTS core, DTS-HD (HRA, MA), the DTS:X
//! extensions and Auro-3D carried in DTS-HD MA, raw and over IEC 61937
//! (burst types 0x0B, 0x0C, 0x0D and 0x11).
//!
//! What it decodes and how it answers the host is the family's
//! (`bridge-family-dts`, a `FamilyPipeline`); the bridge around it and the root
//! module are `bridge-common`'s, the same for every plugin.

use abi_stable::export_root_module;
use bridge_api::{BridgeHostLogSink, BridgeLibRef, FormatBridgeBox};
use bridge_common::plugin::{self, Plugin};
use bridge_family_dts::DtsPipeline;

/// This plugin: the family it decodes and what it is called.
pub struct Dts;

impl Plugin for Dts {
    type Family = DtsPipeline;
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
    plugin::root_module::<Dts>()
}

/// What this library was built from, as logged when a host loads it.
pub fn build_id() -> String {
    plugin::build_id::<Dts>()
}

/// A bridge as the host gets one, for in-process callers.
pub fn new_bridge(strict: bool) -> FormatBridgeBox {
    plugin::new_bridge::<Dts>(strict)
}

/// Install a host log sink, for in-process callers.
pub fn set_log_sink(sink: BridgeHostLogSink) {
    plugin::set_log_sink::<Dts>(sink);
}
