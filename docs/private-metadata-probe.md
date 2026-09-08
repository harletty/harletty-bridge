# Private metadata: standard matrix and alternate-profile candidates

Status: offline research only. The decoder, ABI, fold configuration and output
presentations are unchanged. `dca/examples/xll_private_metadata.rs` is an
independent diagnostic; it is not a general DTS:X navigation parser.

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

These indices fit lower/upper pairs in the full 12-channel mask order. This
is a reproducible **structural hypothesis**, not a validated fold matrix.
In particular, it does not identify which of the five, six or eight decoded
waveforms supplies a row, explain the remaining sources, establish matrix
direction/sign, or recover object motion. The D0 unity-looking entries must
not be substituted for the existing experimental fold coefficients.

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
not resolved by these matches.

The shorter D1 case is a useful counterexample to trusting the reference
blindly: following its mode-0 field consumption reproduces its exported
coordinates, but leaves unexplained nonzero data before type 3. Those values
are not accepted by this probe. A speculative one-bit shift also fails to
establish a consistent grammar and is not implemented.

This remains offline work. No coordinates, auxiliary labels, fold changes
or `has_objects` changes enter playback. Nine synthetic probe tests cover
coordinate calibration, changed positions/targets/gains, auxiliary presence,
truncation and exact end consumption in addition to the earlier CRC checks.

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

The complete suite passes **206 tests** with readable corpus/reference
environment variables, including D0/D1 PCM checks and the D3 bridge test,
and no corpus self-skip messages. The build with warnings denied passes.
No A/B listening claim is made; playback behavior has not changed.

## Reproduction and next boundary

```sh
cargo run -p dca --release --example xll_private_metadata -- \
  --max-mb 64 "$STANDARD_CORPUS" "$D0_CORPUS" "$D1_CORPUS" "$D3_CORPUS"
cargo test -p dca --example xll_private_metadata
RUSTFLAGS='-D warnings' cargo build --all-targets
RUSTFLAGS='-D warnings' cargo test -- --nocapture
```

The full suite additionally needs its existing `HARLETTY_*_CORPUS` and
`HARLETTY_*_REFERENCE` variables. Check output for self-skips. The new probe
requires actual input, rejects empty input, reports counts, and fails on
truncated frames, decode failures and invalid prefix CRCs. Unsupported
metadata remains visibly unresolved. Its byte limit excludes a final frame
that would cross the selected limit.

Next work should recover the shorter D1 variable form and association
navigation, explain the type-3 control word, then validate its waveform-to-row
identity and matrix direction against independently rendered references.
Until those relationships are established, no alternate presentation or
`has_objects` behavior should change.

## Research provenance

The investigation was prompted by the author's first-hand
[legacy DTS:X article](https://touch-max.ru/zvuk/dts-iznutri-gde-v-dts-hd-ma-spryatany-vysotnye-kanaly-i-obekty).
The author's v0.2.5 diagnostic was inspected and run locally against anonymous
excerpts in an isolated environment without network access. Its reported
fields and parser behavior supplied hypotheses; original field readers,
synthetic fixtures, corpus CRCs and existing Harletty/FFmpeg comparisons were
used here. No external binary, decoder implementation or corpus audio is
vendored. The offline tool neither invokes nor depends on that executable.
