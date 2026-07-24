# Spatial object layer reverse-engineering notes

Date: 2026-07-20 (research campaign) / 2026-07-22 (alternate-profile follow-up)

Status: **landed as twelve labeled fixed channels** — see Omniphony
`docs/channel-object-contract.md`. The original campaign fabricated an
`RMetadataFrame` (bed ids + corner-pinned "objects"); that presentation
layer was rolled back and replaced by the contract model: the bridge emits
the reconstructed 7.1.4 as labeled fixed channels and the renderer decides
placement. The reverse-engineering notes below remain the reference for the
XLL-X payload structure and the embedded height downmix.

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

Validated on prefixes from the original nine standard-profile tracks:

- 4,206/4,206 XLL-X frames decoded in the 2 MiB cross-corpus pass;
- channel-set header CRC valid in every frame;
- no optional extension decode error;
- 48 kHz, four channels, full/residual mask `0xf` (independent full-coded
  signals; no core reconstruction is required);
- 24-bit storage, with title-dependent source PCM resolutions of 16, 18 or
  24 bits.

The twenty-three later-added standard-profile tracks, including all eight
the standard-07 through standard-14 series, standard-15, and five later
disk-inventory additions
and three controlled clips, retain
the same fixed 22-byte prefix and still contain
exactly one four-channel XLL set, so they remain fixed 7.1.4 presentations
rather than candidates for additional objects. The common prefix is unchanged:

```text
02 00 08 50 28 4b fa 71 0d 62 02 fa 02 dc 13 71 0d c8 37 3c f1 02
```

## Alternate `0xF14000D0/D1` extension profile

Two newer corpus entries use a second extension envelope that the regular
`0x02000850` path does not decode yet:

- *D0 corpus A* uses `0xF14000D0`;
- *D1 corpus A* uses `0xF14000D1`.

This profile is not a wholly different audio codec. The initial discovery scan
found exactly two byte-aligned XLL channel-set headers per frame, each protected
by a valid CRC16-CCITT. The bounded control-derived resolver described below now
reproduces those boundaries without scanning the whole payload:

| Stream | First set | Second set | Active frames decoded in 64 MiB |
| --- | --- | --- | ---: |
| `D0` | 1 channel, PCM/storage 24/24, residual mask `0x1` | 4 channels, PCM/storage 24/24, residual mask `0xf` | 8,356/8,356 |
| `D1` | 2 channels, PCM/storage 20/24, residual mask `0x3` | 4 channels, PCM/storage 20/24, residual mask `0xf` | 7,293/7,293 |

The first header starts at byte 61 or 62 for `D0`; `D1` starts at byte 62 for
its 7-byte `b2` control and byte 63 for its 8-byte `c3/c4/c5` controls. The
second header moves with the compressed size of the first set. A
compact 7- or 8-byte control word between the fixed profile prefix and the first
header encodes both its NAVI geometry and the field needed to predict that
boundary.

Each channel set has its own CRC-valid XLL-shaped NAVI table; the two sets do
not necessarily share segment geometry. Exact active-frame examples now show:

- `D0` frame 187: a four-entry, 6-bit first-set candidate followed by a
  two-segment, 8-bit terminal quartet (`160 + 114` bytes);
- `D1` frame 191: four 10-bit first-set segments (`223, 253, 274, 286` bytes);
- `D1` frame 192: two 8-bit terminal-quartet segments (`158 + 161` bytes).

This corrects the earlier `D0 = 8 active segments` hypothesis, which came from
a transition/sparse frame and does not describe the active quartet.

The apparent varying channel-mask width was a false interpretation. After the
36-bit standard XLL channel-set base, this profile carries exactly two
profile-specific bits instead of the ordinary one-to-one speaker-mapping block.
Skipping those two bits and resuming at the standard decorrelation/prediction
syntax parses both headers in every frame of the measured 8 MiB samples (1,621
D0 frames and 1,028 D1 frames), including a unique original-channel
permutation. The rest of the lossless codec is unchanged.

The D1 `b2/c3/c4/c5` control families expose the first channel set's segment
geometry in a 13-bit XLL common-header prefix. Its start moves between control
bits 18 and 25; the dominant `c5` form starts at bit 24. Across 7,293 active
frames in the measured 64 MiB prefix it selects:

- 7,130 frames with two segments and 10-bit NAVI sizes;
- 119 with four segments and 10-bit NAVI sizes;
- 32 with two segments and 9-bit sizes;
- nine with two segments and 11-bit sizes;
- two with four segments and 9-bit sizes;
- one with eight segments and 10-bit sizes.

The selector uniquely identifies an immediate CRC-valid NAVI geometry for
7,293/7,293 first sets. The earlier 425/837 result was an artifact of allowing
at most 16 bytes between the first set's NAVI-sized audio data and the second
header. The actual interstitial span is 14..18 bytes; no trailing NAVI is
present.

The D1 channel-set boundaries can also be resolved without a full-payload CRC
scan. If the first common prefix begins at control bit `C`, its preceding size
field has width `C - 14`, starts at bit 9, and predicts
`second_offset = field * 2 + 67`. The probe tests that byte and the single
preceding byte, accepting only a structurally valid, CRC-protected four-channel
XLL header. This bounded rule finds every measured D1 header, including the
7-byte `b2`, shifted `c3/c4` and rare 11-bit `c5` forms.

The bytes between the first set and terminal quartet are not arbitrary padding.
They contain zero to three alignment bytes, the same constant six-byte suffix
`02 34 38 8c 4f 00`, then a second 8- or 9-byte compact control word. Its own
13-bit common prefix starts between bits 19 and 26 and uniquely selects the
terminal quartet's geometry in 7,293/7,293 active D1 frames. The resulting
distribution is 4,148 at 2 x 10 bits, 2,273 at 2 x 11, 823 at 4 x 10, 40 at
4 x 11, four at 8 x 11, two at 2 x 8, and one each at 2 x 6, 4 x 9 and
8 x 10.

D0 uses the same two-control organization. Its first selector starts between
bits 18 and 25 of the 7- or 8-byte outer control; its second selector occupies
bits 20..26 of the interstitial 8- or 9-byte control. Each selector yields
exactly one geometry on all 8,356 active frames. This removes the false
dominant `2 x 6` first-set candidate: the control selects `2 x 5` on 7,791
frames. D0's first header follows its control at byte 61 or 62. Its second
header is predicted by the same field-width rule as D1, using
`second_offset = field * 2 + 66` and the single preceding byte as the bounded
CRC-validated alternative.

The existing XLL primitives now reach PCM well beyond a one-frame probe. With
the two-bit alternate header mode and self-consistent common parameters
(`header size width = NAVI width`, band CRC 0, non-scalable LSBs), corpus-gated
tests report:

| Stream / measured run | First set | Terminal quartet |
| --- | ---: | ---: |
| D0, 8,356 active frames in 64 MiB | 8,356 | 8,356 |
| D1, 7,293 active frames in 64 MiB | 7,293 | 7,293 |

These counts include header CRC, NAVI CRC, bounded entropy decode, prediction,
decorrelation and PCM reconstruction. They are not yet a reference comparison
and do not establish speaker or object identity.

Both alternate profiles now resolve both headers with bounded candidates and
select exactly one NAVI geometry per channel set from their compact controls.
The decoder path uses these rules directly: fixed-size layout/NAVI state,
checked arithmetic, prefix/header/NAVI CRC validation, a maximum 24-byte suffix
search and no full-payload header scan or segment/size enumeration. Malformed
or truncated extension data clears only the optional extension output and does
not invalidate the lossless bed.

At the high-level decoder boundary, every frame of all three complete elementary
streams now produces five speaker-unmapped sources for D0 or six for D1:

| Complete stream | Frames | Sources per frame | PCM resolution | Decode errors |
| --- | ---: | ---: | ---: | ---: |
| *D0 corpus A* (`D0`) | 697,447 | 5 x 512 samples | 24 bits | 0 |
| *D0 corpus B* (`D0`) | 557,057 | 5 x 512 samples | 24 bits | 0 |
| *D1 corpus A* (`D1`) | 863,769 | 6 x 512 samples | 20 bits | 0 |

The bounded-memory validation read both complete D0 streams (5,437,706,120 and
3,930,678,152 bytes) and all 7,070,742,004 D1 bytes with no pending frame,
non-finite sample or out-of-range PCM value. Ordinary inter-sample and
frame-boundary RMS differences remain on the same scale; no gross
mode-transition discontinuity was observed. The full runs include the D0 `c5`,
D1 `b2/c3/c4/c5`, and rare internal `d6` controls.

The bridge selects a presentation automatically after the extension profile
has decoded successfully. There is no environment or bridge-configuration
switch:

- D0 emits X0–X4 as fixed `TFC,TFL,TFR,TBL,TBR` channels and applies only the
  configured, explicitly experimental partial unfolds;
- D1 emits all six sources as fixed `TFL,TFR,Lw,Rw,TBL,TBR` channels and
  applies its configured partial unfolds;
- D3 performs no bed subtraction and emits unchanged named objects X0–X7 at
  the inferred two-wide plus six-top layout documented in
  `dtsx-d1-wide-audition.md`.

The D3 object-to-channel and name declarations are emitted sparsely and reset
on seek or pipeline reset. A static position heartbeat is emitted twice per
second so a newly registered Studio OSC client receives a complete spatial
frame without a startup flood when one input packet contains many decoded
audio frames. The renderer's OSC sender still delta-compresses unchanged
object details. `has_objects` becomes true only after a frame containing
actual object channels is emitted; fixed D0/D1 do not set it.

The assignments remain experimental until reference decoding and the requested
listening sign-off establish the exact profile matrices and confirm the
inferred fixed-channel identities.

It is nevertheless not yet safe to promote these experimental assignments to
the normal presentation. Full-stream PCM range and coarse continuity are
validated, but the channel semantics and fold matrices are inferred rather
than carried by decoded mapping metadata. Reference decoding and A/B sign-off
remain promotion gates.

The CRC-protected fixed profile prefix is constant for every observed frame.
For `D0`, bytes `0..48` have a zero CRC16 and are followed by the six-byte tail
`03 34 38 8c 4f 00`; for `D1`, the zero-CRC range is `0..49` and the same tail
follows. The compact control word then begins at byte 54 (`D0`) or 55 (`D1`).

The public DTS-UHD organization is a useful structural analogy: ETSI TS
103 491 defines persistent XLL frame-header chunks and XLL audio chunks that
carry channel-set headers, data and navigation. It is not byte-for-byte the
same wrapper, so its chunk ordering must not be imposed on these frames without
corpus validation. In particular, a shared CRC-valid NAVI table at the payload
end appears only on sparse/transition material (247/8,541 D0 frames and
197/1,028 D1 frames) and does not describe the active corpus; the two immediate
per-set NAVI tables remain the supported structural hypothesis.

No presentation contract is inferred from `1 + 4` or `2 + 4`. Until the
waveforms are decoded across the corpus and mapping metadata is understood,
these remain extra source-channel candidates. They are not exposed to the
renderer as labeled speakers or objects; all alternate-profile PCM work remains
research-only and corpus-gated.

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

The paired French/English the paired language control analysis is also decisive: 564 initial
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

Before analysis of standard pan control A, the remaining spatial
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
- *paired language control* English: channels 2 and 3 are sample-identical in aggregate and
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
moving pan in that prefix. The silent the paired language control feeds carry zero PCM, not a
frame-wise DC control value.

The next test is frequency-local rather than whole-frame: isolate coherent
components with a short-time covariance or source-separation pass, estimate
each component's four-channel gain vector, normalize it, and track whether the
vector moves smoothly. Under the provisional corner-speaker interpretation,
the normalized gains give a rendered upper-plane barycentre. Stable gains
would instead support fixed channels or static multichannel stems. This can
recover only a rendered direction, not necessarily the original object
coordinates, distance or spread.

Paired language variants provide a control: spatial gain
trajectories belonging to shared music and effects should agree despite the
different dialogue mix. A second useful comparison is coherence between the
four extension waveforms and the 7.1 bed; strong shared components would be
consistent with a pre-rendered 12-channel bus, while independent components
would better support separately carried object waveforms.

### Standard pan control A

The controlled *standard-control-A* clip is a much stronger
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

### Additional movement controls

The complete *standard-control-B* and *standard-control-C* elementary streams add
two more controlled clips with explicitly animated spatial motion. They retain
the same representation as the standard pan control A:

- all 3,117 *standard-control-B* frames and all 5,623 *standard-control-C* frames use
  the standard four-source profile at 48 kHz/24-bit, with no decode error;
- every frame retains the same fixed 22-byte prefix used by the film corpus
  and standard pan control A;
- the extracted elementary streams are bit-identical to the audio tracks in
  their source containers;
- after the decoded audio, every payload has only two to five zero bytes of
  trailer/alignment padding;
- no fifth waveform, D0/D1 layout, or payload-side coordinate record appears.

A height-only 100 ms covariance analysis finds locally coherent, changing
gain vectors in both clips. *standard-control-B* has 111 reliable windows with a
median dominant-source share of 0.9648 and a four-corner separability residual
of 0.0053. *standard-control-C* has 94 such windows, a median share of 0.9481 and
a residual of 0.0267. In both, the reliable gain centroids span nearly the
whole left/right and front/back height plane, normally using two of the four
height feeds. Useful listening/inspection points are:

- *standard-control-B*: 7.7 s, 9.5 s, 19.6 s, 21.6 s and 28.8–30.2 s;
- *standard-control-C*: 16.7 s, 19.7 s, 23.9 s, 34.3–36.4 s and 54.8 s.

These clips therefore provide excellent audible motion regressions, but not
positive controls for retained object metadata. Their motion is observable as
time-varying gains in the four fixed height waveforms. The analysis tool is
`scripts/analyze-dtsx-fixed-pan.py`; it deliberately reports a rendered gain
trajectory rather than calling that trajectory an authoring object.

### Embedded 7.1 compatibility downmix

The regular 7.1 presentation is not the final lower speaker plane when XLL-X is
decoded. It contains a backward-compatible copy of each height feed mixed into
the corresponding lower corner at -3 dB. The standard pan control A exposes this
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
technical reports identify an unusual published disc as a
`7.1.4 + five dynamic objects` encode. A second forum source calls it the
first DTS:X Blu-ray not locked to 7.1.4. These are useful acquisition leads,
not primary-source proof. A dump from that exact US disc, or from the reported
`7.1.4 + one object` *D1 corpus A* encode, is now the highest-value
comparison: it should contain a structural element absent from both the nine
films and the standard pan control A if the reports are accurate.

Relevant public links:

- Public test catalogue:
  <https://kodi.wiki/view/Samples#HD/object-based_Audio_Test_Clips>
- DTS patent application describing both a 7.1-channel presentation plus four
  separate height inputs and an alternate 11.1-channel representation:
  <https://patents.google.com/patent/US20170098452A1/en>
- DTS launch statement distinguishing content rendered from channels from
  objects that are actually embedded and extractable:
  <https://dts.com/insights/welcome-to-dtsx-open-immersive-and-flexible-object-based-audio-coming-to-cinema-and-home/>
- report of five dynamic objects on the unusual disc:
  <https://www.avforums.com/threads/lyngdorf-discussion.1580956/page-592>
- independent recollection that it was the first non-locked DTS:X
  Blu-ray:
  <https://forum.blu-ray.com/showthread.php?p=20520575>

Format launch material says that DTS:X moves sound objects through
mixer-selected locations, but that wording alone does not establish that a
particular consumer bitstream retains time-varying object metadata.

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
- `xll_alt_nav.rs`: map the compact control word and channel-set boundaries in
  the alternate `D0/D1` profile (`[max_mb] quick` skips the expensive header
  syntax enumeration for full-corpus control statistics);
- `xll_x_meta.rs`: analyze the reserved 64-bit EXSS descriptor field;
- `xll_x_pair.rs`: compare aligned language variants;
- `xll_x_mda.rs`: bit-aligned MDA signature and URI scan;
- `analyze-dtsx-pan-control.py`: rank-one gain, raw-FOA, layout activity
  and optional picture/audio trajectory tests for the public control clip;
- older `xll_x_probe`, `xll_x_rice` and `xll_x_scan` diagnostics remain useful
  as historical/raw-payload tools, but their object-count hypothesis is
  obsolete.

Run the control analysis after extracting the regular bed and XLL-X WAVs:

```sh
python3 scripts/analyze-dtsx-pan-control.py \
  --bed /tmp/pan-control-bed.wav \
  --height /tmp/pan-control-x.wav \
  --video /tmp/input.mkv
```

## Corpus

The local corpus contains 34 elementary streams: 31 programme tracks plus
three controlled clips. Source titles, source-container paths and staging
locations are intentionally not recorded in the repository. Corpus-gated
tests receive their inputs through environment variables:

- `HARLETTY_DTSX_STANDARD_CORPUS`: standard four-source elementary stream;
- `HARLETTY_D0_CORPUS`, `HARLETTY_D1_CORPUS`, `HARLETTY_D3_CORPUS`:
  alternate-profile elementary streams;
- `HARLETTY_DTS_CORE_CORPUS` and `HARLETTY_DTS_CORE_REFERENCE`: DTS core
  stream and matching interleaved f32le reference;
- `HARLETTY_DTSX_STANDARD_REFERENCE`: interleaved f32le reference used by the
  standard lossless-bed regression;
- `HARLETTY_DTSHD_16BIT_CORPUS` and `HARLETTY_DTSHD_16BIT_REFERENCE`:
  optional 16-bit lossless-scale regression pair.

The later D1 wide-fold system-identification experiment, retained tooling and
automatic experimental presentations are documented in
[`dtsx-d1-wide-audition.md`](dtsx-d1-wide-audition.md).
