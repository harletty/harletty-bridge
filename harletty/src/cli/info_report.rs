// SPDX-License-Identifier: Apache-2.0
//! The machine-readable form of `harletty info`: one JSON object a caller
//! can rely on, so a catalogue classifies a track by asking this binary
//! instead of carrying its own copy of the decoders and of the label
//! taxonomy.
//!
//! Two numbers at the top of the object are the contract:
//!
//! - `schema` names the shape of the object. It changes only when a field
//!   is removed or changes meaning; a field added keeps the number.
//! - `taxonomy` names the set of strings `spatial.label` can carry: the
//!   DAMF `sourceCodec` labels `decode` writes. It is bumped whenever a
//!   label is added or renamed, so a catalogue that recorded the value it
//!   probed a track under knows which tracks to probe again.

use anyhow::Result;
use serde::Serialize;

use super::command::VERSION_INFO;

/// Shape of the object; see the module documentation.
pub const SCHEMA: u32 = 1;

/// The label set; see the module documentation.
///
/// 1: `TrueHD`, `EAC3-JOC`, `DTS:X-7.1.4`, `DTS:X-7.1.5`, `DTS:X-7.1.4+2`,
/// `DTS:X-7.1.4+3`, `DTS:X-7.1.4+4`, `DTS:X-7.1.4+5`, `DTS:X-5.1+1`,
/// `Auro-3D-9.1`, `Auro-3D-10.1`, `Auro-3D-11.1`, `Auro-3D-13.1`, `Auro-3D`.
///
/// 2: `DTS:X-7.1.5` becomes `DTS:X-7.1.4+1`. The five-feed D0 presentation's
/// first feed is an object at the position its record declares, not a fixed
/// centre-height channel; the same presentation also now reads two further
/// record grammars, so streams that reported no spatial metadata at all can
/// start reporting this label. A consumer holding the old label should map it
/// onto the new one and re-probe, since the feed count did not change but its
/// meaning did.
///
/// 3: DTS:X on a lossy carrier (DTS-HD High Resolution Audio: core + XXCH
/// with the extension after the asset) is decoded. Such tracks reported no
/// spatial metadata and `codec` `DTS`; they now report `DTS:X-7.1.4` and the
/// new `codec` value `DTS-HD HRA`. A consumer holding a `DTS` verdict for a
/// track whose container says DTS-HD HRA should re-probe it.
pub const TAXONOMY_VERSION: u32 = 3;

/// The versions alone, for `harletty taxonomy`.
#[derive(Debug, Serialize)]
struct Versions {
    schema: u32,
    harletty: &'static str,
    build: &'static str,
    taxonomy: u32,
}

/// One stream, as `info --json` reports it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InfoReport {
    pub schema: u32,
    /// The crate version.
    pub harletty: &'static str,
    /// The build: git describe, decoder library version, timestamp.
    pub build: &'static str,
    pub taxonomy: u32,
    /// `TrueHD`, `EAC3`, `DTS` (core only) or `DTS-HD MA`; `null` when no
    /// frame was found, with `error` saying why.
    pub codec: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Channels of the compatible bed. TrueHD: of the channel-based
    /// presentation the stream declares highest.
    pub channels: Option<u32>,
    pub sample_rate: Option<u32>,
    /// What the stream carries beyond its bed; `null` for a plain track.
    pub spatial: Option<Spatial>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truehd: Option<TrueHdFacts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eac3: Option<Eac3Facts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auro: Option<AuroFacts>,
    /// Whether the stream's metadata is signed by the key this machine
    /// holds. Absent for a codec whose signature this binary cannot check,
    /// which today is every codec but TrueHD.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<Signature>,
    /// Frames read before the report settled or the bound was reached.
    pub frames_seen: u64,
    /// Audio those frames cover.
    pub seconds_seen: f64,
}

/// The spatial presentation, named the way `decode` labels the master set.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Spatial {
    /// The DAMF `sourceCodec` label.
    pub label: String,
    /// `atmos`, `joc`, `dtsx` or `auro`.
    pub kind: &'static str,
    /// Waveforms presented as objects, when the presentation says.
    pub objects: Option<u32>,
    /// Waveforms presented as fixed channels beyond the bed, when the
    /// presentation says.
    pub fixed: Option<u32>,
    /// Whether the feed identities rest on corpus evidence rather than on an
    /// established layout.
    pub experimental: bool,
    /// The decoder's own name for the presentation (`ObjectsD4`, …); DTS:X
    /// only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presentation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TrueHdFacts {
    /// Highest independent presentation the major sync declares.
    pub max_presentation: Option<u8>,
    pub atmos: bool,
    pub substreams: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Eac3Facts {
    pub oamd: bool,
    pub joc: bool,
    /// Spectral Extension seen in use within the frames read; only measured
    /// for the JSON report, since it takes a PCM decode.
    pub spx: bool,
    pub bitstream_id: u8,
}

/// The Evolution protection word, as the frames read report it.
///
/// `state` is the verdict a caller displays:
///
/// - `verified`: every word read is the digest the key produces, so the
///   metadata and the audio under it are the encoder's, unaltered;
/// - `mismatch`: at least one is not, so something was rewritten after
///   signing, or another key signed it;
/// - `unsigned`: access units were read and none carried a word at all;
/// - `unchecked`: no key on this machine, so nothing was asked. Not a
///   verdict on the stream.
///
/// The counts are over the frames read, which `--max-seconds` bounds: a
/// verdict is about the head of the stream when the read was bounded.
///
/// Each digest covers the access unit carrying it, and about one access unit
/// in forty carries one, so `verified` says the stream came from an encoder
/// holding the key and was not re-encoded since — not that every sample is
/// accounted for.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Signature {
    pub state: &'static str,
    /// Access units parsed.
    pub units: u64,
    /// Of those, the ones carrying an Evolution frame, which is where the
    /// object metadata and the word that signs it live.
    pub frames: u64,
    /// Of those, the ones carrying a protection word.
    pub checked: u64,
    /// Of those, the ones whose word is the digest of the key.
    pub verified: u64,
    /// Of those, the ones whose word is not.
    pub mismatched: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AuroFacts {
    /// The layout the lossless bed presents (`7.1`, `5.1`).
    pub carrier: Option<&'static str>,
    /// The layout the carrier unfolds to (`7.1_5H_1T`, …).
    pub original: Option<&'static str>,
}

impl InfoReport {
    /// A report with the versions filled in and nothing established yet.
    pub fn new() -> Self {
        Self {
            schema: SCHEMA,
            harletty: env!("CARGO_PKG_VERSION"),
            build: VERSION_INFO,
            taxonomy: TAXONOMY_VERSION,
            codec: None,
            error: None,
            channels: None,
            sample_rate: None,
            spatial: None,
            truehd: None,
            eac3: None,
            auro: None,
            signature: None,
            frames_seen: 0,
            seconds_seen: 0.0,
        }
    }

    /// The report for an input in which no frame of the codec was found.
    pub fn not_found(error: &str) -> Self {
        Self {
            error: Some(error.to_string()),
            ..Self::new()
        }
    }

    /// Write the object on one line of stdout.
    pub fn print(&self) -> Result<()> {
        println!("{}", serde_json::to_string(self)?);
        Ok(())
    }
}

impl Default for InfoReport {
    fn default() -> Self {
        Self::new()
    }
}

/// `harletty taxonomy`: the versions alone, so a caller can tell before any
/// probe which label set this binary speaks.
pub fn cmd_taxonomy() -> Result<()> {
    let versions = Versions {
        schema: SCHEMA,
        harletty: env!("CARGO_PKG_VERSION"),
        build: VERSION_INFO,
        taxonomy: TAXONOMY_VERSION,
    };
    println!("{}", serde_json::to_string(&versions)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn a_dtsx_report_serialises_with_the_documented_fields() {
        let mut report = InfoReport::new();
        report.codec = Some("DTS-HD MA");
        report.channels = Some(8);
        report.sample_rate = Some(48_000);
        report.spatial = Some(Spatial {
            label: "DTS:X-7.1.4+3".to_string(),
            kind: "dtsx",
            objects: Some(3),
            fixed: Some(4),
            experimental: true,
            presentation: Some("ObjectsD0".to_string()),
        });
        report.frames_seen = 1;
        report.seconds_seen = 0.5;
        let value: Value = serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        assert_eq!(value["schema"], json!(SCHEMA));
        assert_eq!(value["taxonomy"], json!(TAXONOMY_VERSION));
        assert_eq!(value["harletty"], json!(env!("CARGO_PKG_VERSION")));
        assert_eq!(value["codec"], json!("DTS-HD MA"));
        assert_eq!(value["channels"], json!(8));
        assert_eq!(value["sample_rate"], json!(48_000));
        assert_eq!(value["spatial"]["label"], json!("DTS:X-7.1.4+3"));
        assert_eq!(value["spatial"]["kind"], json!("dtsx"));
        assert_eq!(value["spatial"]["objects"], json!(3));
        assert_eq!(value["spatial"]["fixed"], json!(4));
        assert_eq!(value["spatial"]["experimental"], json!(true));
        assert_eq!(value["spatial"]["presentation"], json!("ObjectsD0"));
        assert_eq!(value["frames_seen"], json!(1));
        assert_eq!(value["seconds_seen"], json!(0.5));
        let object = value.as_object().unwrap();
        assert!(!object.contains_key("error"));
        assert!(!object.contains_key("truehd"));
        assert!(!object.contains_key("eac3"));
        assert!(!object.contains_key("auro"));
    }

    #[test]
    fn a_plain_track_writes_a_null_spatial_and_no_codec_facts() {
        let mut report = InfoReport::new();
        report.codec = Some("DTS");
        report.channels = Some(6);
        let value: Value = serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        assert!(value.as_object().unwrap().contains_key("spatial"));
        assert_eq!(value["spatial"], Value::Null);
        assert!(!value.as_object().unwrap().contains_key("eac3"));
    }

    #[test]
    fn a_missing_stream_is_a_null_codec_with_a_reason() {
        let report = InfoReport::not_found("no DTS frame found in the input");
        let value: Value = serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        assert_eq!(value["codec"], Value::Null);
        assert_eq!(value["error"], json!("no DTS frame found in the input"));
        assert_eq!(value["schema"], json!(SCHEMA));
    }
}
