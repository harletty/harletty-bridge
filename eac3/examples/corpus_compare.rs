//! Decode every .bin in a directory tree and report harletty stats.
//!
//! `cargo run --example corpus_compare -p eac3 -- <dir>`
//!
//! For each access-unit dump:
//! - inspect_access_unit (header, JOC/OAMD count, EMDF source)
//! - try ObjectPcmDecoder::push_access_unit
//! - try PcmDecoder::push_access_unit
//! - print a TSV row with status and max-abs PCM amplitude.

use std::fs;
use std::path::{Path, PathBuf};

use eac3::{CorePcmFrame, ObjectPcmDecoder, ObjectPcmFrame, PcmDecoder, inspect_access_unit};

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "dumps".into());
    let path = PathBuf::from(&dir);

    let header = [
        "file", "bytes", "bsid", "sr", "blk", "ch", "lfe", "joc", "oamd", "snrstr", "fgsyn",
        "cplupd", "obj_st", "obj_max", "pcm_st", "pcm_max",
    ];
    println!("{}", header.join("\t"));

    let mut entries: Vec<PathBuf> = Vec::new();
    collect_bins(&path, &mut entries);
    entries.sort();

    for path in entries {
        let frame = match fs::read(&path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("read fail {}: {e}", path.display());
                continue;
            }
        };

        let info = inspect_access_unit(&frame);
        let (bsid, sr, blk, ch, lfe, joc, oamd, snrstr, fgsyn, cplupd) = match &info {
            Ok(i) => (
                i.bitstream_id.to_string(),
                i.sample_rate.to_string(),
                i.num_blocks.to_string(),
                i.channels.to_string(),
                (if i.lfe_on { "1" } else { "0" }).to_string(),
                i.joc_payload_count().to_string(),
                i.oamd_payload_count().to_string(),
                i.audio_frame.snr_offset_strategy.to_string(),
                (if i.audio_frame.frame_gain_syntax_enabled {
                    "1"
                } else {
                    "0"
                })
                .to_string(),
                format!(
                    "upd={:?}/inuse={:?}",
                    i.audio_frame.coupling_strategy_updates, i.audio_frame.coupling_in_use
                ),
            ),
            Err(e) => (
                format!("ERR:{e}"),
                "-".into(),
                "-".into(),
                "-".into(),
                "-".into(),
                "-".into(),
                "-".into(),
                "-".into(),
                "-".into(),
                "-".into(),
            ),
        };

        let mut obj = ObjectPcmDecoder::default();
        let (obj_st, obj_max) = match obj.push_access_unit(&frame) {
            Ok(Some(r)) => ("ok".into(), format!("{:.5}", max_abs_object(&r.pcm))),
            Ok(None) => ("none".into(), "-".into()),
            Err(e) => (format!("err:{e}"), "-".into()),
        };

        let mut pcm = PcmDecoder::default();
        let (pcm_st, pcm_max) = match pcm.push_access_unit(&frame) {
            Ok(r) => ("ok".into(), format!("{:.5}", max_abs_core(&r.pcm))),
            Err(e) => (format!("err:{e}"), "-".into()),
        };

        let display = path
            .strip_prefix(&dir)
            .unwrap_or(&path)
            .display()
            .to_string();

        println!(
            "{display}\t{}\t{bsid}\t{sr}\t{blk}\t{ch}\t{lfe}\t{joc}\t{oamd}\t{snrstr}\t{fgsyn}\t{cplupd}\t{obj_st}\t{obj_max}\t{pcm_st}\t{pcm_max}",
            frame.len()
        );
    }
}

fn collect_bins(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_bins(&p, out);
        } else if p.extension().and_then(|s| s.to_str()) == Some("bin") {
            out.push(p);
        }
    }
}

fn max_abs_core(frame: &CorePcmFrame) -> f32 {
    let mut m = 0.0_f32;
    for ch in &frame.fullband_channels {
        for &s in ch {
            if s.abs() > m {
                m = s.abs();
            }
        }
    }
    if let Some(lfe) = &frame.lfe_channel {
        for &s in lfe {
            if s.abs() > m {
                m = s.abs();
            }
        }
    }
    m
}

fn max_abs_object(frame: &ObjectPcmFrame) -> f32 {
    let mut m = max_abs_core(&frame.core);
    for ch in &frame.object_channels {
        for &s in ch {
            if s.abs() > m {
                m = s.abs();
            }
        }
    }
    m
}
