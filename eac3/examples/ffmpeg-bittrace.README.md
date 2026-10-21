# FFmpeg bittrace patch

> **Licence.** `ffmpeg-bittrace.patch` is a diff against FFmpeg's
> `libavcodec/ac3dec.c` and contains FFmpeg source lines, so it is licensed
> `LGPL-2.1-or-later` (FFmpeg's licence), not under the crate's Apache-2.0.
> It is used only as a local test oracle to compare bit positions with this
> decoder, is not part of the crate, and is excluded from the published
> package (`exclude` in `eac3/Cargo.toml`).

Companion to `eac3/examples/bittrace.rs`. Patches `libavcodec/ac3dec.c`
with the same `BITTRACE` checkpoint format harletty emits, so the two
decoders' bit positions can be diff'd line-for-line.

## Apply

```sh
cd /path/to/ffmpeg
git apply /path/to/harletty-bridge/eac3/examples/ffmpeg-bittrace.patch
```

Tested against FFmpeg `master` at commit `2f0e7f5344` (January 2026).
The hunks should still apply against any nearby revision since they
only insert `BITTRACE(...)` calls between existing parser sections.

## Build (minimal)

```sh
./configure --disable-everything --disable-doc --disable-asm \
    --disable-network --disable-iconv --disable-lzma \
    --enable-decoder=ac3 --enable-decoder=eac3 \
    --enable-parser=ac3 --enable-demuxer=ac3 --enable-demuxer=eac3 \
    --enable-protocol=file \
    --enable-encoder=pcm_f32le --enable-muxer=pcm_f32le
make -j ffmpeg
```

## Use

```sh
HARLETTY_EAC3_BITTRACE=1 ./ffmpeg -f eac3 -i frame.bin \
    -f f32le -c:a pcm_f32le -y /tmp/out.f32 2>/tmp/ff_trace.txt

cargo run -q --example bittrace -p eac3 -- frame.bin 2>/tmp/hl_trace.txt

diff <(grep 'BITTRACE.blk' /tmp/ff_trace.txt) \
     <(grep 'BITTRACE.blk' /tmp/hl_trace.txt)
```

The first divergent `bit_pos` localises the parser desync. FFmpeg
will emit the trace twice on a raw `.bin` input (probe pass + decode
pass) — compare against the first pass.

## Tag schema

`BITTRACE\tblk=<block>\t[ch=<n>\t]tag=<name>\tbit_pos=<n>`

Tags emitted in `decode_audio_block` order:
`audblk_start`, `after_dynrng`, `after_spx`, `after_cpl_strategy`,
`after_cpl_coords`, `after_exponents`, `after_bit_alloc`, `after_snr`,
`after_fast_gain`, `after_convsnr`, `after_cpl_leak`, `after_dba`,
`after_skip`, then per channel inside `decode_transform_coeffs`:
`mantissas_ch_start ch=<0..fbw-1>`, `mantissas_cpl_start ch=-1`,
`mantissas_lfe_start ch=-2`, finally `audblk_end`.
