# Spatial object layer reverse-engineering notes

Date: 2026-07-20

Branch: `research/spatial-object-layer`

Status: **four height waveforms decoded and their -3 dB contribution removed from the compatible 7.1 bed**

## Current result

The old conclusion that the post-XLL extension used a bespoke, undecodable
audio grammar was wrong. Its variable block starts with a standard bare XLL
channel-set header at byte 22. Reusing the existing XLL primitives now decodes
four extra, speaker-unmapped waveforms in every tested frame.

The decoder preserves the regular, backward-compatible 7.1 presentation and
exposes the new waveforms separately as `HdFrame::x_samples`. At the bridge
boundary they are provisionally appended as `Tfl`, `Tfr`, `Tbl`, `Tbr`, while
their exact Q15 -3 dB contribution is subtracted from FL/FR/BL/BR. This produces
the reconstructed fixed 7.1.4 presentation without double-rendering height
content onto the lower plane. The reconstruction remains intentionally isolated
from the lossless decoder.

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

Before analysis of the Object Emulator control clip, the remaining spatial
question was whether the four waveforms were:

1. fixed height feeds (most economical explanation);
2. a four-component sound-field representation;
3. four object waveforms whose static mapping is defined by the constant
   profile wrapper or by metadata outside the elementary audio payload.

The public DTS-UHD syntax is only an analogy, but it is an important warning:
it carries object metadata separately from bare XLL audio chunks, and one
object may reference multiple consecutive waveforms. Four decoded waveforms
therefore do not imply four objects.

The controlled tests below resolve this corpus in favour of fixed TFL, TFR,
TBL and TBR feeds embedded into a backward-compatible 7.1 downmix. Labeling
them as independently recoverable moving objects would still be premature.

### Atypical channel-coherence cases

Sample-level measurements over the first 64 MiB of each stream identify three
particularly useful counterexamples to four independent height feeds:

- *Apollo 13*: channels 2 and 3 are exactly silent for 7,501 frames (about
  80 seconds), leaving only two active extension waveforms;
- *La La Land* English: channels 2 and 3 are sample-identical in aggregate and
  exactly zero after the shared 564-frame introduction, for the rest of the
  13,223-frame prefix (about 141 seconds);
- *The Mummy* (1999): channels 0/3 and 1/2 have sample correlations of 0.9981
  and 0.9975. A single gain-scaled copy explains each target with only 6.1%
  and 7.0% residual RMS respectively.

These observations do not prove object coding: fixed speaker feeds can share
or duplicate content. A first time-local coherence test finds the two *Mummy*
pairs in 6,388/6,582 and 6,362/6,582 frames respectively. Their normalized
second-channel gain occupies narrow 10th-to-90th percentile ranges of
0.5063..0.5189 and 0.4412..0.4599. This favours static duplicated stems over a
moving pan in that prefix. The silent *La La Land* feeds carry zero PCM, not a
frame-wise DC control value.

The next test is frequency-local rather than whole-frame: isolate coherent
components with a short-time covariance or source-separation pass, estimate
each component's four-channel gain vector, normalize it, and track whether the
vector moves smoothly. Under the provisional corner-speaker interpretation,
the normalized gains give a rendered upper-plane barycentre. Stable gains
would instead support fixed channels or static multichannel stems. This can
recover only a rendered direction, not necessarily the original object
coordinates, distance or spread.

The paired *La La Land* language tracks provide a control: spatial gain
trajectories belonging to shared music and effects should agree despite the
different dialogue mix. A second useful comparison is coherence between the
four extension waveforms and the 7.1 bed; strong shared components would be
consistent with a pre-rendered 12-channel bus, while independent components
would better support separately carried object waveforms.

### DTS:X Object Emulator control

The public DTS-authored *DTS:X Object Emulator* test clip is a much stronger
control than a feature-film soundtrack because its picture explicitly shows a
moving source, labels the signal as using 3D coordinates, and then illustrates
several playback layouts. It does not print numerical azimuth, elevation or
radius values; the visible source position can only be used as a screen-space
proxy in shots where the camera is fixed. The clip linked by the official Kodi
sample catalogue is 95 seconds long and `ffprobe` identifies its audio as a
48 kHz, 7.1 DTS-HD MA + DTS:X stream.

The complete elementary stream has the same representation as the film corpus:

- 8,902/8,902 frames decode as the regular 7.1 bed plus exactly four full-coded
  XLL-X waveforms;
- the bare channel-set CRC is valid in every frame;
- the XLL-X payload is completely consumed by header, navigation, lossless
  audio and zero alignment;
- no MDA URI or non-random MDA frame signature is present;
- the varying EXSS descriptor bits again encode XLL-X offset and size, not
  coordinates.

The 7.1.4 portion of the demo also supplies a direct ordering clue. Over the
49–70 second motion sequence, frame-energy correlation between each extension
waveform and the regular bed is strongest for these pairs:

| Extension | Bed reference | RMS-envelope correlation | Provisional position |
| --- | --- | ---: | --- |
| X0 | FL | 0.916 | top front left |
| X1 | FR | 0.890 | top front right |
| X2 | BL | 0.835 | top back left |
| X3 | BR | 0.733 | top back right |

Sample correlations for the same pairs are respectively 0.937, 0.917, 0.847
and 0.754. The visible source moves continuously while coherent copies are
gain-panned between the corresponding lower and upper feeds. No independently
transmitted coordinate curve is needed to reproduce this particular stream.

An additional controlled analysis makes the fixed-presentation interpretation
substantially stronger. The picture successively labels its illustrated output
layouts as 7.1.4, 5.1.2, 2.1, 5.1, 7.1 and 7.1.4. Although the elementary audio
stream remains in the same 7.1 plus four-XLL-X container throughout, its actual
decoded channel activity follows those labels:

- during the 5.1.2 section, rear bed channels BL/BR and extension waveforms
  X2/X3 are exactly zero while X0/X1 remain active;
- during the 2.1, 5.1 and 7.1 sections, all four extension waveforms are
  exactly zero;
- all four extension waveforms become active again in the final 7.1.4 section.

The demonstration therefore contains a sequence of layout-specific renders in
a fixed 7.1.4 transport presentation. If X0..X3 were a layout-independent
four-component object basis, changing the illustrated playback layout would
not require two or all four transport components to become bit-identically
zero in precisely this way.

The moving-object audio is also almost ideal for a blind gain-vector test. In
150 ms windows from the first 7.1.4 section, the median share of signal energy
explained by the first covariance eigenvector is 0.9884: it is essentially one
mono waveform distributed by changing gains. Among 247 reliable windows:

- negative height-gain energy is only `4.4e-8` of total height-gain energy;
- the median number of active height outputs is two out of four at a -26 dB
  threshold;
- 74.9% of windows use no more than two height outputs;
- every possible raw-FOA W candidate drops out entirely in some positions; the
  best candidate, X0, has a normalized coefficient variation of 0.786 and is
  below 0.05 in 29.1% of the reliable windows.

This rejects a direct W/X/Y/Z interpretation. Raw first-order ambisonics needs
an omnidirectional W component whose normalized gain remains non-zero and
constant for a single moving point source; it also does not naturally produce
these non-negative, piecewise-sparse corner gains.

A projective test also rejects an unknown fixed real 4x4 rematrix around raw
FOA. A point-source vector `[W, X, Y, Z]` lies on the quadratic cone
`W^2 = X^2 + Y^2 + Z^2`, whose matrix has signature (1,3). By Sylvester's law
of inertia, an invertible real rematrix may rotate or scale that cone but cannot
change its signature. The best homogeneous quadratic fitted on four fifths of
the measured height-gain vectors and evaluated on the held-out fifth has:

- eigenvalues `-0.514, -0.492, +0.494, +0.500`, hence signature (2,2);
- a held-out normalized RMS residual of 0.0145;
- almost exactly the simpler relation `X1*X2 = X0*X3`, with RMS residual
  0.0129 over all reliable windows.

That last relation is the expected separability constraint for four corner
gains `[left*front, right*front, left*back, right*back]`. It is a much more
specific match for two-dimensional speaker panning than for a hidden linear
FOA basis.

As an independent A/V synchronization check, a simple bright-object tracker
finds 46 positions in the fixed-camera 5.1.2 shot. Screen X correlates at 0.671
with decoded right-minus-left energy, while screen Y correlates at -0.667 with
the decoded height-energy fraction (negative is expected because smaller image
Y is higher on screen). The moderate rather than perfect values are expected
from perspective projection, glow/occlusion errors and the use of screen-space
coordinates instead of the undisclosed 3D animation coordinates. They are
nevertheless strong enough to confirm that the visible trajectory and decoded
speaker gains are synchronized.

### Embedded 7.1 compatibility downmix

The regular 7.1 presentation is not the final lower speaker plane when XLL-X is
decoded. It contains a backward-compatible copy of each height feed mixed into
the corresponding lower corner at -3 dB. The Object Emulator exposes this
particularly clearly because it contains isolated single-source pans.

The dominant lower/height gain ratio is `23170 / 32768 = 0.707092285`. This is
not merely close to `1/sqrt(2)`: `23170` is the exact Q15 -3 dB coefficient in
the DTS downmix table already used by the regular XLL decoder. Across reliable
100/150 ms windows, subtracting that contribution makes the corresponding
lower gain nearly zero in 80.7% of FL/X0 windows, 92.1% of FR/X1, 68.2% of
BL/X2 and 65.5% of BR/X3. The remaining positive gain is consistent with an
intentional simultaneous lower-plane component as the source moves between
speakers.

The fixed-point result is more decisive. In windows where the visible source
is at an upper speaker, applying the same rounded multiply used by XLL,
`bed - rmul15(height, 23170)`, leaves only:

| Pair | residual / bed RMS | residual RMS | maximum residual |
| --- | ---: | ---: | ---: |
| FR - X1 | `1.06e-6` | 1.01 PCM LSB | 4 LSB |
| BL - X2 | `1.17e-6` | 0.78 PCM LSB | 2 LSB |
| BR - X3 | `8.80e-7` | 0.78 PCM LSB | 2 LSB |

The trajectory does not include an equally isolated TFL interval; its best
FL-X0 window still leaves only 0.236% of the bed RMS. This establishes the
likely reconstruction matrix:

```text
FL = bed.FL - rmul15(TFL, 23170)    TFL = X0
FR = bed.FR - rmul15(TFR, 23170)    TFR = X1
BL = bed.BL - rmul15(TBL, 23170)    TBL = X2
BR = bed.BR - rmul15(TBR, 23170)    TBR = X3
```

A legacy 7.1 decoder ignores XLL-X and therefore retains all height content in
the compatible bed. A 7.1.4 decoder must undo those four contributions before
adding the height feeds. The bridge now performs this reconstruction; its
previous append-only behavior left a -3 dB floor ghost of height-only sounds.

These tests disprove a fixed invertible 4x4 rematrix to raw FOA, but cannot
disprove a nonlinear or signal-adaptive direction estimator applied by a
receiver to the four height feeds. Such processing would be an upmixer
operating on an already rendered speaker presentation, however. Without side
information it cannot uniquely recover more than four arbitrary,
simultaneously overlapping original object waveforms or their authoring
trajectories.

This is the strongest evidence so far that the current `Tfl/Tfr/Tbl/Tbr`
mapping is correct and that this legacy optical-disc profile normally stores a
pre-rendered fixed 7.1.4 presentation. It also resolves an apparent marketing
contradiction: a mix may originate as 3D objects with coordinates while the
consumer encode carries the result rendered to twelve fixed feeds.

It does not prove that the profile can never append genuine dynamic objects.
DTS stated at launch that an embedded object can be extracted, and independent
technical reports identify the US Blu-ray of *Ip Man 3* as an unusual
`7.1.4 + five dynamic objects` encode. A second forum source calls it the
first DTS:X Blu-ray not locked to 7.1.4. These are useful acquisition leads,
not primary-source proof. A dump from that exact US disc, or from the reported
`7.1.4 + one object` *Independence Day* encode, is now the highest-value
comparison: it should contain a structural element absent from both the nine
films and the Object Emulator if the reports are accurate.

Relevant public links:

- Kodi test catalogue and Object Emulator link:
  <https://kodi.wiki/view/Samples#HD/object-based_Audio_Test_Clips>
- DTS patent application describing both a 7.1-channel presentation plus four
  separate height inputs and an alternate 11.1-channel representation:
  <https://patents.google.com/patent/US20170098452A1/en>
- DTS launch statement distinguishing content rendered from channels from
  objects that are actually embedded and extractable:
  <https://dts.com/insights/welcome-to-dtsx-open-immersive-and-flexible-object-based-audio-coming-to-cinema-and-home/>
- report of five dynamic objects on the US *Ip Man 3* disc:
  <https://www.avforums.com/threads/lyngdorf-discussion.1580956/page-592>
- independent recollection that *Ip Man 3* was the first non-locked DTS:X
  Blu-ray:
  <https://forum.blu-ray.com/showthread.php?p=20520575>

By contrast, DTS's *Ex Machina* announcement says that DTS:X moves sound
objects through mixer-selected locations, but it does not state that this
specific disc retains time-varying object metadata. The local *Ex Machina*
bitstream has the same fixed four-waveform structure, so that press wording is
not sufficient evidence of dynamic objects in the title:
<https://dts.com/insights/lionsgates-ex-machina-blu-ray-disc-is-first-to-feature-dtsx-audio/>.

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
- `analyze-dtsx-object-emulator.py`: rank-one gain, raw-FOA, layout activity
  and optional picture/audio trajectory tests for the public control clip;
- older `xll_x_probe`, `xll_x_rice` and `xll_x_scan` diagnostics remain useful
  as historical/raw-payload tools, but their object-count hypothesis is
  obsolete.

Run the control analysis after extracting the regular bed and XLL-X WAVs:

```sh
python3 scripts/analyze-dtsx-object-emulator.py \
  --bed /tmp/dtsx-object-emulator-bed.wav \
  --height /tmp/dtsx-object-emulator-x.wav \
  --video /tmp/dtsx-object-emulator.mkv
```

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
