# Retiring the standalone `truehdd` fork

`reference-sources/truehdd` (in the workspace, outside this repo) was a fork of
upstream `truehdd/truehdd` carrying four unpushed local commits. It produced the
`.atmos` / `.atmos.metadata` masters under `adm/` and was the binary Atmos
Ranker invoked.

It is now **archive/reference only**. This repo builds a `truehdd` CLI from the
same decoder lineage as the bridge, and Atmos Ranker's `DEFAULT_TRUEHDD_BIN`
points at it.

Why retire it rather than keep both: its decoder crates were frozen at
`truehd` 0.4.0 and it has no DTS path at all, so adding DTS:X → ADM there would
have meant backporting work that already exists here — and maintaining two
decoder lineages indefinitely.

## Disposition of the four local commits

Audited before anything moved (plan phase 0). Verdicts were reached by matching
*semantics*, not text: this repo's crates were rewritten and perf-tuned, so the
equivalent logic often looks nothing like the fork's.

| Commit | Subject | Disposition |
| --- | --- | --- |
| `d7fe72b` | `feat(truehd)`: parse-failure recovery + robustness fixes | **Ported** — `77543a9` |
| `26d29b7` | `feat(eac3)`: EAC3/JOC decoding with codec auto-detection | **Superseded** (crate side) / **imported** (CLI side) |
| `b20095e` | `fix(eac3)`: reset per-frame walk_state flags in stateful aux walk | **Superseded** — already present |
| `014ba6b` | `feat(eac3)`: accept legacy AC-3 frames in the streaming Extractor | **Superseded** — already present |

### `d7fe72b` — ported

The only commit with anything left to port. About 30% was already here (the
substream-0 fix for WALL-E HYPERION-style remuxes). Landed as its own commit so
the CLI import would not smuggle decoder changes in with it:

- `checked_sub` on substream/block sizes for substreams N > 0, plus the two
  reader-position underflows, instead of wrapping arithmetic.
- `RestartSyncWord::from` → `TryFrom`. The old path took the whole process down
  with `panic!` on a corrupt sync word — unacceptable in the realtime bridge,
  which shares this crate.
- A guard on `input_timing_interval == 0` before it reaches `div_ceil`, and
  saturating/checked arithmetic for the seamless-branch `c1..c4` limits. This
  one is a deliberate semantic change: an underflowing limit used to wrap to a
  huge value and satisfy its condition.
- `Parser`/`Decoder::reset_for_next_major_sync()`.
- OAMD `b_object_at_infinity` + `distance_factor_idx` → `distance_factor:
  Option<f64>` resolved from a new `DISTANCE_FACTORS` table. Required by the
  CLI: the DAMF writer reads that field directly, and the E-AC-3 JOC path fills
  the same one. No bridge code referenced the old fields.

### `26d29b7`, `b20095e`, `014ba6b` — superseded

All three touched the `eac3` crate, and every change was already present here —
this repo's `eac3` diverged *further* than the fork (SpX detection, the bittrace
harness, AC-3 2/0 fixes). Verified change by change, including the two easiest
to get wrong:

- the stateful aux-walk per-frame flag reset (`b20095e`);
- legacy AC-3 acceptance in the streaming `Extractor`, `parse_legacy_ac3_header`
  and its regression test (`014ba6b`).

`26d29b7` is split: its `eac3` crate half was superseded, while its CLI-side
files (`eac3_to_oamd.rs`, `codec_probe.rs`, `eac3_handler.rs`, `eac3_thread.rs`)
had no counterpart here and were imported wholesale in phase 2 — the fork's
tree was the *newer* one for those, which is why the resurrection started from
it rather than from a revert of `b8cea40`.

## What replaces it

- Parity was proven by decoding with both binaries and comparing byte for byte:
  E-AC-3 JOC (`.atmos`, `.atmos.metadata` and `.atmos.audio` all identical),
  TrueHD (identical CAF and identical logs), `info` (identical modulo the
  version banner), and Atmos Ranker's exact invocation including its stdin pipe.
- `truehdd/tests/golden.rs` pins that output to a committed fixture, so the
  property survives the reference tree going away.
- On top of parity: a DTS path the fork never had, exporting DTS:X as ADM.

## Residual gaps

- No TrueHD **Atmos** sample exists on this machine, so the OAMD-bearing TrueHD
  path is verified only indirectly, through the E-AC-3 JOC route into the same
  DAMF writer.
- The fork's own history is not preserved here. The four commits above live only
  in that tree; keep it (read-only) until they are no longer worth consulting.
