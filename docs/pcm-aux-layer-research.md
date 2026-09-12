# Auro-Codec: the side channel in 24-bit PCM

Status: detection implemented (`auro` crate, 2026-09-12). This note records
what the format is, what is known publicly, and where the line to a full
decoder lies. It replaces an earlier draft (2026-07-23) that reasoned from a
patent example the shipped streams do not follow.

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

The rest of the payload is the residual layer: predictor seeds,
Golomb-Rice coded residuals and the parameters of the unmix (two outputs
from one carrier, or three for the heavier layouts). Its coding is not
publicly described.

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

`harletty decode` runs the detector over every lossless DTS-HD frame and,
once three consecutive blocks agree, logs the original and carrier layouts.
The output is the carrier bed. Reconstructing the height layer is a
separate piece of work; see the crate documentation for the boundary.

## Corpus

The Trinnov Auro demo clips (DTS-HD MA 7.1 and 5.1, 48 kHz, 24-bit): every
clip without a DTS:X extension is a carrier, configuration 62
(`7.1_5H_1T` on a 7.1 carrier) for the 7.1 clips and 50 (`5.1_5H_1T` on
5.1) for the 5.1 ones. The two clips flagged DTS:X carry no side channel.
`auro/tests/corpus.rs` checks one such extract when
`HARLETTY_AURO_CORPUS` points at it.
