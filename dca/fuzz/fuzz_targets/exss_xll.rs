// Fuzz the DTS-HD decoder: the extension substream (EXSS) parser, the
// lossless (XLL) decoder with its DTS:X extension, and the lossy XXCH route.
// It runs in-process inside the player, so any panic here is a player crash
// in the field.
//
// The input is a stream, demuxed the way the bridge's DTS pipeline does it:
// a core frame followed by its EXSS is decoded as a DTS-HD frame. An EXSS
// standing alone is paired with a known-good core frame (the first frame of
// the stereo tone fixture), so the fuzzer can reach the EXSS and XLL parsers
// without having to build a valid core first: `HdDecoder` decodes the core
// before it looks at the EXSS and stops when the core fails.
//
// One decoder and one output frame serve every frame of the input, as in the
// bridge: XLL buffers across frames (PBR) and keeps its buffers sized from
// one frame to the next. A fresh decoder per input keeps every crash
// reproducible from its input alone.
#![no_main]

use std::sync::LazyLock;

use dca::{HdDecoder, HdFrame, exss_has_xll, exss_kind, exss_substream_size, parse_header};
use libfuzzer_sys::fuzz_target;

const CORE_SYNC: [u8; 4] = 0x7FFE_8001u32.to_be_bytes();
const EXSS_SYNC: [u8; 4] = 0x6458_2025u32.to_be_bytes();

static TONE: &[u8] = include_bytes!("../../../harletty/tests/fixtures/dts_core_tone_10f.dts");

/// The first core frame of the tone fixture.
static CORE_FRAME: LazyLock<&'static [u8]> = LazyLock::new(|| {
    let info = parse_header(TONE).expect("the tone fixture opens on a core frame");
    &TONE[..info.frame_size]
});

fn decode(decoder: &mut HdDecoder, frame: &mut HdFrame, core: &[u8], exss: &[u8]) {
    let _ = exss_kind(exss);
    if decoder.decode_into(core, exss, frame).is_ok() {
        // The integer tap the Auro stage reads after a lossless frame.
        for (_, samples) in decoder.lossless_samples() {
            std::hint::black_box(samples);
        }
    }
}

fuzz_target!(|data: &[u8]| {
    // The classifiers the bridge calls on an EXSS, on the input as it is.
    let _ = exss_kind(data);
    let _ = exss_has_xll(data);
    let _ = exss_substream_size(data);

    let mut decoder = Box::new(HdDecoder::new());
    let mut frame = HdFrame::default();
    let mut pos = 0usize;
    while data.len() - pos >= 4 {
        let rest = &data[pos..];
        if rest[..4] == CORE_SYNC {
            let Ok(info) = parse_header(rest) else {
                pos += 4;
                continue;
            };
            let fs = info.frame_size.max(4);
            if rest.len() < fs + 4 || rest[fs..fs + 4] != EXSS_SYNC {
                pos += fs.min(rest.len());
                continue;
            }
            match exss_substream_size(&rest[fs..]) {
                Some(es) if es >= 4 && fs + es <= rest.len() => {
                    decode(&mut decoder, &mut frame, &rest[..fs], &rest[fs..fs + es]);
                    pos += fs + es;
                }
                _ => pos += fs,
            }
        } else if rest[..4] == EXSS_SYNC {
            match exss_substream_size(rest) {
                Some(es) if es >= 4 && es <= rest.len() => {
                    decode(&mut decoder, &mut frame, &CORE_FRAME, &rest[..es]);
                    pos += es;
                }
                _ => pos += 4,
            }
        } else {
            pos += 1;
        }
    }
});
