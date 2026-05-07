mod ac3_native;
mod bridge;
mod eac3_pipeline;
mod eac3_spdif;
mod frame_builders;
mod labels;
mod logging;
mod mat;
mod metadata;
mod perf;
mod truehd_pipeline;

use abi_stable::{
    export_root_module, prefix_type::PrefixTypeTrait, sabi_trait::prelude::TD_Opaque,
};
use bridge::AtmosBridge;
use bridge_api::{BridgeLib, BridgeLibRef, FormatBridgeBox, FormatBridge_TO};

// Silence unused import warning — FormatBridge is used via the proc-macro generated impl.
#[allow(unused_imports)]
use bridge_api::FormatBridge as _FormatBridgeTrait;

/// Plugin entry point: export the root module so the host can load it.
#[export_root_module]
fn get_library() -> BridgeLibRef {
    BridgeLib {
        new_bridge: create_bridge,
    }
    .leak_into_prefix()
}

extern "C" fn create_bridge(strict: bool) -> FormatBridgeBox {
    FormatBridge_TO::from_value(AtmosBridge::new(strict), TD_Opaque)
}
