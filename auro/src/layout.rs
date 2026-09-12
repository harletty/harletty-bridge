// SPDX-License-Identifier: Apache-2.0
//
// Auro speaker layouts and the channel-input configurations that map onto
// them.
//
// The numeric ids are the encoder's own. Their names come from the public
// reverse-engineering of the format (almirus/Orua-D3, MIT) and are used as
// labels only: nothing here derives speaker geometry from a name.

/// An Auro speaker layout, by the id the bitstream uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Layout(pub u32);

impl Layout {
    /// The layout's conventional name (`5.1_4H`, `7.1_5H_1T`, ...), or `None`
    /// for an id the public tables do not know.
    pub fn name(self) -> Option<&'static str> {
        Some(match self.0 {
            3 => "2.0",
            4 => "1.0",
            7 => "3.0",
            8 => "0.1",
            11 => "2.1",
            12 => "1.1",
            15 => "3.1",
            51 => "4.0",
            55 => "5.0",
            59 => "4.1",
            63 => "5.1",
            71 => "LCRS",
            119 => "6.0",
            127 => "6.1",
            435 => "7.0_no_C",
            439 => "7.0",
            443 => "7.1_no_C",
            447 => "7.1",
            1539 => "2.0_2H",
            1543 => "3.0_2H",
            1547 => "2.1_2H",
            1551 => "3.1_2H",
            1587 => "4.0_2H",
            1591 => "5.0_2H",
            1595 => "4.1_2H",
            1599 => "5.1_2H",
            1971 => "7.0_2H_no_C",
            1975 => "7.0_2H",
            1979 => "7.1_2H_no_C",
            1983 => "7.1_2H",
            3591 => "3.0_3H",
            3599 => "3.1_3H",
            26163 => "4.0_4H",
            26167 => "5.0_4H",
            26171 => "4.1_4H",
            26175 => "5.1_4H",
            26547 => "7.0_4H_no_C",
            26551 => "7.0_4H",
            26555 => "7.1_4H_no_C",
            26559 => "7.1_4H",
            28211 => "4.0_5H",
            28215 => "5.0_5H",
            28219 => "4.1_5H",
            28223 => "5.1_5H",
            28595 => "7.0_5H_no_C",
            28599 => "7.0_5H",
            28603 => "7.1_5H_no_C",
            28607 => "7.1_5H",
            30259 => "4.0_4H_1T",
            30263 => "5.0_4H_1T",
            30267 => "4.1_4H_1T",
            30271 => "5.1_4H_1T",
            30643 => "7.0_4H_1T_no_C",
            30647 => "7.0_4H_1T",
            30651 => "7.1_4H_1T_no_C",
            30655 => "7.1_4H_1T",
            32307 => "4.0_5H_1T",
            32311 => "5.0_5H_1T",
            32315 => "4.1_5H_1T",
            32319 => "5.1_5H_1T",
            32691 => "7.0_5H_1T_no_C",
            32695 => "7.0_5H_1T",
            32699 => "7.1_5H_1T_no_C",
            32703 => "7.1_5H_1T",
            805332543 => "5.1_4H_2T",
            805332927 => "7.1_4H_2T",
            805334591 => "5.1_5H_2T",
            805334975 => "7.1_5H_2T",
            1006659519 => "9.1_4H_2T",
            1006661567 => "9.1_5H_2T",
            2052 => "TestMix2.0",
            6148 => "TestMix3.0",
            _ => return None,
        })
    }
}

/// A channel-input configuration: which original layout was folded into
/// which carrier. Announced by ADOL instruction `0x1E`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChannelConfig(pub u8);

impl ChannelConfig {
    /// The layout the encoder was fed, i.e. what a full decode restores.
    pub fn original(self) -> Option<Layout> {
        Some(Layout(match self.0 {
            1 => 55,
            2 => 63,
            8 => 71,
            11 => 1587,
            12 => 51,
            15 => 1599,
            20 => 26163,
            30 => 26175,
            40 => 30271,
            50 => 32319,
            54 => 26559,
            62 => 32703,
            64 => 3,
            66 => 7,
            67 => 119,
            68 => 127,
            69 => 439,
            70 => 447,
            71 => 26167,
            72 => 30263,
            73 => 32311,
            74 => 26551,
            75 => 1983,
            76 => 30647,
            77 => 30655,
            78 => 32695,
            128 => 4,
            129 => 2052,
            130 => 6148,
            _ => return None,
        }))
    }

    /// The layout physically present in the PCM, i.e. what plays without a
    /// decoder.
    pub fn carrier(self) -> Option<Layout> {
        Some(Layout(match self.0 {
            1 | 8 | 11 | 12 | 66 => 3,
            2 => 11,
            15 | 30 | 40 | 50 | 68 | 70 => 63,
            20 => 51,
            54 | 62 | 75 | 77 => 447,
            64 | 128 | 129 | 130 => 4,
            67 | 69 | 71 | 72 | 73 => 55,
            74 | 76 | 78 => 439,
            _ => return None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_configuration_names_both_of_its_layouts() {
        for id in 0..=255u8 {
            let cfg = ChannelConfig(id);
            match (cfg.original(), cfg.carrier()) {
                (None, None) => {}
                (Some(original), Some(carrier)) => {
                    assert!(
                        original.name().is_some(),
                        "config {id}: original {original:?}"
                    );
                    assert!(carrier.name().is_some(), "config {id}: carrier {carrier:?}");
                }
                other => panic!("config {id} is half-defined: {other:?}"),
            }
        }
    }

    #[test]
    fn the_thirteen_one_demo_configuration() {
        let cfg = ChannelConfig(62);
        assert_eq!(cfg.original().and_then(Layout::name), Some("7.1_5H_1T"));
        assert_eq!(cfg.carrier().and_then(Layout::name), Some("7.1"));
    }
}
