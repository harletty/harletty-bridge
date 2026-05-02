# harletty-bridge Performance Notes

This note captures the main performance costs worth revisiting after the MAT parser fixes.

## Main likely costs

### 1. MAT chunk extraction still copies too much

Current path:

- each IEC 61937 payload is copied into `MatStream.buffer`
- each chunk is copied again into a fresh `Vec<u8>`
- each chunk is byte-swapped while copying
- `push_packet()` collects all chunks into `Vec<Vec<u8>>`
- chunks are then replayed into the extractor

Relevant code:

- `src/mat.rs`
- `src/lib.rs`

Why it matters:

- this runs for every MAT payload
- it creates avoidable allocations and memcpy traffic
- it adds extra cache pressure before the real extractor/parser/decoder work even starts

Best optimization target:

- process MAT chunks as a stream instead of buffering all chunk `Vec`s first
- reuse chunk output buffers
- reduce copies during byte-swap / chunk handoff

### 2. The extractor uses a byte-oriented `VecDeque<u8>` pipeline

Relevant code:

- `truehd/src/process/extract.rs`

Current behavior includes:

- extending a `VecDeque<u8>` with incoming bytes
- scanning it with `range()` / `get()`
- draining from the front frequently
- building temporary `Vec<u8>` copies for CRC / resync validation
- draining again into a pooled frame buffer

Why it matters:

- `VecDeque<u8>` is not ideal for a hot byte-stream parser
- repeated `drain(..)` and temporary collections increase overhead
- the extractor sits on the critical path before parse/decode

Best optimization target:

- replace the byte queue with a more contiguous streaming buffer model
- avoid temporary frame-sized copies during sync/CRC checks

### 3. `push_packet()` buffers all MAT chunks before decoding them

Relevant code:

- `src/lib.rs`

Current behavior:

- MAT output is first accumulated into `chunks: Vec<Vec<u8>>`
- only after extraction finishes are chunks pushed one by one into the extractor

Why it matters:

- adds one extra layer of allocation and pointer chasing
- delays processing even though the downstream path is already stream-oriented

Best optimization target:

- feed extractor input as soon as each chunk is available

### 4. Frame building performs a full PCM repack every decoded frame

Relevant code:

- `src/lib.rs`

Current behavior:

- decoded PCM is sample-major in `DecodedAccessUnit`
- bridge then allocates `RVec<i32>` and repacks every sample/channel into interleaved ABI output

Why it matters:

- fixed O(samples * channels) copy on every decoded frame
- likely one of the largest bridge-local costs after decode itself

Best optimization target:

- either keep this but make it the known dominant bridge-local cost
- or, if feasible, emit directly in bridge ABI layout from the decoder side

### 5. Metadata extraction rebuilds several transient structures

Relevant code:

- `src/lib.rs`
- `truehd/src/structs/oamd.rs`

Current behavior:

- `get_damf_pos()` builds a full `Vec<Vec<[f64; 3]>>`
- `to_index_vec()` allocates bed index vectors
- metadata events, bed indices, and name updates are rebuilt every frame
- object names are re-derived when cache keys change

Why it matters:

- Atmos object metadata can be dense
- this cost becomes visible once MAT/extractor copies are reduced

Best optimization target:

- avoid building full intermediate position vectors when only the first block is used
- reduce per-frame metadata allocations

## Secondary costs

### 6. Per-frame clones in the decode handoff

Relevant code:

- `truehd/src/process/decode.rs`

Current behavior:

- clones `channel_labels`
- clones / collects OAMD payloads into a fresh `Vec`

Why it matters:

- not necessarily the biggest cost, but it is on the hot path
- worth revisiting after the larger copy/queue issues

### 7. `catch_unwind` around normal frame draining

Relevant code:

- `src/lib.rs`

Why it matters:

- useful for robustness
- but it does add overhead on the normal hot path

This is a lower-priority optimization unless profiling shows it is unexpectedly expensive.

### 8. Repeated warnings on unsupported / malformed metadata

Relevant code:

- `src/lib.rs`
- `truehd/src/process/extract.rs`

Why it matters:

- if repeated often, logging can dominate the cost of the bad path
- not usually the primary steady-state cost, but worth gating / rate-limiting

## Priority order

1. Stream MAT output directly into the extractor and remove `Vec<Vec<u8>>` chunk staging.
2. Reduce MAT chunk copy / byte-swap allocations.
3. Rework extractor buffering away from `VecDeque<u8>` + temporary frame copies.
4. Reduce metadata transient allocations (`get_damf_pos()`, `to_index_vec()`, name updates).
5. Revisit PCM repack cost if it remains dominant after the upstream copy path is cleaned up.

## Practical reading

If only a small number of changes are attempted first, the best return-on-effort is likely:

- MAT streaming handoff
- extractor buffering cleanup
- metadata allocation cleanup

Those changes should improve throughput without changing the external bridge ABI.

## What we tried

### A. Stream MAT chunks directly into the extractor

Status:

- implemented in `src/lib.rs`
- removed `Vec<Vec<u8>>` staging in `push_packet()`

Result:

- user-observed gain was already "very important" on the bridge path
- kept

### B. Reuse a chunk output buffer in `MatStream`

Change:

- changed MAT extraction to fill a reusable output buffer instead of returning a fresh `Vec<u8>` per chunk

Result from `dump_mat` profiling:

- baseline comparable run: `mat_next_chunk total_ms=962.567`, `avg_us=17.890`
- after reusable-buffer change: no improvement; this direction was not better than the simple `Vec<u8>` return path

Conclusion:

- reverted
- per-chunk allocation was not the dominant cost in this profiler run

### C. Add a simple fast path for fully-contained chunks

Change:

- if a chunk fits entirely before the next MAT barrier (`MAT_MIDDLE_POS` / `MAT_END_CODE`), return it directly without going through the clipping/continuation slow path

Result from `dump_mat` profiling:

- before: `mat_next_chunk total_ms=962.567`, `avg_us=17.890`
- after: `mat_next_chunk total_ms=906.373`, `avg_us=16.846`

Conclusion:

- kept
- this is a real improvement of roughly 5–6% on the measured `mat_next_chunk` stage

### D. Further local refactor of barrier-distance calculations

Change:

- restructured `next_chunk()` to compute barrier distances once and reuse them in both the continuation and new-chunk paths

Result from `dump_mat` profiling:

- regressed relative to the simple fast path:
- `mat_next_chunk total_ms=927.188`, `avg_us=17.232`

Conclusion:

- reverted
- the simpler fast-path version remains the best measured variant so far

## Current direction

The next MAT-specific question is no longer "does chunk staging matter?" but rather:

- how much of `mat_next_chunk()` is really spent in `copy_swapped_words()`
- how much is spent in parser/control logic around chunk boundaries

To answer that, `dump_mat` now records MAT swap/copy time separately so future runs can compare:

- total `mat_next_chunk`
- isolated MAT chunk swap/copy cost
- residual parser/control cost

### E. Replace `chunks_exact()` byte-swap with a simple indexed loop

Change:

- replaced the chunk byte-swap/copy loop in `copy_swapped_words()` with a direct indexed loop over 2-byte pairs

Result from `dump_mat` profiling:

- before:
  - `mat_next_chunk total_ms=964.894`, `avg_us=17.933`
  - `mat_swap total_ms=471.627`, `avg_us=9.129`
- after:
  - `mat_next_chunk total_ms=575.381`, `avg_us=10.694`
  - `mat_swap total_ms=86.362`, `avg_us=1.672`

Conclusion:

- kept
- this is the first clearly dominant MAT-local win after streaming handoff
- the old `chunks_exact()`/zip path was much more expensive than expected in this workload

## Updated MAT takeaway

With the current measurements:

- MAT chunk swap/copy was a very large fraction of `mat_next_chunk()`
- replacing the swap implementation dropped `mat_swap` dramatically
- `mat_next_chunk()` is now much cheaper overall, but there is still non-trivial residual parser/control overhead

That means the next MAT-side work, if needed, should focus on:

- remaining parser/control overhead in `next_chunk()`
- or a more structural reduction of copies between MAT extraction and the downstream extractor

### F. Fast-path and batch-scan leading `0x0000` padding

Change:

- added a fast path that skips padding handling entirely when the first word is not `0x0000`
- replaced repeated 2-byte `advance()` calls with a local scan over the slice followed by a single `advance(strip_bytes)`
- recorded padding-strip time separately in `dump_mat`

Result from `dump_mat` profiling on the verified bit-perfect baseline:

- before:
  - `mat_next_chunk total_ms=621.407`, `avg_us=11.549`
  - `mat_swap total_ms=92.998`, `avg_us=1.800`
- after:
  - `mat_next_chunk total_ms=201.545`, `avg_us=3.746`
  - `mat_swap total_ms=96.171`, `avg_us=1.861`
  - `mat_padding total_ms=92.837`, `avg_us=1.784`, `calls=52027`

Bit-exact validation:

- aligned comparison against `00007.thd` remained `OK`
- `25598668` bytes matched from the first TrueHD syncword

Conclusion:

- kept
- this is a large MAT-local win and remains bit-perfect under the current controlled test

## Updated MAT takeaway

After the swap-loop and padding-strip optimizations:

- `mat_next_chunk()` is much cheaper than the original baseline
- `copy_swapped_words()` is no longer dominant
- the remaining MAT cost is now much smaller and more evenly distributed

If MAT optimization continues from here, the next candidates should be higher-level copy reduction between MAT extraction and the downstream extractor, rather than more micro-optimizing the old hot loops.
