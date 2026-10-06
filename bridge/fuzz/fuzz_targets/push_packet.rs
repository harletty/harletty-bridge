// Fuzz the bridge as a host drives it: a non-strict bridge, packets handed
// to `push_packet`, and the stream description read back after each one.
// This is where codec sniffing and every codec family's state machine meet.
//
// Input layout:
//
//   byte 0      bit 0     transport: 0 raw, 1 IEC 61937
//               bits 1-3  raw: the declared codec (`input_codec`), auto first;
//                         IEC 61937: the burst data type, from DATA_TYPES
//               bits 4-5  the DRC mode, from the bridge's own list
//               bit 6     a host reset (a seek) halfway through the packets
//   bytes 1-2   packet size, little-endian; 0 hands the rest over as one
//   rest        the stream, cut into packets of that size
//
// Panics are reported although `push_packet` catches them (and the TrueHD and
// IAMF paths catch theirs): libfuzzer-sys installs a panic hook that aborts
// the process before any unwinding, so no `catch_unwind` in the bridge ever
// sees a panic under the fuzzer. A crash here is a panic the guard would
// have turned into a pipeline reset in the field: a bug all the same.
//
// A fresh bridge per input keeps every crash reproducible from its input
// alone.
#![no_main]

use std::sync::Once;

use abi_stable::std_types::{RSlice, RStr};
use bridge_api::{RInputTransport, RLogLevel};
use libfuzzer_sys::fuzz_target;

const CODECS: [&str; 8] = [
    "auto", "eac3", "truehd", "dts", "iamf", "ac3", "mlp", "dtshd",
];

/// AC-3, E-AC-3, TrueHD in MAT, DTS types I/II/III and DTS-HD, and one data
/// type no family takes.
const DATA_TYPES: [u8; 8] = [0x01, 0x15, 0x16, 0x0B, 0x0C, 0x0D, 0x11, 0x07];

extern "C" fn discard_log(_level: RLogLevel, _target: RStr<'_>, _message: RStr<'_>) {}

static INIT: Once = Once::new();

fuzz_target!(|data: &[u8]| {
    // Formatted as a host would get them, then dropped: stderr would slow
    // the fuzzer down to nothing.
    INIT.call_once(|| harletty_bridge::set_log_sink(discard_log));

    let [mode, size_lo, size_hi, stream @ ..] = data else {
        return;
    };
    let selector = usize::from((mode >> 1) & 0x7);
    let (transport, data_type) = if mode & 1 == 0 {
        (RInputTransport::Raw, 0)
    } else {
        (RInputTransport::Iec61937, DATA_TYPES[selector])
    };
    let packet_size = match usize::from(u16::from_le_bytes([*size_lo, *size_hi])) {
        0 => stream.len().max(1),
        n => n,
    };

    let mut bridge = harletty_bridge::new_bridge(false);
    if transport == RInputTransport::Raw {
        bridge.configure(RStr::from("input_codec"), RStr::from(CODECS[selector]));
    }
    let drc_modes = bridge.supported_drc_modes();
    if !drc_modes.is_empty() {
        let drc = &drc_modes[usize::from((mode >> 4) & 0x3) % drc_modes.len()];
        bridge.set_drc_mode(drc.as_rstr());
    }

    let packets = stream.chunks(packet_size);
    let reset_at = if mode & 0x40 != 0 {
        packets.len() / 2
    } else {
        usize::MAX
    };
    for (index, packet) in packets.enumerate() {
        if index == reset_at {
            bridge.reset();
        }
        let result = bridge.push_packet(RSlice::from_slice(packet), transport, data_type);
        std::hint::black_box(&result);
        // What a host reads back after a packet: none of these is guarded.
        std::hint::black_box((
            bridge.is_ready(),
            bridge.has_objects(),
            bridge.source_family(),
            bridge.source_label(),
            bridge.fixed_channel_poses(),
            bridge.channel_tags(),
        ));
    }
});
