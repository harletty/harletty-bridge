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
    assert_eq!(versions["taxonomy"], 2);
    assert_eq!(versions["harletty"], env!("CARGO_PKG_VERSION"));
    assert!(versions["build"].as_str().is_some_and(|s| !s.is_empty()));
}

#[test]
fn the_joc_fixture_reports_its_presentation_and_stops_at_the_bound() {
    let path = fixture("joc_atmos_1s.eac3");
    let report = harletty(&["info", "--json", path.to_str().unwrap()]);
    assert_eq!(report["schema"], 1);
    assert_eq!(report["taxonomy"], 2);
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
