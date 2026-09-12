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

#[cfg(target_arch = "x86_64")]
use sha2::{Digest, Sha256};

/// Length of the decoded CAF payload. Unlike its bytes, this is a property of
/// the stream and not of the build, so it is checked on every target.
const AUDIO_LEN: u64 = 4_548_164;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[cfg(target_arch = "x86_64")]
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

    let status = Command::new(env!("CARGO_BIN_EXE_harletty"))
        .args(["--loglevel", "error", "decode"])
        .arg(fixture("joc_atmos_1s.eac3"))
        .arg("--output-path")
        .arg(&out_base)
        .status()
        .expect("failed to run the harletty binary");
    assert!(status.success(), "harletty decode failed: {status}");

    // .atmos — the presentation. creationToolVersion tracks this package's
    // version, so it is normalised out.
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

    // creationTool is the tool's own identity, taken from CARGO_PKG_NAME. No
    // consumer reads it — Atmos Ranker reads only sourceCodec and language —
    // so it is pinned here simply to catch it changing unnoticed, not because
    // anything downstream depends on the value.
    assert!(
        produced.contains("    creationTool: harletty\n"),
        "creationTool should name this binary. Produced:\n{produced}"
    );
    // sourceCodec, by contrast, *is* a contract: Atmos Ranker folds it into
    // the codec column its Rank filter groups by.
    assert!(
        produced.contains("    sourceCodec: EAC3-JOC\n"),
        "sourceCodec label drifted; Atmos Ranker folds it into the codec column"
    );

    // .atmos.metadata — the per-event object metadata. Compared verbatim.
    let produced_meta = std::fs::read_to_string(out_dir.join("out.atmos.metadata")).unwrap();
    let golden_meta = std::fs::read_to_string(fixture("joc_atmos_1s.atmos.metadata")).unwrap();
    assert_eq!(produced_meta, golden_meta, ".atmos.metadata drifted");

    // .atmos.audio — too big to commit, so what can be pinned is pinned.
    let audio_path = out_dir.join("out.atmos.audio");
    let produced_len = std::fs::metadata(&audio_path).unwrap().len();
    assert_eq!(produced_len, AUDIO_LEN, "decoded audio length drifted");

    // The exact bytes, unlike everything above, are not a property of the
    // source alone. This decoder amplifies any last-bit perturbation anywhere
    // upstream to a fixed -42 dBFS peak / -86 dBFS RMS on ~1% of samples, so a
    // hash pins one target rather than the algorithm. Decoding this same
    // fixture with the same source:
    //
    //     x86_64-unknown-linux-gnu     4c8132a4…   (the committed hash)
    //     aarch64-unknown-linux-musl   —           (NEON QMF path, differs)
    //
    // Both are correct decodes. Asserting the hash off x86_64 would report a
    // failure that is not a regression, so it is scoped rather than dropped —
    // on x86_64 it stays a byte-exact tripwire, which is what it is good at.
    // The aarch64 value moved with the last rebase below and was not
    // recomputed; nothing asserts it, so it is left unstated rather than stale.
    //
    // Rebased five times: once when `float_to_i24` stopped scaling by
    // 2^23 - 1 and truncating (1.6% of samples moved one count away from zero,
    // no sign flips, max delta 1), once when the QMF scalar fallbacks stopped
    // accumulating into a single sum, once when the JOC parameter bands started
    // being expanded onto the subbands they cover on every path, once when
    // the core was delayed to meet the objects it is written beside
    // (`JOC_LATENCY_SAMPLES`) — the bed channels of every JOC frame shift 577
    // samples later — and once when the two short transforms stopped reading
    // the flat coefficient array at the long transform's stride. This fixture
    // carries exactly one short block: frame 31 of its 47 sets `blkswe`, and
    // its last block, block 5, switches the front-left channel. That one block
    // moves 785 of 72 192 samples, 1.1%, every one of them inside the five
    // consecutive 256-sample blocks running from it to four blocks into frame
    // 32 - the block after a short block reads back the delay it left behind,
    // so the damage outlives the block that caused it. Two of the twenty-one
    // master-set channels carry it; the other nineteen, and every other frame,
    // are bit-identical. Both moved channels are near full scale there, so most
    // of the moved samples change sign rather than shift; the clip's peak does
    // not move (0.999969 and 0.999970, before and after). The last three change
    // the audio on purpose; that is what they are for.
    #[cfg(target_arch = "x86_64")]
    {
        let produced_audio = sha256_of(&audio_path);
        let golden_audio =
            std::fs::read_to_string(fixture("joc_atmos_1s.atmos.audio.sha256")).unwrap();
        assert_eq!(
            produced_audio,
            golden_audio.trim(),
            "decoded audio drifted (CAF payload sha256)"
        );
    }
}
