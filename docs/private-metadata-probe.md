# Private metadata: standard matrix and alternate-profile candidates

Status: the reading established here is now applied in playback. The
realtime reader is `dca::XMetadata` (positions, bed folds) with the
subtraction table `dca::FoldPlan`; the bridge and the offline exporter both
present the extension waveforms from it. `dca/examples/xll_private_metadata.rs`
remains the independent offline diagnostic that produced the evidence below;
it is not a general DTS:X navigation parser.

## Confirmed standard result

The standard extension prefix contains a readable sparse matrix, not just an
opaque wrapper. Its reference mask is `0x084b` (eight compatible channels),
its output mask is `0x886b`, and the difference `0x8020` describes the two
height pairs. The four rows specify:

| Supplemental source | Compatible target | Six-bit gain code | Calibrated Q15 gain |
| --- | --- | ---: | ---: |
| TFL | L | 55 | 23170 / 32768 |
| TFR | R | 55 | 23170 / 32768 |
| TBL | Lb | 55 | 23170 / 32768 |
| TBR | Rb | 55 | 23170 / 32768 |

The coefficients agree with the existing fixed-height PCM calibration.
Code 61 is unity. The probe exposes only these verified numerical calibration
points; other codes remain raw codes, not guessed gains.

Important offsets in the supported type-2 form, relative to the extension
prefix, with MSB-first bit numbering:

| Bit offset | Width | Observed field |
| ---: | ---: | --- |
| 0 | 8 | Element type 2 |
| 36 | 12 | Explicit reference mask `0x084b` |
| 59 | 16 | Output mask `0x886b` |
| 89 | 6 | Mix code 61 |
| 96 | 8 | First sparse target mask, `0x02` |
| 104 | 6 | First gain code, 55 |
| 110 | 14 | Second row: target mask `0x04`, gain 55 |
| 124 | 14 | Third row: target mask `0x10`, gain 55 |
| 138 | 14 | Fourth row: target mask `0x20`, gain 55 |
| 152 | 16 | CRC16-CCITT |

CRC16 with polynomial `0x1021` and initial state `0xffff` over the first
**21 bytes** has zero remainder. The existing bare XLL header starts at byte
22. This independently establishes the protected prefix boundary; it does
not establish that the first five bytes alone are a separately protected
metadata element.

The implementation reads the masks and sparse rows from fields. It does not
return a fixed matrix merely because a sync marker matched. Synthetic tests
change a target and a coefficient while regenerating the CRC and verify that
the reported matrix changes accordingly. All single-bit corruptions and all
truncations of the synthetic standard block are rejected. Uninterpreted
header flags are restricted to the observed form, rather than silently
assuming their meanings.

## Navigation: what is and is not established

The existing descriptor navigation resolves the extension offset and extent
on 310,905 of the 311,648 standard frames in this pass. All those words also
carry the value 69 at bit 37 of the captured descriptor tail. The other 743
frames use the existing decoder fallback; their matrix CRCs still validate.

This is evidence for the type-69 association lead, but **not a recovered
general association-table grammar**. The diagnostic deliberately does not
reinterpret arbitrary payload matches as chunk boundaries.

The external diagnostic reports type 68 for alternate associations. Its
JSON extent fields are not a safe oracle: an initial standard frame reports
extent 6146 for a 132-byte XLL frame, and alternate frames also report extents
larger than their assets. Some reported descriptor consumption extends beyond
the declared descriptor. Those values were not used as buffer lengths or
incorporated into the production parser.

## D0, D1 and D3

Each observed alternate prefix has a valid outer CRC and terminates before
the established compact-control suffix. Within that bounded prefix, the
probe finds a candidate type-3 declaration of `0x084b -> 0x886b`. Its position
moves with the outer prefix. It occupies the final 25 bytes of the protected
prefix, including the envelope CRC. The type-3 body does **not** have an
independently validated standalone CRC.

Applying the strict standard matrix grammar stops at the inline-matrix flag.
It would be incorrect to reuse the standard matrix at this point.

A separate candidate reading consumes four sparse rows over **12 columns**
after the unresolved control word `0x3fa`, ending with five zero padding bits
immediately before the envelope CRC:

| Row | Column indices | D0 codes | D1 / D3 codes |
| ---: | --- | --- | --- |
| 0 | 1, 4 | 61, 61 | 55, 61 |
| 1 | 2, 5 | 61, 61 | 55, 61 |
| 2 | 6, 10 | 61, 61 | 55, 61 |
| 3 | 7, 11 | 61, 61 | 55, 61 |

These indices fit lower/upper pairs in the full 12-channel mask order. On its
own this is a reproducible **structural hypothesis**, not a validated fold
matrix: the bytes do not say which of the five, six or eight decoded
waveforms supplies a row, which direction the matrix applies in, or what the
control word means. The PCM comparison in "What the PCM says about the folds"
below supplies the waveform identity (the last four sources, in row order),
the direction (the first entry of each row is already in the compatible bed)
and the gain calibration. The control word remains unread.

The example keeps `matrix=Err(Unsupported(...))` separate from
`unverified_rows12=...` so that candidates cannot accidentally be promoted to
accepted playback metadata.

## Type-241 static declarations

The offline probe now reads the initial type-241 declaration fields inside
the CRC-validated alternate envelope, bounded before the candidate type-3
element. This is a partial reader: it stops before the variable data and
does not claim to decode coordinates, motion or waveform associations.

In the supported form, the four-bit count-minus-one is at bit 28. The
explicit reference mask starts at bit 44 and is `0x084b`. Declarations start
at bit 64; each consumes 18 bits: an active flag, a two-bit field equal to
3, a three-bit index, a two-bit mode and ten component/parameter bits equal
to zero. Other flag or component forms remain visibly unsupported.

| Profile / observed variant | Declaration indices | Raw modes | End bit | Type-3 byte offset |
| --- | --- | --- | ---: | --- |
| D0 | 0 | 1 | 82 | 23 |
| D1, shorter prefix | 0, 1 | 0, 0 | 100 | 24 |
| D1, longer prefix | 0, 1 | 1, 1 | 100 | 29 |
| D3 | 0, 1, 2, 3 | 1, 1, 1, 1 | 136 | 47–56, observed subset |

The D1 distinction is material: its sync marker alone does not distinguish
these metadata forms. Mode numbers here are raw fields, not labels such as
"fixed" or "moving". The counts agree with the first bare-XLL set's 1, 2
and 4 decoded waveforms, but that agreement does not establish identity,
ordering or a route into the type-3 matrix. The second four-waveform set
and the meaning of control `0x3fa` remain unresolved. The waveform comparison
below supplies a bounded validation for D0 and D3, rather than a general
association-table grammar.

A fresh 64 MiB prefix pass read every static declaration successfully on
25,169 D0 frames, 12,897 D1 frames and 66,016 D3 frames: **104,082 alternate
frames across 15 inputs**, with valid outer CRCs and no extension errors.
Of the D1 frames, 7,484 use mode 0 and 5,413 use mode 1. Counts and indices
are read from fields; synthetic tests also change their order and exercise
a five-declaration form, rather than inferring them from a profile marker.
All byte truncations of those synthetic declarations are rejected.

At this follow-up checkpoint the warnings-denied all-target build passes,
and the full suite reports 208 passing tests. All configured DTS:X corpus
checks execute; three unrelated legacy fixture tests self-skip because their
local files are absent. The eight-second compatible-bed bit comparison was
repeated for all four profiles, again with nonzero reference audio and zero
differing float bit patterns over 384,000 samples per channel.

## Mode-1 variable data and waveform validation

The probe now consumes the supported mode-1 data from the static declaration
end through the exact byte boundary before type 3. It reads one position per
declaration, two optional sparse rows over the eight reference channels, an
optional auxiliary section, and zero alignment padding. It rejects extra
bytes instead of silently ignoring unexplained trailing data. Unsupported
options remain errors inside the diagnostic result.

The observed position form uses a six-bit distance, eight-bit azimuth and
seven-bit elevation. Calibrated integer units are:

- Azimuth: `min(3 * raw - 360, 357)` half-degrees, with raw 47 and 193
  representing -220 and +220 half-degrees respectively.
- Elevation: `min(3 * (raw - 60), 180)` half-degrees.
- Distance: raw zero gives zero; otherwise `(raw + 1) / 64`.

The mode-1 records examined have gain code 61, no extent, and only the first
of the two reference rows present. The probe reads row masks and coefficients
from fields; it does not substitute a fixed list of targets. D0 also has an
auxiliary declaration with raw layout mask `0x80` and gain code 61. That
auxiliary mask is deliberately not assigned a speaker label here.

| Profile / variant | Frames completely consumed | Result |
| --- | ---: | --- |
| D0 | 25,169 | Position, reference row and auxiliary section |
| D1, longer prefix | 5,413 | Two positions and reference rows |
| D3 | 66,016 | Four positions and reference rows |
| Total supported mode 1 | 96,598 | Exact end boundary and zero padding |
| D1, shorter prefix | 7,484 | Explicitly unsupported variable mode 0 |

Six D3 input prefixes contain changes in the first declaration's angular
codes within the same stream. This is evidence of transmitted position
changes, not merely different static placements across programmes. It does
not yet establish interpolation or a general runtime object-state contract.

Independent local comparisons used the external diagnostic in an isolated
environment without network access. Its D3 coordinate export agrees with the
field conversion, for example `(-145.5, 28.5, 1)` and `(145.5, 27, 1)` in
degrees and distance units. D0 agrees at `(0, 25.5, 1)`. These comparisons
verify the examined states, not every possible coordinate form.

More importantly, the exported waveform samples were compared to Harletty's
additional PCM sources over **384,000 samples per waveform**. Each D3 export
0–3 matches exactly one source, respectively additional sources 0–3, with
zero differing float bit patterns and over 142,000 nonzero samples each.
The D0 export matches additional source 0 exactly, including 826 nonzero
samples. A first one-second silent comparison was discarded as inconclusive.
The remaining four supplemental sources and the type-3 matrix direction are
not resolved by these matches; the PCM analysis below addresses them.

The shorter D1 case is a useful counterexample to trusting the reference
blindly: following its mode-0 field consumption reproduces its exported
coordinates, but leaves unexplained nonzero data before type 3. Those values
are not accepted by the mode-1 reader. A speculative global one-bit shift
also fails to establish a consistent grammar and is not implemented. A
per-record candidate that does consume the form exactly is described next.

This remains offline work. No coordinates, auxiliary labels, fold changes
or `has_objects` changes enter playback. Ten synthetic probe tests cover
coordinate calibration, changed positions/targets/gains, auxiliary presence,
the mode-0 candidate, truncation and exact end consumption in addition to
the earlier CRC checks.

## Mode-0 candidate for the shorter D1 form

The shorter D1 prefix declares two records in mode 0. A candidate reader
consumes each record by reading a four-bit option field equal to 1 where
mode 1 has three zero option bits, then the same position form, and no
reference rows. With that one difference it ends exactly at the byte before
type 3 (bit 187 of a 24-byte prefix, five zero padding bits) on all
**7,484 mode-0 frames** of the single input that carries this form. The two
positions are (-34.5°, 12°, 1) and (34.5°, 12°, 1). A synthetic test builds
the form from fields, changes the azimuths, and checks that the extra bit is
required in each record and that every truncation or extension is rejected.

This is exact consumption, not a grammar. The extra bit's meaning is unknown,
the form was seen in one input, and the probe reports it as
`unverified_mode0` next to `dynamic241=Err(Unsupported(...))` instead of
promoting it into the accepted reader. The PCM analysis below shows why a
mode-0 record carries no reference rows.

## What the PCM says about the folds

The bytes above describe matrices; only the audio can say what they are
matrices *of*. The following comparisons use `xll_pcm_range` dumps (bed in
DCA index order, then the additional sources), sixty-second excerpts, and
integer-domain arithmetic in 24-bit units. They are offline analysis, kept
out of the repository together with the corpus identities; the numbers here
are the results. Sources are numbered `X0..` in decoded order; the compatible
bed uses the reference-layout names, with `Lb`/`Rb` for the rear pair.

### The second waveform set is the four fixed heights

In every alternate profile the last four additional sources sit in the
compatible bed in `L`, `R`, `Lb`, `Rb` order, which is also the row order of
the type-3 candidate. Independently, a least-squares fit of the external
diagnostic's 7.1.4 output on Harletty's decoded sources places exactly those
four at gain 1.000 on its four height outputs, reproduces the compatible bed
at identity (largest deviation 1.5 LSB), and pans the object sources between
rear and rear-height outputs according to their positions.

### Type-3 rows: the embedded height fold plus the height's own speaker

The first entry of each type-3 row is the gain at which that height already
exists in the compatible bed; the second entry is the height's own output at
unity. The profiles differ in the first entry, and the PCM follows the code:

| Profile | Bed-column code | Measured gain of the height in its bed channel |
| --- | ---: | --- |
| D0, two inputs | 61 (unity) | An identical passage in both inputs, in which all five waveforms carry the same signal, reproduces `Lb = X3 + X3·(32768/32768)` with 0.5–0.6 LSB rms residual, `Rb` likewise (0.5–0.7 LSB). Sixty-second least squares: 0.996 and 1.019 for `Lb`, 0.87 and 0.99 for `Rb`. |
| D1, shorter input (20-bit PCM) | 55 (23170/32768) | Single-source clean blocks: `R` 0.702 (19 blocks), `Lb` 0.7185 (209), `Rb` 0.7115 (126). |
| D3, static input | 55 | `Lb` 0.7094 in the one clean block; `Rb` best blocks 0.699–0.722. The front pair is masked by continuous programme in `L`/`R`. |

So the type-3 element carries, over twelve columns, the same fact the
standard type-2 matrix carries over eight: how much of each height feed the
compatible bed already contains, and therefore what to subtract before
rendering the feed at its own position. The D0 unity fold is a genuine
profile difference, not a misread: it is the value the audio reproduces to
the LSB.

### Mode-1 reference rows: the object's fold into the bed

The optional sparse rows attached to a mode-1 position are the object's
contribution to the compatible bed, quantised to gain codes:

- Static D3 input, rows `Lb:61, Rb:21` / `Lb:20, Rb:61` / `Lb:61` / `Rb:61`:
  sixty-second least squares gives `Lb` 0.976 and 0.994 for objects 0 and 2,
  `Rb` 0.989 and 0.996 for objects 1 and 3, and cross terms 0.053 (code 21 is
  0.0562) and 0.035 (code 20 is 0.0501).
- Longer D1 input: `Lb` receives object 0 at 1.00–1.05 in the blocks it
  dominates.
- Moving-object D3 input, 160 metadata states aligned to audio with
  `--segments`: two static objects at (0°, 25.5°) with rows `C:58, L:46,
  R:46` measure `L` 0.406/0.419 and `R` 0.437/0.449 against 0.4217, and `C`
  0.734 + 0.950 = 1.684 against 2 × 0.8414 (the two share position and rows
  and are not separable), with R² 0.93–0.98 over about ninety segments. An
  object rising at azimuth -150° keeps `Lb:61` (measured 0.93–1.17 as R²
  climbs to 0.95) while its `Rb` code steps 13, 15, 16, 18, 19 with
  elevation; objects at ±30° acquire a `C` entry of 12, 15, 18, 21, 23 at
  1.5°, 3°, 4.5°, 6°, 7.5°. Codes that small (below -23 dB) are under what
  these short segments resolve; their trend, not their value, is confirmed.

### Mode-0 objects are folded without rows

The shorter D1 input carries its two mode-0 objects in the bed as well, at
gains that are not on the half-decibel grid: for the object at (-34.5°, 12°),
`L` 0.9152 (10th–90th percentile 0.915–0.916 over 4,028 clean blocks), the
opposite front 0.1305, `Lss` 0.3430 (25th–75th percentile 0.3427–0.3432 over
892 dominant blocks), `Lb` 0.1039, `C` about 0.09–0.13 in the few blocks
that expose it, nothing in LFE; the squares sum to 1.00. This is a panning
law computed by the encoder, not coded rows. Removing a mode-0 object from
the bed therefore means reproducing that law, and one position in one input
is not enough to recover it.

### Gain-code calibration

Every verified code is the decoder's downmix-table entry at index
`4 · code - 3`, i.e. half a decibel per code with 61 as unity: 61 → 32768,
55 → 23170, and now 46 → 13818 and 58 → 27571. The latter two are fixed by
the D0 passage above: `L = X1 + X1(61) + X0(46)` leaves 1.25 and 1.67 LSB
rms in the two inputs, codes 45 and 47 leave 11.1 and 11.7; `C = X0 +
X0(58)` leaves 1.7 and 2.9 LSB, codes 57 and 59 leave 22 and 23. The probe
exposes exactly these four points and keeps every other code raw; the table
relation is a hypothesis outside them.

### The external diagnostic is not an oracle for the folds

Rewriting the four type-3 codes 55 to 0, and separately to 61, with the
envelope CRC recomputed on all 1,096 protected prefixes of the excerpt,
changes no sample of the diagnostic's output (4,608,000 samples compared).
Its bed output is Harletty's compatible bed at identity. It therefore renders
heights and objects on top of a bed that still contains them, and cannot
validate a subtraction. Coordinates and waveform identities from it remain
useful; its rendering does not.

### Remaining reading hypotheses

- The control word `0x3fa` reads, against the type-2 grammar, as an inline
  flag 0, a three-bit field 3, the mix code 61 and a zero flag. Every
  alternate prefix in the corpus carries the same value, so nothing here can
  distinguish that split from any other.
- The auxiliary mask `0x80` is bit 7 of the DTS speaker-activity mask, the
  centre-height position, at unity for D0's single object at (0°, 25.5°).
  This agrees with the existing top-front-centre D0 presentation but is not
  assigned a label in code.

## Local validation

A bounded 64 MiB prefix pass, using anonymous input indices in output:

| Profile | Input streams | Decoded frames | Result |
| --- | ---: | ---: | --- |
| Standard | 31 | 311,648 | Same parsed matrix; all prefix CRCs valid |
| D0 | 3 | 25,169 | Same type-3 candidate rows; outer CRCs valid |
| D1 | 2 | 12,897 | Same type-3 candidate rows; outer CRCs valid |
| D3 | 10 | 66,016 | Same type-3 candidate rows; outer CRCs valid |
| Total | 46 | 415,730 | No extension errors or invalid-prefix results |

Input streams are not necessarily independent programmes. The results describe
the examined prefixes, not full-stream or universal format coverage.

Observed alternate prefix lengths: D0 48 bytes; D1 49 and 54 bytes; D3
72, 73, 74, 75, 76, 78, 79 and 81 bytes. The moving type-3 location is why a
single hard-coded alternate offset would be insufficient.

The eight compatible bed channels were also compared directly to FFmpeg on
one eight-second excerpt per profile: **384,000 samples/channel**, nonzero
reference audio in every case, zero differing float bit patterns after the
known channel permutation, and maximum absolute error zero. This comparison
covers the compatible bed, not an authored clean bed or an alternate spatial
render.

At the latest checkpoint the complete suite passes **211 tests** with
readable corpus/reference environment variables, including D0/D1 PCM checks
and the D3 bridge test, with no DTS:X corpus self-skip; three unrelated
legacy fixture tests self-skip because their local inputs are absent. The
ten probe tests run in normal CI. The build with warnings denied passes.
No A/B listening claim is made; playback behavior has not changed.

## Reproduction and next boundary

```sh
cargo run -p dca --release --example xll_private_metadata -- \
  --max-mb 64 "$STANDARD_CORPUS" "$D0_CORPUS" "$D1_CORPUS" "$D3_CORPUS"
cargo run -p dca --release --example xll_private_metadata -- \
  --max-mb 64 --segments "$D3_CORPUS"
cargo run -p dca --release --example xll_pcm_range -- "$D3_CORPUS" d3.f32 0 60
cargo test -p dca --example xll_private_metadata
RUSTFLAGS='-D warnings' cargo build --all-targets
RUSTFLAGS='-D warnings' cargo test -- --nocapture
```

`--segments` prints one line per run of consecutive frames with the same
observation, with its frame and bed-sample span, so metadata states can be
aligned to a PCM dump for offline analysis.

The full suite additionally needs its existing `HARLETTY_*_CORPUS` and
`HARLETTY_*_REFERENCE` variables. Check output for self-skips. The new probe
requires actual input, rejects empty input, reports counts, and fails on
truncated frames, decode failures and invalid prefix CRCs. Unsupported
metadata remains visibly unresolved. Its byte limit excludes a final frame
that would cross the selected limit.

The reading questions that gated playback work are now answered by the
audio itself: which sources the type-3 rows describe, in which direction,
at which gains, and what the mode-1 rows are. The realtime reader lives in
`dca::XMetadata` with the subtraction table in `dca::FoldPlan`; the bridge
and the offline exporter both build their presentation from it (fixed
heights as labeled channels, objects with transmitted positions, every
stated fold removed from the bed, an unstated fold left in the bed with the
feed muted). What remains:

1. Recover the mode-0 panning law, which needs more mode-0 inputs than the
   single one at hand; until then a mode-0 object stays in the bed and its
   object channel is silent.
2. Confirm the gain-code table relation on codes other than the four
   verified points; the reader applies it to every code in 1..=61.
3. Explain the type-3 control word and the general association navigation,
   which the corpus cannot distinguish from constants.
4. A/B listen before merging any of this into a release.



## Research provenance

The investigation was prompted by the author's first-hand
[legacy DTS:X article](https://touch-max.ru/zvuk/dts-iznutri-gde-v-dts-hd-ma-spryatany-vysotnye-kanaly-i-obekty).
The author's v0.2.5 diagnostic was inspected and run locally against anonymous
excerpts in an isolated environment without network access. Its reported
fields and parser behavior supplied hypotheses; original field readers,
synthetic fixtures, corpus CRCs and existing Harletty/FFmpeg comparisons were
used here. No external binary, decoder implementation or corpus audio is
vendored. The offline tool neither invokes nor depends on that executable.
