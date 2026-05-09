//! Decode a single E-AC3 access unit with bit-position tracing enabled.
//!
//! `cargo run --example bittrace -p eac3 -- <frame.bin>`
//!
//! The trace is emitted on stderr by the decoder when
//! `HARLETTY_EAC3_BITTRACE` is set in the environment. This binary
//! sets that flag before decoding so the caller does not have to.
//!
//! Each line is `BITTRACE\tblk=<block>\t[ch=<ch>\t]tag=<name>\tbit_pos=<n>`.
//! `ch=-1` denotes the coupling channel; `ch=-2` denotes the LFE.
//!
//! Compare two frames with `diff -u <(this a.bin) <(this b.bin)` —
//! the first divergent `bit_pos` localises where parsing drifts.

use std::fs;

use eac3::{ObjectPcmDecoder, PcmDecoder};

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: bittrace <frame.bin>");
        std::process::exit(1);
    });

    // Force the trace on for this run regardless of how the parent shell was set up.
    // SAFETY: single-threaded process, called once at startup before any decode runs.
    unsafe {
        std::env::set_var("HARLETTY_EAC3_BITTRACE", "1");
    }

    let frame = fs::read(&path).expect("read frame");
    eprintln!("BITTRACE\t# file={path} bytes={}", frame.len());

    let want_obj = std::env::args().any(|a| a == "--object");
    if want_obj {
        let mut obj = ObjectPcmDecoder::default();
        match obj.push_access_unit(&frame) {
            Ok(Some(_)) => eprintln!("BITTRACE\t# obj_decode=ok"),
            Ok(None) => eprintln!("BITTRACE\t# obj_decode=no-joc"),
            Err(e) => eprintln!("BITTRACE\t# obj_decode=err:{e}"),
        }
        return;
    }

    let mut pcm = PcmDecoder::default();
    match pcm.push_access_unit(&frame) {
        Ok(_) => eprintln!("BITTRACE\t# pcm_decode=ok"),
        Err(e) => eprintln!("BITTRACE\t# pcm_decode=err:{e}"),
    }
}
