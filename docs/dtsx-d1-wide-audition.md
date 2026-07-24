# Alternate-profile experimental presentations

This note records the evidence and reproduction steps for the experimental D0
height and D1 wide-channel presentations. They are research, not normative
format claims. Harletty now selects the standard four-source or inferred
alternate presentation automatically from the decoded extension profile.

## Corpus update

The tracks added alongside and after the D0/D1 samples were probed over their
first 128 MiB with `xll_corpus_probe`. All use the standard four-source XLL-X
profile and decode without extension errors:

| Track | Sources | PCM | Initial source activity |
| --- | ---: | ---: | --- |
| *standard-01* | 4 | 24-bit | all active, about 99% non-zero |
| *standard-02* | 4 | 24-bit | all active, about 94–95% non-zero |
| *standard-03* | 4 | 16-bit | all active, about 87% non-zero |
| *standard-04* | 4 | 16-bit | all active, about 89% non-zero |
| *standard-05* | 4 | 24-bit | all active, about 98% non-zero |
| *standard-06* | 4 | 24-bit | all active, about 99% non-zero |
| *standard-07* | 4 | 18-bit | all active, about 99% non-zero |
| *standard-08* | 4 | 18-bit | X0/X1/X3 about 90%; X2 about 57% non-zero |
| *standard-09* | 4 | 18-bit | X0/X1 about 80%; X2/X3 about 89% non-zero |
| *standard-10* | 4 | 18-bit | X0/X1 about 82%; X2 about 79%; X3 about 91% non-zero |
| *standard-11* | 4 | 18-bit | X0/X1/X2 about 87–89%; X3 about 93% non-zero |
| *standard-12* | 4 | 18-bit | X0/X1/X2 about 76–77%; X3 about 95% non-zero |
| *standard-13* | 4 | 18-bit | all active, about 89–93% non-zero |
| *standard-14* | 4 | 18-bit | X0/X1 about 44%; X2 about 88%; X3 about 94% non-zero |
| *standard-15* | 4 | 24-bit | all active, about 99% non-zero |
| *standard-16* | 4 | 24-bit | X0/X1 fully active; X2/X3 silent in the opening range |
| *standard-17* | 4 | 24-bit | all active, about 79% non-zero |
| *standard-18* | 4 | 24-bit | all active, about 99% non-zero |
| *standard-19* | 4 | 24-bit | all active, more than 99% non-zero |
| *standard-20* | 4 | 24-bit | all active, 100% non-zero |
| *standard-control-A* | 4 | 24-bit | X0/X1 about 72–73%; X2/X3 about 63% non-zero |
| *standard-control-B* | 4 | 24-bit | X0/X1 about 89%; X2/X3 about 71% non-zero |
| *standard-control-C* | 4 | 24-bit | all active, more than 99% non-zero |

They strengthen the ordinary 7.1.4 regression corpus but add no new alternate
layout. *D0 corpus A* and *D0 corpus B* provide two independent
D0/five-source programmes. *D1 corpus B* is a second D1/six-source stream,
with a longer CRC-delimited prefix than *D1 corpus A*.

The eight related standard-07 through standard-14 tracks also retain the
standard 22-byte prefix. The first four were scanned over 64 MiB each and the
remaining four over 128 MiB, for 152,397 decoded extension frames in total:

```text
02 00 08 50 28 4b fa 71 0d 62 02 fa 02 dc 13 71 0d c8 37 3c f1 02
```

The three controlled clips retain that exact prefix over their complete
elementary streams (17,642 frames total). Their source extractions are
bit-identical to the streams in the downloaded containers. Despite the visible
and audible moving material, neither *standard-control-B* nor *standard-control-C*
introduces an alternate layout or a fifth extension waveform.

The five later disk-inventory additions were sampled over 100–128 MiB near the
start, middle and end of each programme. Every sampled frame remains a standard
four-source, 24-bit XLL-X presentation with no extension decode error. Each
range also retains the same 22-byte prefix shown above, locates its only
four-channel XLL set at byte 22 and validates that channel-set header's
CRC16-CCITT. They add fixed 7.1.4 programme material, not another D0/D1 layout
or an encoded fifth object source.

The fixed-height pan analysis nevertheless found useful listening controls:

- *standard-17* has strongly coherent single-corner material around `00:36–00:40`,
  moving between rear-left and the front-left/front-right height outputs.
- *standard-20* has a front-height left-to-right transition around
  `01:26:13–01:26:29`.
- *standard-18* opens with an almost static four-height image whose compatible-bed
  links reproduce the expected `+0.7071` fold on all four corners.
- the sampled action ranges from *standard-16*, *standard-19* and
  *standard-18* were predominantly diffuse at the conservative rank-one gate;
  absence of a selected window is not proof that the full programme contains
  no rendered motion.

The extracted streams remain local corpus artifacts and are intentionally not
identified by source title, path or content hash in this repository.

## Second D0 encode control

An alternate *D0 corpus A* encode is a second extraction of the same D0
programme, not an independent five-source title. Its first 128 MiB contain
15,797 D0 frames, all with five 24-bit sources and no decode error. X0 is
nearly silent in that opening range (3.4% non-zero), while X1–X4 are each
about 87% non-zero, matching the qualitative signature of the first dump.

Both dumps retain the same layout over 20,000 frames:

```text
(profile D0, first set 1 channel/bits 0, second set 4 channels/bits 0)
```

Their fixed D0 prefixes are byte-identical and their only post-audio trailers
are two to five zero bytes. A 16 MiB paired comparison aligns the alternate dump
94 frames before the first dump; all 2,623 overlapping payloads, EXSS reserved
words and decoded source RMS envelopes then match exactly (`r = 1.0`). The
different whole-stream MD5 therefore reflects trim/framing, not a different
spatial presentation.

## D0 fixed-height working map

The complete *D0 corpus B* stream is a second independent D0 programme:
557,057 frames, five 24-bit sources per frame and no extension decode error.
It has the same fixed D0 prefix and `1 + 4` channel-set structure as
*D0 corpus A*.

Across both programmes, X1–X4 retain the front-left, front-right, rear-left,
rear-right top-quartet signature. X0 is sparse and front-centred. Its strongest
global relations are with X1/X2 rather than X3/X4, and after controlling for the
top quartet its compatible-bed relation remains concentrated in C/L/R rather
than the rear channels. The current fixed working map is therefore:

```text
X0 = TFC
X1 = TFL    X2 = TFR    X3 = TBL    X4 = TBR
```

`TFC` is the standard abbreviation for Top Front Center. This assignment is
still an inference from programme material, not decoded speaker metadata.

The apparent explained variance does not establish a fold coefficient. A
whole-programme regression of X0 on all four tops explains about 32.2% of X0
in *D0 corpus B* but only 11.4% in *D0 corpus A*. This is the fraction of X0
predicted by correlated programme content, not the fraction of X0 embedded in
TFL/TFR. Windowed X0-to-TFL/TFR fits confirm the distinction: even
near-perfect-correlation windows produce gains ranging from nearly zero to
multiple units, with different left/right gains and no common plateau across
the two titles. Removing the top quartet from the compatible 7.1 bed also
leaves only weak X0-to-bed partial correlations.

Consequently the D0 presentation emits the five sources as fixed labeled
channels and applies only a conservative partial subtraction: `0.5 * X0` from
C and `0.707107 * X1..X4` from their corresponding compatible-bed channels.
These values are experimental controls, not decoded metadata or a
profile-exact D0 fold matrix. The partial subtraction deliberately leaves
some material that may have been authored in both the lower and top feeds.

## D1 system-identification result

The compatible bed and the six decoded D1 sources were analyzed in 4096-sample
windows. Windows were retained only when the left/right three-source predictor
matrix had condition number at most 3 and all three corresponding bed channels
had `R² >= 0.999`.

Between 50 and 66 minutes of *D1 corpus A*, 66 left and 48 right windows
passed the joint gate. The repeated X2/X3 contribution was:

```text
X2 -> L  ~= 1.183    X2 -> Ls ~= 0.209    X2 -> Lb ~= 0
X3 -> R  ~= 1.183    X3 -> Rs ~= 0.209    X3 -> Rb ~= 0
```

The relation is sample-aligned and well described by scalar gains. No delay or
frequency-dependent decorrelation filter was observed. The front-heavy,
left/right-symmetric fold supports treating X2/X3 as `Lw/Rw` candidates.

Program material alone cannot prove the exact coefficients: native bed content
can be correlated with the extension sources and produces other apparently
exact local matrices. *D1 corpus B* now supplies a second D1 stream, but the
conditioned system-identification result above has not yet been reproduced on
it. Cross-validation there, a synthetic encode, or native 9.1.4 output from a
reference decoder is still required before this becomes a production
presentation.

## Extended D1 and D3 profiles

The complete *D1 corpus B* elementary stream contains 6,392 D1 frames.
The older parser assumed that every D1 outer suffix started after a 49-byte
prefix. *D1 corpus B* instead has a 54-byte prefix:

- the CRC16-CCITT over the first 54 bytes is zero;
- the standard outer suffix follows immediately;
- the outer control starts with `b2`;
- the two CRC-valid XLL channel sets still contain `2 + 4` sources.

The decoder now finds a unique outer suffix within a bounded 96-byte window and
accepts it only when the preceding prefix has a valid CRC. Both known D1 prefix
lengths therefore use the same relative control and channel-set parser.
*D1 corpus B* decodes all 6,392 frames into six 24-bit sources without an
extension error, clipped value or non-finite sample.

*D3 corpus A* introduces syncword `f1 40 00 d3`, present once
in every one of its 7,674 frames. D3 uses the same CRC-delimited outer and
inner structure but carries two four-channel sets. All 7,674 frames decode as
eight 24-bit sources without an extension error. The bridge exposes all eight
unchanged sources as named objects. Their static positions are an inferred
listening layout, not decoded metadata.

The complete D3 source-to-bed correlations, measured over 3,929,088 aligned
samples, show the strongest sample-level relations:

| Source | Bed channel | Pearson `r` |
| --- | --- | ---: |
| X0 | Lb | 0.100 |
| X1 | Rb | 0.095 |
| X2 | Lb | 0.278 |
| X3 | Rb | 0.269 |
| X4 | Ls | 0.169 |
| X5 | Rs | 0.165 |
| X6 | Lb | 0.673 |
| X7 | Rb | 0.683 |

X6/X7 are strongly left/right-associated with the rear bed pair. X2/X3 show
the same pairing more weakly; X4/X5 are broad left/right components spread
across fronts, sides and backs. No extension source has a meaningful
sample-level relation with LFE. Frame-RMS correlations for X6/X7 reach about
0.83 against both rear channels because their activity envelopes commonly
rise together; the sample correlations, not those envelope correlations,
provide the left/right evidence.

The still-uninterpreted D3 data in *D3 corpus A* separates into static and
dynamic regions:

| Region | Whole-stream observation |
| --- | --- |
| 73-byte D3 prefix | one byte-identical value in all 7,674 frames |
| Channel topology and mapping bits | fixed `4 + 4`, mapping bits zero |
| Outer control | 643 distinct sequences |
| Inner control | 1,388 distinct sequences |
| Header/audio layout | 2,837 size/offset configurations |
| Post-audio trailer | one to four zero bytes only |
| 91-bit EXSS descriptor tail | fixed length; 1,547 captured 64-bit words |

The lower 32 bits of the captured descriptor word remain `70aa2220`; its upper
32 bits vary. The fixed D3 prefix could contain a static layout or fold
declaration, but the variable controls track channel-set sizes and XLL
navigation closely enough that they should not be interpreted as dynamic gain
or position metadata without further evidence.

Five subsequently completed corpus entries confirm D3 as a recurring profile:

| Demo | Frames | Sources | PCM | Distinct CRC-delimited prefixes |
| --- | ---: | ---: | ---: | ---: |
| *D3 corpus B* | 13,731 | 8 | 24-bit | 161 |
| *D3 corpus C* | 10,986 | 8 | 24-bit | 451 |
| *D3 corpus D* | 9,625 | 8 | 24-bit | 2 |
| *D3 corpus E* | 14,888 | 8 | 24-bit | 27 |
| *D3 corpus F* | 9,292 | 8 | 24-bit | 2 |

All 58,522 new frames retain the same `4 + 4` topology and zero mapping bits.
Four streams decoded immediately with the D3 parser. *D3 corpus E* contributed 94
valid `c6` outer controls whose geometry field extends through bit 31 rather
than the bit-25 limit sufficient for D0/D1 and the earlier D3 controls. The
bounded D3 geometry search now covers that form; all 14,888 *D3 corpus E* frames
decode without errors.

Four further completed demos add 72,127 whole-stream D3 frames:

| Demo | Frames | Prefixes | Header layouts | Outer controls | Inner controls |
| --- | ---: | ---: | ---: | ---: | ---: |
| *D3 corpus G* | 22,975 | 2 | 10,780 | 2,085 | 1,785 |
| *D3 corpus H* | 21,034 | 306 | 19,229 | 1,283 | 1,116 |
| *D3 corpus I* | 15,369 | 1 | 7,232 | 1,174 | 1,897 |
| *D3 corpus J* | 12,749 | 26 | 283 | 122 | 1,980 |

Every frame again decodes as two four-channel, zero-mapping-bit sets of
24-bit PCM without an extension error. The complete D3 corpus now contains
138,323 decoded frames across ten demos, including *D3 corpus A*. The unknown
regions are not uniformly static: *D3 corpus I* has one CRC-delimited prefix while
*D3 corpus H* has 306, and all four streams have variable controls and
header/audio offsets. Conversely, their topology, mapping bits and PCM
resolution remain constant. Their post-audio regions contain only zero
padding, from zero to six bytes depending on the stream. This reinforces the
working interpretation that the varying controls and offsets are XLL
framing/navigation data, not evidence of a changing source-to-speaker matrix.

The new corpus also shows that a byte-identical 73-byte prefix is a property of
*D3 corpus A*, not a general D3 rule. Prefix length remains CRC-delimited and
bounded, while prefix content can change with the programme. Topology,
channel-set mapping and PCM resolution remain fixed across those changes.

Whole-stream source-to-bed correlations vary with programme content but repeat
several directional signatures:

- X6 is most often associated with Lb and X7 with Rb;
- X4/X5 generally form a broad left/right side pair;
- X2/X3 can behave as a front L/R pair (*D3 corpus B*) or a rear Lb/Rb pair
  (*D3 corpus F*);
- X0/X1 range from weak/sparse material to a strong rear pair in *D3 corpus E* and
  duplicated left-heavy material in *D3 corpus D*.

The four latest D3 streams reproduce that programme dependence:

- *D3 corpus G* has X0/X1 weakly front-left/front-right, X2/X3 rear-associated,
  and X6/X7 most strongly paired with Lb/Rb (`r = 0.608/0.575`);
- *D3 corpus H* has a broad frontal X2/X3 pair and a very strong X6/Lb,
  X7/Rb relation (`r = 0.768/0.800`);
- *D3 corpus I* has X0/Lb and X1/Rb at `r = 0.458/0.545`, X2/X3 front-associated,
  and X6/X7 rear-associated;
- *D3 corpus J* makes X0/X1 and X2/X3 sparse, pairwise-identical
  signals, while X4–X7 carry most of the extension activity.

This programme dependence rules out assigning eight fixed speaker labels from
global correlation alone. D3 is therefore presented as eight named objects at
the inferred static defaults described below.

### Conservative D3 bed-fold limits

`scripts/analyze-dtsx-bed-fold-limits.py` tests every D3 extension source
against every compatible-bed channel without fitting a free matrix. For each
512-sample block where the extension source is within 60 dB of its peak block
energy, it evaluates:

```text
E(B - gX) - E(B) = g² E(X) - 2g <B,X>
```

The reported limit is the largest positive `g` for which the right-hand side
is negative in at least 95% of those active blocks. Exact-silence blocks are
excluded: counting their unchanged energy as a successful decrease would let
a sparse source pass the test at an arbitrary gain.

The limit is not itself an estimated fold coefficient. If `B = aX` exactly,
energy falls for `0 < g < 2a`; the maximum is `2a`. The report therefore also
gives `p05_beta = limit / 2`, the corresponding fifth-percentile projection
coefficient. This remains a conservative content measurement, not a decoded
matrix.

The ten D3 `4+4` streams give the following recurring results with
the default 512-sample protocol:

| Candidate | Streams passing 95% | Median limit | Median `p05_beta` | Pass at -3 dB | Pass at -6 dB |
| --- | ---: | ---: | ---: | ---: | ---: |
| X6 -> Lb | 5/10 | 0.592 | 0.296 (-10.6 dB) | 1/10 | 3/10 |
| X7 -> Rb | 4/10 | 0.511 | 0.256 (-11.8 dB) | 1/10 | 2/10 |
| X2 -> Lb | 2/10 | 0.846 | 0.423 (-7.5 dB) | 1/10 | 2/10 |
| X3 -> Rb | 2/10 | 0.733 | 0.366 (-8.7 dB) | 1/10 | 1/10 |
| X4 -> any bed | 0/10 | - | - | 0/10 | 0/10 |
| X5 -> any bed | 0/10 | - | - | 0/10 | 0/10 |

X0/X1 have only isolated 512-sample candidates and no corpus-level gain.
Longer blocks make the recurring rear relation clearer but also average away
local energy increases: at 2,048 samples X6/Lb passes 7/10 and X7/Rb 4/10;
at 4,800 samples they pass 8/10 and 5/10. Changing the activity gate from
-60 to -40 dB leaves the 512-sample X6/X7 counts at 5/10 and 4/10.

The defensible inference is directional, not a fold matrix: X6 is repeatedly
rear-left-associated and X7 rear-right-associated. X2/X3 sometimes reproduce
the same rear pairing, while X4/X5 consistently fail this conservative
energy test. No positive gain satisfies the requested 95% condition for all
ten programmes, and neither -3 nor -6 dB is corpus-wide safe under this
criterion. Failure does not disprove a real authoring fold: independent or
deliberately phase-opposed content can make a correct subtraction increase
energy locally.

Example:

```console
python3 scripts/analyze-dtsx-bed-fold-limits.py \
  /path/to/extracted-audio \
  --output /tmp/d3-bed-fold-limits.tsv \
  --plot-dir /tmp/d3-bed-fold-coverage-maps \
  --position-plot-dir /tmp/d3-bed-fold-positions \
  --animation-dir /tmp/d3-bed-fold-animations \
  --video-dir /path/to/original-video-directory \
  --plot-gain 0.5
```

The optional plot directory receives one fixed-scale PNG per stream: bed
channels on the horizontal axis, extension sources on the vertical axis and
colour equal to the active-block coverage at the selected fixed gain.
Supported plot gains are `0.5` (-6.02 dB), `0.707107` (-3.01 dB) and `1.0`
(0 dB).

The optional position plot uses the selected fixed-gain coverages as
barycentric weights over the seven spatial bed coordinates. LFE is excluded
because it has no spatial coordinate. Each stream receives one common-scale
`x/y` PNG, and the exact derived positions are written to `positions.tsv`.

The animation mode recomputes those coverage barycentres in a one-second
sliding window and emits a real-time H.264 MP4 per stream. Original MKV audio
start PTS and container duration are preserved: no points are shown during
video pre-roll or after decoded DTS:X audio ends. Marker size and opacity
track extension-source activity, a three-second trail shows recent movement,
and `temporal-positions.tsv` retains the underlying coordinates. The video is
a silent research visualization and does not claim decoded object motion.

### D3 coordinate-field falsification

`xll_d3_frame_features` exports the CRC-delimited prefix, outer and inner
controls, EXSS descriptor tail and known layout quantities for every decoded
D3 frame. `scripts/analyze-d3-coordinate-fields.py` aligns those values with
the one-second coverage windows above. It tests Cartesian `x/y`, radius, and
azimuth as `sin/cos` rather than correlating a wrapped angle directly.

The scan includes every byte and bit of each region, plus every possible
8-, 10- and 12-bit field in the still-unknown prefix. Each candidate prefix
field is also interpreted cyclically as a possible quantized angle. Linear
dependence on all eight extension activity envelopes and on the known payload,
header, geometry and boundary quantities is removed before comparison. A
candidate is selected on the first half of each programme and checked on the
second half with block-shifted temporal controls.

The corpus supplies important negative controls:

- *D3 corpus I* has no Atmos position change after setup, yet its outer and inner
  controls change on 7,275 and 14,853 of 15,368 frame boundaries;
- *D3 corpus A* has one byte-identical prefix despite 1,683 Atmos position
  changes;
- *D3 corpus D* and *D3 corpus F* each have two prefix values and one
  prefix transition despite 1,015 and 4,940 Atmos position changes;
- *D3 corpus E* has 27 prefix values and 26 transitions despite 4,902 Atmos position
  changes.

No candidate in the unknown prefix repeats convincingly across programmes.
The best same-direction counts are only 3/10 programmes for Cartesian `x`,
`y`, radius and azimuth cosine, and 4/10 for azimuth sine. Their corresponding
median absolute partial correlations are 0.021, 0.039, 0.105, 0.117 and
0.011. No prefix field reaches five programmes with a median absolute partial
correlation above 0.15. Large single-programme correlations do occur, up to
almost one in a low-state-count *D3 corpus J* range, but disappear at
the same bit offset in the other programmes and are compatible with section
identity or estimator noise.

Some outer-control bits look more repeatable in polar form, but the leading
candidates are already structural. Outer bit 10 belongs to the encoded span
that locates the second channel-set header. Bit 30 falls in that variable-width
span or in the following segment geometry. Bit 36 is geometry when its detected
offset is at least 24 and otherwise belongs to the still-uninterpreted trailing
control bits, so it is not even a stable semantic position across layouts. The
EXSS tail is likewise the already verified payload offset/size navigation
word. Remaining inner/outer correlations change sign or source between
programmes, and the fixed-position *D3 corpus I* control produces similarly large
local correlations.

There is therefore no current evidence for a dynamic Cartesian or polar
coordinate in these regions. Polar conversion exposes more local correlation
because it normalizes the noisy barycentre radius, but it does not reveal a
stable field. This does not prove that D3 lacks spatial declarations: the
piecewise-constant prefix may still describe a static layout or cluster
configuration. Establishing a dynamic coordinate requires a controlled encode
whose PCM is held constant while one known source is swept through position.

### D3 prefix versus temporal bed-fold coverage

The temporal analysis now retains the complete `X x bed` coverage tensor, not
only its barycentre. `--temporal-coverage-output` writes the fixed-gain
coverage, active-block count and validity for every video-time window.
`scripts/analyze-d3-prefix-coverages.py` aligns those windows with the D3
prefix observed over the same second and performs three tests:

1. treat the exact prefix as a categorical state and measure explained
   coverage variance;
2. learn state means on the first half and predict only prefix states seen in
   the second half;
3. scan every possible 8-, 10- and 12-bit field after removing the verified
   two-byte CRC, while partialling out extension activity and known navigation
   quantities.

The categorical result is dominated by programme segmentation:

| Demo | Prefix states | Median in-sample `eta²` | Median holdout skill | Holdout states seen | Coverage RMS at/away from prefix transitions |
| --- | ---: | ---: | ---: | ---: | ---: |
| *D3 corpus B* | 161 | 0.363 | 0.054 | 1.000 | 0.062 / 0.044 |
| *D3 corpus H* | 306 | 0.904 | unavailable | 0.007 | 0.054 / 0.055 |
| *D3 corpus C* | 451 | 0.408 | 0.055 | 0.595 | 0.066 / 0.090 |
| Other seven D3 demos | 1--27 | at most 0.012 median | at most 0.003 median | - | no repeatable excess |

*D3 corpus H* demonstrates the overfit directly: an exact prefix labels a
short section well enough to explain about 90% of coverage variance in the
same samples, but only 0.7% of the second-half windows reuse a state learned
in the first half. Its coverage changes are no larger at prefix transitions
than elsewhere. *D3 corpus B* has a real transition association, but only a small
median predictive gain; *D3 corpus C* has no transition excess.

The most suggestive exploratory field is the ten bits beginning at prefix bit
299:

```text
value = ((prefix[37] & 0x1f) << 5) | (prefix[38] >> 3)
```

It repeatedly takes values such as 94, 542, 635 and 964. Mapping the value to
`cos(2*pi*value/1024)` produces a front/rear-looking signature for X2/X3 in
*D3 corpus B* and *D3 corpus C*. That relationship does not survive the independent
programme control. Across all ten programmes, the programme-mean correlations
are only `+0.186` for X2/L, `-0.217` for X2/Lb, `+0.176` for X3/R and `-0.236`
for X3/Rb. More importantly, on the seven programmes outside the three where
the clue was discovered, they become `-0.091`, `+0.073`, `-0.091` and
`+0.059`: no front/rear prediction remains. In the second half, the field is
constant in *D3 corpus B*, is weak and non-significant in *D3 corpus H*, and
retains only the rear relation in *D3 corpus C*. No scanned field keeps the same
coverage relationship across the corpus.

Exact prefixes reused by different demos provide another control. Four shared
prefix pairs have enough active coverage for comparison. Only the *D3 corpus G*
versus *D3 corpus H* pair is more similar than most different-prefix pairs
from the same programmes (91st percentile). The other shared pairs land at
the 38th, 20th and 0th percentiles. An exact shared prefix therefore does not
identify a reusable bed-fold matrix.

The bounded conclusion is that prefix state and bed-fold coverage can be
correlated inside a programme because both follow programme sections. The
current corpus does not support interpreting the prefix, or bit 299, as a
normative or programme-independent fold declaration. Bit 299 remains worth
tracking as a structured configuration field, but its angular appearance is
not yet a decoded coordinate.

*paired standard D* is not D3: its complete 20,148-frame stream is the standard
four-source, 24-bit profile, with all four sources active in about 95% of
frames and no decode error. Its sample correlations most strongly associate
X2 with Lb (`r = 0.773`) and X3 with Rb (`r = 0.764`); X0/X1 are broader,
weaker left/right components. It expands the ordinary profile corpus but
requires no parser change.

## Paired Atmos comparison

The matching Atmos editions of all fifteen corpus programmes were extracted as
elementary TrueHD and decoded with the local `truehdd` 0.4.0 build. Every
presentation contains sixteen 48 kHz PCM elements: an LFE-only bed and fifteen
dynamic objects. The DAMF metadata and PCM were retained as
`.atmos`, `.atmos.audio` and `.atmos.metadata` files.

The alternate encodes were aligned on 512-sample total-power envelopes. The
following table reports motion after the first matched programme second, so
the common initial placeholder position is not counted. `Envelope r` measures
programme-level temporal agreement. `PCM r` is the mean, over DTS bed and
extension channels, of the strongest absolute sample correlation with any
Atmos element in an active ten-second range.

| Demo | DTS profile/sources | Moving Atmos objects | Position changes | Offset | Envelope `r` | Mean best PCM `|r|` |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| *D3 corpus A* | D3 / 8 | 15 | 1,683 | +4.981 s | 0.990 | 0.181 |
| *paired standard A* | standard / 4 | 15 | 2,672 | -0.021 s | 0.983 | 0.184 |
| *D1 corpus B* | D1 / 6 | 15 | 3,264 | +4.981 s | 0.902 | 0.201 |
| *D3 corpus B* | D3 / 8 | 10 | 129 | +9.984 s | 0.967 | 0.164 |
| *paired standard B* | standard / 4 | 15 | 777 | +9.984 s | 0.915 | 0.167 |
| *paired standard C* | standard / 4 | 15 | 713 | +5.099 s | 0.933 | 0.376 |
| *D3 corpus G* | D3 / 8 | 13 | 430 | +5.024 s | 0.659 | 0.048 |
| *D3 corpus H* | D3 / 8 | 15 | 10,454 | +10.123 s | 0.799 | 0.238 |
| *D3 corpus I* | D3 / 8 | 0 | 0 | +9.984 s | 0.949 | 0.285 |
| *D3 corpus C* | D3 / 8 | 15 | 6,042 | +5.003 s | 0.961 | 0.209 |
| *D3 corpus J* | D3 / 8 | 15 | 1,388 | +4.971 s | 0.926 | 0.342 |
| *D3 corpus D* | D3 / 8 | 15 | 1,015 | +6.891 s | 0.980 | 0.221 |
| *paired standard D* | standard / 4 | 15 | 2,768 | -0.021 s | 0.835 | 0.269 |
| *D3 corpus E* | D3 / 8 | 15 | 4,902 | +4.939 s | 0.968 | 0.190 |
| *D3 corpus F* | D3 / 8 | 15 | 4,940 | +9.984 s | 0.821 | 0.184 |

Eleven pairs have total-power envelope correlation above 0.90, establishing
that they contain the same timed programme rather than unrelated material.
The weaker *D3 corpus G*, *D3 corpus H*, *paired standard D* and *D3 corpus F* results are
consistent with different edits or more substantial format-specific mixing.

The PCM is nevertheless not the same multichannel master copied or permuted
between formats. This remains true for D3: its compatible 7.1 bed plus eight
extension sources gives the same total of sixteen signals as the Atmos
presentation, but the mean best sample correlations are only 0.048–0.342.
A static least-squares matrix fitted over each active probe range explains
almost none of the D3 extension PCM. The best exceptions are *paired standard C*
(standard four-source, about 52% in-sample explained extension energy),
*D3 corpus J* (about 30%), *D3 corpus I* (about 14%) and *D3 corpus H*
(about 14%); those fits collapse on the second half of the range, so they are
not stable fold matrices.

Five-second power fits usually improve over a whole-programme fit, and the
dominant Atmos predictor changes in roughly 41–87% of adjacent windows. This
is compatible with a time-varying render or clustering, but it is not proof:
the local fits remain weak enough that independently authored
format-specific mixes can produce the same observation.

The paired corpus therefore supports these bounded conclusions:

- the Atmos editions are not generally fixed-object masters; most have
  substantial movement, with *D3 corpus H*, *D3 corpus C*, *D3 corpus E* and
  *D3 corpus F* especially dynamic;
- *D3 corpus I* is a useful fixed control: after its setup event all fifteen Atmos
  objects remain at fixed coordinates, while *D3 corpus B* is mostly fixed with
  small local movements;
- D3 is not a direct carriage of the fifteen Atmos object waveforms, despite
  its matching sixteen-signal total;
- the evidence is consistent with DTS:X using a different, possibly dynamic
  clustering or renderer-domain allocation, but does not distinguish that
  from a separately authored format-specific mix;
- consequently neither the Atmos object IDs nor a correlation-derived
  one-to-one speaker map should be assigned to D3 sources.

### D3 corpus I fixed-layout control

*D3 corpus I* permits a more constrained comparison because the Atmos presentation
stops moving after its setup event. It is not a conventional 7.1.6 bed:
the decoder reports an LFE-only bed and fifteen objects. Those objects settle
at the following fixed normalized coordinates:

| Atmos signal | Coordinate `(x, y, z)` | Nominal position |
| --- | --- | --- |
| O10 | `(-1, 1, 0)` | L |
| O11 | `(1, 1, 0)` | R |
| O12 | `(-1, 0.677, 0)` | Lw |
| O13 | `(0, 1, 0)` | C |
| O14 | `(-1, 0, 0)` | Ls |
| O15 | `(-1, -1, 0)` | Lb |
| O16 | `(-1, 1, 1)` | TFL |
| O17 | `(1, 0.677, 0)` | Rw |
| O18 | `(1, 0, 0)` | Rs |
| O19 | `(-1, 0, 1)` | TML |
| O20 | `(1, -1, 0)` | Rb |
| O21 | `(-1, -1, 1)` | TBL |
| O22 | `(1, 1, 1)` | TFR |
| O23 | `(1, 0, 1)` | TMR |
| O24 | `(1, -1, 1)` | TBR |

The coordinates therefore contain positions matching every DTS 7.1 bed
label, in addition to Lw/Rw and six top positions. Content matching is less
direct. At the common fine alignment of about ten samples, only L and R retain
strong whole-programme sample correlations with their namesakes
(`|r| = 0.576/0.571`). In each DTS bed channel's most active ten seconds,
with a bounded `+/-1024`-sample delay search:

- L matches Atmos L first (`|r| = 0.732`); R matches Atmos R essentially
  jointly with L (`|r| = 0.723`);
- LFE matches LFE first, but only at `|r| = 0.275`;
- Ls and Rs match their namesakes second at `|r| = 0.307/0.296`;
- Lb matches Lb third at `|r| = 0.491`;
- C and Rb do not select their nominal Atmos positions.

This is compatible with matching physical positions whose programme content
was allocated differently, but it does not establish a common 7.1 PCM bed.
The useful source similarity groups are likewise many-to-many:

- DTS L/R and Atmos L/R are the robust direct pair;
- DTS X2/X3 are a robust left/right front-elevation pair. In their active
  range, X2 matches TFL at `|r| = 0.877` and X3 matches TFR at `|r| = 0.864`,
  but the same Atmos range has Rw/TFR correlation `0.993` and Lw/TFL
  correlation `0.991`. X2/X3 therefore identify the correlated
  front/high-left and front/high-right groups, not unique speaker labels;
- X0/X1 are sparse and weak at sample level; X4/X5 follow broad
  front-left/front-right material; X6/X7 remain broad and ambiguous.

A bounded folding test used `corrected bed = bed - f * X`, with a shared
left/right coefficient, `0 <= f <= 1`, and no unconstrained gain compensation.
It measured the whole programme and independent five-second windows:

| Bed pair and source pair | Best `f` | Level | Baseline `|r|` | Best `|r|` | Median window gain |
| --- | ---: | ---: | ---: | ---: | ---: |
| L/R minus X4/X5 | 0.765 | -2.33 dB | 0.5731 | 0.5792 | +0.0014 |
| Ls/Rs minus X2/X3 | 1.000 | 0.00 dB | 0.0256 | 0.0855 | +0.0000 |
| Lb/Rb minus X2/X3 | 1.000 | 0.00 dB | 0.0597 | 0.0858 | +0.0000 |

The first result has a plausible coefficient but an insignificant effect:
only seven of 33 windows gain at least 0.01 even when each window is allowed
to choose its own coefficient, and those local optima have an interquartile
range of `0.77/1.00/1.00`. The other apparent gains start from almost
uncorrelated signals and stop at the allowed boundary, while their median
window gains are zero. Testing every X source against C improves its global
correlation by at most 0.0011. No measured fold is therefore stable or useful
enough to adopt as a D3 presentation rule.

Run the reproducible fixed-layout analysis after producing aligned 16-channel
`f32le` files:

```console
python3 scripts/analyze-dtsx-fixed-layout-fold.py DTSX.f32le ATMOS.f32le
```

## Automatic bridge presentations

There is no presentation switch. Once the extension has decoded successfully,
Harletty selects its output from the observed source count:

| Profile | Automatic presentation |
| --- | --- |
| Standard four-source | fixed 7.1.4 `TFL,TFR,TBL,TBR`, with the configured height fold removed from the compatible bed |
| D0, five-source | fixed `TFC,TFL,TFR,TBL,TBR`, with the configured experimental partial unfolds |
| D1, six-source | fixed `TFL,TFR,Lw,Rw,TBL,TBR`, with the configured experimental partial unfolds |
| D3, eight-source | unchanged named objects X0–X7 at the inferred defaults below; no bed subtraction |

Fixed standard/D0/D1 channels carry no object labels, fabricated positions or
metadata, and do not set `has_objects`. D3 does because it is the only current
alternate presentation emitted through object channels.

The D1 operation is:

```text
L  -= 0.707107 * X0 + 1.183 * X2
Ls -= 0.209 * X2
Lb -= 0.707107 * X4
TFL = X0
TBL = X4
Lw  = X2

R  -= 0.707107 * X1 + 1.183 * X3
Rs -= 0.209 * X3
Rb -= 0.707107 * X5
TFR = X1
TBR = X5
Rw  = X3
```

All six D1 sources are exposed as fixed labeled channels; no
objects or fabricated metadata are emitted. The decisions are hoisted per
channel; the sample loop performs only the required multiply/subtracts. No
adaptive decorrelator or per-sample allocation is introduced. The wide gains
are inferred from *D1 corpus A* alone and remain experimental pending
cross-validation on *D1 corpus B* or reference-decoder confirmation.

The D3 presentation deliberately performs no subtraction from the
compatible bed: neither the existence nor the coefficients of a D3 fold have
been established. It keeps the eight extension waveforms unchanged and emits
the following static object defaults:

| Source | Experimental role | Cartesian position |
| --- | --- | --- |
| X0 | left wide | `[-1.0, 0.5, 0.0]` |
| X1 | right wide | `[1.0, 0.5, 0.0]` |
| X2 | top front left | `[-1.0, 1.0, 1.0]` |
| X3 | top front right | `[1.0, 1.0, 1.0]` |
| X4 | top side left | `[-1.0, 0.0, 1.0]` |
| X5 | top side right | `[1.0, 0.0, 1.0]` |
| X6 | top back left | `[-1.0, -1.0, 1.0]` |
| X7 | top back right | `[1.0, -1.0, 1.0]` |

The left/right ordering is repeatable in the D3 corpus. X6/X7 have the
strongest recurring rear-bed association, X2/X3 match the front-elevation
group in the paired *D3 corpus I* Atmos control, and X4/X5 form the broad side
pair. X0/X1 are assigned to the wides by elimination and have the weakest
positional evidence. These events are explicitly fabricated presentation defaults,
not stream coordinates; their names and object declarations are emitted
sparsely, and `has_objects` reflects the latest frame where this D3
presentation is emitted.

## Reproduction tools

- `dca/examples/xll_corpus_probe.rs`: bounded profile/source/activity survey.
- `dca/examples/xll_pcm_range.rs`: extract interleaved bed plus extension PCM
  for a selected time range.
- `dca/examples/xll_alt_layout_probe.rs`: validate D0/D1/D3 channel-set
  headers, CRC-delimited prefixes, controls, layouts and post-audio padding.
- `dca/examples/xll_bed_correlation.rs`: whole-stream sample and frame-RMS
  correlation matrices between alternate sources and the lossless 7.1 bed.
- `dca/examples/xll_rms_envelope.rs`: compact per-frame RMS extraction for
  aligning alternate encodes without writing their complete decoded PCM.
- `scripts/analyze-dtsx-atmos-pairs.py`: align paired DTS:X/Atmos
  editions, survey Atmos object motion and test fixed versus windowed
  cross-format mixture hypotheses.
- `scripts/analyze-dtsx-system-id.py`: scalar delay/gain/frequency response
  analysis, including the known height-fold calibration paths.
- `scripts/analyze-dtsx-matrix-id.py`: conditioned multichannel matrix fit and
  coefficient-plateau report.
- `scripts/analyze-dtsx-fixed-pan.py`: local height-plane rank-one/gain-vector
  analysis for finding coherent pans in a decoded fixed 7.1.4 presentation.
- `scripts/compare-dtsx-bed.py`: auto-match the eight compatible-bed channels
  from `xll_pcm_range` to an ffmpeg f32le reference and enforce a lossless RMSE
  gate. Use `--skip-seconds 1` for independently seeked snippets that lack XLL
  pre-roll.

Example:

```sh
cargo run -p dca --release --example xll_pcm_range -- \
  /path/to/d1-corpus.dts \
  /tmp/d1-50-66.f32le 3000 3960

python3 scripts/analyze-dtsx-matrix-id.py \
  /tmp/d1-50-66.f32le --extensions 6 --window 4096

scripts/compare-dtsx-bed.py \
  /tmp/decoded-bed-plus-x.f32le /tmp/ffmpeg-7.1.f32le \
  --skip-seconds 1
```

The extractor prints the active DCA speaker order. The Python tools currently
expect the eight-channel order used by this corpus:
`C,L,R,Ls,Rs,LFE,Lb,Rb`, followed by the extension sources.

## Promotion gate

These automatic experimental presentations must not be merged to `main`
without:

1. quantitative confirmation on *D1 corpus B* or a reference-decoder capture;
2. local corpus tests that demonstrably ran;
3. lossless-bed regression against ffmpeg;
4. explicit compatible-bed-versus-spatial-presentation A/B listening sign-off.
