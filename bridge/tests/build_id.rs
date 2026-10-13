//! The library says what it was built from (#121): its version, the
//! `bridge_api` it links and the Omniphony commit, once, through the host's
//! sink. A process of its own, so no other test has logged it first.

use abi_stable::std_types::RStr;
use bridge_api::RLogLevel;
use std::sync::Mutex;

static RECEIVED: Mutex<Vec<String>> = Mutex::new(Vec::new());

extern "C" fn sink(_level: RLogLevel, _target: RStr<'_>, message: RStr<'_>) {
    RECEIVED.lock().unwrap().push(message.as_str().to_owned());
}

#[test]
fn the_build_id_is_logged_once_through_the_host_sink() {
    harletty_bridge::set_log_sink(sink);
    let _first = harletty_bridge::new_bridge(false);
    // A host installs its sink again for every bridge it opens.
    harletty_bridge::set_log_sink(sink);
    let _second = harletty_bridge::new_bridge(false);

    let id = harletty_bridge::build_id();
    let received = RECEIVED.lock().unwrap();
    let logged: Vec<_> = received.iter().filter(|m| **m == id).collect();
    assert_eq!(logged.len(), 1, "build id {id:?} in {received:?}");

    assert!(id.contains(env!("CARGO_PKG_VERSION")), "{id}");
    assert!(
        id.contains(&format!("bridge_api {}", bridge_api::VERSION)),
        "{id}"
    );
    assert!(id.contains(harletty_bridge::OMNIPHONY_COMMIT), "{id}");
}

#[test]
fn the_omniphony_commit_is_a_sha_or_unknown() {
    let commit = harletty_bridge::OMNIPHONY_COMMIT;
    // The release workflow sets the variable for the build and the tests
    // alike: the library must carry exactly the commit it pinned.
    if let Some(expected) = std::env::var("HARLETTY_OMNIPHONY_COMMIT")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        assert_eq!(commit, expected.trim());
    }
    assert!(
        commit == "unknown"
            || (commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit())),
        "{commit:?}"
    );
}
