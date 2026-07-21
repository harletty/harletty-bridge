# DTS:X spatial extension — review findings and working agreement

Companion to [`dtsx-objects-campaign.md`](dtsx-objects-campaign.md) (the
reverse-engineering notes) and Omniphony's
`docs/channel-object-contract.md` (the design the re-landing follows).

This document exists so the next session that touches DTS:X decoding does
not repeat the first campaign. It records **what went wrong** — both the
implementation and the way it was landed — and **how we work** so it does
not happen again.

---

## 1. What the first campaign shipped, and why it was rolled back

The goal was legitimate: decode the DTS:X height extension (the XLL-X
end-of-frame blob) and present a full 7.1.4 image instead of the
backward-compatible 7.1 bed. The decoding work was sound. The **presentation
layer** was not, and the way it reached `main` was worse.

To surface the four height channels, the bridge **fabricated an
`RMetadataFrame`** for a stream that carries no dynamic objects: eight
position-less "bed" events on the legacy 0–9 bed-id scheme plus four
"objects" pinned to cube corners. That single decision cascaded into a
string of hacks across two repos (see §3). All of it was pushed **straight
to `main`, in nine commits, with no pull request and no CI** (see §2).

Resolution: the bridge `main` was rolled back (force-push to the
pre-campaign commit — `main` was unprotected and no release tag pointed
into the range), the work preserved on `research/spatial-object-layer`, and
the two Omniphony commits reverted (Omniphony `main` is protected, so
reverts rather than a force-push). The feature was then re-landed cleanly,
in phases, through the channel/object contract.

---

## 2. The process failures (these are the important part)

The technical defects in §3 were recoverable. The process failures are what
made them dangerous, because nothing caught them before they were on `main`.

1. **Direct push to `main`, no PR.** The nine bridge commits (and two
   Omniphony commits) bypassed the documented promotion model
   (`feat/* → PR → CI green → main`). No second pair of eyes, no review,
   no gate. Earlier bridge work (PRs #5–#8) had gone through PRs; this did
   not.

2. **No CI ever ran on the bridge commits.** The bridge repo's only GitHub
   workflow was a **`Release` job triggered on `v*` tags** — it was
   misleadingly named `ci.yml` but never ran on a branch push or PR. So the
   nine commits were never built or tested by CI. (Fixed since: a real
   `ci.yml` now gates every push/PR to `main` on build + tests; the old
   tag job is `release.yml`.)

3. **Corpus-gated tests silently self-skipped.** The tests that would have
   exercised the real stream pointed at a dump path that did not exist on
   this machine (`/mnt/local/SSD_B-CT4000/…` instead of
   `/mnt/local/SSD_A-CT4000/DTS:X-Dumps/…`). Because they skip when the file
   is absent, they had **never run** — the coverage looked green while
   testing nothing.

4. **The campaign doc was out of sync** — it announced a branch the work no
   longer lived on.

5. **The public ABI leaked research state.** `HdFrame` grew ~12 diagnostic
   fields consumed only by research examples; they rode the shipped decode
   path (see §3, performance).

**Rule of thumb:** if a change cannot be described in a PR and pass CI, it is
not ready for `main`. "It's just decoding, it works on my machine" is
exactly the situation CI and review exist for.

---

## 3. The technical review findings

Grouped by what they would do to a user. Each notes how the re-landing
resolved it, so this doubles as a checklist of what "done right" looks like.

### 3a. Correctness / safety — would break playback or crash

- **A malformed EXSS descriptor tail could stall a track forever.** The
  campaign parsed speculative, later-revision descriptor fields
  *unconditionally*; on any mismatch the whole EXSS parse returned an error.
  The pipeline treats a failed EXSS parse as "not enough data buffered yet"
  and stops consuming input — so `dts_buf` grows without bound and audio
  goes **permanently silent**. A previously-playing DTS-HD MA title could be
  bricked by research parsing. This was the single most dangerous defect.
  → *Re-landing:* the speculative fields are parsed on a savepoint and
  discarded with a rewind on any failure or overrun; neither ffmpeg nor
  libdcadec reads them either.

- **Panic on untrusted stream data.** Per-speaker sample buffers were
  indexed without validating their lengths against the frame's sample
  count; a decoder bug or crafted stream panics *in the audio path*. The
  hardening was asymmetric — added for the height quartet but not the bed.
  → *Re-landing:* every bed channel length is validated before indexing; a
  bad frame is dropped, never a crash.

- **Integer underflow in the XLL LSB seek.** `band_data_end -
  lsb_section_size * 8` could underflow (the size was only validated against
  the far larger outer frame size), panicking in debug builds.
  → *Re-landing:* `checked_sub` against the band size.

- **Clicks at the fold/unfold boundary + mid-stream channel-count flips.**
  When the height quartet was absent or invalid for a frame, the emitted
  channel count flipped 12↔8 with no crossfade, forcing the renderer to
  renegotiate. → *Re-landing:* once a valid quartet locks, dropout frames
  keep the 12-channel shape (composite bed + silent heights); the renderer's
  constant-rate gain slew handles the transitions.

### 3b. Contract violations — the root design mistake

- **Fabricated metadata.** The bridge invented objects/bed-ids the format
  does not contain. This forced the engine onto the object path, which
  VBAP-panned the fixed heights at corner positions instead of routing them
  to matching speakers, unlike every other fixed presentation.

- **Wrong id mapping.** DCA rear-centre (Cs) was mapped to bed id 6 = `Lb`,
  because the 0–9 scheme cannot express what the format carries. New code
  encoding a wrong contract.

- **Downstream hacks.** Omniphony grew a reverse-lookup to re-derive display
  names the labels already carried; Studio inferred "this is a speaker feed"
  from `directSpeakerIndex`.

  → *Re-landing:* **the bridge describes, the renderer decides.** DTS:X 7.1.4
  is emitted as twelve **labeled fixed channels**, no fabricated metadata,
  `has_objects` stays false; placement (virtualize vs direct, crossover) is
  the renderer's call, identically to every other fixed stream.

### 3c. Performance — the realtime rules

- **Per-frame allocations paid by *all* DTS-HD streams.** The research
  descriptor tail was `vec!`-allocated and filled bit-by-bit every frame,
  then `.clone()`d into `HdFrame` — even though the shipped bridge never
  read it. The "non-DTS:X streams unaffected" property held for the XLL side
  but not the EXSS side. → *Re-landing:* captured byte-wise, only for XLL
  assets, moved not cloned.

- **Branchy per-sample hot loop.** The unfold ran a `match spkr` plus an
  `Option` deref per channel *per sample*, though the pairing is static per
  frame. → *Re-landing:* a per-channel fold table hoisted out of the sample
  loop.

- **Per-frame error `String`.** A persistently failing extension formatted a
  `String` every frame into a field nobody reads. → *Re-landing:* an
  allocation-free `&'static str` kind.

### 3d. Hygiene

- Research diagnostics polluting the shipped `HdFrame` (belong behind a
  feature or in a separate struct).
- Magic syncword literals (now named constants).
- Dead code (`let _ = base;`), sticky/time-dependent flags.
- The inconsistent, non-existent test dump path (§2.3).

---

## 4. How we work — the standard process

Follow this and the campaign cannot recur.

### Branch & promotion model

```
feat/* ──(open PR, CI green, review)──▶ main ──(when ready)──▶ merge main→release ──▶ tag v… on release
```

- **Never commit to `main` directly, ever.** Every change is a branch and a
  PR. This is not bureaucracy — it is the only place review and CI run.
- **Never force-push a shared `main`.** The one force-push in this saga was a
  deliberate *rollback* of unreviewed work, done only because `main` was
  unprotected and no tag pointed into the range. It is not a normal move.
- **One concern per branch.** Do not stack unrelated work on an existing
  branch. If a request is outside the current branch's scope, or big enough
  to deserve its own branch, start a new one.
- **Commit finished local work before starting the next thing** — do not
  build new changes on top of an uncommitted tree.
- **Branch names:** generic and descriptive. On the **Omniphony** repo,
  names must not reference proprietary formats/brands (TrueHD, Atmos, MAT,
  MLP, Dolby, DTS, …) — use `fix/spdif-parser`, `fix/iec61937-framing`,
  `diag/pd-logging`. Prefer the same discipline elsewhere.
- **Back-merge discipline:** any hotfix committed on `release` must be
  merged back into `main`, or `main` regresses at the next promotion.

### CI must gate the work

- The bridge now runs a real `ci.yml` on every push/PR to `main` (build
  `--all-targets` with warnings denied, plus the full test suite). Green CI
  is a merge precondition. The bridge CI checks out Omniphony `main` for its
  path dependencies, so when a change spans both repos, **merge the
  Omniphony side first**.
- Do not merge red. Do not merge un-run.

### The channel/object contract (the design lesson)

Read `docs/channel-object-contract.md` in Omniphony before touching how a
format is presented. In one line:

> **The bridge *describes*, the renderer *decides*.**

- Fixed channels are described by `RChannelLabel` on every frame. Object
  channels are declared explicitly. A fixed presentation (DTS:X 7.1.4
  included) emits **no metadata** — the bridge never fabricates positions,
  bed-ids, or objects, and never assumes an output layout.
- `has_objects` is a **live fact** about the stream, not a rendering mode,
  and must not be latched by hosts.

### Realtime & safety conventions

- **Never panic in the audio path.** No `unwrap`/`expect`/unchecked indexing
  on stream-driven counts. Validate lengths and counts; use checked
  arithmetic. A malformed stream produces a dropped frame or an `Err`, never
  a crash.
- **No steady-state allocations.** No per-frame or per-sample heap
  allocation; preallocate and reuse. No branchy special-casing or repeated
  `Option` derefs in per-sample loops — hoist static decisions out.
- **Surface errors, don't swallow them.** But rate-limit logging in the hot
  path (a per-frame `String`/`format!` is itself a defect).
- **Keep research state out of the shipped path.** Feature-gate diagnostics
  or put them in a separate struct; don't grow the ABI frame with them.
- **Named constants, not magic numbers.** English-only comments and messages.
- **Do not touch `truehd-bridge/truehd/`** — vendored upstream, kept
  byte-for-byte aligned.

### Corpus tests

- The real DTS:X corpus lives at `/mnt/local/SSD_A-CT4000/DTS:X-Dumps/`
  (e.g. `Ex.Machina.2014.dtsx.eng.dts`). Corpus-gated tests must point
  there.
- These tests **self-skip when the dump is absent**, so **CI cannot run
  them** — they only prove anything on a machine with the corpus. A green CI
  run does **not** mean the DTS:X path was exercised. Run the gated tests
  locally, and say so in the PR.
- When you change the presentation, verify the lossless bed is still
  **bit-exact vs ffmpeg** (the existing `xll_*_matches_ffmpeg_lossless`
  tests), and do an **A/B listen** before merging — decode correctness is
  not audibility.

### Pre-PR checklist for DTS:X work

- [ ] On a `feat/*` (or `fix/*`) branch, never on `main`.
- [ ] `cargo build --all-targets` clean with `-D warnings`.
- [ ] Full test suite green **with the corpus present**; gated tests
      actually ran (not skipped).
- [ ] Lossless bed still bit-exact vs ffmpeg.
- [ ] No fabricated metadata / bed-ids; fixed channels are labeled, objects
      declared (contract).
- [ ] No panic path, no per-frame/per-sample allocation, no research state
      in the shipped frame.
- [ ] If the change spans Omniphony, that PR merges first (bridge CI depends
      on it).
- [ ] A/B listen done, result noted in the PR.
- [ ] PR opened; **do not self-merge** DTS:X presentation changes without an
      A/B sign-off.

---

## 5. For reference — the rollback mechanics

- **Bridge `main`:** force-pushed back to the last pre-campaign commit. Safe
  only because `main` was unprotected and no release tag pointed into the
  range; the work was preserved on `research/spatial-object-layer`.
- **Omniphony `main`:** protected, so the two commits were **reverted**
  (not force-pushed), leaving a clean history and a trace of the decision.

Force-push is a rollback tool, not a workflow. The workflow is §4.
