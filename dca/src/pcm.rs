// SPDX-License-Identifier: Apache-2.0
//
// DCA core PCM decoder. The public shape mirrors `eac3::PcmDecoder` /
// `eac3::CorePcmFrame` so the bridge can drive both through one code path.
//
// STATUS: framing + header parse + channel-layout mapping are implemented and
// tested. The subband DSP decode (bit allocation, scale factors, high-frequency
// VQ, ADPCM prediction, 32-band QMF synthesis) is ported in `dcadec/` and wired
// here incrementally. Until that lands, `push_access_unit` returns a
// correctly-shaped frame with `decoded == false` (silence), never fabricated
// audio, so callers can gate on it.

use crate::parser::{AudioMode, FrameInfo, ParseError, parse_header};
use crate::types::BedChannel;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error(transparent)]
    Header(#[from] ParseError),
}

/// Decoded core-channel PCM for one access unit. Layout matches
/// `eac3::CorePcmFrame`, plus a `decoded` flag (false while only the header has
/// been parsed and the samples are silence placeholders).
#[derive(Debug, Clone, PartialEq)]
pub struct CorePcmFrame {
    pub sample_rate: u32,
    pub fullband_channel_order: Vec<BedChannel>,
    pub fullband_channels: Vec<Vec<f32>>,
    pub lfe_channel: Option<Vec<f32>>,
    /// True once the subband DSP path produces real samples (not silence).
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

/// Map a DCA core audio mode to the bridge bed-channel order. ffmpeg emits WAV
/// order via `ff_dca_set_channel_layout`; for the supported beds that is:
///   5.1 (3F2R + LFE): FL FR FC LFE BL BR  (LFE handled separately)
/// The non-LFE fullband order returned here is the per-channel decode order
/// remapped to WAV speaker order.
pub fn bed_layout(mode: AudioMode) -> Vec<BedChannel> {
    use BedChannel::*;
    match mode {
        AudioMode::Mono => vec![Center],
        AudioMode::MonoDual | AudioMode::Stereo | AudioMode::StereoSumDiff | AudioMode::StereoTotal => {
            vec![FrontLeft, FrontRight]
        }
        AudioMode::ThreeF => vec![FrontLeft, FrontRight, Center],
        AudioMode::TwoF1R => vec![FrontLeft, FrontRight, RearCenter],
        AudioMode::ThreeF1R => vec![FrontLeft, FrontRight, Center, RearCenter],
        AudioMode::TwoF2R => vec![FrontLeft, FrontRight, SurroundLeft, SurroundRight],
        AudioMode::ThreeF2R => vec![FrontLeft, FrontRight, Center, SurroundLeft, SurroundRight],
    }
}

#[derive(Debug, Default)]
pub struct PcmDecoder {
    frames_seen: u64,
}

impl PcmDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.frames_seen = 0;
    }

    pub fn frames_seen(&self) -> u64 {
        self.frames_seen
    }

    /// Parse one core access unit and produce a PCM frame. See module status:
    /// currently the frame is correctly shaped silence (`pcm.decoded == false`)
    /// until the subband DSP decode is wired in.
    pub fn push_access_unit(&mut self, access_unit: &[u8]) -> Result<PcmPushResult, DecodeError> {
        let info = parse_header(access_unit)?;
        self.frames_seen += 1;

        let order = bed_layout(info.audio_mode);
        let samples = info.samples_per_channel();
        let fullband_channels = vec![vec![0.0f32; samples]; order.len()];
        let lfe_channel = info.has_lfe().then(|| vec![0.0f32; samples]);

        Ok(PcmPushResult {
            frames_seen: self.frames_seen,
            info,
            pcm: CorePcmFrame {
                sample_rate: info.sample_rate,
                fullband_channel_order: order,
                fullband_channels,
                lfe_channel,
                decoded: false,
            },
        })
    }
}
