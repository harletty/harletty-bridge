use sys::live_log;

pub(crate) fn bridge_diag_log(level: log::Level, message: &str) {
    live_log::emit_external_record(
        level,
        "harletty-bridge::eac3",
        message.trim_end_matches('\n'),
    );
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
