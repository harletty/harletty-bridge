// Fuzz the DTS core decoder. It runs in-process inside the player, so any
// panic here is a player crash in the field.
//
// The input is decoded twice: once whole, as a single access unit (a header
// whose frame size disagrees with the bytes actually handed over), then as a
// stream split by the `Extractor`, every frame through one decoder and one
// kept output frame, as the bridge drives it. Cross-frame state (the
// synthesis filter history, the buffers sized by earlier frames) is where a
// hostile frame can leave corruption behind for the next one. A fresh decoder
// per input keeps every crash reproducible from its input alone.
#![no_main]

use dca::{CorePcmFrame, Extractor, PcmDecoder};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = PcmDecoder::new().push_access_unit(data);

    let mut extractor = Extractor::default();
    extractor.push_bytes(data);
    let mut decoder = PcmDecoder::new();
    let mut pcm = CorePcmFrame::default();
    loop {
        match extractor.next_frame() {
            Ok(Some(frame)) => {
                let _ = decoder.decode_into(frame.as_bytes(), &mut pcm);
            }
            Ok(None) => break,
            // The extractor has already resynced past the bad header.
            Err(_) => {}
        }
    }
});
