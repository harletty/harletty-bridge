// SPDX-License-Identifier: Apache-2.0

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
    TopFrontLeft,
    TopFrontRight,
    TopSurroundLeft,
    TopSurroundRight,
    TopRearLeft,
    TopRearRight,
    WideLeft,
    WideRight,
    LowFrequencyEffects2,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectAnchor {
    Room,
    Screen,
    Speaker,
}
