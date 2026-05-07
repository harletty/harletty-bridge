// SPDX-License-Identifier: Apache-2.0

mod allocation;
mod bitstream;
mod decoder;
mod imdct;
mod joc;
mod metadata;
mod pcm;
mod qmf;
mod syncframe;

pub use decoder::{Decoder, PushResult};
pub use metadata::{
    JocObject, JocObjectData, JocPayload, OamdBlockUpdate, OamdElement, OamdElementKind,
    OamdObjectBlock, OamdObjectElement, OamdPayload, ParsedEmdfPayloadData, ParsedEmdfPayloadKind,
};
pub use pcm::{
    CorePcmFrame, ObjectPcmDecoder, ObjectPcmFrame, ObjectPcmPushResult, PcmDecoder, PcmPushResult,
};
pub use syncframe::{
    AccessUnitInfo, AuxParseStatus, EmdfBlockInfo, EmdfPayloadInfo, FrameType, ParseError,
    PayloadInfo, SkipFieldInfo, inspect_access_unit,
};

pub(crate) use joc::JocObjectDecoderState;
pub(crate) use metadata::MetadataParseState;
pub(crate) use syncframe::{
    AuxDataDecodeState, CoreDecodeState, decode_core_pcm_frame_with_state_into,
    inspect_access_unit_with_metadata_state,
};
