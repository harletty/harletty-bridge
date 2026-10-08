//! The library says what it was built from: its name and version, the
//! `bridge_api` it links and the Omniphony commit, once, through the host's
//! sink. A process of its own, so no other test has logged it first.

use abi_stable::std_types::RStr;
use bridge_api::RLogLevel;
use harletty_iamf_bridge as plugin;
use std::sync::Mutex;

static RECEIVED: Mutex<Vec<String>> = Mutex::new(Vec::new());

extern "C" fn sink(_level: RLogLevel, _target: RStr<'_>, message: RStr<'_>) {
    RECEIVED.lock().unwrap().push(message.as_str().to_owned());
}

#[test]
fn the_build_id_is_logged_once_through_the_host_sink() {
    plugin::set_log_sink(sink);
    let _first = plugin::new_bridge(false);
    // A host installs its sink again for every bridge it opens.
    plugin::set_log_sink(sink);
    let _second = plugin::new_bridge(false);
    let id = plugin::build_id();
    let logged = RECEIVED
        .lock()
        .unwrap()
        .iter()
        .filter(|m| **m == id)
        .count();
    assert_eq!(logged, 1, "{id}");
    assert!(
        id.starts_with(concat!(
            env!("CARGO_PKG_NAME"),
            " ",
            env!("CARGO_PKG_VERSION"),
            " "
        )),
        "{id}"
    );
    assert!(
        id.contains(&format!("bridge_api {}", bridge_api::VERSION)),
        "{id}"
    );
}
