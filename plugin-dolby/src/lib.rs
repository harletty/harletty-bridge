//! The Dolby family as an Omniphony bridge plugin of its own
//! (`libharletty_dolby_bridge.so`, `harletty_dolby_bridge.dll`,
//! `libharletty_dolby_bridge.dylib`): TrueHD (raw, and MAT over IEC 61937,
//! burst type 0x16) and E-AC-3 / AC-3 with their JOC objects (raw, and IEC
//! 61937 burst types 0x15 and 0x01).
//!
//! What it decodes and how it answers the host is the family's
//! (`bridge-family-dolby`, a `FamilyPipeline`); the bridge around it and the root
//! module are `bridge-common`'s, the same for every plugin.

use abi_stable::export_root_module;
use bridge_api::{BridgeHostLogSink, BridgeLibRef, FormatBridgeBox};
use bridge_common::plugin::{self, Plugin};
use bridge_family_dolby::DolbyPipeline;

/// This plugin: the family it decodes and what it is called.
pub struct Dolby;

impl Plugin for Dolby {
    type Family = DolbyPipeline;
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
    plugin::root_module::<Dolby>()
}

/// What this library was built from, as logged when a host loads it.
pub fn build_id() -> String {
    plugin::build_id::<Dolby>()
}

/// A bridge as the host gets one, for in-process callers.
pub fn new_bridge(strict: bool) -> FormatBridgeBox {
    plugin::new_bridge::<Dolby>(strict)
}

/// Install a host log sink, for in-process callers.
pub fn set_log_sink(sink: BridgeHostLogSink) {
    plugin::set_log_sink::<Dolby>(sink);
}
