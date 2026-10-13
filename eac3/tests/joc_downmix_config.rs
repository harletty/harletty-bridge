// SPDX-License-Identifier: Apache-2.0

//! The downmix a JOC payload declares, read back off a parsed access unit.
//!
//! ETSI TS 103 420 Table 47 maps `joc_dmx_config_idx` onto the channels the
//! encoder computed its matrices against: configurations 0 and 3 declare five
//! (L, R, C, Ls, Rs), configurations 1, 2 and 4 declare seven. The
//! reconstruction is only valid against that exact signal, so a caller holding
//! an access unit has to be able to see which downmix the stream asks for
//! before it decides what to hand the object decoder.
//!
//! The header is already decoded during inspection, so what these tests pin is
//! that it can be read back off the `AccessUnitInfo` the caller is holding -
//! the alternative being to parse the same bytes a second time, matrices and
//! all, to recover three bits.

use eac3::inspect_access_unit;

const INDEPENDENT_JOC: &[u8] = include_bytes!("data/short_packet_independent_joc.bin");
const INDEPENDENT_NO_JOC: &[u8] = include_bytes!("data/aht_independent_stereo.bin");

/// An independent access unit carrying its own five channels declares the
/// five-channel downmix - the configuration that must never be reconstructed
/// against a bed a dependent was overlaid onto.
#[test]
fn a_parsed_access_unit_surfaces_the_joc_downmix_configuration() {
    let info = inspect_access_unit(INDEPENDENT_JOC).expect("fixture must parse");
    let joc = info
        .first_joc_payload()
        .expect("fixture carries a JOC payload");

    assert_eq!(joc.downmix_config, 3);
    assert_eq!(joc.channel_count, 5);
}

/// Table 47's mapping, asserted rather than assumed.
#[test]
fn the_channel_count_follows_the_configuration_index() {
    let info = inspect_access_unit(INDEPENDENT_JOC).expect("fixture must parse");
    let joc = info
        .first_joc_payload()
        .expect("fixture carries a JOC payload");

    let declared = match joc.downmix_config {
        0 | 3 => 5,
        1 | 2 | 4 => 7,
        other => panic!("joc_dmx_config_idx {other} is outside Table 47"),
    };
    assert_eq!(joc.channel_count, declared);
}

/// An access unit with no JOC payload has no downmix to declare, so callers
/// gating on the configuration are not handed one that does not exist.
#[test]
fn an_access_unit_without_joc_declares_no_downmix() {
    let info = inspect_access_unit(INDEPENDENT_NO_JOC).expect("fixture must parse");

    assert_eq!(info.joc_payload_count(), 0);
    assert!(info.first_joc_payload().is_none());
}
