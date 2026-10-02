// SPDX-License-Identifier: Apache-2.0
//
// Steady state: once a stream has sized their buffers,
// `HdDecoder::decode_into` and `PcmDecoder::decode_into` decode it without
// allocating. The corpus lives outside the repo (it is derived from
// copyrighted content), so the tests SKIP when it is absent. Point
// HARLETTY_DTS_HD_CORPUS at one or more `[core][exss]` elementary streams
// (DTS-HD MA, HRA, DTS:X, ...), and optionally HARLETTY_DTS_CORE_CORPUS at a
// plain DTS core stream (as for `core_regression`):
//
//   SRC=<input.mkv>
//   ffmpeg -v error -y -i "$SRC" -map 0:a:0 -t 30 -c:a copy -f dts dumps/hd.dts
//   export HARLETTY_DTS_HD_CORPUS="$PWD/dumps/hd.dts[:more.dts...]"

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

/// Counts the allocations made on a thread while it is counting, so the test
/// harness's own threads do not interfere.
struct Counting;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

fn note_allocation() {
    if COUNTING.with(Cell::get) {
        ALLOCATIONS.with(|count| count.set(count.get() + 1));
    }
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note_allocation();
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note_allocation();
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note_allocation();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Decode every `[core][exss]` frame of `data`, returning how many decoded
/// and how many allocations the decoded ones made (when counting). A frame
/// that fails may allocate its error message; it is not counted.
fn decode_stream(
    decoder: &mut dca::HdDecoder,
    frame: &mut dca::HdFrame,
    data: &[u8],
    count: bool,
) -> (usize, u64) {
    const CORE_SYNC: [u8; 4] = 0x7FFE_8001u32.to_be_bytes();
    const EXSS_SYNC: [u8; 4] = 0x6458_2025u32.to_be_bytes();
    let (mut decoded, mut allocations) = (0usize, 0u64);
    let mut pos = 0usize;
    while let Some(offset) = data[pos..].windows(4).position(|w| w == CORE_SYNC) {
        pos += offset;
        let Ok(info) = dca::parse_header(&data[pos..]) else {
            pos += CORE_SYNC.len();
            continue;
        };
        let rest = &data[pos..];
        let core_size = info.frame_size;
        if rest.len() < core_size + EXSS_SYNC.len() || rest[core_size..][..4] != EXSS_SYNC {
            pos += core_size.max(CORE_SYNC.len());
            continue;
        }
        let Some(exss_size) = dca::exss_substream_size(&rest[core_size..]) else {
            break;
        };
        if rest.len() < core_size + exss_size {
            break;
        }
        let before = ALLOCATIONS.with(Cell::get);
        COUNTING.with(|c| c.set(count));
        let result = decoder.decode_into(
            &rest[..core_size],
            &rest[core_size..core_size + exss_size],
            frame,
        );
        COUNTING.with(|c| c.set(false));
        if result.is_ok() {
            decoded += 1;
            allocations += ALLOCATIONS.with(Cell::get) - before;
        }
        pos += core_size + exss_size;
    }
    (decoded, allocations)
}

/// Buffers grow to the largest frame they meet (a smoothed bitrate varies
/// the frame size), so the first pass over a stream sizes them and a second
/// pass, with the same decoder and frame, must not allocate at all.
#[test]
fn hd_decode_into_does_not_allocate_per_frame() {
    let Ok(corpus) = std::env::var("HARLETTY_DTS_HD_CORPUS") else {
        eprintln!("skipping: HARLETTY_DTS_HD_CORPUS is not set");
        return;
    };
    for path in corpus.split(':') {
        let data = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let mut decoder = dca::HdDecoder::new();
        let mut frame = dca::HdFrame::default();
        decode_stream(&mut decoder, &mut frame, &data, false);
        let (decoded, allocations) = decode_stream(&mut decoder, &mut frame, &data, true);
        assert!(decoded > 0, "{path}: no DTS-HD frame decoded");
        assert_eq!(
            allocations, 0,
            "{path}: allocations over {decoded} frames of the second pass"
        );
    }
}

/// Decode the core of every frame of `data` (the core alone, as for a plain
/// DTS stream or an extension the HD decoder does not read), returning how
/// many decoded and how many allocations they made (when counting).
fn decode_cores(
    decoder: &mut dca::PcmDecoder,
    pcm: &mut dca::CorePcmFrame,
    data: &[u8],
    count: bool,
) -> (usize, u64) {
    const CORE_SYNC: [u8; 4] = 0x7FFE_8001u32.to_be_bytes();
    let (mut decoded, mut allocations) = (0usize, 0u64);
    let mut pos = 0usize;
    while let Some(offset) = data[pos..].windows(4).position(|w| w == CORE_SYNC) {
        pos += offset;
        let Ok(info) = dca::parse_header(&data[pos..]) else {
            pos += CORE_SYNC.len();
            continue;
        };
        let Some(core) = data.get(pos..pos + info.frame_size) else {
            break;
        };
        let before = ALLOCATIONS.with(Cell::get);
        COUNTING.with(|c| c.set(count));
        let result = decoder.decode_into(core, pcm);
        COUNTING.with(|c| c.set(false));
        if result.is_ok() {
            decoded += 1;
            allocations += ALLOCATIONS.with(Cell::get) - before;
        }
        pos += info.frame_size.max(CORE_SYNC.len());
    }
    (decoded, allocations)
}

/// The same for the core decoder: `PcmDecoder::decode_into`, on the core of
/// every stream of either corpus (`HARLETTY_DTS_CORE_CORPUS` is a plain DTS
/// core, as for `core_regression`).
#[test]
fn core_decode_into_does_not_allocate_per_frame() {
    let corpus: Vec<String> = ["HARLETTY_DTS_CORE_CORPUS", "HARLETTY_DTS_HD_CORPUS"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .flat_map(|paths| paths.split(':').map(str::to_owned).collect::<Vec<_>>())
        .collect();
    if corpus.is_empty() {
        eprintln!("skipping: neither HARLETTY_DTS_CORE_CORPUS nor HARLETTY_DTS_HD_CORPUS is set");
        return;
    }
    for path in &corpus {
        let data = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let mut decoder = dca::PcmDecoder::new();
        let mut pcm = dca::CorePcmFrame::default();
        decode_cores(&mut decoder, &mut pcm, &data, false);
        let (decoded, allocations) = decode_cores(&mut decoder, &mut pcm, &data, true);
        assert!(decoded > 0, "{path}: no DTS core frame decoded");
        assert_eq!(
            allocations, 0,
            "{path}: allocations over {decoded} frames of the second pass"
        );
    }
}
