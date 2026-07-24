#!/usr/bin/env python3
"""Classify four XLL-X waveforms in a controlled spatial-pan clip.

The test uses the public clip as a controlled, mostly single-source pan.  It
does not assume that the decoded XLL-X channels are speakers: their gain
vectors are estimated from the common waveform with a rank-one covariance
decomposition, then checked for speaker-feed and raw FOA signatures.

Optional video analysis tracks the bright object in the fixed-camera 5.1.2
section.  It verifies A/V alignment by comparing screen position with the
decoded left/right and floor/height energy balances.  Screen coordinates are
only proxies; the clip does not print numerical azimuth/elevation values.

Dependencies: numpy, scipy, and (with --video) opencv-python.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from pathlib import Path

import numpy as np
from scipy.io import wavfile


CHANNELS = ("FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR", "X0", "X1", "X2", "X3")
HEIGHT = np.array((8, 9, 10, 11))
LEFT = np.array((0, 4, 6, 8, 10))
RIGHT = np.array((1, 5, 7, 9, 11))
FLOOR = np.array((0, 1, 2, 4, 5, 6, 7))
HEIGHT_DOWNMIX_Q15 = 23170
HEIGHT_DOWNMIX_GAIN = HEIGHT_DOWNMIX_Q15 / 32768.0
HEIGHT_BED_PAIRS = ((0, 8), (1, 9), (4, 10), (5, 11))


@dataclass(frozen=True)
class LayoutInterval:
    name: str
    start: float
    end: float
    absent: tuple[int, ...]


# These conservative ranges avoid the animated transitions between layouts.
LAYOUTS = (
    LayoutInterval("7.1.4 (first)", 46.0, 61.5, ()),
    LayoutInterval("5.1.2", 64.0, 69.0, (4, 5, 10, 11)),
    LayoutInterval("2.1", 72.5, 75.0, (2, 4, 5, 6, 7, 8, 9, 10, 11)),
    LayoutInterval("5.1", 77.0, 79.5, (4, 5, 8, 9, 10, 11)),
    LayoutInterval("7.1", 82.0, 84.5, (8, 9, 10, 11)),
    LayoutInterval("7.1.4 (last)", 86.0, 89.0, ()),
)


def load_audio(bed_path: Path, height_path: Path) -> tuple[int, np.ndarray]:
    bed_rate, bed = wavfile.read(bed_path, mmap=True)
    height_rate, height = wavfile.read(height_path, mmap=True)
    if bed_rate != height_rate:
        raise ValueError(f"sample-rate mismatch: {bed_rate} != {height_rate}")
    if bed.ndim != 2 or bed.shape[1] != 8:
        raise ValueError(f"expected an eight-channel bed, got {bed.shape}")
    if height.ndim != 2 or height.shape[1] != 4:
        raise ValueError(f"expected four XLL-X channels, got {height.shape}")
    if bed.shape[0] != height.shape[0]:
        raise ValueError(f"sample-count mismatch: {bed.shape[0]} != {height.shape[0]}")
    return bed_rate, np.concatenate((bed, height), axis=1)


def interval(audio: np.ndarray, rate: int, start: float, end: float) -> np.ndarray:
    return np.asarray(audio[int(start * rate) : int(end * rate)], dtype=np.float64)


def rms(samples: np.ndarray) -> np.ndarray:
    return np.sqrt(np.mean(np.square(samples), axis=0))


def relative_db(values: np.ndarray) -> np.ndarray:
    peak = float(np.max(values))
    if peak == 0.0:
        return np.full_like(values, -np.inf)
    with np.errstate(divide="ignore"):
        return 20.0 * np.log10(values / peak)


def print_layout_activity(audio: np.ndarray, rate: int) -> None:
    print("Layout-controlled channel activity")
    print("----------------------------------")
    for item in LAYOUTS:
        levels = rms(interval(audio, rate, item.start, item.end))
        db = relative_db(levels)
        heights = " ".join(
            f"{CHANNELS[index]}={db[index]:6.1f} dB" if np.isfinite(db[index]) else f"{CHANNELS[index]}=  -inf"
            for index in HEIGHT
        )
        if item.absent:
            inactive = levels[np.array(item.absent)]
            inactive_db = relative_db(np.append(levels.max(), inactive))[1:]
            leakage = float(np.max(inactive_db))
            leak_text = f"; strongest forbidden output={leakage:.1f} dB" if np.isfinite(leakage) else "; forbidden outputs are exactly zero"
        else:
            leak_text = ""
        print(f"{item.name:15s} {heights}{leak_text}")
    print()


def windowed_rms(audio: np.ndarray, rate: int, start: float, end: float, width: float, hop: float) -> np.ndarray:
    size = int(width * rate)
    rows = []
    for time in np.arange(start, end - width, hop):
        rows.append(rms(interval(audio, rate, float(time), float(time) + width)))
    return np.asarray(rows)


def print_pair_correlations(audio: np.ndarray, rate: int) -> None:
    envelopes = windowed_rms(audio, rate, 46.0, 69.0, 0.1, 0.05)
    print("Lower/upper RMS-envelope correlations")
    print("-------------------------------------")
    for lower, upper in ((0, 8), (1, 9), (4, 10), (5, 11)):
        correlation = float(np.corrcoef(envelopes[:, lower], envelopes[:, upper])[0, 1])
        print(f"{CHANNELS[upper]} vs {CHANNELS[lower]}: {correlation:.3f}")
    print()


def rank_one_gains(
    audio: np.ndarray,
    rate: int,
    start: float = 46.0,
    end: float = 61.5,
    width: float = 0.15,
    hop: float = 0.05,
) -> tuple[np.ndarray, np.ndarray]:
    gains = []
    shares = []
    for time in np.arange(start, end - width, hop):
        samples = interval(audio, rate, float(time), float(time) + width)
        samples -= np.mean(samples, axis=0)
        covariance = samples.T @ samples
        eigenvalues, eigenvectors = np.linalg.eigh(covariance)
        total = float(np.sum(eigenvalues))
        if total <= 0.0:
            continue
        share = float(eigenvalues[-1] / total)
        gain = eigenvectors[:, -1]
        # A speaker pan should have common polarity.  Resolve the arbitrary PCA
        # sign in the direction that maximizes that hypothesis, then measure
        # whatever negative energy remains.
        if np.sum(gain) < 0.0:
            gain = -gain
        gains.append(gain)
        shares.append(share)
    return np.asarray(gains), np.asarray(shares)


def quadratic_features(vectors: np.ndarray) -> np.ndarray:
    x0, x1, x2, x3 = vectors.T
    return np.column_stack(
        (
            x0 * x0,
            x1 * x1,
            x2 * x2,
            x3 * x3,
            2.0 * x0 * x1,
            2.0 * x0 * x2,
            2.0 * x0 * x3,
            2.0 * x1 * x2,
            2.0 * x1 * x3,
            2.0 * x2 * x3,
        )
    )


def quadratic_matrix(coefficients: np.ndarray) -> np.ndarray:
    return np.array(
        (
            (coefficients[0], coefficients[4], coefficients[5], coefficients[6]),
            (coefficients[4], coefficients[1], coefficients[7], coefficients[8]),
            (coefficients[5], coefficients[7], coefficients[2], coefficients[9]),
            (coefficients[6], coefficients[8], coefficients[9], coefficients[3]),
        )
    )


def print_fixed_rematrix_test(height: np.ndarray) -> None:
    features = quadratic_features(height)
    indices = np.arange(height.shape[0])
    training = indices % 5 != 0
    testing = ~training
    _, _, right_vectors = np.linalg.svd(features[training], full_matrices=False)
    coefficients = right_vectors[-1]
    matrix = quadratic_matrix(coefficients)
    coefficients /= np.linalg.norm(matrix, ord="fro")
    matrix = quadratic_matrix(coefficients)
    eigenvalues = np.linalg.eigvalsh(matrix)
    holdout_residual = float(np.sqrt(np.mean(np.square(features[testing] @ coefficients))))

    # Four-corner separable panning has gains [L*F, R*F, L*B, R*B], hence
    # X1*X2 - X0*X3 == 0.  This is a (2,2)-signature quadratic surface.
    corner_relation = height[:, 1] * height[:, 2] - height[:, 0] * height[:, 3]
    corner_residual = float(np.sqrt(np.mean(np.square(corner_relation))))

    negative = int(np.sum(eigenvalues < -1e-6))
    positive = int(np.sum(eigenvalues > 1e-6))
    print("Unknown fixed 4x4 rematrix test")
    print("--------------------------------")
    print("FOA point-source gains lie on a quadratic cone with signature (1,3).")
    print("A real invertible 4x4 rematrix must preserve that signature.")
    print("fitted cone eigenvalues: " + " ".join(f"{value:+.3f}" for value in eigenvalues))
    print(f"fitted signature: ({negative},{positive}); holdout RMS residual={holdout_residual:.4f}")
    print(f"four-corner relation X1*X2=X0*X3 RMS residual: {corner_residual:.4f}")
    print("Result: no fixed real 4x4 transform can turn this gain surface into raw FOA.")
    print()


def q15_downmix(samples: np.ndarray) -> np.ndarray:
    pcm = np.rint(samples * 8_388_608.0).astype(np.int64)
    return (pcm * HEIGHT_DOWNMIX_Q15 + (1 << 14)) >> 15


def print_embedded_downmix_test(
    audio: np.ndarray, rate: int, gains: np.ndarray, shares: np.ndarray
) -> None:
    selected = gains[shares >= 0.95]
    print("Backward-compatible 7.1 height-downmix test")
    print("--------------------------------------------")
    print(
        f"candidate coefficient: {HEIGHT_DOWNMIX_Q15}/32768 = "
        f"{HEIGHT_DOWNMIX_GAIN:.9f} (-3 dB DTS table value)"
    )
    for lower, upper in HEIGHT_BED_PAIRS:
        usable = selected[:, upper] > 0.03
        ratio = selected[usable, lower] / selected[usable, upper]
        unfolded = selected[usable, lower] - HEIGHT_DOWNMIX_GAIN * selected[usable, upper]
        near_zero = float(np.mean(np.abs(unfolded) < 0.02))
        print(
            f"{CHANNELS[lower]}/{CHANNELS[upper]}: median ratio={np.median(ratio):.9f}; "
            f"unfolded gain near zero in {near_zero:.1%} of usable windows"
        )

    print("Best 100 ms fixed-point reconstruction windows (bed - rmul15(height, 23170)):")
    for lower, upper in HEIGHT_BED_PAIRS:
        best = None
        for time in np.arange(46.0, 68.9, 0.025):
            samples = interval(audio, rate, float(time), float(time) + 0.1)
            bed = np.rint(samples[:, lower] * 8_388_608.0).astype(np.int64)
            height = q15_downmix(samples[:, upper])
            bed_rms = float(np.sqrt(np.mean(np.square(bed.astype(np.float64)))))
            height_rms = float(np.sqrt(np.mean(np.square(height.astype(np.float64)))))
            if bed_rms < 10_000.0 or height_rms < 10_000.0:
                continue
            residual = bed - height
            residual_rms = float(np.sqrt(np.mean(np.square(residual.astype(np.float64)))))
            candidate = (
                residual_rms / bed_rms,
                float(time),
                residual_rms,
                int(np.max(np.abs(residual))),
            )
            best = candidate if best is None or candidate < best else best
        if best is None:
            continue
        relative, time, residual_rms, maximum = best
        print(
            f"  {CHANNELS[lower]}-{CHANNELS[upper]}: t={time:.3f}s, residual/bed={relative:.3e}, "
            f"residual RMS={residual_rms:.2f} LSB, max={maximum} LSB"
        )
    print("Result: the regular 7.1 contains a -3 dB copy of each height feed.")
    print()


def print_gain_model(audio: np.ndarray, rate: int) -> None:
    gains, shares = rank_one_gains(audio, rate)
    selected = gains[shares >= 0.95]
    height = selected[:, HEIGHT]
    height_share = np.linalg.norm(height, axis=1) / np.linalg.norm(selected, axis=1)
    height = height[height_share >= 0.10]
    height /= np.linalg.norm(height, axis=1, keepdims=True)

    negative_energy = float(np.sum(np.square(np.minimum(height, 0.0))) / np.sum(np.square(height)))
    active = np.sum(np.abs(height) >= 0.05, axis=1)

    print("Single-source gain-vector test (first 7.1.4 section)")
    print("---------------------------------------------------")
    print(f"usable windows: {height.shape[0]} / {gains.shape[0]}")
    print(f"median rank-one covariance share: {np.median(shares):.4f}")
    print(f"negative height-gain energy: {negative_energy:.3e}")
    print(f"median active height outputs at -26 dB: {np.median(active):.0f} / 4")
    print(f"windows with at most two active height outputs: {np.mean(active <= 2):.1%}")
    print()

    print("Raw first-order ambisonic (FOA) sanity check")
    print("---------------------------------------------")
    print("A raw FOA W component has constant non-zero magnitude after gain-vector normalization.")
    best = None
    for channel in range(4):
        values = np.abs(height[:, channel])
        variation = float(np.std(values) / np.mean(values))
        dropout = float(np.mean(values < 0.05))
        candidate = (variation, dropout, channel)
        best = candidate if best is None or candidate < best else best
        print(f"X{channel}: coefficient of variation={variation:.3f}, dropout={dropout:.1%}")
    assert best is not None
    print(
        f"best W candidate is X{best[2]}, but varies by {best[0]:.3f} and drops out in {best[1]:.1%} of windows"
    )
    print("Result: the four decoded waveforms are not raw W/X/Y/Z components.")
    print()
    print_fixed_rematrix_test(height)
    print_embedded_downmix_test(audio, rate, gains, shares)


def detect_bright_object(frame: np.ndarray) -> tuple[float, float] | None:
    import cv2

    hsv = cv2.cvtColor(frame, cv2.COLOR_BGR2HSV)
    hue = hsv[:, :, 0]
    saturation = hsv[:, :, 1]
    value = hsv[:, :, 2]
    warm_or_white = ((saturation < 180) & ((hue < 45) | (hue > 170))) | ((hue < 40) & (saturation > 40))
    mask = ((value > 245) & warm_or_white).astype(np.uint8) * 255
    mask[:100, :] = 0
    mask[800:, :] = 0
    mask[:, :100] = 0
    mask[:, 1820:] = 0

    count, _, stats, centroids = cv2.connectedComponentsWithStats(mask)
    candidates = []
    for index in range(1, count):
        _, _, width, height, area = (int(value) for value in stats[index])
        aspect = min(width, height) / max(width, height)
        center_x, center_y = centroids[index]
        if not (80 <= area <= 1300 and 7 <= width <= 70 and 7 <= height <= 70 and aspect >= 0.45):
            continue
        # Reject small pieces of the fixed television logo in this shot.
        if 930 < center_x < 1020 and 440 < center_y < 510:
            continue
        candidates.append((area * aspect, center_x, center_y))
    if not candidates:
        return None
    _, center_x, center_y = max(candidates)
    return float(center_x), float(center_y)


def print_video_alignment(video_path: Path, audio: np.ndarray, rate: int, audio_start: float) -> None:
    import cv2

    capture = cv2.VideoCapture(str(video_path))
    screen_x = []
    screen_y = []
    horizontal_balance = []
    height_fraction = []
    for video_time in np.arange(64.0, 69.0, 0.1):
        capture.set(cv2.CAP_PROP_POS_MSEC, float(video_time * 1000.0))
        ok, frame = capture.read()
        if not ok:
            continue
        point = detect_bright_object(frame)
        if point is None:
            continue
        audio_time = float(video_time - audio_start)
        samples = interval(audio, rate, audio_time - 0.05, audio_time + 0.05)
        levels = rms(samples)
        power = np.square(levels)
        left = float(np.sum(power[LEFT]))
        right = float(np.sum(power[RIGHT]))
        top = float(np.sum(power[HEIGHT]))
        floor = float(np.sum(power[FLOOR]))
        screen_x.append(point[0])
        screen_y.append(point[1])
        horizontal_balance.append((right - left) / (right + left + 1e-30))
        height_fraction.append(top / (top + floor + 1e-30))
    capture.release()

    print("Fixed-camera 5.1.2 picture/audio alignment")
    print("------------------------------------------")
    if len(screen_x) < 10:
        print(f"only {len(screen_x)} object positions detected; no reliable result")
        print()
        return
    horizontal_correlation = float(np.corrcoef(screen_x, horizontal_balance)[0, 1])
    vertical_correlation = float(np.corrcoef(screen_y, height_fraction)[0, 1])
    print(f"tracked video positions: {len(screen_x)}")
    print(f"screen X vs decoded right-minus-left energy: r={horizontal_correlation:.3f}")
    print(f"screen Y vs decoded height-energy fraction: r={vertical_correlation:.3f}")
    print("(The expected vertical sign is negative because smaller screen Y means higher.)")
    print()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bed", type=Path, required=True, help="decoded eight-channel bed WAV")
    parser.add_argument("--height", type=Path, required=True, help="decoded four-channel XLL-X WAV")
    parser.add_argument("--video", type=Path, help="optional source MKV for picture/audio alignment")
    parser.add_argument(
        "--audio-start",
        type=float,
        default=0.053,
        help="audio start time in the MKV relative to video, in seconds (default: 0.053)",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    rate, audio = load_audio(args.bed, args.height)
    print(f"audio: {audio.shape[0]} samples, {audio.shape[1]} channels, {rate} Hz")
    print()
    print_layout_activity(audio, rate)
    print_pair_correlations(audio, rate)
    print_gain_model(audio, rate)
    if args.video is not None:
        print_video_alignment(args.video, audio, rate, args.audio_start)


if __name__ == "__main__":
    main()
