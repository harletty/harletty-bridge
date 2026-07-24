//! Independent DTS Coherent Acoustics (DCA) bitstream parser and decoder.
//!
//! A native Rust port of ffmpeg's `dca` decoder, structured like the sibling
//! `eac3` crate. Phase 1 targets the DTS **core** (5.1 lossy bed); the EXSS +
//! XLL lossless extension (7.1) is added in a later phase.
//!
//! Reference C sources live (read-only) under
//! `reference-sources/ffmpeg_sources/libavcodec/dca*.c`.

mod bitstream;
#[cfg(test)]
mod bitstream_writer;

mod dcadec;
pub mod extract;
pub mod hd;
pub mod parser;
mod pcm;
mod types;

pub use extract::{ExtractError, Extractor, Frame};
pub use parser::{
    AudioMode, FrameInfo, ParseError as HeaderParseError, SYNCWORD_CORE_BE, SYNCWORD_SUBSTREAM,
    parse_header,
};
pub use hd::{exss_has_xll, exss_substream_size, HdDecoder, HdError, HdFrame};
pub use pcm::{CorePcmFrame, DecodeError, PcmDecoder, PcmPushResult, bed_layout};
pub use types::BedChannel;
