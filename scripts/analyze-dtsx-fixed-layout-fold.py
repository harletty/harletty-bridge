#!/usr/bin/env python3
"""Detailed fixed-layout comparison for paired spatial-audio encodes.

Inputs are the aligned, interleaved f32le files produced from:

* DTS:X: ``xll_pcm_range``, eight bed channels then X0..X7;
* Atmos: ``truehdd`` DAMF audio, trimmed to the coarse 512-sample alignment.

The script finds pair-specific delays/correlations in the most active programme
window.  Folding tests are deliberately performed only after this pairing
stage; correlation of programme envelopes alone is not treated as a channel
identity.
"""

from __future__ import annotations

import argparse
import math
from pathlib import Path

import numpy as np
from scipy.optimize import linear_sum_assignment, nnls
from scipy.signal import correlate, correlation_lags


SAMPLE_RATE = 48_000
FRAME_SAMPLES = 512
DTS_NAMES = [
    "C",
    "L",
    "R",
    "Ls",
    "Rs",
    "LFE",
    "Lb",
    "Rb",
    "X0",
    "X1",
    "X2",
    "X3",
    "X4",
    "X5",
    "X6",
    "X7",
]
ATMOS_NAMES = [
    "LFE",
    "L",
    "R",
    "Lw",
    "C",
    "Ls",
    "Lb",
    "TFL",
    "Rw",
    "Rs",
    "TML",
    "Rb",
    "TBL",
    "TFR",
    "TMR",
    "TBR",
]
EXPECTED_BED = {
    "C": "C",
    "L": "L",
    "R": "R",
    "Ls": "Ls",
    "Rs": "Rs",
    "LFE": "LFE",
    "Lb": "Lb",
    "Rb": "Rb",
}
STEREO_FOLD_TARGETS = [
    ("L/R", "L", "R"),
    ("Ls/Rs", "Ls", "Rs"),
    ("Lb/Rb", "Lb", "Rb"),
]
STEREO_X_PAIRS = [
    ("X0/X1", "X0", "X1"),
    ("X2/X3", "X2", "X3"),
    ("X4/X5", "X4", "X5"),
    ("X6/X7", "X6", "X7"),
]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("dtsx_f32le", type=Path)
    parser.add_argument("atmos_f32le", type=Path)
    parser.add_argument("--window-seconds", type=float, default=10.0)
    parser.add_argument("--max-lag-samples", type=int, default=1024)
    parser.add_argument("--fine-lag-samples", type=int, default=10)
    parser.add_argument("--fold-window-seconds", type=float, default=5.0)
    return parser.parse_args()


def open_pcm(path: Path) -> np.memmap:
    samples = path.stat().st_size // (4 * 16)
    return np.memmap(path, dtype="<f4", mode="r", shape=(samples, 16))


def frame_rms(pcm: np.ndarray) -> np.ndarray:
    samples = len(pcm) // FRAME_SAMPLES * FRAME_SAMPLES
    framed = pcm[:samples].reshape(-1, FRAME_SAMPLES, 16)
    return np.sqrt(np.mean(framed.astype(np.float64) ** 2, axis=1))


def active_window(
    energy: np.ndarray, seconds: float
) -> tuple[int, int]:
    frames = max(1, round(seconds * SAMPLE_RATE / FRAME_SAMPLES))
    count = len(energy)
    frames = min(frames, count)
    cumulative = np.concatenate(([0.0], np.cumsum(energy)))
    window_energy = cumulative[frames:] - cumulative[:-frames]
    start_frame = int(np.argmax(window_energy)) if len(window_energy) else 0
    return start_frame * FRAME_SAMPLES, frames * FRAME_SAMPLES


def lagged_correlation(
    dtsx: np.ndarray, atmos: np.ndarray, max_lag: int
) -> tuple[float, int]:
    dtsx = dtsx.astype(np.float64)
    atmos = atmos.astype(np.float64)
    dtsx -= np.mean(dtsx)
    atmos -= np.mean(atmos)
    cross = correlate(atmos, dtsx, mode="full", method="fft")
    lags = correlation_lags(len(atmos), len(dtsx), mode="full")
    selected = np.flatnonzero(np.abs(lags) <= max_lag)
    denominator = np.linalg.norm(atmos) * np.linalg.norm(dtsx)
    if denominator == 0.0:
        return 0.0, 0
    values = cross[selected] / denominator
    index = int(np.argmax(np.abs(values)))
    return float(values[index]), int(lags[selected[index]])


def windowed_power_groups(
    dtsx_rms: np.ndarray,
    atmos_rms: np.ndarray,
    window_seconds: float = 5.0,
) -> tuple[np.ndarray, np.ndarray]:
    frames = max(100, round(window_seconds * SAMPLE_RATE / FRAME_SAMPLES))
    aggregate = np.zeros((16, 16))
    fit_scores: list[list[float]] = [[] for _ in range(16)]
    for start in range(0, min(len(dtsx_rms), len(atmos_rms)), frames):
        end = min(
            start + frames, len(dtsx_rms), len(atmos_rms)
        )
        if end - start < 100:
            continue
        design = atmos_rms[start:end].astype(np.float64) ** 2
        scale = np.sqrt(np.mean(design * design, axis=0))
        scale[scale == 0.0] = 1.0
        design_scaled = design / scale
        for channel in range(16):
            target = dtsx_rms[start:end, channel].astype(np.float64) ** 2
            if np.max(target) <= 1e-12:
                continue
            coefficient, _ = nnls(design_scaled, target)
            weights = coefficient / scale
            prediction = design @ weights
            total = np.sum((target - np.mean(target)) ** 2)
            residual = np.sum((target - prediction) ** 2)
            score = max(0.0, 1.0 - residual / total) if total > 0 else 0.0
            fit_scores[channel].append(score)
            weight_sum = np.sum(weights)
            if score >= 0.2 and weight_sum > 0.0:
                aggregate[channel] += weights / weight_sum * score
    score_medians = np.array(
        [
            np.median(scores) if scores else 0.0
            for scores in fit_scores
        ]
    )
    return aggregate, score_medians


def aligned_channels(
    dtsx: np.ndarray,
    atmos: np.ndarray,
    dtsx_channel: int,
    atmos_channel: int,
    lag: int,
) -> tuple[np.ndarray, np.ndarray]:
    """Return a DTS/Atmos pair aligned using the lag convention above."""
    if lag >= 0:
        return (
            dtsx[: len(dtsx) - lag, dtsx_channel],
            atmos[lag:, atmos_channel],
        )
    return (
        dtsx[-lag:, dtsx_channel],
        atmos[: len(atmos) + lag, atmos_channel],
    )


def covariance(
    left: np.ndarray, right: np.ndarray
) -> tuple[float, float, float]:
    left64 = left.astype(np.float64)
    right64 = right.astype(np.float64)
    left64 -= np.mean(left64)
    right64 -= np.mean(right64)
    return (
        float(np.dot(left64, right64)),
        float(np.dot(left64, left64)),
        float(np.dot(right64, right64)),
    )


def fold_curve(
    target: np.ndarray,
    bed: np.ndarray,
    extension: np.ndarray,
    folds: np.ndarray,
) -> np.ndarray:
    """Absolute correlation of target with ``bed - fold * extension``."""
    target64 = target.astype(np.float64)
    bed64 = bed.astype(np.float64)
    extension64 = extension.astype(np.float64)
    target64 -= np.mean(target64)
    bed64 -= np.mean(bed64)
    extension64 -= np.mean(extension64)
    yy = np.dot(target64, target64)
    bb = np.dot(bed64, bed64)
    xx = np.dot(extension64, extension64)
    yb = np.dot(target64, bed64)
    yx = np.dot(target64, extension64)
    bx = np.dot(bed64, extension64)
    variance = bb + folds * folds * xx - 2.0 * folds * bx
    denominator = np.sqrt(np.maximum(yy * variance, 1e-300))
    return np.abs((yb - folds * yx) / denominator)


def stereo_fold_analysis(
    dtsx: np.ndarray,
    atmos: np.ndarray,
    lag: int,
    window_seconds: float,
) -> None:
    """Test bounded, symmetric subtraction folds without free gain fitting."""
    folds = np.linspace(0.0, 1.0, 201)
    window = max(1, round(window_seconds * SAMPLE_RATE))
    print(
        "symmetric_folds: corrected_bed=bed-f*X, "
        f"lag={lag:+d}, f in [0,1]"
    )
    for target_name, left_name, right_name in STEREO_FOLD_TARGETS:
        left_dts = DTS_NAMES.index(left_name)
        right_dts = DTS_NAMES.index(right_name)
        left_atmos = ATMOS_NAMES.index(left_name)
        right_atmos = ATMOS_NAMES.index(right_name)
        left_bed, left_target = aligned_channels(
            dtsx, atmos, left_dts, left_atmos, lag
        )
        right_bed, right_target = aligned_channels(
            dtsx, atmos, right_dts, right_atmos, lag
        )
        for pair_name, left_x_name, right_x_name in STEREO_X_PAIRS:
            left_x, _ = aligned_channels(
                dtsx,
                atmos,
                DTS_NAMES.index(left_x_name),
                left_atmos,
                lag,
            )
            right_x, _ = aligned_channels(
                dtsx,
                atmos,
                DTS_NAMES.index(right_x_name),
                right_atmos,
                lag,
            )
            full_score = (
                fold_curve(left_target, left_bed, left_x, folds)
                + fold_curve(right_target, right_bed, right_x, folds)
            ) / 2.0
            best_index = int(np.argmax(full_score))
            best_fold = float(folds[best_index])
            baseline = float(full_score[0])
            best = float(full_score[best_index])

            window_deltas: list[float] = []
            applied_deltas: list[float] = []
            window_folds: list[float] = []
            common = min(
                len(left_target),
                len(right_target),
                len(left_x),
                len(right_x),
            )
            for start in range(0, common, window):
                end = min(start + window, common)
                if end - start < SAMPLE_RATE:
                    continue
                left_energy = float(
                    np.mean(left_target[start:end].astype(np.float64) ** 2)
                )
                right_energy = float(
                    np.mean(right_target[start:end].astype(np.float64) ** 2)
                )
                if max(left_energy, right_energy) < 1e-10:
                    continue
                scores = (
                    fold_curve(
                        left_target[start:end],
                        left_bed[start:end],
                        left_x[start:end],
                        folds,
                    )
                    + fold_curve(
                        right_target[start:end],
                        right_bed[start:end],
                        right_x[start:end],
                        folds,
                    )
                ) / 2.0
                index = int(np.argmax(scores))
                window_folds.append(float(folds[index]))
                window_deltas.append(float(scores[index] - scores[0]))
                applied_deltas.append(
                    float(scores[best_index] - scores[0])
                )

            applied_median_delta = (
                float(np.median(applied_deltas))
                if applied_deltas
                else 0.0
            )
            applied_positive = sum(delta > 0.0 for delta in applied_deltas)
            improving = [
                fold
                for fold, delta in zip(window_folds, window_deltas)
                if delta >= 0.01
            ]
            if improving:
                lower, median, upper = np.percentile(
                    improving, [25.0, 50.0, 75.0]
                )
                stability = (
                    f"improving={len(improving)}/{len(window_folds)}, "
                    f"f_IQR={lower:.2f}/{median:.2f}/{upper:.2f}"
                )
            else:
                stability = (
                    f"improving=0/{len(window_folds)}, f_IQR=-"
                )
            gain = (
                20.0 * math.log10(best_fold)
                if best_fold > 0.0
                else float("-inf")
            )
            print(
                f"  {target_name:<5} - {pair_name}: "
                f"base={baseline:.4f}, best={best:.4f}, "
                f"delta={best - baseline:+.4f}, "
                f"f={best_fold:.3f} ({gain:+.2f} dB), "
                f"applied_window_delta={applied_median_delta:+.4f}, "
                f"positive={applied_positive}/{len(applied_deltas)}, "
                f"{stability}"
            )


def center_fold_analysis(
    dtsx: np.ndarray,
    atmos: np.ndarray,
    lag: int,
    window_seconds: float,
) -> None:
    folds = np.linspace(0.0, 1.0, 201)
    window = max(1, round(window_seconds * SAMPLE_RATE))
    center_bed, center_target = aligned_channels(
        dtsx,
        atmos,
        DTS_NAMES.index("C"),
        ATMOS_NAMES.index("C"),
        lag,
    )
    print("center_folds: corrected_C=C-f*X, f in [0,1]")
    for extension_name in DTS_NAMES[8:]:
        extension, _ = aligned_channels(
            dtsx,
            atmos,
            DTS_NAMES.index(extension_name),
            ATMOS_NAMES.index("C"),
            lag,
        )
        scores = fold_curve(
            center_target, center_bed, extension, folds
        )
        best_index = int(np.argmax(scores))
        best_fold = float(folds[best_index])
        deltas: list[float] = []
        common = min(len(center_target), len(extension))
        for start in range(0, common, window):
            end = min(start + window, common)
            if end - start < SAMPLE_RATE:
                continue
            if np.mean(
                center_target[start:end].astype(np.float64) ** 2
            ) < 1e-10:
                continue
            local = fold_curve(
                center_target[start:end],
                center_bed[start:end],
                extension[start:end],
                folds[[0, best_index]],
            )
            deltas.append(float(local[1] - local[0]))
        gain = (
            20.0 * math.log10(best_fold)
            if best_fold > 0.0
            else float("-inf")
        )
        median_delta = float(np.median(deltas)) if deltas else 0.0
        positive = sum(delta > 0.0 for delta in deltas)
        print(
            f"  C - {extension_name}: base={scores[0]:.4f}, "
            f"best={scores[best_index]:.4f}, "
            f"delta={scores[best_index] - scores[0]:+.4f}, "
            f"f={best_fold:.3f} ({gain:+.2f} dB), "
            f"applied_window_delta={median_delta:+.4f}, "
            f"positive={positive}/{len(deltas)}"
        )


def expected_bed_correlations(
    dtsx: np.ndarray,
    atmos: np.ndarray,
    lag: int,
) -> None:
    print(f"expected_bed_geometry: fixed_lag={lag:+d}")
    for dtsx_name, atmos_name in EXPECTED_BED.items():
        bed, target = aligned_channels(
            dtsx,
            atmos,
            DTS_NAMES.index(dtsx_name),
            ATMOS_NAMES.index(atmos_name),
            lag,
        )
        cross, bed_power, target_power = covariance(bed, target)
        correlation = (
            cross / math.sqrt(bed_power * target_power)
            if bed_power > 0.0 and target_power > 0.0
            else 0.0
        )
        print(
            f"  {dtsx_name:>3} -> {atmos_name:<3}: "
            f"r={correlation:+.4f}, abs={abs(correlation):.4f}"
        )


def expected_bed_active_ranks(
    correlations: np.ndarray,
    lags: np.ndarray,
) -> None:
    print("expected_bed_active_windows:")
    for dtsx_name, atmos_name in EXPECTED_BED.items():
        dtsx_channel = DTS_NAMES.index(dtsx_name)
        atmos_channel = ATMOS_NAMES.index(atmos_name)
        order = np.argsort(
            np.abs(correlations[dtsx_channel])
        )[::-1]
        rank = int(np.flatnonzero(order == atmos_channel)[0]) + 1
        best = int(order[0])
        print(
            f"  {dtsx_name:>3} -> {atmos_name:<3}: "
            f"abs_r={abs(correlations[dtsx_channel, atmos_channel]):.4f}, "
            f"rank={rank}/16, "
            f"best={ATMOS_NAMES[best]} "
            f"({correlations[dtsx_channel, best]:+.4f}"
            f"@{lags[dtsx_channel, best]:+d})"
        )


def main() -> None:
    args = parse_args()
    dtsx = open_pcm(args.dtsx_f32le)
    atmos = open_pcm(args.atmos_f32le)
    common = min(len(dtsx), len(atmos))
    dtsx_rms = frame_rms(dtsx[:common])

    correlations = np.zeros((16, 16))
    lags = np.zeros((16, 16), dtype=np.int32)
    starts = np.zeros(16, dtype=np.int64)
    for dtsx_channel in range(16):
        start, length = active_window(
            dtsx_rms[:, dtsx_channel] ** 2,
            args.window_seconds,
        )
        starts[dtsx_channel] = start
        end = min(start + length, common)
        dtsx_window = dtsx[start:end]
        atmos_window = atmos[start:end]
        for atmos_channel in range(16):
            value, lag = lagged_correlation(
                dtsx_window[:, dtsx_channel],
                atmos_window[:, atmos_channel],
                args.max_lag_samples,
            )
            correlations[dtsx_channel, atmos_channel] = value
            lags[dtsx_channel, atmos_channel] = lag

    dtsx_indices, atmos_indices = linear_sum_assignment(
        -np.abs(correlations)
    )
    print(f"channel_windows={args.window_seconds:.3f}s at per-channel peak")
    print("forced_one_to_one_assignment_diagnostic:")
    for dtsx_channel, atmos_channel in zip(
        dtsx_indices, atmos_indices
    ):
        value = correlations[dtsx_channel, atmos_channel]
        lag = lags[dtsx_channel, atmos_channel]
        top = np.argsort(np.abs(correlations[dtsx_channel]))[::-1][:4]
        top_text = ", ".join(
            f"{ATMOS_NAMES[index]}:"
            f"{correlations[dtsx_channel, index]:+.3f}"
            f"@{lags[dtsx_channel, index]:+d}"
            for index in top
        )
        print(
            f"  {DTS_NAMES[dtsx_channel]:>3} -> "
            f"{ATMOS_NAMES[atmos_channel]:<3} "
            f"r={value:+.4f} lag={lag:+d} "
            f"window={starts[dtsx_channel] / SAMPLE_RATE:.3f}s; "
            f"top {top_text}"
        )
    expected_bed_active_ranks(correlations, lags)

    groups, group_scores = windowed_power_groups(dtsx_rms, frame_rms(atmos[:common]))
    print("windowed_power_groups:")
    for channel, name in enumerate(DTS_NAMES):
        order = np.argsort(groups[channel])[::-1]
        weight_sum = np.sum(groups[channel])
        if weight_sum == 0.0:
            top_text = "-"
        else:
            top_text = ", ".join(
                f"{ATMOS_NAMES[index]}:{groups[channel, index] / weight_sum:.2f}"
                for index in order[:4]
                if groups[channel, index] > 0.0
            )
        print(
            f"  {name:>3}: median_R2={group_scores[channel]:.3f}; "
            f"{top_text}"
        )

    expected_bed_correlations(
        dtsx[:common], atmos[:common], args.fine_lag_samples
    )
    stereo_fold_analysis(
        dtsx[:common],
        atmos[:common],
        args.fine_lag_samples,
        args.fold_window_seconds,
    )
    center_fold_analysis(
        dtsx[:common],
        atmos[:common],
        args.fine_lag_samples,
        args.fold_window_seconds,
    )


if __name__ == "__main__":
    main()
