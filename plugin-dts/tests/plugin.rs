//! The plugin as a host loads it: its root module and the bridges it makes.

use harletty_dts_bridge as PLUGIN;

const PRIMARY_CODECS: &[&str] = &["dts"];
const FOREIGN_CODECS: &[&str] = &["truehd", "eac3", "ac3", "iamf"];
const FAMILIES: &[&str] = &["dts", "auro"];
const BURST_TYPES: &[u8] = &[0x0B, 0x0C, 0x0D, 0x11];

use abi_stable::std_types::RSlice;
use bridge_api::{BridgeLibRef, RInputTransport, RProbe, RProbeVerdict};

fn lib() -> BridgeLibRef {
    PLUGIN::library()
}

fn probe(data: &[u8], transport: RInputTransport, data_type: u8) -> RProbe {
    lib().probe()(RSlice::from_slice(data), transport, data_type)
}

/// The root module lists the codecs a host routes here by, each one the
/// bridge takes as `input_codec`, lower case.
#[test]
fn every_listed_input_codec_is_taken() {
    let codecs: Vec<String> = lib().input_codecs()()
        .iter()
        .map(|c| c.to_string())
        .collect();
    assert_eq!(&codecs[..PRIMARY_CODECS.len()], PRIMARY_CODECS);
    for codec in &codecs {
        assert_eq!(*codec, codec.to_ascii_lowercase());
        let mut bridge = lib().new_bridge()(false);
        assert!(
            bridge.configure("input_codec".into(), codec.as_str().into()),
            "{codec}"
        );
    }
    let mut bridge = lib().new_bridge()(false);
    for other in FOREIGN_CODECS {
        assert!(
            !bridge.configure("input_codec".into(), (*other).into()),
            "{other}"
        );
    }
    assert!(bridge.configure("input_codec".into(), "auto".into()));
}

/// The families it declares, and the one an idle bridge reports.
#[test]
fn it_declares_its_families_only() {
    let families: Vec<String> = lib().source_families()()
        .iter()
        .map(|f| f.name.to_string())
        .collect();
    assert_eq!(families, FAMILIES);
    let bridge = lib().new_bridge()(false);
    assert_eq!(bridge.source_family().as_str(), FAMILIES[0]);
    assert!(!bridge.is_ready());
}

/// IEC 61937 burst types: exactly the family's (docs/multi-bridge.md in
/// Omniphony).
#[test]
fn it_claims_its_burst_types_only() {
    for data_type in 0..=u8::MAX {
        let expected = if BURST_TYPES.contains(&data_type) {
            RProbe::claim(0)
        } else {
            RProbe::none(0)
        };
        assert_eq!(
            probe(&[0; 16], RInputTransport::Iec61937, data_type),
            expected,
            "{data_type:#04x}"
        );
    }
}

/// What a host sends a bridge before any audio: every plugin takes the
/// default presentation and the TrueHD values, and `log_level`.
#[test]
fn the_host_start_up_configuration_is_taken() {
    let mut bridge = lib().new_bridge()(false);
    for p in ["best", "0", "1", "2", "3"] {
        assert!(bridge.configure("presentation".into(), p.into()), "{p}");
    }
    assert!(!bridge.configure("presentation".into(), "4".into()));
    assert!(bridge.configure("log_level".into(), "info".into()));
}

const DTS: &[u8] = include_bytes!("../../harletty/tests/fixtures/dts_core_tone_10f.dts");
const EAC3: &[u8] = include_bytes!("../../harletty/tests/fixtures/joc_atmos_1s.eac3");

#[test]
fn a_dts_stream_is_claimed_and_decoded() {
    assert_eq!(probe(DTS, RInputTransport::Raw, 0), RProbe::claim(0));
    assert_ne!(
        probe(EAC3, RInputTransport::Raw, 0).verdict,
        RProbeVerdict::Claim
    );
    let mut bridge = lib().new_bridge()(false);
    let result = bridge.push_packet(RSlice::from_slice(DTS), RInputTransport::Raw, 0);
    assert!(result.error_message.is_empty(), "{}", result.error_message);
    assert!(!result.frames.is_empty());
    assert_eq!(bridge.source_label().as_str(), "DTS");
    // DRC is Dolby's: this plugin offers none and takes none.
    assert!(bridge.supported_drc_modes().is_empty());
    assert!(!bridge.set_drc_mode("Standard".into()));
}
