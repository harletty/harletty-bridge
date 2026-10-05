//! Bridge diagnostics, forwarded to the host's log sink (stderr without one).
//!
//! Several call sites sit in the per-packet decode path, so a message the host
//! would drop must cost nothing: [`bridge_log!`] compares the level with a
//! cached maximum before it evaluates or formats anything, and the sink is an
//! atomic, not a lock. The maximum is `HARLETTY_LOG` (`off`, `error`, `warn`,
//! `info`, `debug`, `trace`), `info` when unset; a host can change it at any
//! time through the `log_level` configure key.

use abi_stable::std_types::RStr;
use bridge_api::{BridgeHostLogSink, RLogLevel};
use std::fmt;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

/// The target of the bridge's general diagnostics.
pub(crate) const DIAG_TARGET: &str = "harletty-bridge::diag";

/// The host's sink as a function address, 0 while none is registered.
static HOST_LOG_SINK: AtomicUsize = AtomicUsize::new(0);
/// The most verbose level forwarded, as a `log::LevelFilter` discriminant, or
/// [`LEVEL_UNSET`] until `HARLETTY_LOG` has been read.
static MAX_LEVEL: AtomicU8 = AtomicU8::new(LEVEL_UNSET);
const LEVEL_UNSET: u8 = u8::MAX;
const DEFAULT_LEVEL: log::LevelFilter = log::LevelFilter::Info;
static DRC_LOG_ENABLED: OnceLock<bool> = OnceLock::new();

/// Serialises the tests that change the process-wide level.
#[cfg(test)]
pub(crate) static LEVEL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Log through the host sink, formatting only when `level` is enabled.
///
/// `bridge_log!(level, "fmt", args..)` logs to [`DIAG_TARGET`];
/// `bridge_log!(target: "...", level, "fmt", args..)` picks the target.
macro_rules! bridge_log {
    (target: $target:expr, $level:expr, $($arg:tt)+) => {{
        let level: ::log::Level = $level;
        if $crate::logging::log_enabled(level) {
            $crate::logging::forward_log(level, $target, &::std::fmt::format(format_args!($($arg)+)));
        }
    }};
    ($level:expr, $($arg:tt)+) => {
        $crate::logging::bridge_log!(target: $crate::logging::DIAG_TARGET, $level, $($arg)+)
    };
}
pub(crate) use bridge_log;

pub(crate) extern "C" fn register_host_log_sink(sink: usize) {
    HOST_LOG_SINK.store(sink, Ordering::Release);
}

/// Whether a message at `level` would be forwarded.
#[inline]
pub(crate) fn log_enabled(level: log::Level) -> bool {
    let max = match MAX_LEVEL.load(Ordering::Relaxed) {
        LEVEL_UNSET => max_level_from_env(),
        max => max,
    };
    level as u8 <= max
}

#[cold]
fn max_level_from_env() -> u8 {
    let level = std::env::var("HARLETTY_LOG")
        .ok()
        .and_then(|value| value.trim().parse::<log::LevelFilter>().ok())
        .unwrap_or(DEFAULT_LEVEL) as u8;
    // A level the host set in the meantime wins over the environment.
    match MAX_LEVEL.compare_exchange(LEVEL_UNSET, level, Ordering::Relaxed, Ordering::Relaxed) {
        Ok(_) => level,
        Err(current) => current,
    }
}

/// Set the most verbose level forwarded (the `log_level` configure key).
pub(crate) fn set_max_level(level: log::LevelFilter) {
    MAX_LEVEL.store(level as u8, Ordering::Relaxed);
}

/// Log an already formatted message, if `level` is enabled.
pub(crate) fn bridge_diag_log(level: log::Level, message: &str) {
    if log_enabled(level) {
        forward_log(level, DIAG_TARGET, message);
    }
}

pub(crate) fn drc_diag_log_enabled() -> bool {
    *DRC_LOG_ENABLED.get_or_init(|| {
        std::env::var_os("HARLETTY_LOG_DRC")
            .map(|value| value != "0")
            .unwrap_or(false)
    })
}

/// Hand a message to the host sink, or stderr without one. The level check is
/// the caller's: use [`bridge_log!`] or [`bridge_diag_log`].
pub(crate) fn forward_log(level: log::Level, target: &str, message: &str) {
    let trimmed = message.trim_end_matches('\n');
    match HOST_LOG_SINK.load(Ordering::Acquire) {
        0 => eprintln!("{trimmed}"),
        sink => {
            // SAFETY: a non-zero value was stored by `register_host_log_sink`,
            // which the host calls with a `BridgeHostLogSink` as `usize`.
            let callback = unsafe { std::mem::transmute::<usize, BridgeHostLogSink>(sink) };
            callback(
                encode_log_level(level),
                RStr::from(target),
                RStr::from(trimmed),
            );
        }
    }
}

/// Bytes as space-separated upper-case hex pairs, formatted only when shown.
pub(crate) struct HexBytes<'a>(pub &'a [u8]);

impl fmt::Display for HexBytes<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, byte) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            write!(f, "{byte:02X}")?;
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct CountFormats<'a>(&'a Cell<u32>);

    impl fmt::Display for CountFormats<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.set(self.0.get() + 1);
            f.write_str("x")
        }
    }

    #[test]
    fn a_disabled_level_formats_nothing() {
        let _guard = LEVEL_TEST_LOCK.lock().unwrap();
        let formats = Cell::new(0);
        set_max_level(log::LevelFilter::Info);
        assert!(log_enabled(log::Level::Error));
        assert!(log_enabled(log::Level::Info));
        assert!(!log_enabled(log::Level::Debug));
        bridge_log!(log::Level::Debug, "{}", CountFormats(&formats));
        assert_eq!(formats.get(), 0);
        bridge_log!(log::Level::Info, "{}", CountFormats(&formats));
        assert_eq!(formats.get(), 1);

        set_max_level(log::LevelFilter::Off);
        assert!(!log_enabled(log::Level::Error));
        set_max_level(log::LevelFilter::Trace);
        assert!(log_enabled(log::Level::Trace));
        set_max_level(DEFAULT_LEVEL);
    }

    #[test]
    fn hex_bytes_match_the_joined_preview() {
        let bytes = [0x0B, 0x77, 0x00, 0xFF];
        let joined = bytes
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(HexBytes(&bytes).to_string(), joined);
        assert_eq!(HexBytes(&[]).to_string(), "");
    }
}
