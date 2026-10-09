//! The root module a plugin library exports, for any [`FamilyPipeline`]:
//! a plugin crate names its family ([`Plugin`]) and exports
//! [`root_module`] under `#[export_root_module]`.

use abi_stable::std_types::{RSlice, RString, RVec};
use abi_stable::{prefix_type::PrefixTypeTrait, sabi_trait::prelude::TD_Opaque};
use bridge_api::{
    BridgeHostLogSink, BridgeLib, BridgeLibRef, FormatBridge_TO, FormatBridgeBox, RInputTransport,
    RProbe, RSourceFamily,
};

use crate::family::{FamilyPipeline, PluginBridge, source_families};

/// The mgth/Omniphony commit this library's `bridge_api` was built from
/// (`.omniphony-ref` in a release build), or `unknown` when the sibling
/// checkout was not a git one (build.rs).
pub const OMNIPHONY_COMMIT: &str = env!("HARLETTY_BUILD_OMNIPHONY_COMMIT");

/// One plugin library: the family it decodes and what it is called.
pub trait Plugin: 'static {
    type Family: FamilyPipeline;
    /// The package name, as the build id gives it.
    const NAME: &'static str;
    /// The package version.
    const VERSION: &'static str;
}

/// What a library was built from, as logged when a host loads it: compare
/// it with the host's `bridge_api` when a bridge will not load.
pub fn build_id<P: Plugin>() -> String {
    format!(
        "{} {} (bridge_api {}, Omniphony {OMNIPHONY_COMMIT})",
        P::NAME,
        P::VERSION,
        bridge_api::VERSION
    )
}

/// Log [`build_id`] once per library: when the host installs its log sink
/// at load, or at the first bridge if it never does.
fn log_build_id_once<P: Plugin>() {
    static LOGGED: std::sync::Once = std::sync::Once::new();
    LOGGED.call_once(|| {
        crate::bridge_log!(log::Level::Info, "{}", build_id::<P>());
    });
}

/// The root module of plugin `P`.
pub fn root_module<P: Plugin>() -> BridgeLibRef {
    BridgeLib {
        new_bridge: create_bridge::<P>,
        set_host_log_sink: set_host_log_sink::<P>,
        source_families: declared_families::<P>,
        probe: probe::<P::Family>,
        input_codecs: input_codecs::<P::Family>,
    }
    .leak_into_prefix()
}

/// A bridge of plugin `P` as the host gets one, for in-process callers
/// (tests, benches) rather than loading the library.
pub fn new_bridge<P: Plugin>(strict: bool) -> FormatBridgeBox {
    create_bridge::<P>(strict)
}

/// Install a host log sink, for the same in-process callers as
/// [`new_bridge`]: without one, every diagnostic down to `Debug` goes to
/// stderr, which no real host does.
pub fn set_log_sink<P: Plugin>(sink: BridgeHostLogSink) {
    set_host_log_sink::<P>(sink as usize);
}

extern "C" fn create_bridge<P: Plugin>(strict: bool) -> FormatBridgeBox {
    log_build_id_once::<P>();
    FormatBridge_TO::from_value(PluginBridge::<P::Family>::new(strict), TD_Opaque)
}

extern "C" fn set_host_log_sink<P: Plugin>(sink: usize) {
    crate::logging::register_host_log_sink(sink);
    if sink != 0 {
        log_build_id_once::<P>();
    }
}

extern "C" fn declared_families<P: Plugin>() -> RVec<RSourceFamily> {
    source_families::<P::Family>()
}

/// `BridgeLib::probe` of family `F`: IEC 61937 by burst type, raw by the
/// family's own [`FamilyPipeline::probe_raw`].
pub extern "C" fn probe<F: FamilyPipeline>(
    data: RSlice<'_, u8>,
    transport: RInputTransport,
    data_type: u8,
) -> RProbe {
    crate::probe::probe(
        data.as_slice(),
        transport,
        data_type,
        F::accepts_data_type,
        F::probe_raw,
    )
}

/// `BridgeLib::input_codecs` of family `F`.
pub extern "C" fn input_codecs<F: FamilyPipeline>() -> RVec<RString> {
    F::INPUT_CODECS.iter().map(|&name| name.into()).collect()
}
