# Auro-Codec: the side channel in 24-bit PCM

Status: decoded (`auro` crate, 2026-09-12): detection, the stream layer and
the unfold, checked against a published original/encoded pair. This note
records what the format is and where each part of the knowledge comes
from. It replaces an earlier draft (2026-07-23) that reasoned from a patent
example the shipped streams do not follow.

## What it is

Auro-3D delivers its height layer inside an ordinary lossless 5.1 or 7.1
track. The encoder mixes the height channels into the carrier channels with
its own gains, then borrows the `m` least-significant bits of every carrier
sample for a serial side channel. Without a decoder the track plays as the
carrier mix; a decoder reads the side channel and separates bed and height
again. On disc the carrier is a DTS-HD MA track, so the side channel
survives only a bit-exact lossless decode: any gain, dither or float round
trip ahead of the reader destroys it. In harletty it is read off
`dca::HdDecoder::lossless_samples`, the integer XLL output, before the
conversion to `f32`.

`m` is chosen per block and per channel. On the demo material at hand it
runs from 3 (metadata only: the LFE and rear channels of a 7.1 carrier
never carry more) to 10 on loud front channels.

## Block layout

The side channel is framed in blocks of `block_size` samples, 1000 on every
stream seen so far. The first sixteen samples of a block are its header:

```text
bit 0 of samples 0..16   sync: all ones
bit 1 of samples 0..16   CRC-16/CCITT of the block, MSB first
bit 2 of samples 0..8    block-size code (size = code * 16 + 16; 0x3D means 1000)
bit 2 of samples 8..12   four flag bits
bit 2 of samples 12..16  14 - m
```

The CRC covers the three bytes of every sample of the block, low byte
first, with bit 1 of the header samples masked out, and is inverted at the
end. Bits 3..m of the header samples and the low `m` bits of every later
sample, MSB first, form the payload; one bit every `16 * m` payload
positions is reserved and must be zero. The first reserved bit is bit 0 of
sample 16, which is what stops the sync run at exactly sixteen ones.

The payload opens with a fixed set of fields (widths known, meanings not
public), four stream-id slots, an optional small table, then one or more
ADOL blocks: a bytecode of one-byte opcodes with 0-, 8-, 16-, 24- or
32-bit operands, terminated by opcode 0. Opcode `0x1E` names the
channel-input configuration, which maps to an original layout (what a full
decode restores) and a carrier layout (what is physically in the PCM). The
crate's `layout.rs` carries both tables.

## The stream layer

Everything after the layout bytecode was worked out here by analysis of the
carriers and confirmed on the published original/encoded pair
(`auro/tests/pair.rs`: correlation 0.99999 per channel, gain within
0.01 dB). Per block and per carrier:

- A 112-bit header: a 16-bit field (`0x010A` on every stream seen), a
  32-bit word holding the Rice parameter (bits 27..24), an adaptive-Rice
  flag (bit 30), the codebook count code (bits 23..16), the ADOL block
  count (bits 15..8) and the codebook entry width (bits 7..0); then the
  four stream-id slots (`0xFF` = empty) and four unused bytes.
- Predictor seeds: two 32-bit words when two streams are folded, five for
  three, none for one.
- The ADOL blocks. Besides `0x1E` (layout), `0x40` carries a per-stream
  gain code (channel, scaler) in tenths of a dB and `0x41` an offset added
  to every code.
- The codebook: `count` entries of `width` bits, sign and magnitude; twice
  for a three-stream fold. `count` is `2c + 8` for codes below 5, `4c` up
  to 15, `8c - 64` up to `0x53`.
- The Golomb-Rice stream: one index per sample (unary prefix, then `k`
  suffix bits least-significant first), selecting a codebook entry — the
  same index selects from both codebooks in a three-stream fold.

The stream ids: 0 L, 1 R, 2 C, 3 LFE, 4 Ls, 5 Rs, 7 Lb, 8 Rb, 9 HL, 10 HR,
11 HC, 12 T, 13 HLs, 14 HRs, read off the channel-identification material.

## The unfold

With its borrowed bits dropped, the carrier is the exact sum of the folded
streams. Each sample one stream is extrapolated from its recent history —
`2a - b` for two streams, a three-phase `(b + 3(4a - 3b)) / 4` for three —
and the others are derived from the sum; the residual corrects the derived
value before it feeds the next prediction. Outputs are then scaled by
their gain codes (`10^(code/200)`, with exact powers of two pinned every
60 codes) and clamped to 24 bits. Blocks are independent (state resets at
each), so output lags input by one block.

The outputs sum back to the carrier exactly; each one carries the other's
extrapolation error, which is what "virtually lossless" means here: about
50 dB below the signal on real material.

## Sources

- almirus, *Auro-3D: how height channels are hidden in ordinary PCM*,
  Habr, August 2026 — <https://habr.com/ru/articles/1068212/>.
- almirus/Orua-D3 on GitHub (MIT): the Python detector these tables come
  from, and the archived Auro white papers.
- MediaArea/MediaInfoLib pull request 2531: the same detector in C++.
- The commercial decoder `orua3d-decode` (closed, Windows, non-commercial
  licence) is the only public implementation of the residual layer. It can
  serve as an oracle for testing a clean-room decoder; nothing in it may be
  copied.

## What harletty does with it

`harletty decode` runs the detector over every lossless DTS-HD frame,
holding the frames back until three consecutive blocks agree. Then it
unfolds every carrier, joins the streams (`auro::Unfolder`) and writes a
master set of the original layout: the bed and the four corner heights as
bed channels, the centre height and the top as static objects. Samples no
block claims (before the first block, after the last) play as the carrier.
A track that turns out not to be a carrier is written as before.

The realtime bridge does the same on its DTS-HD path (`bridge/src/
auro_pipeline.rs`): frames are held back until the verdict — at most two
of the largest blocks when no block validates, four when blocks validate
but the layout has not latched — then replayed into the unfolder, and
every frame from then on is the unfolded layout as labelled fixed
channels (the corner heights as Tfl/Tfr/Tbl/Tbr, the centre height as
Tfc, the top as Tc). Output lags input by one block (21 ms at 48 kHz for
1000-sample blocks); the last block of a stream stays in the unfolder,
as the bridge has no end-of-stream flush.

## Corpus

The Trinnov Auro demo clips (DTS-HD MA 7.1 and 5.1, 48 kHz, 24-bit): every
clip without a DTS:X extension is a carrier, configuration 62
(`7.1_5H_1T` on a 7.1 carrier) for the 7.1 clips and 50 (`5.1_5H_1T` on
5.1) for the 5.1 ones. The two clips flagged DTS:X carry no side channel.
`auro/tests/corpus.rs` checks one such extract when
`HARLETTY_AURO_CORPUS` points at it.
