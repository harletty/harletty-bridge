#![doc = include_str!("../README.md")]

mod eac3dec;
pub mod extract;
pub mod parser;
mod types;

pub use eac3dec::{
    AccessUnitInfo, AudioFrameInfo, AuxParseStatus, BlockDrcInfo, CorePcmFrame, Decoder,
    EmdfBlockInfo, EmdfPayloadInfo, EmdfSource, FrameType, JocObject, JocObjectData, JocPayload,
    OamdBlockUpdate, OamdElement, OamdElementKind, OamdObjectBlock, OamdObjectElement, OamdPayload,
    ObjectPcmDecoder, ObjectPcmFrame, ObjectPcmPushResult, ParseError as AccessUnitParseError,
    ParsedEmdfPayloadData, ParsedEmdfPayloadKind, PayloadInfo, PcmDecoder, PcmPushResult,
    PushResult, SkipFieldInfo, dependent_chanmap_positions, inspect_access_unit,
    merge_core_with_dependent,
};
pub use extract::{ExtractError, Extractor, Frame};
pub use parser::{
    ChannelMode, FrameInfo, ParseError as HeaderParseError, SYNCWORD, SampleRateCode, StreamType,
    parse_header, parse_legacy_ac3_header,
};
pub use types::{BedChannel, ObjectAnchor, Vec3};
