//! The bridge a host loads: a router over the codec families, each in its
//! own crate (`bridge-family-dolby`, `bridge-family-dts`,
//! `bridge-family-iamf`), on what they share (`bridge-common`).

mod bridge;

// The `crate::` paths the router uses for what the families share.
use bridge_common::{logging, shared};
#[cfg(feature = "iamf")]
use bridge_family_iamf as iamf_pipeline;

use abi_stable::std_types::{RSlice, RString, RVec};
use abi_stable::{
    export_root_module, prefix_type::PrefixTypeTrait, sabi_trait::prelude::TD_Opaque,
};
use bridge::AtmosBridge;
use bridge_api::{
    BridgeLib, BridgeLibRef, FormatBridge_TO, FormatBridgeBox, RInputTransport, RProbe,
    RSourceFamily,
};

// Silence unused import warning — FormatBridge is used via the proc-macro generated impl.
#[allow(unused_imports)]
use bridge_api::FormatBridge as _FormatBridgeTrait;

/// Plugin entry point: export the root module so the host can load it.
#[export_root_module]
fn get_library() -> BridgeLibRef {
    BridgeLib {
        new_bridge: create_bridge,
        set_host_log_sink,
        source_families,
        probe,
        input_codecs,
    }
    .leak_into_prefix()
}

extern "C" fn create_bridge(strict: bool) -> FormatBridgeBox {
    log_build_id_once();
    FormatBridge_TO::from_value(AtmosBridge::new(strict), TD_Opaque)
}

extern "C" fn set_host_log_sink(sink: usize) {
    logging::register_host_log_sink(sink);
    if sink != 0 {
        log_build_id_once();
    }
}

/// This bridge's version.
pub const BRIDGE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The mgth/Omniphony commit this library's `bridge_api` was built from
/// (`.omniphony-ref` in a release build), or `unknown` when the sibling
/// checkout was not a git one (build.rs).
pub const OMNIPHONY_COMMIT: &str = env!("HARLETTY_BUILD_OMNIPHONY_COMMIT");

/// What this library was built from, as logged when a host loads it: compare
/// it with the host's `bridge_api` when a bridge will not load.
pub fn build_id() -> String {
    format!(
        "harletty-bridge {BRIDGE_VERSION} (bridge_api {}, Omniphony {OMNIPHONY_COMMIT})",
        bridge_api::VERSION
    )
}

/// Log [`build_id`] once per process: when the host installs its log sink at
/// load, or at the first bridge if it never does.
fn log_build_id_once() {
    static LOGGED: std::sync::Once = std::sync::Once::new();
    LOGGED.call_once(|| {
        bridge_common::bridge_log!(log::Level::Info, "{}", build_id());
    });
}

extern "C" fn source_families() -> RVec<RSourceFamily> {
    bridge::source_families()
}

extern "C" fn probe(data: RSlice<'_, u8>, transport: RInputTransport, data_type: u8) -> RProbe {
    bridge::probe(data.as_slice(), transport, data_type)
}

extern "C" fn input_codecs() -> RVec<RString> {
    bridge::input_codecs()
}

/// A bridge as the host gets one, for in-process callers linking the rlib —
/// `examples/bridge_bench.rs` — rather than loading the library.
#[doc(hidden)]
pub fn new_bridge(strict: bool) -> FormatBridgeBox {
    create_bridge(strict)
}

/// Install a host log sink, for the same in-process callers as
/// [`new_bridge`]: without one, every diagnostic down to `Debug` goes to
/// stderr, which no real host does.
#[doc(hidden)]
pub fn set_log_sink(sink: bridge_api::BridgeHostLogSink) {
    set_host_log_sink(sink as usize);
}
