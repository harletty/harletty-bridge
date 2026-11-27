// SPDX-License-Identifier: LGPL-2.1-or-later

/// Speaker assignment for a decoded bed channel. Mirrors `eac3::BedChannel` so
/// the bridge can map both decoders' output through the same `labels.rs` path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BedChannel {
    FrontLeft,
    FrontRight,
    Center,
    LowFrequencyEffects,
    SurroundLeft,
    SurroundRight,
    RearCenter,
    RearLeft,
    RearRight,
    WideLeft,
    WideRight,
}
