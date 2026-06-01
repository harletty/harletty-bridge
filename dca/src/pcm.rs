// SPDX-License-Identifier: Apache-2.0
//
// DCA core PCM decoder. The public shape mirrors `eac3::PcmDecoder` /
// `eac3::CorePcmFrame` so the bridge can drive both through one code path.
// Decodes the DTS core (5.1 lossy bed) end to end: header -> subband decode
// (core.rs) -> fixed-point QMF synthesis (synth.rs) -> f32 PCM.

use crate::dcadec::core::{CoreDecoder, CoreError, primary_bed_layout};
use crate::dcadec::synth::SynthState;
use crate::parser::{AudioMode, FrameInfo, ParseError, parse_header};
use crate::types::BedChannel;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error(transparent)]
    Header(#[from] ParseError),
    #[error("bitstream underrun during core decode")]
    Bitstream,
    #[error("invalid core data: {0}")]
    Invalid(&'static str),
}

impl From<CoreError> for DecodeError {
    fn from(e: CoreError) -> Self {
        match e {
            CoreError::Bitstream => DecodeError::Bitstream,
            CoreError::Invalid(s) => DecodeError::Invalid(s),
        }
    }
}

/// Decoded core-channel PCM for one access unit. Layout matches
/// `eac3::CorePcmFrame`. `fullband_channels` are in DCA primary-channel order,
/// each paired with its `fullband_channel_order` bed label.
#[derive(Debug, Clone, PartialEq)]
pub struct CorePcmFrame {
    pub sample_rate: u32,
    pub fullband_channel_order: Vec<BedChannel>,
    pub fullband_channels: Vec<Vec<f32>>,
    pub lfe_channel: Option<Vec<f32>>,
    /// True once real samples were produced (always true on the decode path).
    pub decoded: bool,
}

impl CorePcmFrame {
    pub fn samples_per_channel(&self) -> usize {
        self.fullband_channels
            .first()
            .map(Vec::len)
            .or_else(|| self.lfe_channel.as_ref().map(Vec::len))
            .unwrap_or(0)
    }

    pub fn total_channels(&self) -> usize {
        self.fullband_channels.len() + usize::from(self.lfe_channel.is_some())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PcmPushResult {
    pub frames_seen: u64,
    pub info: FrameInfo,
    pub pcm: CorePcmFrame,
}

/// Map a DCA core audio mode to the (WAV-order) bed-channel set. Retained for
/// callers that want the canonical layout; the decode path uses
/// `core::primary_bed_layout` to match synthesized channel order.
pub fn bed_layout(mode: AudioMode) -> Vec<BedChannel> {
    primary_bed_layout(mode)
}

#[derive(Default)]
pub struct PcmDecoder {
    frames_seen: u64,
    core: CoreDecoder,
    synth: SynthState,
}

impl PcmDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.frames_seen = 0;
        self.core.reset();
        self.synth.reset();
    }

    pub fn frames_seen(&self) -> u64 {
        self.frames_seen
    }

    /// Decode one core access unit into PCM.
    pub fn push_access_unit(&mut self, access_unit: &[u8]) -> Result<PcmPushResult, DecodeError> {
        let info = parse_header(access_unit)?;
        self.core.decode_frame(&info, access_unit)?;
        let (fullband_channels, lfe_channel) = self.synth.synthesize(&mut self.core);
        self.frames_seen += 1;

        Ok(PcmPushResult {
            frames_seen: self.frames_seen,
            info,
            pcm: CorePcmFrame {
                sample_rate: info.sample_rate,
                fullband_channel_order: primary_bed_layout(info.audio_mode),
                fullband_channels,
                lfe_channel,
                decoded: true,
            },
        })
    }
}
