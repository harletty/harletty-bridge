//! The DTS family as a [`FamilyPipeline`]: `PluginBridge<DtsPipeline>` is a
//! bridge that decodes DTS and nothing else.

use abi_stable::std_types::RVec;
use bridge_api::{RChannelPose, RPushResult, RSourceFamily};
use bridge_common::family::{FamilyPipeline, source_family};

use crate::dts_pipeline::{DtsPipeline, FAMILY_AURO, FAMILY_DTS};
use crate::shared::{AfterPush, SharedState};

/// The family's one raw-transport codec: its demuxer tells the core, the
/// extension substream and what rides in it apart on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtsCodec {
    Dts,
}

/// The core frame and extension substream sync words.
const CORE_SYNC: [u8; 4] = 0x7FFE_8001u32.to_be_bytes();
const SUBSTREAM_SYNC: [u8; 4] = 0x6458_2025u32.to_be_bytes();

impl FamilyPipeline for DtsPipeline {
    type Codec = DtsCodec;

    fn new(_shared: &SharedState) -> Self {
        DtsPipeline::new()
    }

    fn input_codec(name: &str) -> Option<DtsCodec> {
        matches!(name, "dts" | "dca" | "dtsx" | "dts:x" | "dts-hd" | "dtshd")
            .then_some(DtsCodec::Dts)
    }

    fn sniff(data: &[u8]) -> Option<DtsCodec> {
        let head = data.get(..4)?;
        (head == CORE_SYNC || head == SUBSTREAM_SYNC).then_some(DtsCodec::Dts)
    }

    /// The demuxer finds the next core sync word on its own: a packet that
    /// starts mid-frame (after a seek) is still the stream's.
    fn unsniffed(&self) -> Option<DtsCodec> {
        Some(DtsCodec::Dts)
    }

    fn push_raw(
        &mut self,
        _codec: Option<DtsCodec>,
        shared: &mut SharedState,
        data: &[u8],
        out: &mut RPushResult,
    ) -> AfterPush {
        DtsPipeline::push_raw(self, shared, data, out)
    }

    fn accepts_data_type(data_type: u8) -> bool {
        crate::dts_spdif::accepts_data_type(data_type)
    }

    fn push_iec61937(
        &mut self,
        shared: &mut SharedState,
        data: &[u8],
        data_type: u8,
        out: &mut RPushResult,
    ) -> AfterPush {
        DtsPipeline::push_iec61937(self, shared, data, data_type, out)
    }

    fn reset(&mut self, _shared: &SharedState) {
        DtsPipeline::reset(self);
    }

    fn is_ready(&self) -> bool {
        DtsPipeline::is_ready(self)
    }

    fn has_objects(&self) -> bool {
        DtsPipeline::has_objects(self)
    }

    fn source_family(&self) -> &'static str {
        DtsPipeline::source_family(self)
    }

    fn source_label(&self, label: &mut String) {
        DtsPipeline::source_label(self, label);
    }

    fn fixed_channel_poses(&self) -> RVec<RChannelPose> {
        DtsPipeline::fixed_channel_poses(self)
    }

    /// DTS states ITU angles but has always rendered in the room; an
    /// unfolded Auro-3D carrier asks for its speakers equidistant on a
    /// sphere.
    fn source_families(out: &mut RVec<RSourceFamily>) {
        out.push(source_family(FAMILY_DTS, "DTS", "room"));
        out.push(source_family(FAMILY_AURO, "Auro-3D", "sphere"));
    }
}
