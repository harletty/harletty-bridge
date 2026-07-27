use abi_stable::std_types::RStr;
use bridge_api::{BridgeHostLogSink, RLogLevel};
use std::sync::{Mutex, OnceLock};

static HOST_LOG_SINK: Mutex<Option<BridgeHostLogSink>> = Mutex::new(None);
static DRC_LOG_ENABLED: OnceLock<bool> = OnceLock::new();

pub(crate) extern "C" fn register_host_log_sink(sink: usize) {
    let mut slot = HOST_LOG_SINK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    *slot = if sink == 0 {
        None
    } else {
        Some(unsafe { std::mem::transmute::<usize, BridgeHostLogSink>(sink) })
    };
}

pub(crate) fn bridge_diag_log(level: log::Level, message: &str) {
    bridge_external_log(level, "harletty-bridge::diag", message);
}

pub(crate) fn drc_diag_log_enabled() -> bool {
    *DRC_LOG_ENABLED.get_or_init(|| {
        std::env::var_os("HARLETTY_LOG_DRC")
            .map(|value| value != "0")
            .unwrap_or(false)
    })
}

pub(crate) fn bridge_external_log(level: log::Level, target: &str, message: &str) {
    let trimmed = message.trim_end_matches('\n');
    let sink = {
        let slot = HOST_LOG_SINK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *slot
    };
    if let Some(callback) = sink {
        callback(
            encode_log_level(level),
            RStr::from(target),
            RStr::from(trimmed),
        );
    } else {
        eprintln!("{trimmed}");
    }
}

pub(crate) fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "Unknown panic during frame processing".to_string()
    }
}

fn encode_log_level(level: log::Level) -> RLogLevel {
    match level {
        log::Level::Error => RLogLevel::Error,
        log::Level::Warn => RLogLevel::Warn,
        log::Level::Info => RLogLevel::Info,
        log::Level::Debug => RLogLevel::Debug,
        log::Level::Trace => RLogLevel::Trace,
    }
}
