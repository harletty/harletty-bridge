# Spatial object layer reverse-engineering notes

Date: 2026-07-20

Branch: `research/spatial-object-layer`

Status: **four additional lossless waveforms decoded and provisionally rendered as 7.1.4**

## Current result

The old conclusion that the post-XLL extension used a bespoke, undecodable
audio grammar was wrong. Its variable block starts with a standard bare XLL
channel-set header at byte 22. Reusing the existing XLL primitives now decodes
four extra, speaker-unmapped waveforms in every tested frame.

The regular 7.1 bed remains separate and bit-exact. The new waveforms are
exposed as `HdFrame::x_samples`. At the bridge boundary they are provisionally
appended as `Tfl`, `Tfr`, `Tbl`, `Tbr`, producing a fixed 7.1.4 channel bed.
This mapping is intentionally isolated from the lossless decoder so it can be
replaced if the profile semantics are recovered later.

Validated on prefixes from all nine currently available tracks:

- 4,206/4,206 XLL-X frames decoded in the 2 MiB cross-corpus pass;
- channel-set header CRC valid in every frame;
- no optional extension decode error;
- 48 kHz, four channels, full/residual mask `0xf` (independent full-coded
  signals; no core reconstruction is required);
- 24-bit storage, with title-dependent source PCM resolutions of 16, 18 or
  24 bits.

## Correct XLL-X payload structure

```text
[0:4]       0x02000850 XLL-X syncword
[4:22]      18-byte fixed profile wrapper
[22:...]    standard bare XLL channel-set header, four channels, CRC16
[header end] standard NAVI: four segment sizes, byte alignment, CRC16
[NAVI end]  four standard XLL segment-data blocks (512 samples total)
[audio end] two zero bytes, followed by 0..3 zero bytes for DWORD alignment
```

For the corpus geometry, the inherited XLL frame has four 128-sample segments
and 11-bit NAVI sizes. The channel-set header is intentionally not mapped
one-to-one to speakers. Its otherwise unparsed tail is always only 16..23 bits,
exactly the CRC16 plus byte alignment; it contains no hidden object metadata.

The formerly reported `N = byte22 - 3` object count was a false interpretation.
Byte 22 is the high part of the normative 10-bit `nChSetHeaderFSize` field. All
bit-budget, per-count and bespoke-Rice conclusions derived from that alleged
count are obsolete.

## Reproducible audio extraction

`xll_x_audio` writes the four decoded waveforms as 32-bit float WAV:

```sh
cargo run -p dca --release --example xll_x_audio -- \
  input.dts output.wav 256
```

The optional final argument limits input to MiB. Classic RIFF output is capped
below 4 GiB. A smoke test produced a valid 48 kHz, four-channel `pcm_f32le` WAV
with 960/960 frames and no failures.

The paired French/English *La La Land* analysis is also decisive: 564 initial
silent frames have bit-identical extension payloads, while the decoded
waveforms then diverge with the two mixes. The block therefore carries real
audio, not only spatial coordinates.

## Spatial metadata investigation

No time-varying coordinates remain inside the XLL-X payload after accounting
for the standard channel-set header, NAVI, audio segments and zero padding.
The 18-byte profile wrapper is constant across frames and titles. Direct
bit-aligned searches also found no MDA frame signature or MDA URI markers, so
this is not a byte-for-byte MDA packet encapsulation.

The EXSS audio-asset descriptor contains a 64-bit profile-specific word per
frame. Corpus analysis now identifies it as XLL-X navigation, not DRC or
spatial coordinates. Its observed bit layout is:

- 8-bit marker `0x18`;
- 11-bit XLL-X payload offset divided by four, followed by 13 zero bits;
- 16-bit marker `0x8a28`;
- 10-bit `(payload size - 24) / 4`, followed by 6 zero bits.

The decoded offset and size exactly match the XLL-X block located after the
regular XLL band data: 93,243/93,243 frames across 64 MiB from each of the nine
available streams. `xll_x_meta` checks this identity over the corpus. The
decoder now prefers these values to locate the block, while validating its
bounds and syncword and retaining the legacy aligned band-end probe as a
fallback. This makes extension extraction more robust, but supplies neither a
gain curve nor an object trajectory.

The remaining spatial question is therefore whether the four waveforms are:

1. fixed height feeds (most economical explanation);
2. a four-component sound-field representation;
3. four object waveforms whose static mapping is defined by the constant
   profile wrapper or by metadata outside the elementary audio payload.

Until that is resolved, labeling the four outputs as individual moving objects
would be premature. Treating them as a fixed 7.1.4 height bed is nevertheless a
credible working interpretation: optical-disc DTS:X officially targets up to
7.1.4 output, channels 0/1 and 2/3 repeatedly behave as stereo energy pairs,
and no dynamic coordinate block has been found. It is not yet proof of the
front/rear ordering, and some titles show cross-pair energy correlations that
remain compatible with matrixed or sound-field content.

## Public specifications used

- ETSI TS 102 114 V1.6.1 describes the regular EXSS and XLL syntax reused by
  the discovered channel set:
  <https://www.etsi.org/deliver/etsi_ts/102100_102199/102114/01.06.01_60/ts_102114v010601p.pdf>
- ETSI TS 103 491 describes DTS-UHD object navigation and bare XLL audio chunks.
  It is not the format carried here, but it supplied a useful structural model:
  <https://www.etsi.org/deliver/etsi_ts/103400_103499/103491/01.01.01_60/ts_103491v010101p.pdf>
- ETSI TS 103 223 describes MDA packet and coordinate syntax used for the
  negative signature/URI tests:
  <https://www.etsi.org/deliver/etsi_ts/103200_103299/103223/01.01.01_60/ts_103223v010101p.pdf>

Current FFmpeg still only detects the `0x02000850` syncword and skips to the end
of the XLL frame; it does not parse this channel set.

## Tools

- `xll_x_audio.rs`: decode the four waveforms to WAV;
- `xll_x_chs.rs`: prove/characterize the bare XLL channel set and CRC;
- `xll_x_meta.rs`: analyze the reserved 64-bit EXSS descriptor field;
- `xll_x_pair.rs`: compare aligned language variants;
- `xll_x_mda.rs`: bit-aligned MDA signature and URI scan;
- older `xll_x_probe`, `xll_x_rice` and `xll_x_scan` diagnostics remain useful
  as historical/raw-payload tools, but their object-count hypothesis is
  obsolete.

## Corpus

Re-extracted elementary streams are in:

```text
/mnt/local/SSD_A-CT4000/DTS:X-Dumps/
```

Available: *Apollo 13*, *Carlito's Way*, *Ex Machina*, *Gladiator*, *La La
Land* (English and French), *The Mummy* (1999), *The Mummy Returns*, and *The
Mummy* (2008). `ffprobe` identifies every track as `DTS-HD MA + DTS:X`, 48 kHz,
8-channel bed. The previously used *Harry Potter and the Deathly Hallows: Part
1* source was not found and has not been re-extracted.
