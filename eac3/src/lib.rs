#![doc = include_str!("../README.md")]

pub mod extract;
pub mod parser;

pub use extract::{ExtractError, Extractor, Frame};
pub use parser::{
    ChannelMode, FrameInfo, ParseError, SYNCWORD, SampleRateCode, StreamType, parse_header,
};
