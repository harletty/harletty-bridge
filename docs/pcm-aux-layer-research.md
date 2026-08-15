# PCM auxiliary-layer / Auro-Codec research

Status: exploratory notes, last updated 2026-07-23. This is not yet an
implementation specification. The commercial bitstream may differ from the
public patent examples, so a real bit-perfect Auro-Codec carrier remains the
required oracle.

## Main finding

The height channels should not be treated as raw PCM serialized directly into
the least-significant bits. The public Aurophonic carrier patent describes a
different construction:

- two audio signals are alternately sample-rate-reduced and added into the
  significant PCM portion of one backwards-compatible carrier channel;
- the freed LSBs carry the information required to undo that mix: sync, block
  length, PCM offset, seed samples, attenuation, optional error/index tables,
  and a CRC;
- the decoder reconstructs two channel waveforms from the significant carrier
  PCM plus this auxiliary information, then interpolates the missing samples
  and applies the transmitted correction/gain data.

The patent gives common 24-bit divisions of 20+4 and 18+6 bits, also mentions
2/4/6 auxiliary bits per sample, and allows the width to be selected
dynamically. The width therefore must be detected from valid framing, not from
LSB entropy alone.

Primary public description:

- <https://patents.google.com/patent/WO2008043858A1/en>
- <https://www.auro-3d.com/consumer_2024/>
- <https://www.auro-3d.com/content/>

## Proposed detector

The detector must receive the exact decoded integer PCM. No volume operation,
dither, resampling, or float round trip may occur before detection. Harletty
currently converts the XLL output from 24-bit integers to `f32` in
`dca/src/hd.rs`, and later converts it back to `i32` in `src/dts_pipeline.rs`.
An implementation must tap or retain the integer XLL output before that
conversion.

For each carrier channel and each candidate auxiliary width `q` (initially
2, 4, and 6; 1 through 8 in the research tool):

```text
mask      = (1 << q) - 1
symbol[n] = unsigned(sample[n]) & mask
audio[n]  = signed(sample[n] & ~mask)
```

Build one symbol stream per carrier channel first. Also test interleaved
channel order as a secondary hypothesis, plus both possible bit orders inside
each `q`-bit symbol.

Score a candidate only from structure across multiple consecutive blocks:

1. Find repeated sync candidates. The patent example is a rotating one-hot
   sequence, e.g. `1, 2, 4, 8` for `q=4`.
2. Parse the following 12-bit block length and signed 12-bit PCM offset.
3. Require plausible bounds and a repeatable block cadence.
4. Parse the seed samples and verify that they reproduce the next stored seed
   under the unmix recurrence.
5. Determine the CRC width/polynomial and require repeated CRC success over
   both the significant PCM and auxiliary data.

Marginal entropy is only a diagnostic. Dithered PCM and compressed auxiliary
data can both have nearly perfect one/zero balance.

## Two-channel reconstruction described by the patent

Let `C[n]` be the carrier sample with the auxiliary LSBs cleared. Given seed
samples `A[0]` and `B[1]` at the block's declared PCM offset:

```text
B[0] = C[0] - A[0]
A[1] = C[1] - B[1]

even n >= 2: B[n] = B[n-1]; A[n] = C[n] - B[n]
odd  n >= 3: A[n] = A[n-1]; B[n] = C[n] - A[n]
```

This yields the alternating exact samples of the two reduced streams. Fill the
missing samples with interpolation, then apply any error-approximation table
and undo the declared attenuation. Channel identity and layout must come from
validated stream information; fixed Auro presentations must be emitted as
labeled fixed channels, never fabricated objects.

## Harletty implementation outline

1. Add an offline integer-PCM probe before touching the realtime bridge.
2. Preserve the lossless XLL output as integer PCM through `dca::HdFrame` (or
   provide an integer analysis tap without doubling steady-state buffers).
3. Implement a bounded streaming synchronizer/parser with fixed reusable
   storage and checked lengths/offset arithmetic.
4. Latch the detected format only after several valid blocks and CRCs.
5. Add the unmix/interpolation stage, then emit stable labeled-channel frames.
6. Keep all research logging and corpus dumping outside the shipped ABI hot
   path.

## Local corpus comparison

The official content page was compared on 2026-07-23 against:

- 351 MKV files under the local `Films`/`atmos` directories;
- 89 MKV files under `/mnt/nas/backup/Videos`;
- 440 MKV files total.

The official page has two materially different categories:

- **Cinema** means the film had an Auro theatrical mix. It does not prove that
  an arbitrary Blu-ray/UHD edition contains an Auro carrier.
- **Movies (Blu-ray/Digital)** identifies home releases, but territory and
  edition still matter.

### Matching home-release titles

| Official title | Local/NAS edition | Observed audio | Assessment |
|---|---|---|---|
| Ballerina (2025) | `/mnt/local/HDD_F/Films/Ballerina.2025.Multi.Truefrench.2160p.Bluray.Remux.DV.HDR10.HEVC-BDHD.mkv` | French and English TrueHD Atmos 7.1/24-bit | Matching title, but this remux contains Atmos rather than an identifiable Auro carrier. The official entry is territory-specific. |
| Blade Runner 2049 | `/mnt/nas/backup/Videos/TrueHD_Atmos/Blade.Runner.2049.2017.Multi.2160p.BluRay.REMUX.HEVC.HYBRID.DoVi.TrueHD.Atmos.7.1-ONLY.mkv` | English TrueHD Atmos 7.1/24-bit; French DTS-HD MA 5.1/16-bit | No plausible 24-bit Auro carrier in this remux. |
| Borderlands (2024) | `/mnt/nas/backup/Videos/TrueHD_Atmos/Borderlands.2024.MULTi.TRUEFRENCH.2160p.UHD.BluRay.REMUX.DV.HDR10.TrueHD.7.1.HEVC-REBiRTH.mkv` | French and English TrueHD 7.1/24-bit, no Atmos profile reported | Initially promising, but both tracks have their low four bits almost entirely zero over a 20-second probe and no patent sync signature. This is not the listed Benelux Auro carrier, or its auxiliary layer is absent. |
| Everything Everywhere All at Once (2022) | `/mnt/nas/backup/Videos/TrueHD_Atmos/Everything.Everywhere.All.at.Once.2022.MULTI.VFF.2160p.UHD.BLURAY.REMUX.DV.HDR.TrueHD.7.1.x265-OPTIMUM.mkv` | French and English TrueHD Atmos 7.1/24-bit | Correct title/4K class, but not the specific Auro edition. |
| Everything Everywhere All at Once (2022) | `/mnt/local/SSD_A-CT4000/Films/[ Torrent911.cc ] Everything.Everywhere.All.at.Once.2022.MULTi.1080p.BluRay.DTS.x264-EXTREME.mkv` | French and English lossy DTS 5.1 | Lossy transport cannot preserve the auxiliary LSB layer. |
| Salyut 7 (2017) | `/mnt/local/HDD_D/Films/Salyut.7.2017.2160p.UHD.BLURAY.REMUX.HDR.HEVC.MULTI.DTS-HDMA.mkv` | Russian DTS-HD MA 7.1/48 kHz/16-bit; French lossy DTS 5.1 | Not the Russian 3D Blu-ray Auro edition. The 16-bit Russian LSBs show no structured auxiliary signature. |
| Salyut 7 (2017) | `/mnt/nas/backup/Videos/Salyut.7.-.La.storia.di.un.impresa.(2017).1080p.BluRay.DTS.ITA.AC3.RUS.Subs.x264.mkv` | Italian lossy DTS 5.1; Russian AC-3 5.1 | Cannot carry the bit-perfect auxiliary layer. |

The local `Jumanji.1995` is not a match: the official table links to IMDb
`tt2283362` (the 2017 film). `Ford v Ferrari`, `Guardians of the Galaxy`,
`Twisters`, `Despicable Me 4`, and `John Wick 3/4` were also rejected as title
matcher false positives for different films or sequels.

### Matching cinema-only titles

These local files match films in the official Cinema list, but their local
home editions do not thereby inherit the theatrical Auro mix:

| Film | Local file / relevant audio | Result |
|---|---|---|
| American Sniper (2014) | `/mnt/local/HDD_B/Films/American.Sniper.2014.UHD.MULTi.VFi.2160p.UHD.BluRay.REMUX.HDR.HEVC.TrueHD.7.1.Atmos-ONLY.mkv` — TrueHD Atmos | Not an Auro test carrier. |
| Everest (2015) | `/mnt/local/HDD_D/Films/Everest.2015.MULTi.VFF.VFQ.Hybrid.2160p.UHD.BluRay.REMUX.CUSTOM.DV.HDR10Plus.HEVC.TrueHD.7.1.Atmos-ONLY.mkv` — TrueHD Atmos | Not an Auro test carrier. |
| First Man (2018) | `/mnt/local/HDD_A/Films/TrueHD Atmos/First.Man.2018.MULTI.VFF.2160p.UHD.BLURAY.REMUX.DV.HDR10.TrueHD.7.1.HEVC-BREMBO.mkv` — English TrueHD Atmos, French E-AC-3 | Not an Auro test carrier. |
| In the Heart of the Sea (2015) | `/mnt/local/HDD_D/Films/Au.cœur.de.l.Océan.2015.mkv` — French and English TrueHD Atmos | Not an Auro test carrier. |
| Lucy (2014) | `/mnt/local/HDD_A/Films/TrueHD Atmos/Lucy.2014.2160p.UHD.BLURAY.REMUX.HDR.HEVC.MULTI.VFF.DTS-HDMA.x265-EXTREME.mkv` — French DTS-HD MA 5.1/24-bit, English TrueHD Atmos | The French DTS-HD track was probed. Its low bits are uniformly dither-like; the patent `q=4` hits occur at chance rate and no `q=6` sync is present. Not currently promising. |
| Minions (2015) | `/mnt/local/HDD_D/Films/Minions.2015.MULTI.VFF.2160p.UHD.BluRay.Remux.HDR.EAC3.7.1.Atmos.HEVC-D5T0.mkv` — E-AC-3/TrueHD Atmos | Not an Auro test carrier. |
| Sing (2016) | `/mnt/local/HDD_D/Films/Tous.En.Scene.2016.MULTI.VF2.2160p.UHD.BluRay.Remux.HDR.EAC3.7.1.Atmos-tlub.mkv` — TrueHD/E-AC-3 Atmos | Not an Auro test carrier. |
| The Legend of Tarzan (2016) | `/mnt/local/HDD_A/Films/The.Legend.of.Tarzan.2016.MULTi.VFF.VFQ.2160p.UHD.BluRay.REMUX.CUSTOM.HEVC.HDR.DV.TrueHD.Atmos.7.1-HDForever.mkv` — TrueHD Atmos | Not an Auro test carrier. |
| The Mummy (2017) | `/mnt/nas/backup/Videos/EAC3_Atmos/La.Momie.2017.mkv` — E-AC-3/TrueHD Atmos | Correct film (IMDb `tt2345759`), wrong home audio carrier. |
| The Secret Life of Pets (2016) | `/mnt/local/HDD_D/Films/Comme.Des.Betes.2016.MULTI.VF2.2160p.UHD.BluRay.Remux.HDR.EAC3.TrueHD.7.1.Atmos.HEVC-TSC.mkv` — includes English DTS-HD MA 7.1/24-bit | The DTS-HD track has zero/padded low bits rather than an auxiliary stream. |
| Warcraft (2016) | `/mnt/local/HDD_F/Films/Warcraft.Le.Commencement.2016..mkv` — French and English TrueHD Atmos | Not an Auro test carrier. |

## Current conclusion and required sample

No scanned MKV is currently confirmed as an Auro-Codec carrier. The highest
value next sample remains the Russian 3D Blu-ray edition of *Salyut 7*, expected
to expose a 5.1, 48 kHz, 24-bit lossless carrier. A 30-60 second bit-perfect
DTS-HD MA extract from that exact edition should be sufficient to establish the
auxiliary width, sync ordering, and initial block grammar before implementing
the realtime decoder.
