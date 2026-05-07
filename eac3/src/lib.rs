#![doc = include_str!("../README.md")]

mod eac3dec;
pub mod extract;
pub mod parser;
mod types;

pub use eac3dec::{
    AccessUnitInfo, AuxParseStatus, CorePcmFrame, Decoder, EmdfBlockInfo, EmdfPayloadInfo,
    FrameType, JocObject, JocObjectData, JocPayload, OamdBlockUpdate, OamdElement, OamdElementKind,
    OamdObjectBlock, OamdObjectElement, OamdPayload, ObjectPcmDecoder, ObjectPcmFrame,
    ObjectPcmPushResult, ParseError as AccessUnitParseError, ParsedEmdfPayloadData,
    ParsedEmdfPayloadKind, PayloadInfo, PcmDecoder, PcmPushResult, PushResult, SkipFieldInfo,
    inspect_access_unit,
};
pub use extract::{ExtractError, Extractor, Frame};
pub use parser::{
    ChannelMode, FrameInfo, ParseError as HeaderParseError, SYNCWORD, SampleRateCode, StreamType,
    parse_header,
};
pub use types::{BedChannel, ObjectAnchor, Vec3};
