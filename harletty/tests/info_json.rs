// SPDX-License-Identifier: Apache-2.0
//! `info --json` and `taxonomy` through the built binary: the committed
//! E-AC-3 fixture always, DTS and TrueHD corpus files when the variables
//! name them (each such test self-skips, loudly, otherwise).

use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn harletty(args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_harletty"))
        .args(["--loglevel", "off"])
        .args(args)
        .output()
        .expect("run harletty");
    assert!(
        output.status.success(),
        "harletty {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    assert_eq!(stdout.lines().count(), 1, "one line on stdout: {stdout:?}");
    serde_json::from_str(&stdout).expect("one JSON object on stdout")
}

fn corpus(variable: &str) -> Option<String> {
    let path = std::env::var(variable).ok()?;
    if !std::path::Path::new(&path).is_file() {
        eprintln!("skipping: {variable}={path} is not a file");
        return None;
    }
    Some(path)
}

#[test]
fn taxonomy_reports_its_versions() {
    let versions = harletty(&["taxonomy"]);
    assert_eq!(versions["schema"], 1);
    assert_eq!(versions["taxonomy"], 3);
    assert_eq!(versions["harletty"], env!("CARGO_PKG_VERSION"));
    assert!(versions["build"].as_str().is_some_and(|s| !s.is_empty()));
}

#[test]
fn the_joc_fixture_reports_its_presentation_and_stops_at_the_bound() {
    let path = fixture("joc_atmos_1s.eac3");
    let report = harletty(&["info", "--json", path.to_str().unwrap()]);
    assert_eq!(report["schema"], 1);
    assert_eq!(report["taxonomy"], 3);
    assert_eq!(report["codec"], "EAC3");
    assert_eq!(report["channels"], 6);
    assert_eq!(report["sample_rate"], 48_000);
    assert_eq!(report["spatial"]["label"], "EAC3-JOC");
    assert_eq!(report["spatial"]["kind"], "joc");
    assert_eq!(report["spatial"]["experimental"], false);
    assert_eq!(report["eac3"]["joc"], true);
    assert_eq!(report["eac3"]["oamd"], true);
    assert_eq!(report["eac3"]["spx"], false);
    assert!(report.get("error").is_none());
    let seconds = report["seconds_seen"].as_f64().unwrap();
    assert_eq!(report["frames_seen"], 47);
    assert!(
        (1.4..=1.6).contains(&seconds),
        "47 frames of 1536 samples: {seconds}"
    );

    let bounded = harletty(&[
        "info",
        "--json",
        "--max-seconds",
        "0.25",
        path.to_str().unwrap(),
    ]);
    let seconds = bounded["seconds_seen"].as_f64().unwrap();
    assert!(
        (0.25..0.5).contains(&seconds),
        "stops at the bound: {seconds}"
    );
    assert!(bounded["frames_seen"].as_u64().unwrap() < report["frames_seen"].as_u64().unwrap());
    assert_eq!(bounded["spatial"]["label"], "EAC3-JOC");
}

#[test]
fn the_text_report_is_unchanged_by_the_bound() {
    let path = fixture("joc_atmos_1s.eac3");
    let output = Command::new(env!("CARGO_BIN_EXE_harletty"))
        .args(["--loglevel", "off", "info", "--max-seconds", "0.25"])
        .arg(&path)
        .output()
        .expect("run harletty");
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("JOC          : yes"), "{text}");
    assert!(!text.trim_start().starts_with('{'));
}

#[test]
fn a_three_component_dtsx_corpus_reports_three_objects() {
    let Some(path) = corpus("HARLETTY_ALT_LCR_CORPUS") else {
        eprintln!("skipping: HARLETTY_ALT_LCR_CORPUS is not set");
        return;
    };
    let report = harletty(&["info", "--json", "--max-seconds", "4", &path]);
    assert_eq!(report["codec"], "DTS-HD MA");
    assert_eq!(report["channels"], 8);
    assert_eq!(report["sample_rate"], 48_000);
    assert_eq!(report["spatial"]["label"], "DTS:X-7.1.4+3");
    assert_eq!(report["spatial"]["kind"], "dtsx");
    assert_eq!(report["spatial"]["objects"], 3);
    assert_eq!(report["spatial"]["fixed"], 4);
    assert_eq!(report["spatial"]["experimental"], true);
    assert_eq!(report["spatial"]["presentation"], "ObjectsD0");
    assert!(report.get("auro").is_none());
}

#[test]
fn a_standard_dtsx_corpus_reports_the_height_quartet() {
    let Some(path) = corpus("HARLETTY_DTSX_STANDARD_CORPUS") else {
        eprintln!("skipping: HARLETTY_DTSX_STANDARD_CORPUS is not set");
        return;
    };
    let report = harletty(&["info", "--json", "--max-seconds", "4", &path]);
    assert_eq!(report["codec"], "DTS-HD MA");
    assert_eq!(report["spatial"]["label"], "DTS:X-7.1.4");
    assert_eq!(report["spatial"]["objects"], 0);
    assert_eq!(report["spatial"]["fixed"], 4);
    assert_eq!(report["spatial"]["experimental"], false);
}

#[test]
fn a_truehd_corpus_reports_its_presentations_within_the_bound() {
    let Some(path) = corpus("HARLETTY_TRUEHD_CORPUS") else {
        eprintln!("skipping: HARLETTY_TRUEHD_CORPUS is not set");
        return;
    };
    let report = harletty(&["info", "--json", "--max-seconds", "2", &path]);
    assert_eq!(report["codec"], "TrueHD");
    assert_eq!(report["sample_rate"], 48_000);
    assert!(report["channels"].as_u64().is_some_and(|c| c >= 2));
    assert!(report["truehd"]["max_presentation"].as_u64().is_some());
    assert!(
        report["truehd"]["substreams"]
            .as_u64()
            .is_some_and(|s| s >= 1)
    );
    let atmos = report["truehd"]["atmos"].as_bool().unwrap();
    assert_eq!(report["spatial"].is_null(), !atmos);
    if atmos {
        assert_eq!(report["spatial"]["label"], "TrueHD");
        assert_eq!(report["spatial"]["kind"], "atmos");
    }
    let seconds = report["seconds_seen"].as_f64().unwrap();
    assert!(
        (2.0..2.1).contains(&seconds),
        "stops at the bound: {seconds}"
    );
}

/// A settings file in the test's own temporary directory, so the machine's
/// own configuration — which may hold a real key — plays no part.
fn config(name: &str, body: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, body).expect("write the settings file");
    path
}

#[test]
fn a_stream_read_without_a_key_reports_an_unchecked_signature() {
    let Some(path) = corpus("HARLETTY_TRUEHD_CORPUS") else {
        eprintln!("skipping: HARLETTY_TRUEHD_CORPUS is not set");
        return;
    };
    let empty = config("no-key.yaml", "# no key here\n");
    let report = harletty(&[
        "--config",
        empty.to_str().unwrap(),
        "info",
        "--json",
        "--max-seconds",
        "2",
        &path,
    ]);
    assert_eq!(report["signature"]["state"], "unchecked");
    assert_eq!(report["signature"]["units"], 0);
    assert_eq!(report["signature"]["checked"], 0);

    // And the same with a key configured but the check turned off.
    let keyed = config(
        "dummy-key.yaml",
        &format!("evolution_key: \"{}\"\n", "00".repeat(32)),
    );
    let skipped = harletty(&[
        "--config",
        keyed.to_str().unwrap(),
        "info",
        "--json",
        "--max-seconds",
        "2",
        "--no-signature",
        &path,
    ]);
    assert_eq!(skipped["signature"]["state"], "unchecked");
}

/// A key that did not sign the stream must not verify it: whatever the
/// corpus carries, every protection word in it is refused. A corpus that
/// carries none says so instead, which is the other half of the contract.
#[test]
fn a_key_that_signed_nothing_verifies_nothing() {
    let Some(path) = corpus("HARLETTY_TRUEHD_CORPUS") else {
        eprintln!("skipping: HARLETTY_TRUEHD_CORPUS is not set");
        return;
    };
    let wrong = config(
        "wrong-key.yaml",
        &format!("evolution_key: \"{}\"\n", "5a".repeat(32)),
    );
    let report = harletty(&[
        "--config",
        wrong.to_str().unwrap(),
        "info",
        "--json",
        "--max-seconds",
        "2",
        &path,
    ]);
    let signature = &report["signature"];
    assert!(signature["units"].as_u64().unwrap() > 0, "{signature}");
    assert_eq!(signature["verified"], 0, "{signature}");
    if signature["checked"].as_u64().unwrap() > 0 {
        assert_eq!(signature["state"], "mismatch", "{signature}");
        assert_eq!(signature["mismatched"], signature["checked"], "{signature}");
    } else if signature["frames"].as_u64().unwrap() > 0 {
        assert_eq!(signature["state"], "unsigned", "{signature}");
    } else {
        assert_eq!(signature["state"], "absent", "{signature}");
    }
}

/// `harletty info --json -` with `input` on stdin, the way a catalogue sends
/// the head of a stream it copied out of a container.
fn harletty_piped(args: &[&str], input: &[u8]) -> Value {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new(env!("CARGO_BIN_EXE_harletty"))
        .args(["--loglevel", "off"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run harletty");
    let mut stdin = child.stdin.take().expect("stdin");
    let bytes = input.to_vec();
    // The decoder may stop reading before the input ends; the write then
    // fails harmlessly.
    let feeder = std::thread::spawn(move || {
        let _ = stdin.write_all(&bytes);
    });
    let output = child.wait_with_output().expect("wait for harletty");
    let _ = feeder.join();
    assert!(
        output.status.success(),
        "harletty {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("one JSON object on stdout")
}

/// A pipe cannot be reopened at its first byte the way a file can, so the
/// bytes the codec probe read have to be read again: the same stream piped in
/// reports what it reports from a file, first frames included.
#[test]
fn a_piped_stream_reads_like_the_same_file() {
    let path = fixture("joc_atmos_1s.eac3");
    let from_file = harletty(&["info", "--json", path.to_str().unwrap()]);
    let piped = harletty_piped(&["info", "--json", "-"], &std::fs::read(&path).unwrap());
    assert_eq!(piped["codec"], "EAC3");
    assert_eq!(piped["frames_seen"], from_file["frames_seen"]);
    assert_eq!(piped["spatial"], from_file["spatial"]);
}

/// A TrueHD head shorter than the codec probe's own read — a catalogue
/// sends a few hundred bytes — is the whole of what arrives, and still has
/// to be read as TrueHD.
#[test]
fn a_short_truehd_head_piped_in_is_still_truehd() {
    let Some(path) = corpus("HARLETTY_TRUEHD_CORPUS") else {
        eprintln!("skipping: HARLETTY_TRUEHD_CORPUS is not set");
        return;
    };
    let bytes = std::fs::read(&path).unwrap();
    let head = &bytes[..bytes.len().min(4096)];
    let report = harletty_piped(&["info", "--json", "--max-seconds", "4", "-"], head);
    assert_eq!(report["codec"], "TrueHD", "{report}");
    assert!(report["frames_seen"].as_u64().unwrap() > 0, "{report}");
}
