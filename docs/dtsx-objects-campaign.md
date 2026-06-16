# DTS:X object layer — reverse-engineering campaign notes

Date: 2026-06-02
Branch: `exp-spatial-decode`
Status: **investigation complete / blocked on proprietary access.** The 7.1
lossless bed decodes bit-exactly; the DTS:X object layer is characterised but
not decodable without the (unpublished) bitstream grammar.

## TL;DR

- Our streams are **DTS-HD MA + DTS:X "XLL X" extension** (syncword
  `0x02000850`), *not* DTS-UHD/ACE.
- ffmpeg / dcadec / libdca decode only the **7.1 lossless bed** (which we now
  also decode bit-exactly). The DTS:X height/object content lives in a separate
  blob appended at the end of every XLL frame, which **no open-source decoder
  parses** and which has **no public specification**.
- We characterised that blob in full at the byte/bit level and gathered strong
  evidence for **what** it is: extra **height/object waveforms coded as residual
  audio matrixed against the 7.1 bed** (the same family DTS uses for 5.1→7.1).
- We could **not** recover **how** it is coded (the per-object bit grammar):
  it is a bespoke, non-XLL layout, and the grammar is protected by trade secret
  (confirmed against patents, open-source, MediaInfo and forums).
- The only realistic unblock is **ground truth from a licensed encoder/decoder**
  (DTS:X Encoder Suite to author known-signal test streams; or an AVR→capture to
  validate the model). Everything else is exhausted.

All work here is diagnostic and **off the audio path** — the DTS-HD MA decoder
is untouched and still bit-exact.

## Stream facts

- Profile `DTS-HD MA + DTS:X`, 48 kHz, 8 ch (7.1), confirmed on **9 films / 10
  tracks** (corpus below). `nframesamples = 512`.
- At the end of every XLL frame (dword-aligned, after band data) a blob starts
  with `DCA_SYNCWORD_XLL_X = 0x02000850`. Present in **100 %** of frames.
- Variants: corpus is homogeneous — **all `0x02000850`**. No IMAX
  (`0xF14000D0`) and no `0xF14000D1`/`0xF14000D4` (MediaInfo lists these as other
  "X?" sub-syncwords). The 1–2 stray IMAX byte-matches per film are chance
  (1 in ~96 MB).
- ffmpeg (`dca_xll.c` ~L1060) only dword-aligns, peeks the syncword, sets a
  profile flag, then seeks to end of frame — **payload never parsed**. MediaInfo
  (`File_Dts.cpp::Extensions2`) likewise: matches the syncword, labels it `"X?"`,
  then `Skip_XX(..., "(Unknown)")`.

## Blob structure (byte/bit level)

```
[0:4]   0x02000850                       syncword
[4:22]  18 bytes, CONSTANT & UNIVERSAL   format/version magic (see below)
        284bfa71 0d6202fa 02dc1371 0dc8373c f102
──────────────────────────────────────────────── 22-byte header (byte-aligned)
[22]    N+3   per-frame object count      N = byte22 - 3 (3 => 0 objects, the
                                          48-byte null frame); observed 3..14
[23]    2-bit selector | 0b001111         values 0f/4f/8f/cf
[24]    per-TITLE constant byte           {ef,df,e3,e7}; LLL eng==fra → tied to
                                          the mix, not the encoder/language
[25]    0x7c                              constant marker
[26]    2-bit field                       values 04..07
[27:]   variable-length, entropy ~7.98 b/byte   object waveform data
```

Key cross-title result: the **18-byte header is bit-identical across all 9
films** (proof it is a *universal format constant*, not a per-title GUID — La La
Land eng and fra share it). Per-title variation lives only at byte 24.

## Evidence: the objects are residual AUDIO, not position metadata

Model: each active object = one mono waveform of `nframesamples` (512) samples,
coded as residual and matrixed against the 7.1 bed. Bit budget
`(payload - 27)*8 / (N * 512)` lands in an **audio-residual bitrate range and
tracks content** across titles:

| Title | median bits/sample |
|---|---|
| La La Land (musical) | 0.39 (objects ~silent) |
| Gladiator (action) | 2.28 |
| Ex Machina | 4.32 |
| The Mummy 1999 | 4.30 |

A 10× content-dependent swing in *audio bitrate* is the signature of coded audio;
positional metadata would cost roughly the same per object regardless of genre.
This matches the documented DTS:X home model (16 waveforms +2 LFE; 7.1 bed + up
to ~9 objects; silent waveforms not coded → variable per-frame count).

## What it is NOT (falsified hypotheses)

- **Not** fixed-size object records: for a fixed N the payload spreads ~10×
  (count=8: 356–3988 B); records are variable-length (entropy/predictive).
- **Not** a plain global-k Rice stream: divisibility test ≈ 1/N (chance), F never
  consistent, under either unary convention, k=0..14, start `base*8 + a*N`.
- **No nested DCA/XLL framing**: blobs contain no `0x41A29547`(XLL)/CORE/XXCH/
  X96/XBR syncword.
- The 18-byte header is **not** an XLL `chs`-header: bit-walk for the signature
  (storage_bit_res ∈ {16,20,24}, freq = 48 kHz, reserved bits = 0) → 0 hits.

Conclusion: it reuses the XLL coding *family* (residual audio matrixed against
the bed) but with a **bespoke bitstream layout** (no sync, non-XLL header). Our
bit-exact XLL primitives (Rice/linear, LMS prediction, decorrelation, undo
downmix) cannot be pointed at it directly.

## External research (all dead ends for the bitstream grammar)

- **Format**: it is the DTS-HD MA "XLL X" extension, **not** DTS-UHD. So
  **ETSI TS 103 491** (DTS-UHD/ACE) does not describe it.
- **Patents** (DTS/Xperi) confirm the architecture but contain **no bitstream
  syntax** — they are deliberately codec-agnostic:
  - US9779739B2 *Residual encoding in an object-based audio system* — total mix
    `C = A + ΣBi`, objects coded separately, base recovered by subtraction
    `A' = C' − ΣBi'`. Confirms the residual/matrix architecture; no syntax.
  - US9552819B2 *Multiplet-based matrix mixing…* — parametric spatial matrixing
    (multiplets, ICPD, pan angle/radius) for height; no syntax.
  - US20170098452 (DME + height objects), US9721575B2 (base vs extension
    objects) — architecture only.
  - Verdict: DTS protects the XLL X syntax by **trade secret**, not patent.
    Chasing patents for a bit table is a structural dead end.
- **Open source**: ffmpeg, dcadec (foo86), libdca, Const-me all stop at the bed.
  No GitHub project parses the post-`0x02000850` payload. No forum (doom9 /
  hydrogenaudio / MakeMKV / AVS) documents it at the bit level.
- **MediaInfo**: detects and labels `"X?"`, payload is `(Unknown)` — the author
  knows where the blob is, not what's in it.

We are at the public state of the art: nobody has published more than what is
measured here.

## The only realistic unblock: ground truth

To go from *what* to *how* we need a known bitstream↔PCM pair:

1. **DTS:X Encoder Suite / Creator (keystone).** Author a stream with a single
   object carrying a known impulse/sine at a fixed position; confront against the
   produced bitstream → unambiguous, bidirectional calibration. Access is
   licensed/NDA via Xperi — pursue via a pro channel.
2. **AVR/processor (DTS:X Pro) → multichannel capture (TASCAM).** A commercial
   film's rendered 7.1.4 = bed + objects mixed (not cleanly invertible), but the
   **height channels ≈ rendered object audio**, enough to *validate* the model
   (e.g. byte22=3 ⇒ silent height; high N ⇒ active height) without an NDA.
   Requires DTS:X Pro hardware.

No public DTS:X conformance vectors exist (FATE covers only the bed).

## What ships now, regardless

- **7.1 lossless bed** — bit-exact DTS-HD MA decode (unchanged, validated).
- **Per-frame object count** (`N = byte22 − 3`) — a reliable, cheap "immersive
  intensity" signal that the renderer could use to drive an algorithmic
  height/upmix. (Not yet wired into the bridge.)

## Tooling & references (commits on `exp-spatial-decode`)

Diagnostic examples in `dca/examples/` (read a raw `.dts`, decode via
`HdDecoder`, analyse the captured `HdFrame::x_payload`):

- `xll_x_probe.rs` — presence, size histogram, byte entropy, per-byte
  cardinality, fixed-prefix detection, payload↔energy correlation, per-count
  floor, CSV.
- `xll_x_audio.rs` — bits/sample budget (audio-hypothesis test).
- `xll_x_rice.rs` — global-k Rice hypothesis (negative).
- `xll_x_scan.rs` — nested-syncword scan (negative).

Capture path: `XllDecoder::detect_x_extension` (in `src/dcadec/xll.rs`) mirrors
ffmpeg's syncword detection and additionally retains the raw payload; surfaced on
`HdFrame::{x_present, x_imax, x_payload, x_payload_offset}` (in `src/hd.rs`).
This runs after band-data decode and does not affect PCM output.

Commits: `925e0b8` (capture), `35ead79` (fixed header + correlation), `b8d5398`
(bit-level header + count field), `bdd8852` (record-structure test), `4bbdb6d`
(audio evidence), `715999b` (Rice negative), `736ff87` (framing/header
negatives), `dd15aa6` (corpus path).

## Corpus

Full DTS:X tracks (`-c copy`, X extension included) at
`/mnt/local/SSD_B-CT4000/Dumps/`:
Ex.Machina.2014, Apollo.13.1995, Carlitos.Way.1993, Gladiator.2000,
Harry.Potter.DH1.2010, La.La.Land.2016 (eng + fra), The.Mummy.1999,
The.Mummy.Returns.2001, The.Mummy.2008 — all `DTS-HD MA + DTS:X`, 48 kHz, 8 ch.
