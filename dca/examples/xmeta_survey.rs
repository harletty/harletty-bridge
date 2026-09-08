// SPDX-License-Identifier: Apache-2.0
// Per-frame survey of the private DTS:X metadata as the library reads it:
// parse result per frame, and every change of object position or fold.
// Research tool only; it is not used by the realtime decoder.
//
// cargo run -p dca --release --example xmeta_survey -- [--max-mb 64] input.dts

use std::collections::BTreeMap;
use std::io::{BufReader, Read};

use dca::{
    BedFold, HdDecoder, HdError, SourceRole, XMetadata, XPresentation, exss_substream_size,
    parse_header,
};

fn describe(metadata: &XMetadata) -> String {
    metadata
        .sources()
        .enumerate()
        .map(|(feed, source)| {
            let fold = match source.fold {
                BedFold::Known(columns) => columns
                    .iter()
                    .enumerate()
                    .filter(|(_, gain)| **gain != 0.0)
                    .map(|(column, gain)| format!("{column}:{:.3}", *gain))
                    .collect::<Vec<_>>()
                    .join(","),
                BedFold::Unknown => "unknown".to_string(),
            };
            match source.role {
                SourceRole::Height(channel) => format!("X{feed}={channel:?}[{fold}]"),
                SourceRole::Object {
                    position,
                    centre_height_alternative,
                } => format!(
                    "X{feed}=obj({:.1},{:.1},{:.2}){}[{fold}]",
                    position.azimuth_degrees(),
                    position.elevation_degrees(),
                    position.distance(),
                    if centre_height_alternative { "+Ch" } else { "" }
                ),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mut limit = 64usize * 1024 * 1024;
    let mut path = None;
    while let Some(arg) = args.next() {
        if arg == "--max-mb" {
            limit = args
                .next()
                .ok_or("missing --max-mb")?
                .parse::<usize>()
                .map_err(|_| "invalid --max-mb")?
                .saturating_mul(1024 * 1024);
        } else {
            path = Some(arg);
        }
    }
    let path = path.ok_or("usage: xmeta_survey [--max-mb 64] input.dts")?;
    let mut reader = BufReader::new(std::fs::File::open(&path).map_err(|e| e.to_string())?);
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(limit as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;

    let mut decoder = HdDecoder::new();
    let mut offset = 0;
    let mut frames = 0usize;
    let mut sample_offset = 0u64;
    let mut ok = 0usize;
    let mut errors = BTreeMap::<String, usize>::new();
    let mut last = None::<String>;
    let mut changes = 0usize;
    while offset + 18 <= bytes.len() {
        let info = parse_header(&bytes[offset..]).map_err(|e| format!("core header: {e:?}"))?;
        let core_end = offset + info.frame_size;
        if core_end + 16 > bytes.len() {
            break;
        }
        let Some(exss_size) = exss_substream_size(&bytes[core_end..]) else {
            break;
        };
        let exss_end = core_end + exss_size;
        if exss_end > bytes.len() {
            break;
        }
        match decoder.decode(&bytes[offset..core_end], &bytes[core_end..exss_end]) {
            Ok(frame) => {
                frames += 1;
                let presentation = XPresentation::detect(&frame);
                let description = match presentation {
                    Some(presentation) => {
                        match XMetadata::parse(&frame.x_payload, presentation.feed_count()) {
                            Ok(metadata) => {
                                ok += 1;
                                describe(&metadata)
                            }
                            Err(error) => {
                                let kind = format!("{error:?}");
                                *errors.entry(kind.clone()).or_default() += 1;
                                format!("ERROR {kind}")
                            }
                        }
                    }
                    None => "no presentation".to_string(),
                };
                if last.as_deref() != Some(&description) {
                    changes += 1;
                    println!(
                        "frame={} sample={} t={:.2}s {description}",
                        frames - 1,
                        sample_offset,
                        sample_offset as f64 / f64::from(frame.sample_rate.max(1))
                    );
                    last = Some(description);
                }
                sample_offset += frame.bed_sample_count() as u64;
            }
            Err(HdError::Pending) => {}
            Err(error) => return Err(format!("decode error at byte {offset}: {error:?}")),
        }
        offset = exss_end;
    }
    println!("frames={frames} parsed_ok={ok} changes={changes} errors={errors:?} bytes={offset}");
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("xmeta survey: {error}");
        std::process::exit(1);
    }
}
