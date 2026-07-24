#!/usr/bin/env python3
"""Compare an xll_pcm_range bed against an interleaved ffmpeg f32 reference."""

from __future__ import annotations

import argparse
import itertools

import numpy as np


BED_NAMES = ("C", "L", "R", "Ls", "Rs", "LFE", "Lb", "Rb")
FFMPEG_NAMES = ("FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR")


def load(path: str, channels: int) -> np.ndarray:
    raw = np.memmap(path, dtype="<f4", mode="r")
    samples = raw.size // channels
    if samples == 0:
        raise ValueError(f"{path}: empty or shorter than one frame")
    return raw[: samples * channels].reshape(samples, channels)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("decoded", help="xll_pcm_range interleaved f32le")
    parser.add_argument("reference", help="ffmpeg interleaved 7.1 f32le")
    parser.add_argument(
        "--decoded-channels",
        type=int,
        default=12,
        help="total decoded channels, including four XLL-X sources",
    )
    parser.add_argument("--bed-channels", type=int, default=8)
    parser.add_argument("--reference-channels", type=int, default=8)
    parser.add_argument("--rate", type=int, default=48_000)
    parser.add_argument(
        "--skip-seconds",
        type=float,
        default=0.0,
        help="ignore matching decoder pre-roll at the start of both inputs",
    )
    parser.add_argument("--gate", type=float, default=1e-5)
    args = parser.parse_args()

    decoded = load(args.decoded, args.decoded_channels)
    reference = load(args.reference, args.reference_channels)
    if args.bed_channels > decoded.shape[1]:
        parser.error("--bed-channels exceeds --decoded-channels")
    if args.bed_channels != args.reference_channels:
        parser.error("bed and reference channel counts must match")

    skip = round(args.skip_seconds * args.rate)
    samples = min(decoded.shape[0], reference.shape[0]) - skip
    if samples <= 0:
        parser.error("--skip-seconds removes the complete comparison range")
    bed = np.asarray(
        decoded[skip : skip + samples, : args.bed_channels], dtype=np.float64
    )
    ref = np.asarray(reference[skip : skip + samples], dtype=np.float64)
    cost = np.empty((args.reference_channels, args.bed_channels))
    for ref_channel in range(args.reference_channels):
        for bed_channel in range(args.bed_channels):
            delta = ref[:, ref_channel] - bed[:, bed_channel]
            cost[ref_channel, bed_channel] = np.sqrt(np.mean(delta * delta))

    mapping = min(
        itertools.permutations(range(args.bed_channels)),
        key=lambda permutation: sum(
            cost[reference_channel, bed_channel]
            for reference_channel, bed_channel in enumerate(permutation)
        ),
    )
    worst = 0.0
    print(
        f"samples={samples} seconds={samples / args.rate:.3f} "
        f"skipped={skip}"
    )
    for reference_channel, bed_channel in enumerate(mapping):
        delta = ref[:, reference_channel] - bed[:, bed_channel]
        rmse = float(np.sqrt(np.mean(delta * delta)))
        maximum = float(np.max(np.abs(delta)))
        worst = max(worst, rmse)
        ref_name = (
            FFMPEG_NAMES[reference_channel]
            if args.reference_channels == len(FFMPEG_NAMES)
            else f"ref{reference_channel}"
        )
        bed_name = (
            BED_NAMES[bed_channel]
            if args.bed_channels == len(BED_NAMES)
            else f"bed{bed_channel}"
        )
        print(
            f"{ref_name:>4} -> {bed_name:<3} "
            f"rmse={rmse:.9e} maxabs={maximum:.9e}"
        )
    print(f"worst_rmse={worst:.9e} gate={args.gate:.9e}")
    if worst >= args.gate:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
