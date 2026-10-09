//! The IAMF family as a [`FamilyPipeline`]: `PluginBridge<IamfPipeline>` is
//! a bridge that decodes IAMF and nothing else.

use abi_stable::std_types::RVec;
use bridge_api::{RChannelPose, RChannelTag, RProbe, RPushResult, RSourceFamily};
use bridge_common::family::{FamilyPipeline, source_family};
use bridge_common::probe::Start;
use bridge_common::shared::{AfterPush, SharedState};

use crate::{IamfState, push_iamf};

/// The source family IAMF streams report.
pub const FAMILY_IAMF: &str = "iamf";

/// The family's one codec: IAMF comes raw only, as an OBU stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IamfCodec {
    Iamf,
}

/// The IAMF decode state, boxed: a host may create the bridge on a thread
/// with a small stack.
#[derive(Default)]
pub struct IamfPipeline {
    state: Box<IamfState>,
}

impl IamfPipeline {
    pub fn state(&self) -> &IamfState {
        &self.state
    }
}

impl FamilyPipeline for IamfPipeline {
    type Codec = IamfCodec;
    const INPUT_CODECS: &'static [&'static str] = &["iamf"];

    fn probe_raw(data: &[u8]) -> RProbe {
        crate::probe::probe_raw(data)
    }

    fn new(_shared: &SharedState) -> Self {
        Self::default()
    }

    fn input_codec(name: &str) -> Option<IamfCodec> {
        (name == "iamf").then_some(IamfCodec::Iamf)
    }

    /// The sequence header the probe claims, at the first byte.
    fn sniff(data: &[u8]) -> Option<IamfCodec> {
        (crate::probe::sequence_header(data) == Start::Claim).then_some(IamfCodec::Iamf)
    }

    /// An IAMF stream only announces itself in its sequence header, so a
    /// reset (a seek) is followed by temporal units with nothing to sniff:
    /// while the sequence is still configured they are its continuation.
    /// Before any sequence, an unrecognised packet is dropped.
    fn unsniffed(&self) -> Option<IamfCodec> {
        self.state.has_sequence().then_some(IamfCodec::Iamf)
    }

    fn push_raw(
        &mut self,
        codec: Option<IamfCodec>,
        shared: &mut SharedState,
        data: &[u8],
        out: &mut RPushResult,
    ) -> AfterPush {
        match codec {
            Some(IamfCodec::Iamf) => push_iamf(&mut self.state, shared, data, out),
            None => AfterPush::Continue,
        }
    }

    /// IAMF has no IEC 61937 data type.
    fn accepts_data_type(_data_type: u8) -> bool {
        false
    }

    fn push_iec61937(
        &mut self,
        _shared: &mut SharedState,
        _data: &[u8],
        _data_type: u8,
        _out: &mut RPushResult,
    ) -> AfterPush {
        AfterPush::Continue
    }

    /// The stream position goes, the sequence's configuration stays, so
    /// decoding resumes without waiting for a sequence header.
    fn reset(&mut self, _shared: &SharedState) {
        self.state.reset();
    }

    /// After a panic the decoder's state is unknown: it is rebuilt from the
    /// next sequence header.
    fn discard_after_panic(&mut self) {
        *self.state = IamfState::default();
    }

    fn is_ready(&self) -> bool {
        self.state.is_ready()
    }

    /// Channel-based and scene-based elements are rendered to a bed in the
    /// bridge; IAMF v2.0 objects reach the renderer as objects.
    fn has_objects(&self) -> bool {
        self.state.has_objects()
    }

    fn source_family(&self) -> &'static str {
        FAMILY_IAMF
    }

    fn source_label(&self, label: &mut String) {
        label.push_str(self.state.description().unwrap_or("IAMF"));
    }

    /// The bed is the layout the decoder rendered to (BS.2051 System J, or
    /// 9.1.6 with its wides), whose angles the recommendation states.
    fn fixed_channel_poses(&self) -> RVec<RChannelPose> {
        self.state.declared_poses()
    }

    /// The dialogue element, when a mix codes it apart.
    fn channel_tags(&self) -> RVec<RChannelTag> {
        self.state.channel_tags()
    }

    /// IAMF's loudspeaker layouts are ITU BS.2051 angles: a sphere.
    fn source_families(out: &mut RVec<RSourceFamily>) {
        out.push(source_family(FAMILY_IAMF, "Eclipsa / IAMF", "sphere"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sequence_header_is_sniffed_at_the_first_byte() {
        // OBU type 31, obu_size 6, then ia_code "iamf" and the profiles.
        let header = [0xF8, 0x06, b'i', b'a', b'm', b'f', 0x00, 0x00];
        assert_eq!(IamfPipeline::sniff(&header), Some(IamfCodec::Iamf));
        // A two-byte obu_size moves the code along.
        let long = [0xF8, 0x86, 0x00, b'i', b'a', b'm', b'f', 0x00, 0x00];
        assert_eq!(IamfPipeline::sniff(&long), Some(IamfCodec::Iamf));
        // An extension before the code.
        let extended = [
            0xF9, 0x09, 0x02, 0xAA, 0xBB, b'i', b'a', b'm', b'f', 0x00, 0x00,
        ];
        assert_eq!(IamfPipeline::sniff(&extended), Some(IamfCodec::Iamf));
        assert_eq!(
            IamfPipeline::sniff(&[0xF8, 0x06, b'x', b'a', b'm', b'f']),
            None
        );
        assert_eq!(IamfPipeline::sniff(&[0xF8, 0x06, b'i', b'a']), None);
    }

    #[test]
    fn before_a_sequence_an_unrecognised_packet_is_dropped() {
        let pipeline = IamfPipeline::default();
        assert_eq!(pipeline.unsniffed(), None);
    }
}
