mod ac3_native;
mod bridge;
mod dolby;
mod eac3_pipeline;
mod eac3_spdif;
mod labels;
mod mat;
mod metadata;
mod perf;
mod truehd_pipeline;

// The codec families and what they share live in their own crates; these
// keep the `crate::` paths the remaining in-crate paths use.
use bridge_common::{frame_builders, logging, shared};
#[cfg(feature = "iamf")]
use bridge_family_iamf as iamf_pipeline;

use abi_stable::std_types::RVec;
use abi_stable::{
    export_root_module, prefix_type::PrefixTypeTrait, sabi_trait::prelude::TD_Opaque,
};
use bridge::AtmosBridge;
use bridge_api::{BridgeLib, BridgeLibRef, FormatBridge_TO, FormatBridgeBox, RSourceFamily};

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
    }
    .leak_into_prefix()
}

extern "C" fn create_bridge(strict: bool) -> FormatBridgeBox {
    FormatBridge_TO::from_value(AtmosBridge::new(strict), TD_Opaque)
}

extern "C" fn set_host_log_sink(sink: usize) {
    logging::register_host_log_sink(sink);
}

extern "C" fn source_families() -> RVec<RSourceFamily> {
    bridge::source_families()
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
