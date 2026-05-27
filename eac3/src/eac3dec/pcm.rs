// SPDX-License-Identifier: Apache-2.0

use super::joc::{JocObjectDecoderState, JocObjectMatrices};
use super::metadata::{JocPayload, MetadataParseState, OamdPayload, ParsedEmdfPayloadData};
use super::syncframe::{
    AccessUnitInfo, AuxDataDecodeState, CoreDecodeState, ParseError,
    decode_core_pcm_frame_with_state, inspect_access_unit_with_metadata_state,
    inspect_legacy_ac3_access_unit,
};
use crate::BedChannel;

#[derive(Debug, Clone, PartialEq)]
/// Decoded core channel PCM for one access unit.
pub struct CorePcmFrame {
    pub sample_rate: u32,
    pub fullband_channel_order: Vec<BedChannel>,
    pub fullband_channels: Vec<Vec<f32>>,
    pub lfe_channel: Option<Vec<f32>>,
}

impl CorePcmFrame {
    /// Number of samples carried by each channel in this frame.
    pub fn samples_per_channel(&self) -> usize {
        self.fullband_channels
            .first()
            .map(|channel| channel.len())
            .or_else(|| self.lfe_channel.as_ref().map(Vec::len))
            .unwrap_or(0)
    }

    /// Total channel count including the optional LFE channel.
    pub fn total_channels(&self) -> usize {
        self.fullband_channels.len() + usize::from(self.lfe_channel.is_some())
    }
}

#[derive(Debug, Clone, PartialEq)]
/// Object-audio PCM plus the metadata that was active for the same access unit.
///
/// `object_channels` only covers dynamic objects. Bed channels remain in [`CorePcmFrame`].
pub struct ObjectPcmFrame {
    pub core: CorePcmFrame,
    pub object_channels: Vec<Vec<f32>>,
    pub object_active: Vec<bool>,
    pub joc: JocPayload,
    pub oamd_payloads: Vec<(OamdPayload, Option<u16>)>,
}

impl ObjectPcmFrame {
    /// Number of samples carried by each decoded channel in this frame.
    pub fn samples_per_channel(&self) -> usize {
        self.core.samples_per_channel()
    }

    /// Number of dynamic object channels decoded for this frame.
    pub fn object_count(&self) -> usize {
        self.object_channels.len()
    }
}

#[derive(Debug, Clone, PartialEq)]
/// Result returned by [`PcmDecoder::push_access_unit`].
pub struct PcmPushResult {
    pub frames_seen: u64,
    pub info: AccessUnitInfo,
    pub pcm: CorePcmFrame,
}

#[derive(Debug, Clone, PartialEq)]
/// Result returned by [`ObjectPcmDecoder::push_access_unit`] when the frame contains object data.
pub struct ObjectPcmPushResult {
    pub frames_seen: u64,
    pub info: AccessUnitInfo,
    pub pcm: ObjectPcmFrame,
}

#[derive(Debug)]
/// Stateful decoder for the core channel PCM path.
///
/// This decoder keeps both bitstream syntax state and cross-frame metadata state, so callers must
/// preserve frame order and call [`PcmDecoder::reset`] after discontinuities.
pub struct PcmDecoder {
    frames_seen: u64,
    aux_state: AuxDataDecodeState,
    core_state: CoreDecodeState,
    metadata_state: MetadataParseState,
    debug_log_level: log::Level,
}

impl Default for PcmDecoder {
    fn default() -> Self {
        Self {
            frames_seen: 0,
            aux_state: AuxDataDecodeState::default(),
            core_state: CoreDecodeState::default(),
            metadata_state: MetadataParseState::default(),
            debug_log_level: log::Level::Debug,
        }
    }
}

impl PcmDecoder {
    /// Create a fresh PCM decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset all cross-frame decode state.
    pub fn reset(&mut self) {
        self.frames_seen = 0;
        self.aux_state.reset();
        self.core_state.reset();
        self.metadata_state.reset();
    }

    /// Number of access units accepted since the last reset.
    pub fn frames_seen(&self) -> u64 {
        self.frames_seen
    }

    /// Whether the most recently decoded block had Spectral Extension
    /// active (`spxinu == 1`). Useful for picking content that exercises
    /// the SPX synthesis path.
    pub fn last_spx_in_use(&self) -> bool {
        self.core_state.spx_in_use_snapshot()
    }

    /// Per-channel SPX participation flags (`chinspx[ch]`) at the end of
    /// the most recently decoded block. Empty before the first frame.
    pub fn last_chinspx(&self) -> &[bool] {
        self.core_state.chinspx_snapshot()
    }

    /// Configure the metadata / aux diagnostic log level for this decoder instance.
    pub fn set_debug_log_level(&mut self, level: log::Level) {
        self.debug_log_level = level;
    }

    fn apply_debug_log_level(&self) {
        super::metadata::set_metadata_log_level(self.debug_log_level);
        super::syncframe::set_aux_log_level(self.debug_log_level);
    }

    /// Decode one complete access unit into core PCM.
    pub fn push_access_unit(&mut self, access_unit: &[u8]) -> Result<PcmPushResult, ParseError> {
        self.apply_debug_log_level();
        let info = inspect_access_unit_with_metadata_state(
            access_unit,
            &mut self.metadata_state,
            Some(&mut self.aux_state),
        )?;

        if access_unit.len() < info.frame_size {
            return Err(ParseError::TruncatedFrame {
                expected: info.frame_size,
                available: access_unit.len(),
            });
        }
        if access_unit.len() != info.frame_size {
            return Err(ParseError::TrailingData {
                expected: info.frame_size,
                provided: access_unit.len(),
            });
        }

        let pcm = decode_core_pcm_frame_with_state(access_unit, &info, &mut self.core_state)?;
        self.frames_seen += 1;
        Ok(PcmPushResult {
            frames_seen: self.frames_seen,
            info,
            pcm,
        })
    }

    /// Decode one complete legacy AC-3 syncframe into core PCM.
    pub fn push_legacy_ac3_access_unit(
        &mut self,
        access_unit: &[u8],
    ) -> Result<PcmPushResult, ParseError> {
        self.apply_debug_log_level();
        let info = inspect_legacy_ac3_access_unit(access_unit)?;

        if access_unit.len() < info.frame_size {
            return Err(ParseError::TruncatedFrame {
                expected: info.frame_size,
                available: access_unit.len(),
            });
        }
        if access_unit.len() != info.frame_size {
            return Err(ParseError::TrailingData {
                expected: info.frame_size,
                provided: access_unit.len(),
            });
        }

        let pcm = decode_core_pcm_frame_with_state(access_unit, &info, &mut self.core_state)?;
        self.frames_seen += 1;
        Ok(PcmPushResult {
            frames_seen: self.frames_seen,
            info,
            pcm,
        })
    }
}

#[derive(Debug)]
/// Stateful decoder for dynamic object PCM.
///
/// This is the highest-level decoder before rendering. It returns `Ok(None)` for frames that do
/// not carry the required dynamic-object payloads.
pub struct ObjectPcmDecoder {
    frames_seen: u64,
    aux_state: AuxDataDecodeState,
    core_state: CoreDecodeState,
    joc_state: JocObjectDecoderState,
    metadata_state: MetadataParseState,
    debug_log_level: log::Level,
}

impl Default for ObjectPcmDecoder {
    fn default() -> Self {
        Self {
            frames_seen: 0,
            aux_state: AuxDataDecodeState::default(),
            core_state: CoreDecodeState::default(),
            joc_state: JocObjectDecoderState::default(),
            metadata_state: MetadataParseState::default(),
            debug_log_level: log::Level::Debug,
        }
    }
}

impl ObjectPcmDecoder {
    /// Create a fresh object decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset all cross-frame decode state.
    pub fn reset(&mut self) {
        self.frames_seen = 0;
        self.aux_state.reset();
        self.core_state.reset();
        self.joc_state.reset();
        self.metadata_state.reset();
    }

    /// Number of access units accepted since the last reset.
    pub fn frames_seen(&self) -> u64 {
        self.frames_seen
    }

    /// Per-object subband matrices reconstructed for the most recent decoded frame.
    ///
    /// This is primarily useful for debugging and offline comparison tools.
    pub fn last_joc_matrices(&self) -> &JocObjectMatrices {
        self.joc_state.last_frame_matrices()
    }

    /// Configure the metadata / aux diagnostic log level for this decoder instance.
    pub fn set_debug_log_level(&mut self, level: log::Level) {
        self.debug_log_level = level;
    }

    fn apply_debug_log_level(&self) {
        super::metadata::set_metadata_log_level(self.debug_log_level);
        super::syncframe::set_aux_log_level(self.debug_log_level);
    }

    /// Decode one complete access unit into dynamic object PCM.
    ///
    /// Returns `Ok(None)` when the frame is valid E-AC-3 but does not contain the object payloads
    /// needed for this stage.
    pub fn push_access_unit(
        &mut self,
        access_unit: &[u8],
    ) -> Result<Option<ObjectPcmPushResult>, ParseError> {
        self.apply_debug_log_level();
        let info = inspect_access_unit_with_metadata_state(
            access_unit,
            &mut self.metadata_state,
            Some(&mut self.aux_state),
        )?;

        if access_unit.len() < info.frame_size {
            return Err(ParseError::TruncatedFrame {
                expected: info.frame_size,
                available: access_unit.len(),
            });
        }
        if access_unit.len() != info.frame_size {
            return Err(ParseError::TrailingData {
                expected: info.frame_size,
                provided: access_unit.len(),
            });
        }

        let joc = info.payloads().find_map(|payload| match &payload.parsed {
            ParsedEmdfPayloadData::Joc(joc) => Some(joc.clone()),
            _ => None,
        });
        let Some(joc) = joc else {
            return Ok(None);
        };

        let core = decode_core_pcm_frame_with_state(access_unit, &info, &mut self.core_state)?;
        let object_channels = self.joc_state.decode_frame(&core, &joc)?;
        let object_active = joc.objects.iter().map(|object| object.active).collect();
        let oamd_payloads = info
            .payloads()
            .filter_map(|payload| match &payload.parsed {
                ParsedEmdfPayloadData::Oamd(oamd) => {
                    Some((oamd.clone(), payload.info.sample_offset))
                }
                _ => None,
            })
            .collect();

        self.frames_seen += 1;
        Ok(Some(ObjectPcmPushResult {
            frames_seen: self.frames_seen,
            info,
            pcm: ObjectPcmFrame {
                core,
                object_channels,
                object_active,
                joc,
                oamd_payloads,
            },
        }))
    }

    /// Decode dynamic object PCM from a dependent access unit using externally decoded core PCM.
    pub fn push_access_unit_with_core(
        &mut self,
        access_unit: &[u8],
        core: CorePcmFrame,
    ) -> Result<Option<ObjectPcmPushResult>, ParseError> {
        self.apply_debug_log_level();
        let info = inspect_access_unit_with_metadata_state(
            access_unit,
            &mut self.metadata_state,
            Some(&mut self.aux_state),
        )?;

        if access_unit.len() < info.frame_size {
            return Err(ParseError::TruncatedFrame {
                expected: info.frame_size,
                available: access_unit.len(),
            });
        }
        if access_unit.len() != info.frame_size {
            return Err(ParseError::TrailingData {
                expected: info.frame_size,
                provided: access_unit.len(),
            });
        }

        let joc = info.payloads().find_map(|payload| match &payload.parsed {
            ParsedEmdfPayloadData::Joc(joc) => Some(joc.clone()),
            _ => None,
        });
        let Some(joc) = joc else {
            return Ok(None);
        };

        let object_channels = self.joc_state.decode_frame(&core, &joc)?;
        let object_active = joc.objects.iter().map(|object| object.active).collect();
        let oamd_payloads = info
            .payloads()
            .filter_map(|payload| match &payload.parsed {
                ParsedEmdfPayloadData::Oamd(oamd) => {
                    Some((oamd.clone(), payload.info.sample_offset))
                }
                _ => None,
            })
            .collect();

        self.frames_seen += 1;
        Ok(Some(ObjectPcmPushResult {
            frames_seen: self.frames_seen,
            info,
            pcm: ObjectPcmFrame {
                core,
                object_channels,
                object_active,
                joc,
                oamd_payloads,
            },
        }))
    }
}
