# DTS:X on a lossy carrier (DTS-HD High Resolution Audio)

Status: decoded in playback and offline. The bed comes out of the core
decoder (`dca::dcadec::core`, primary set plus XXCH set), the four height
feeds out of the same decoder run over the bare channel set the extension
carries, and the fold out of the existing metadata reader (`dca::XMetadata`).
The stream is classified by `dca::exss_kind` (`ExssKind::Lossy`) and taken
through `HdDecoder::decode` like a Master Audio one; the frame it returns
has `lossless == false`.

Corpus: two Universal UHD titles whose English track ffprobe reports as
`DTS-HD HRA 7.1` (3.46 Mb/s, 48 kHz), the only lossy-carrier DTS:X streams
seen so far. Everything below was read off those bytes; where a field's
meaning is not established it says so, and the decoder pins it to the
observed value rather than guessing.

## Frame layout

```
[core 5.1, 2012 B][EXSS 2596 B: header 28 | asset = XXCH (~940 B) | 6 B | X extension … | 00 padding]
```

- The core is an ordinary 1.5 Mb/s 5.1 frame (`amode` 9, 48 kHz, 512
  samples, one subframe of two subsubframes, `sync_ssf` set, LFE 64x).
- The EXSS asset declares 8 channels, 24 bits, speaker mask `0x84b`
  (C, L/R, LFE, Lsr/Rsr, Lss/Rss), a single audio presentation, and the
  coding components `CORE` (in the core substream) + `XXCH`. No XBR, no
  XLL, no descriptor tail.
- The XXCH component is a normal one (`parse_xxch_frame` in ffmpeg terms):
  one channel set of two channels, speaker mask Lsr/Rsr, core activity mask
  naming the core's surrounds Lss/Rss, an embedded downmix (scale code
  0.707, each rear folded into its side surround) the decoder undoes after
  synthesis. ffmpeg decodes exactly this; the eight decoded speakers match
  its float output to an RMS error of 7e-7 (full-band channels).
- The asset ends; six bytes follow (`3a 42 9b 0a 00 11`, constant, not
  CRC-protected on their own, meaning unknown); then the DTS:X extension
  runs to the end of the substream, zero-padded. The decoder locates it by
  its marker within 32 bytes of the asset's end.

## The extension

```
[0:22]   type-2 layout element: the fold matrix, CRC16 over [0:22]
[22:26]  75 9a 19 08            constant
[26:28]  00 40                  constant
[28:30]  u16 size = bytes from [33] to the end of the substream
[30:32]  CRC16 over [26:32]
[32]     02                     constant
[33:]    bare channel set: header (size(8)+1 bytes, CRC16 at its end), subframes
```

### The matrix (the wrapper `XMetadata` reads)

The same type-2 element as the lossless profile's (see
`private-metadata-probe.md`): reference mask `0x84b`, output mask `0x886b`,
four rows folding TFL, TFR, TBL, TBR into L, R, Lsr, Rsr at code 55
(0.707). Its envelope differs in four places, all accepted by the reader:

| Lossless carrier | Lossy carrier |
| --- | --- |
| level field present (unity) | level field absent (the presence bit is 0) |
| matrix form flag 1 | matrix form flag 0 |
| — | two reserved bits (0) before the rows |
| — | one trailing byte after the rows and the alignment (`0x54`, meaning unknown) |

The rows are read before the trailing byte, so its value cannot change the
fold; the CRC covers it.

### The bare channel set

A channel set in the core syntax without a frame header of its own — the
lossy counterpart of the bare XLL channel set a Master Audio carrier's
XLL-X extension holds. It inherits the core's subframe layout
(`nsubframes`, `nsubsubframes`, `sync_ssf`) and is decoded by the same
subband decoder and QMF bank as the core, appended after the XXCH channels.
Each of its subsubframes ends on the core's `0xffff` DSYNC marker, which
the decoder checks: every frame of both titles decodes with every marker in
place, which is the evidence for the grammar below.

Header, 26 bytes in the corpus:

| Field | Bits | Corpus | Notes |
| --- | ---: | --- | --- |
| header size − 1 | 8 | 25 | CRC16 over the whole header (checked) |
| reserved | 3 | 0 | pinned |
| VQ start − 1, per channel | 5 × 4 | 27 | bands from 28 on are VQ-coded |
| unknown, per channel | 5 × 4 | 27, 15, 15, 15 | read and ignored; not the VQ start (the allocation covers 28 bands on every channel) |
| joint-like field | 3 × 4 | 3, 0, 0, 0 | read and ignored: there is no joint codebook select after it, and no band is joint-coded |
| transient codebook | 2 × 4 | 3 | |
| scale factor codebook | 3 × 4 | 6 | 7-bit indices, no Huffman |
| bit allocation codebook | 3 × 4 | 6 | 5-bit indices, no Huffman |
| quantizer codebooks | 24 × 4 | all max | block codes / raw samples, no Huffman |
| padding + CRC16 | | | |

The channel count is not in the header: it is the four rows of the matrix
(`FIXED_HEIGHT_COUNT`).

Subframe header, as the core's, with two differences in loop bounds:

- the prediction flags cover all 32 subbands per channel (four 32-bit
  words), not only the active ones;
- the scale factors cover all 32 subbands per channel (28 allocated + 4
  VQ-coded), so the set has 32 active subbands with VQ from 28.

Then the audio, as the core's: VQ indices for bands 28–31, no LFE, the
samples of bands 0–27 for each subsubframe, DSYNC after each.

In both titles the third and fourth feeds (TBL, TBR) are the first two
(TFL, TFR) 6 dB down — identical prediction flags and predictor indices,
allocation one step lower, scale indices six steps lower — with a little
independent content on the second title. That is what the encoder wrote;
the decoder reproduces it.

## Playback

The bridge and the CLI take `ExssKind::Lossy` substreams through the HD
decoder, present the quartet as the fixed `DTS:X 7.1.4` presentation
(`XPresentation::Height`) with the fold subtracted from the bed, and name
the stream `DTS-HD HRA + DTS:X 7.1.4`. A lossy carrier has no side channel
to read: it is never an Auro-3D carrier. An XBR-only HRA stream (no XXCH,
no extension) still plays its core.

The LFE of a lossy carrier is interpolated with the fixed-point 64x filter
of the lossless path (the one ffmpeg reserves for XLL reconstruction),
whose passband is flatter up to 120 Hz than the float filter ffmpeg's own
float output uses; the two differ by 10–15 % in level on these titles.

## Verifying

```
cargo run -p dca --release --example lossy_x_probe -- <track.dts> [ffmpeg-7.1.f32]
HARLETTY_LOSSY_X_CORPUS=<track.dts> HARLETTY_LOSSY_X_REFERENCE=<7.1.f32> cargo test -p dca -p harletty-bridge lossy
```

The reference is `ffmpeg -i <track.dts> -f f32le <7.1.f32>`; a track is
`ffmpeg -i <film.mkv> -map 0:<n> -c copy -f dts <track.dts>`.
