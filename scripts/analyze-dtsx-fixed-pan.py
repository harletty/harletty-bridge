#!/usr/bin/env python3
"""Find locally coherent pans in a decoded fixed 7.1.4 presentation.

The input is the interleaved f32le produced by the ``xll_pcm_range`` example:
eight compatible-bed channels in DCA order followed by X0..X3.  This is a
research tool.  It estimates a dominant gain vector in short windows; it does
not claim to recover authoring objects or coordinates from the bitstream.
"""

from __future__ import annotations

import argparse
import math

import numpy as np


CHANNELS = ("C", "L", "R", "Ls", "Rs", "LFE", "Lb", "Rb", "X0", "X1", "X2", "X3")
HEIGHT = np.array((8, 9, 10, 11))
HEIGHT_BED_PAIRS = ((8, 1), (9, 2), (10, 6), (11, 7))


def clock(seconds: float) -> str:
    minute, second = divmod(seconds, 60.0)
    hour, minute = divmod(int(minute), 60)
    return f"{hour:02d}:{minute:02d}:{second:06.3f}"


def load(path: str) -> np.ndarray:
    raw = np.memmap(path, dtype="<f4", mode="r")
    samples = raw.size // len(CHANNELS)
    return raw[: samples * len(CHANNELS)].reshape(samples, len(CHANNELS))


def analyze(audio: np.ndarray, rate: int, width_ms: float, hop_ms: float, threshold: float) -> None:
    width = max(1, round(width_ms * rate / 1000.0))
    hop = max(1, round(hop_ms * rate / 1000.0))
    starts = np.arange(0, max(0, audio.shape[0] - width + 1), hop)
    count = starts.size
    shares = np.zeros(count)
    height_fractions = np.zeros(count)
    gains = np.zeros((count, 4))
    height_rms = np.zeros((count, 4))
    fold_beta = np.full((count, 4), np.nan)
    fold_corr = np.zeros((count, 4))

    for row, start in enumerate(starts):
        samples = np.asarray(audio[start : start + width], dtype=np.float64)
        samples -= samples.mean(axis=0, keepdims=True)
        covariance = samples.T @ samples
        height_covariance = covariance[np.ix_(HEIGHT, HEIGHT)]
        eigenvalues, eigenvectors = np.linalg.eigh(height_covariance)
        total = max(float(np.sum(eigenvalues)), 0.0)
        if total > 0.0:
            shares[row] = max(float(eigenvalues[-1]), 0.0) / total
            gain = eigenvectors[:, -1]
            if np.sum(gain) < 0.0:
                gain = -gain
            gains[row] = gain
        powers = np.diag(covariance)
        height_fractions[row] = float(np.sum(powers[HEIGHT]) / max(np.sum(powers), 1e-300))
        height_rms[row] = np.sqrt(np.maximum(powers[HEIGHT], 0.0) / width)
        for pair, (height_col, bed_col) in enumerate(HEIGHT_BED_PAIRS):
            xx = covariance[height_col, height_col]
            yy = covariance[bed_col, bed_col]
            xy = covariance[height_col, bed_col]
            if xx > 1e-30 and yy > 1e-30:
                fold_beta[row, pair] = xy / xx
                fold_corr[row, pair] = xy / math.sqrt(xx * yy)

    peak_height = np.max(height_rms, axis=1)
    active_floor = max(float(np.quantile(peak_height, 0.40)), 1e-12)
    reliable = (shares >= threshold) & (height_fractions >= 0.05) & (peak_height >= active_floor)
    selected = gains[reliable]
    selected_times = (starts[reliable] + width / 2) / rate

    print(
        f"duration={clock(audio.shape[0] / rate)} windows={count} "
        f"width={width_ms:g}ms hop={hop_ms:g}ms"
    )
    print(f"reliable rank-one height windows: {selected.shape[0]}/{count} (share>={threshold:.3f})")
    if selected.shape[0] == 0:
        return

    norms = np.linalg.norm(selected, axis=1)
    normalized = selected / np.maximum(norms[:, None], 1e-300)
    negative_energy = float(
        np.sum(np.square(np.minimum(normalized, 0.0))) / np.sum(np.square(normalized))
    )
    amplitudes = np.maximum(normalized, 0.0)
    amplitude_sum = np.sum(amplitudes, axis=1)
    valid_amplitude = amplitude_sum > 1e-12
    amplitudes[valid_amplitude] /= amplitude_sum[valid_amplitude, None]
    x = amplitudes[:, 1] + amplitudes[:, 3] - amplitudes[:, 0] - amplitudes[:, 2]
    front = amplitudes[:, 0] + amplitudes[:, 1] - amplitudes[:, 2] - amplitudes[:, 3]
    active = np.sum(amplitudes >= np.max(amplitudes, axis=1, keepdims=True) * 10 ** (-26 / 20), axis=1)
    corner = normalized[:, 1] * normalized[:, 2] - normalized[:, 0] * normalized[:, 3]
    corner_rms = float(np.sqrt(np.mean(np.square(corner))))

    print(f"median dominant covariance share: {np.median(shares[reliable]):.4f}")
    print(f"negative height-gain energy: {negative_energy:.3e}")
    print(f"median active height outputs at -26 dB: {np.median(active):.0f}/4")
    print(f"four-corner separability residual: {corner_rms:.4f}")
    print(
        "gain-centroid span: "
        f"left/right={np.min(x):+.3f}..{np.max(x):+.3f} "
        f"back/front={np.min(front):+.3f}..{np.max(front):+.3f}"
    )

    envelope = height_rms
    print("height RMS-envelope correlation:")
    correlation = np.corrcoef(envelope.T)
    for row in range(4):
        print("  " + " ".join(f"X{col}={correlation[row, col]:+.3f}" for col in range(4)))

    print("compatible-bed scalar links (windows with |r|>=.99):")
    for pair, (height_col, bed_col) in enumerate(HEIGHT_BED_PAIRS):
        mask = reliable & (np.abs(fold_corr[:, pair]) >= 0.99)
        values = fold_beta[mask, pair]
        if values.size:
            quantiles = np.quantile(values, (0.1, 0.5, 0.9))
            print(
                f"  {CHANNELS[height_col]}->{CHANNELS[bed_col]} n={values.size} "
                f"beta={quantiles[1]:+.6f}[{quantiles[0]:+.6f},{quantiles[2]:+.6f}]"
            )
        else:
            print(f"  {CHANNELS[height_col]}->{CHANNELS[bed_col]} n=0")

    if selected.shape[0] < 2:
        return
    delta = np.hypot(np.diff(x), np.diff(front))
    elapsed = np.diff(selected_times)
    adjacent = elapsed <= hop_ms / 1000.0 * 1.5
    motion = np.zeros(selected.shape[0])
    motion[1:] = np.where(adjacent, delta, 0.0)
    order = np.argsort(motion)[::-1]
    chosen: list[float] = []
    print("strongest coherent gain-vector motion windows:")
    for index in order:
        if motion[index] <= 0.0:
            break
        time = float(selected_times[index])
        if any(abs(time - previous) < 0.5 for previous in chosen):
            continue
        chosen.append(time)
        vector = normalized[index]
        print(
            f"  {clock(time)} step={motion[index]:.3f} share={shares[reliable][index]:.4f} "
            f"centroid=({x[index]:+.3f},{front[index]:+.3f}) "
            + " ".join(f"X{channel}={vector[channel]:+.3f}" for channel in range(4))
        )
        if len(chosen) == 12:
            break


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("raw", help="interleaved 12-channel f32le")
    parser.add_argument("--rate", type=int, default=48_000)
    parser.add_argument("--window-ms", type=float, default=100.0)
    parser.add_argument("--hop-ms", type=float, default=25.0)
    parser.add_argument("--rank-one", type=float, default=0.90)
    args = parser.parse_args()
    analyze(load(args.raw), args.rate, args.window_ms, args.hop_ms, args.rank_one)


if __name__ == "__main__":
    main()
