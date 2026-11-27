//! A logger that one thread can silence for a while.
//!
//! The parallel TrueHD decoder replays the stretch of stream before each run of
//! access units it hands a worker, so that the worker reaches the run's first
//! major sync in the state a decoder that never stopped would be in. What that
//! replay logs was already logged by the worker that decoded the stretch for
//! real, so the replaying thread mutes itself for its duration.

use std::cell::Cell;

thread_local! {
    static MUTED: Cell<bool> = const { Cell::new(false) };
}

fn muted() -> bool {
    MUTED.with(Cell::get)
}

/// Wraps the logger the CLI installs; records from a muted thread go nowhere.
pub struct Mutable<L>(pub L);

impl<L: log::Log> log::Log for Mutable<L> {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        !muted() && self.0.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        if !muted() {
            self.0.log(record);
        }
    }

    fn flush(&self) {
        self.0.flush();
    }
}

/// Silences the current thread's log records until it is dropped.
pub struct Mute(());

impl Mute {
    pub fn new() -> Self {
        MUTED.with(|m| m.set(true));
        Mute(())
    }
}

impl Drop for Mute {
    fn drop(&mut self) {
        MUTED.with(|m| m.set(false));
    }
}
