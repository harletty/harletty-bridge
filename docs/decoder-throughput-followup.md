# Decoder throughput follow-up

Measured against `8175deb`, after the earlier decode, SIMD, buffer-reuse and
bit-allocation work had landed. The retained changes precompute the fixed AHT
inverse-transformed vectors and the JOC Huffman prefixes, with a direct path for
the one-bit zero code. They do not change PCM, metadata or the public API.

## Protocol

Ryzen 9 9950X, rustc 1.94.0, release with thin LTO and line-table debug information.
The existing `harletty/examples/decode_bench.rs` loads the input before timing,
warms up once, then measures framing and decoding on one thread. Each entry below
is the median of three round medians, five timed passes per round. Baseline and
candidate run consecutively on CPU 4, reversing their order each round. Every
run produced identical frame/sample/channel tallies and zero decode errors.
The corpus has 26 elementary streams and 34 presentation/mode cases.

Individual experiments used CPU 8. A later control series on that CPU became
unstable even for the baseline and was excluded; the final series was repeated
on CPU 4 after builds had stopped. Neither a fastest outlier nor a noisy series
is used in the final table. Corpus identifiers are anonymous; source mappings,
raw measurements and output digests are kept outside the repository.

## Experiments

Negative percentages mean less elapsed decode time. Each trial had its own
saved release executable and was compared with the matching baseline.

| Proposal | Measured result | Decision |
|---|---|---|
| TrueHD block parser compiled for BMI2 | -1.3% to +4.4% across eight presentation cases | Rejected: mostly slower |
| TrueHD CRC slicing extended from 8 to 16 bytes | -0.6% to +2.0% | Rejected: no consistent gain, larger tables |
| TrueHD sample-range checks proven once per block | -1.4% to +1.0% | Rejected: marginal and inconsistent |
| DTS Rice remainder extracted with one shift and a mask | About +2.5% to +3.7% on lossless streams | Rejected: slower in the full decoder |
| Precompute AHT VQ inverse transforms | 6.941 to 6.653 ms in isolation (-4.1%), 3.5% fewer instructions | Retained |
| JOC eight-bit Huffman prefix tables | Up to -9.8% bed decode and about -3% objects; a short-code case regressed | Refined with the zero-code path |
| Pack each Huffman prefix into 16 bits | Saves 3 KiB but slower than the unpacked table on every tested JOC case | Rejected |
| Direct one-bit zero code before Huffman lookup | Removes the short-code regression; final JOC bed -1.6% to -5.9%, objects -0.3% to -2.4% | Retained; trades some long-code gain for consistent results |

The AHT tables replace roughly 11 KiB of raw codebook data with 22 KiB of
inverse-transformed data. The Huffman prefixes add roughly 6 KiB. Both are
read-only, built at compile time, with no per-frame allocation or new ISA
requirement. GAQ still runs its transform; only fixed VQ entries can be cached.
The Huffman tail preserves the checked tree walk and its exact error position.

## Final combined decoder measurements

| Case | Family | Channels | Audio s | Baseline ms | Candidate ms | Change |
|---|---|---:|---:|---:|---:|---:|
| corpus-01-2 | truehd | 8 | 31.00 | 85.540 | 86.902 | +1.59% |
| corpus-01-3 | truehd | 12 | 31.00 | 123.724 | 124.269 | +0.44% |
| corpus-02-2 | truehd | 8 | 60.00 | 153.604 | 155.833 | +1.45% |
| corpus-02-3 | truehd | 12 | 60.00 | 249.776 | 251.722 | +0.78% |
| corpus-03-2 | truehd | 8 | 90.37 | 181.443 | 183.572 | +1.17% |
| corpus-03-3 | truehd | 14 | 90.37 | 319.774 | 321.569 | +0.56% |
| corpus-04-2 | truehd | 8 | 30.09 | 80.879 | 81.857 | +1.21% |
| corpus-04-3 | truehd | 12 | 30.09 | 124.746 | 125.455 | +0.57% |
| corpus-05-bed | eac3 | 6 | 29.98 | 13.203 | 12.973 | -1.74% |
| corpus-06-bed | eac3 | 1 | 25.41 | 2.748 | 2.741 | -0.28% |
| corpus-07-bed | eac3 | 1 | 30.02 | 3.002 | 3.000 | -0.06% |
| corpus-08-bed | eac3 | 8 | 30.02 | 24.277 | 24.094 | -0.75% |
| corpus-09-bed | eac3 | 8 | 60.00 | 50.162 | 49.389 | -1.54% |
| corpus-10-bed | eac3 | 2 | 28.67 | 6.728 | 6.522 | -3.05% |
| corpus-11-bed | eac3 | 6 | 31.01 | 25.210 | 24.420 | -3.13% |
| corpus-11-objects | eac3 | 21 | 31.01 | 77.805 | 77.611 | -0.25% |
| corpus-12-bed | eac3 | 6 | 60.00 | 51.670 | 48.749 | -5.65% |
| corpus-12-objects | eac3 | 21 | 60.00 | 157.039 | 153.552 | -2.22% |
| corpus-13-bed | eac3 | 6 | 40.42 | 29.944 | 28.185 | -5.88% |
| corpus-13-objects | eac3 | 17 | 40.42 | 84.442 | 82.382 | -2.44% |
| corpus-14-bed | eac3 | 6 | 193.12 | 121.376 | 119.389 | -1.64% |
| corpus-14-objects | eac3 | 21 | 193.12 | 449.499 | 446.907 | -0.58% |
| corpus-15-auto | dts | 6 | 8.01 | 5.282 | 5.258 | -0.45% |
| corpus-16-auto | dts | 6 | 30.01 | 40.879 | 40.872 | -0.02% |
| corpus-17-auto | dts | 2 | 60.00 | 63.530 | 63.471 | -0.09% |
| corpus-18-auto | dts | 12 | 60.31 | 99.891 | 99.891 | +0.00% |
| corpus-19-auto | dts | 12 | 10.69 | 17.488 | 17.414 | -0.42% |
| corpus-20-auto | dts | 12 | 30.01 | 68.745 | 68.591 | -0.22% |
| corpus-21-auto | dts | 8 | 0.76 | 1.292 | 1.292 | -0.00% |
| corpus-22-auto | dts | 17 | 120.46 | 286.448 | 286.360 | -0.03% |
| corpus-23-auto | dts | 13 | 120.19 | 273.105 | 272.768 | -0.12% |
| corpus-24-auto | dts | 15 | 49.34 | 132.629 | 132.397 | -0.18% |
| corpus-25-auto | dts | 7 | 180.01 | 302.739 | 303.177 | +0.14% |
| corpus-26-auto | dts | 17 | 38.92 | 113.037 | 112.753 | -0.25% |

TrueHD's combined-binary timing shifts by +0.4% to +1.6% despite unchanged
retired instruction counts on the profiled control. Its actual CLI control
(`--no-audio`, seven alternating rounds) changes by +0.34% for presentation 2
and -0.20% for presentation 3. This is consistent with binary placement effects,
not a changed decoder algorithm; no TrueHD performance improvement is claimed.
DTS controls stay within -0.5% to +0.2%. Small changes on ordinary AC-3/E-AC-3
streams that use neither optimized path are not attributed to these algorithms.

## CLI measurement

The CLI was timed with `perf stat -e task-clock`, pinned to CPU 4, over seven
alternating rounds. `--no-audio` retains framing, PCM decoding and metadata
handling but removes PCM file writing, which was too variable for a useful
comparison. These are whole-process CPU times, including setup and input reads.

| Case | Baseline ms | Candidate ms | Change |
|---|---:|---:|---:|
| corpus-10-bed | 11.49 | 11.14 | -3.05% |
| corpus-11-objects | 93.99 | 93.45 | -0.57% |
| corpus-12-objects | 187.76 | 184.67 | -1.65% |
| corpus-13-objects | 100.46 | 98.14 | -2.31% |
| corpus-14-objects | 535.72 | 533.65 | -0.39% |

## Correctness and checks

- The combined tables reproduce 171 output files across 109 CLI cases, including
  all TrueHD/MLP presentation selections, DTS families, JOC and damaged inputs.
  PCM, metadata and exit codes match. After adding the direct zero path, all 21
  affected E-AC-3 cases were decoded and compared again, with identical results.
- The Huffman test compares every leaf and every truncation at all eight bit
  offsets with the original tree walk, including exact reader position.
- The AHT randomized checked-reference test still uses the original raw VQ
  tables and performs the IDCT at runtime.
- `RUSTFLAGS='-D warnings' cargo build --all-targets` and `cargo test`: passed,
  414 tests reported successful. E-AC-3 release suite: 99 tests reported successful.
- Bridge-perf feature check, IAMF feature suite (101 tests reported successful)
  and crate-graph isolation check: passed. Release CLI and bridge build: passed.
- External corpus/oracle suites still self-skip when their media is absent.
  The full DTS:X corpus was unavailable; the local excerpt comparisons above
  are separate checks and do not stand in for that corpus gate. No DTS change
  is retained. No listening or real ARM performance measurement was performed.

## Reproduce

Build and save the example separately at the baseline and candidate revisions:

```sh
cargo build --release -p harletty --example decode_bench
```

For an input supplied by the caller, alternate the two executables rather than
running all baseline passes first. Repeat three times, reversing order:

```sh
taskset -c "$CPU" "$BASE_BENCH" eac3 bed 5 "$INPUT"
taskset -c "$CPU" "$CANDIDATE_BENCH" eac3 bed 5 "$INPUT"
```

Use `eac3 objects` for reconstructed JOC, `truehd 2`/`truehd 3` for the selected
presentations and `dts auto` for DTS. Compare all tally fields before accepting
a timing. For the line-table build used here, export
`CARGO_PROFILE_RELEASE_DEBUG=line-tables-only` consistently for both revisions.
