// Fuzz the stateful E-AC-3 decode entry points. The decoder runs in-process
// inside the player, so any panic here is a player crash in the field.
//
// The decoders are kept alive across inputs on purpose: cross-frame state
// (IMDCT delay, coupling/SPX reuse flags, JOC differential state) is exactly
// where partially-decoded hostile frames can plant corruption.
#![no_main]

use eac3::{ObjectPcmDecoder, PcmDecoder};
use libfuzzer_sys::fuzz_target;
use std::cell::RefCell;

thread_local! {
    static PCM: RefCell<PcmDecoder> = RefCell::new(PcmDecoder::new());
    static OBJECT: RefCell<ObjectPcmDecoder> = RefCell::new(ObjectPcmDecoder::new());
}

fuzz_target!(|data: &[u8]| {
    PCM.with(|d| {
        let _ = d.borrow_mut().push_access_unit(data);
    });
    OBJECT.with(|d| {
        let _ = d.borrow_mut().push_access_unit(data);
    });
});
