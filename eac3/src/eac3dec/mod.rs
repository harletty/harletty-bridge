// SPDX-License-Identifier: Apache-2.0

mod aht;
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
    dependent_chanmap_positions, merge_core_with_dependent,
};
pub use syncframe::{
    AccessUnitInfo, AudioFrameInfo, AuxParseStatus, BlockDrcInfo, EmdfBlockInfo, EmdfPayloadInfo,
    EmdfSource, FrameType, ParseError, PayloadInfo, SkipFieldInfo, inspect_access_unit,
};
