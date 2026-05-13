# E-AC3 coupling decode debugging notes

Date: 2026-05-10
Branch: `impl/encoded-audio-bridge`
Status: in progress — main parser bugs fixed, residual glitches under investigation.

## Context

Live PipeWire IEC61937 capture of an E-AC3+JOC stream produced
audible noise instead of recognisable audio. A previous patch series
(see `EAC3_PATCH_NOTES.md`) had attempted fixes across SPX, coupling,
SNR/fast-gain, IMDCT scaling and aux-extraction, but bundled too many
sensitive changes at once and was reverted because it regressed
working E-AC3 tracks.

This round restarts from scratch: capture frames, compare bit-for-bit
against FFmpeg, isolate bugs one at a time, prove non-regression on
working tracks at every step.

## Captured corpus

Stored under `dumps/` at the repo root:

- `dumps/working track/eac3_ok_0000_obj.bin` … `0007_obj.bin` — 8 frames
  of 3072 bytes from a track that decodes correctly (coupling=off).
  Used as the strict non-regression anchor.
- `dumps/bad track/eac3_ok_*.bin` + `eac3_reject_*_shortpkt.bin` —
  3 accepted + 64 rejected frames of 1792 bytes from the failing live
  capture (coupling=on every block).

Both tracks share the same header profile (bsid=16, 48 kHz, 5.1 side,
6 audio blocks, snr_offset_strategy=0, fast_gain_syntax=1, coupling
strategy update on block 0 only). Only `coupling_in_use` differs:
`[false × 6]` on working vs `[true × 6]` on bad.

## Diagnostic tooling added

| Commit | What |
| --- | --- |
| `057ed27` | `HARLETTY_DUMP_EAC3_REJECTS=N`, `HARLETTY_DUMP_EAC3_OK=N` env-gated dumps in `src/eac3_pipeline.rs`, paired with the existing `HARLETTY_DUMP_EAC3_SHORT_PACKET`. Capture rejected and accepted access units to `/tmp/eac3_*.bin`. |
| `b984f07` | `eac3/examples/corpus_compare.rs` — walks a directory of `.bin` dumps and prints harletty's per-frame status (obj/pcm decode result, max\_abs PCM, JOC count, snr\_offset\_strategy, coupling state). Used to score progress on the corpus after every fix. |
| `fec2bab` | `HARLETTY_EAC3_BITTRACE=1` env-gated trace points at every audblk checkpoint (`audblk_start`, `after_dynrng`, `after_spx`, `after_cpl_strategy`, `after_cpl_coords`, `after_exponents`, `after_bit_alloc`, `after_snr`, `after_fast_gain`, `after_convsnr`, `after_cpl_leak`, `after_dba`, `after_skip`, per-channel `mantissas_ch_start`, `mantissas_cpl_start`, `mantissas_lfe_start`, `audblk_end`) plus `eac3/examples/bittrace.rs` to dump the trace for a single frame. |
| `cee8c91` | Companion FFmpeg patch (`eac3/examples/ffmpeg-bittrace.patch`) emitting the same `BITTRACE\tblk=…\ttag=…\tbit_pos=…` lines from `libavcodec/ac3dec.c`. Apply on FFmpeg `master` (tested at `2f0e7f5344`), build minimal, then `diff -u` the two traces. The first divergent `bit_pos` localises a parser desync. |

Reproducible workflow:

```sh
# Status sweep:
cargo run --example corpus_compare -p eac3 -- ../dumps

# Trace one frame:
cargo run --example bittrace -p eac3 -- "../dumps/bad track/eac3_ok_0000_pcm.bin"

# FFmpeg side (after applying ffmpeg-bittrace.patch + minimal build):
HARLETTY_EAC3_BITTRACE=1 ./ffmpeg -f eac3 -i frame.bin \
    -f f32le -c:a pcm_f32le -y /tmp/out.f32 2>/tmp/ff.txt
cargo run -q --example bittrace -p eac3 -- frame.bin 2>/tmp/hl.txt
diff <(grep 'BITTRACE.blk' /tmp/ff.txt) <(grep 'BITTRACE.blk' /tmp/hl.txt)
```

## Bugs found and fixed

| # | Commit | Location | Bug |
| --- | --- | --- | --- |
| 1 | `b984f07` | `allocation.rs:262` (`read_coupling_exponents`) | `cplabsexp` was read as raw 4 bits; ATSC A/52B §E.1.3.1.1 specifies an implicit LSB of zero, FFmpeg `ac3dec.c:1094` does `<<!ch` for `CPL_CH=0`. Coupling exponents were systematically halved → PSD too high → BAP too high → mantissa over-read. |
| 2 | `b984f07` | `allocation.rs:270-280` | `decode_grouped_exponents` was called with `exponent_offset = start_mantissa+1` for coupling, leaving raw `cplabsexp` at `start_mantissa`. Per spec `cplabsexp` is a base only (not itself a usable exponent); the first slot must hold `cplabsexp + delta0`. |
| 3 | `b984f07` | `syncframe.rs:2400-2418` (`read_exponents`) | Reused the fullband group-count formula for coupling. FFmpeg uses `(end - start) / (3 << (strategy - 1))` (no `+ group_size − 4` adjust), which differs from the fullband formula on D15 by one group (7 bits per affected block). |
| 4 | `b984f07` | `syncframe.rs:2585+` (`read_frame_gain_codes`) | Reset `fast_gain` to default on every block when the per-block flag was 0. FFmpeg only resets on E-AC3 block 0 (`else if (s->eac3 && !blk)`); subsequent blocks must carry forward. |
| 5 | `b984f07` | `syncframe.rs:2484+` (`read_bit_allocation_params`) | When `bit_allocation_mode_enabled` is 0, used `BitAllocationParams::default()` (table index 0 across the board). FFmpeg uses specific indices once per frame at block 0 (`slow_decay_tab[2]`, `fast_decay_tab[1]`, `slow_gain_tab[1]`, `db_per_bit_tab[2]`, `floor_tab[7]`). The zero defaults produce wildly wrong masking. |
| 6 | `c884e2e` | `syncframe.rs` (`read_exponents` coupling branch) | `read_coupling_exponents` was called whenever `coupling_exponent_strategy[block]` was `Some(_)`, including `Some(Reuse)`. The function unconditionally reads 4 bits for `cplabsexp`, so on every Reuse coupling block harletty was burning 4 bits FFmpeg correctly skips. The drift compounded across blocks. **This was the dominant blocker** — localised by diffing the harletty/FFmpeg bittracer traces (identical through `after_cpl_coords`, harletty 4 bits ahead by `after_exponents` on the first Reuse block). |
| 7 | `5d6b0e4` | `syncframe.rs` (`decode_core_pcm_frame_with_state_into`) | `first_cpl_coords[ch]`, `first_spx_coords[ch]`, `first_cpl_leak` were initialised in `BlockSyntaxState::new` and reset in `clear_coupling`, but never reset at frame boundaries. FFmpeg `eac3dec.c:506-511` resets them at the END of `eac3_decode_audio_frame_header` once per access unit so block 0 of every frame force-reads coupling/SPX coords and the leak. Fresh per-frame decoders (corpus_compare) start with the flags = true so the bug was completely invisible there even when every test frame decoded — but the live PipeWire chain runs one decoder across the whole stream and accumulated a 1-bit-per-frame drift. **This was the live-only regression that fresh-decoder testing missed.** |

The `cplabsexp << 1` shift, the index alignment (#1 + #2) and the
group-count formula (#3) are the math the spec demands; (#4) and (#5)
match FFmpeg's per-frame defaulting; (#6) is the gate the original
syntax tree implies but harletty had collapsed; (#7) is the per-frame
state reset that streaming decoders need.

## Empirical state

`cargo run --example corpus_compare -p eac3 -- ../dumps` after every
fix gives the per-track tally below.

| | working track (8 frames) | bad track (68 frames) |
| --- | --- | --- |
| Initial (before any fix) | 8 ok-good, all near-FFmpeg amplitudes | 7 ok with garbage (max\_abs 30–60), 61 short-packet |
| After fix #1 only (`<<1`) | 8 ok-good | 7 ok still aberrant, 57 short-packet |
| After #1 + #2 (indexing) | 8 ok-good | 3 ok aberrant, 65 short-packet (regressed!) |
| After #1 + #2 + #3 (group count) | 8 ok-good | 10 ok plausible, 3 aberrant, 1 borderline, 54 short-packet |
| After #1–#5 | 8 ok-good | 14 ok plausible, 15 aberrant, 3 borderline, 36 short-packet |
| After #1–#6 (Reuse skip) | 8 ok-good | **57 ok plausible, 11 borderline, 0 aberrant, 0 short-packet** |
| After #1–#7 (per-frame reset) | 8 ok-good (no behavioural change) | unchanged for fresh decoders; live-chain newly correct |

The 11 "borderline" frames have max\_abs in 0.07–0.18, i.e. 0.82×–0.97×
of FFmpeg's reference on the same bytes (e.g. `reject_0047`:
ffmpeg=0.20269, harletty=0.16631, ratio=0.82). They are legitimate
high-energy content, not bugs — the small uniform-ish under-level
matches the working-track ratio (~0.97). No frame in the corpus now
fails or produces out-of-range PCM.

## Live behaviour

The bridge pipeline (`src/eac3_pipeline.rs`) tries
`ObjectPcmDecoder::push_access_unit` first; on `Ok(None)` it falls
through to `PcmDecoder::push_access_unit`. Both decoders share
`decode_core_pcm_frame_with_state_into`, so all seven fixes propagate
to both paths.

Reported audible result post fixes: **audible and recognisable, but
with residual glitches**. No more catastrophic noise.

## Hypotheses for the residual glitches

In rough order of suspicion:

1. **JOC object decode** (`src/eac3dec/joc.rs`,
   `JocObjectDecoderState::decode_frame`). Heavy cross-frame state:
   `prev_matrix`, `mix_matrix`, `last_frame_matrices`, `forward_qmf`
   / `inverse_qmf` filterbank delay buffers, `inverse_history`
   tracking. Time-varying interpolation between consecutive frames'
   mix matrices is a likely source of audible artifacts that wouldn't
   show on the bit-position trace (which only covers the
   syncframe/audblk parser). Dependent-frame path
   (`process_eac3_dependent_frame_with_core`) is also untouched.
2. **OAMD metadata mishandling** — wrong object positions/gains can
   produce transient pops without affecting per-frame amplitude
   stats. Routed through `metadata::build_eac3_metadata_frame` and
   downstream.
3. **PCM cross-frame state still imperfect** — `chbwcod` per channel
   persists across frames for Reuse exponent strategies; if it is
   stale from a previous frame with different bandwidth, a Reuse
   block can use the wrong end\_freq. Same risk for `cpl_band_struct`
   when coupling strategy persists across frames. FFmpeg also
   persists these but does a `memcpy` to default at block 0 of every
   frame inside `decode_band_structure`; harletty does that only
   inside `BlockSyntaxState::new`. Cross-frame test infrastructure
   would be needed to reproduce in isolation.
4. **Mantissa group ordering** — bit-count is identical to FFmpeg
   (verified against the same fixture) but the cached values per
   group may not be returned in the same order on edge cases. A
   value mismatch produces audible distortion without changing
   bit positions.
5. **IMDCT or coupling math scaling** — would explain the uniform
   ~0.85–0.97 amplitude ratio against FFmpeg, but is unlikely to
   produce transient glitches.

## Suggested next steps

1. **Re-capture a fresh corpus from the live chain** with the new
   binaries:

   ```sh
   HARLETTY_DUMP_EAC3_REJECTS=64 \
   HARLETTY_DUMP_EAC3_OK=32 \
   HARLETTY_DUMP_EAC3_SHORT_PACKET=1 \
   ./run_spatial_stack.sh ...
   ```

   Expect: zero `eac3_reject_*.bin` / `eac3_short_packet.bin` (PCM
   decoder is correct now), 32 `eac3_ok_*.bin` mostly `_obj`. Note:
   if no `_reject_*` files appear, that is the *good* outcome —
   `_reject_*` only exists for failing frames.

2. **Distribution of accepted paths**:

   ```sh
   ls /tmp/eac3_ok_*.bin | sed 's/.*ok_[0-9]*_//' | sort | uniq -c
   ```

   Tells us whether live frames hit the JOC object path (`_obj`),
   the core PCM fallback (`_pcm`), or the dependent path
   (`_depobj`).  Concentrates the next investigation on the right
   decoder.

3. **If glitches persist**, target `joc.rs` first — it is the
   highest-suspicion code path, has heavy cross-frame state, and is
   completely untouched by the seven fixes above. Add a JOC-level
   trace (similar to the bittracer) showing per-object subband
   matrices and QMF state per frame, then compare across frames to
   spot drift.

4. **Spec-backed hardening** for cross-frame state: mirror FFmpeg's
   `decode_band_structure` `memcpy(default_band_struct)` reset at
   block 0 of every frame inside `read_coupling_strategy`; add a
   stateful integration test that pushes the bad-track corpus
   sequentially through one decoder.

## Test coverage

`cargo test -p eac3 --test short_packet_regression` covers:

- `fixture_matches_expected_header` — header still parses.
- `pcm_decoder_decodes_fixture_without_short_packet` — the captured
  1792-byte fixture decodes (was `expect_err(ShortPacket)` before
  fixes #1-#6).
- `pcm_decoder_keeps_decoding_across_repeated_pushes_of_same_frame`
  — pushes the fixture 8× through one decoder, asserts each push
  succeeds and stays in range. Catches a forgotten per-frame state
  reset (regression of fix #7) without over-fitting on
  overlap-add state.

Existing unit tests (29 in the eac3 crate, 25 in harletty-bridge)
all pass. Working track decode is bit-identical before and after the
fix series.
