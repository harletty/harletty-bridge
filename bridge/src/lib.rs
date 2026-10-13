//! The combined bridge: a router over every codec family built in, each in
//! its own crate (`bridge-family-dolby`, `bridge-family-dts`,
//! `bridge-family-iamf`), on what they share (`bridge-common`).
//!
//! Not a plugin library: harletty ships one plugin per family
//! (`plugin-dolby`, `plugin-dts`, `plugin-iamf`). This crate stays for what
//! links the bridge in-process — the fuzz target, `bridge_bench` and the
//! bit-exactness kit — while those move to the family plugins.

mod bridge;

// The `crate::` paths the router uses for what the families share.
#[cfg(any(test, not(all(feature = "dolby", feature = "dts", feature = "iamf"))))]
use bridge_common::logging;
use bridge_common::shared;
#[cfg(feature = "iamf")]
use bridge_family_iamf as iamf_pipeline;

use bridge::Families;
use bridge_common::plugin::{self, Plugin};

/// The combined bridge as a [`Plugin`], for the root-module helpers.
struct Combined;

impl Plugin for Combined {
    type Family = Families;
    const NAME: &'static str = env!("CARGO_PKG_NAME");
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");
}

/// This bridge's version.
pub const BRIDGE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The mgth/Omniphony commit this library's `bridge_api` was built from
/// (`.omniphony-ref` in a release build), or `unknown` when the sibling
/// checkout was not a git one (bridge-common's build.rs).
pub const OMNIPHONY_COMMIT: &str = plugin::OMNIPHONY_COMMIT;

/// What this library was built from, as logged at the first bridge.
pub fn build_id() -> String {
    plugin::build_id::<Combined>()
}

/// A bridge as a host gets one, for in-process callers linking this crate
/// (`examples/bridge_bench.rs`, the fuzz target).
#[doc(hidden)]
pub fn new_bridge(strict: bool) -> bridge_api::FormatBridgeBox {
    plugin::new_bridge::<Combined>(strict)
}

/// Install a host log sink, for the same in-process callers as
/// [`new_bridge`]: without one, every diagnostic down to `Debug` goes to
/// stderr, which no real host does.
#[doc(hidden)]
pub fn set_log_sink(sink: bridge_api::BridgeHostLogSink) {
    plugin::set_log_sink::<Combined>(sink);
}
