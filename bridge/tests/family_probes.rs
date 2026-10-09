//! Each family's probe on the baseline corpus, when
//! `HARLETTY_BASELINE_CORPUS` names its manifest (`<family> <transport>
//! <path>` per line; the streams are not in the repository): a stream is
//! claimed by its own family where it starts, however a host cuts it into
//! reads, and by no other family anywhere in its opening.
#![cfg(all(feature = "dolby", feature = "dts"))]

use bridge_api::RProbe;
use bridge_common::probe::replay;

/// How much of each stream is scanned.
const OPENING: usize = 1 << 20;

/// A family's raw probe.
type Probe = fn(&[u8]) -> RProbe;

fn families() -> Vec<(&'static str, Probe)> {
    #[cfg_attr(not(feature = "iamf"), allow(unused_mut))]
    let mut families: Vec<(&'static str, Probe)> = vec![
        ("dolby", bridge_family_dolby::probe::probe_raw),
        ("dts", bridge_family_dts::probe::probe_raw),
    ];
    #[cfg(feature = "iamf")]
    families.push(("iamf", bridge_family_iamf::probe::probe_raw));
    families
}

#[test]
fn every_corpus_stream_is_claimed_by_its_family_only() {
    let Some(manifest) = std::env::var_os("HARLETTY_BASELINE_CORPUS") else {
        eprintln!("skipping: HARLETTY_BASELINE_CORPUS is not set");
        return;
    };
    let manifest = std::fs::read_to_string(manifest).unwrap();
    let mut checked = 0;
    for line in manifest.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [family, "raw", path, ..] = fields.as_slice() else {
            continue;
        };
        if !families().iter().any(|(name, _)| name == family) {
            eprintln!("skipping {path}: family {family} not built");
            continue;
        }
        let mut stream = std::fs::read(path).unwrap();
        stream.truncate(OPENING);
        for (name, probe) in families() {
            if name == *family {
                for read in [1, 7, 4096, 64 * 1024] {
                    let found = replay(probe, &stream, read);
                    assert_eq!(found.claim, Some(0), "{path}: {name}, reads of {read}");
                }
            } else {
                let found = replay(probe, &stream, 64 * 1024);
                assert_eq!(found.claim, None, "{path}: claimed by {name}");
                let found = replay(probe, &stream[..stream.len().min(1 << 16)], 1);
                assert_eq!(found.claim, None, "{path}: claimed by {name}, byte by byte");
            }
        }
        checked += 1;
    }
    eprintln!("{checked} corpus streams checked");
    assert!(checked > 0);
}

/// An IAMF stream whose reserved OBU, right after the sequence header,
/// carries a whole E-AC-3 frame: the IAMF probe claims the stream where it
/// starts, the Dolby probe only the frame inside it, later, so a host that
/// routes to the earliest start picks IAMF, however the bytes are cut.
#[cfg(feature = "iamf")]
#[test]
fn a_sync_word_inside_an_iamf_prefix_starts_later_than_the_stream() {
    const EAC3: &[u8] = include_bytes!("../../harletty/tests/fixtures/joc_atmos_1s.eac3");
    let frame_len = ((usize::from(EAC3[2] & 0x07) << 8) | usize::from(EAC3[3])) * 2 + 2;
    let mut stream = vec![0xF8, 0x06, b'i', b'a', b'm', b'f', 0x00, 0x01];
    // A reserved OBU (type 24) whose payload is the frame.
    stream.push(24 << 3);
    let mut size = frame_len;
    loop {
        let byte = (size & 0x7F) as u8;
        size >>= 7;
        if size == 0 {
            stream.push(byte);
            break;
        }
        stream.push(byte | 0x80);
    }
    let frame_at = stream.len();
    stream.extend_from_slice(&EAC3[..frame_len]);
    for read in [1, 3, 64, 1 << 16] {
        let iamf = replay(bridge_family_iamf::probe::probe_raw, &stream, read);
        let dolby = replay(bridge_family_dolby::probe::probe_raw, &stream, read);
        assert_eq!(iamf.claim, Some(0), "reads of {read}");
        assert_eq!(dolby.claim, Some(frame_at), "reads of {read}");
        assert_eq!(
            replay(bridge_family_dts::probe::probe_raw, &stream, read).claim,
            None
        );
    }
}
