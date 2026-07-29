// Fuzz the legacy AC-3 decode entry point (see push_access_unit.rs for why
// the decoder is kept alive across inputs).
#![no_main]

use eac3::PcmDecoder;
use libfuzzer_sys::fuzz_target;
use std::cell::RefCell;

thread_local! {
    static PCM: RefCell<PcmDecoder> = RefCell::new(PcmDecoder::new());
}

fuzz_target!(|data: &[u8]| {
    PCM.with(|d| {
        let _ = d.borrow_mut().push_legacy_ac3_access_unit(data);
    });
});
