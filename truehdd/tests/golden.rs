//! Golden-file regression for the offline decode path.
//!
//! When the CLI was resurrected (plan phase 2) it was validated by decoding
//! real streams with both the new binary and the reference one in
//! `reference-sources/truehdd`, and diffing the master sets byte for byte.
//! That reference tree is going away, so the property it proved is pinned here
//! instead: this fixture's master set must not change.
//!
//! The fixture is 1.5 s of the 7.1.4 Atmos channel-check clip (E-AC-3 JOC,
//! 768 kbit/s). It exercises the whole chain the CLI exists for —
//! eac3 decode -> JOC -> OAMD -> DAMF metadata + CAF audio — and it is the
//! path Atmos Ranker drives. Metadata is compared verbatim; the 4.5 MB of
//! audio is pinned by hash rather than committed.

use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn sha256_of(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn joc_master_set_matches_golden() {
    let out_dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("golden_joc");
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&out_dir).unwrap();

    // The base name leaks into the .atmos file (it names its sibling files), so
    // it has to match the one the golden was generated with.
    let out_base = out_dir.join("out");

    let status = Command::new(env!("CARGO_BIN_EXE_truehdd"))
        .args(["--loglevel", "error", "decode"])
        .arg(fixture("joc_atmos_1s.eac3"))
        .arg("--output-path")
        .arg(&out_base)
        .status()
        .expect("failed to run the truehdd binary");
    assert!(status.success(), "truehdd decode failed: {status}");

    // .atmos — the presentation. creationToolVersion tracks this package's
    // version, so it is normalised out; creationTool itself is the contract
    // Atmos Ranker reads and is asserted verbatim below.
    let produced = std::fs::read_to_string(out_dir.join("out.atmos")).unwrap();
    let normalised = produced
        .lines()
        .map(|line| match line.strip_prefix("    creationToolVersion: ") {
            Some(_) => "    creationToolVersion: {VERSION}".to_string(),
            None => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let golden = std::fs::read_to_string(fixture("joc_atmos_1s.atmos")).unwrap();
    assert_eq!(normalised, golden, ".atmos presentation drifted");

    assert!(
        produced.contains("    creationTool: truehdd\n"),
        "creationTool must stay `truehdd`; Atmos Ranker and the masters under \
         adm/ key off it. Produced:\n{produced}"
    );
    assert!(
        produced.contains("    sourceCodec: EAC3-JOC\n"),
        "sourceCodec label drifted; scan.rs folds it into the codec column"
    );

    // .atmos.metadata — the per-event object metadata. Compared verbatim.
    let produced_meta = std::fs::read_to_string(out_dir.join("out.atmos.metadata")).unwrap();
    let golden_meta = std::fs::read_to_string(fixture("joc_atmos_1s.atmos.metadata")).unwrap();
    assert_eq!(produced_meta, golden_meta, ".atmos.metadata drifted");

    // .atmos.audio — too big to commit, pinned by hash.
    let produced_audio = sha256_of(&out_dir.join("out.atmos.audio"));
    let golden_audio = std::fs::read_to_string(fixture("joc_atmos_1s.atmos.audio.sha256")).unwrap();
    assert_eq!(
        produced_audio,
        golden_audio.trim(),
        "decoded audio drifted (CAF payload sha256)"
    );
}
