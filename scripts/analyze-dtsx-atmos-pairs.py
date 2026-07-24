#!/usr/bin/env python3
"""Compare aligned DTS:X and TrueHD Atmos programme envelopes.

This research script consumes frame-RMS files produced by the
``xll_rms_envelope`` example and the DAMF files produced by ``truehdd``.  It
aligns alternate encodes by their total-power envelopes, measures
channel-to-channel activity correspondence, and fits a non-negative static
power mixture for each DTS channel.  It does not claim to decode DTS:X object
metadata or a normative clustering matrix.
"""

from __future__ import annotations

import argparse
import collections
import csv
import re
import subprocess
from dataclasses import dataclass
from pathlib import Path

import numpy as np
import yaml
from scipy.optimize import nnls
from scipy.signal import correlate, correlation_lags


FRAME_SAMPLES = 512
SAMPLE_RATE = 48_000
FRAME_SECONDS = FRAME_SAMPLES / SAMPLE_RATE
ATMOS_CHANNELS = 16


@dataclass
class DtsxEnvelope:
    values: np.ndarray
    names: list[str]


@dataclass
class ObjectMotion:
    object_id: int
    unique_positions: int
    changes: int
    displacement: float


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--atmos-dir",
        type=Path,
        required=True,
        help="directory containing truehdd .atmos.audio/.metadata outputs",
    )
    parser.add_argument(
        "--dtsx-rms-dir",
        type=Path,
        required=True,
        help="directory containing <title>.dtsx-rms.f32le/.txt files",
    )
    parser.add_argument(
        "--dtsx-audio-dir",
        type=Path,
        help="directory containing the extracted DTS elementary streams",
    )
    parser.add_argument(
        "--xll-pcm-range",
        type=Path,
        help="compiled dca xll_pcm_range example for waveform checks",
    )
    parser.add_argument(
        "--cache-dir",
        type=Path,
        required=True,
        help="directory for compact Atmos RMS caches",
    )
    parser.add_argument(
        "--output",
        type=Path,
        required=True,
        help="TSV summary output",
    )
    parser.add_argument(
        "--max-lag-seconds",
        type=float,
        default=60.0,
        help="maximum absolute alignment offset",
    )
    parser.add_argument(
        "--waveform-seconds",
        type=float,
        default=10.0,
        help="active programme duration used for sample-level checks",
    )
    return parser.parse_args()


def read_dtsx_envelope(directory: Path, title: str) -> DtsxEnvelope:
    info_path = directory / f"{title}.dtsx-rms.txt"
    pcm_path = directory / f"{title}.dtsx-rms.f32le"
    info = info_path.read_text()
    names_match = re.search(r"^channel_order=(.+)$", info, re.MULTILINE)
    if names_match is None:
        raise ValueError(f"missing channel order in {info_path}")
    names = names_match.group(1).split(",")
    values = np.fromfile(pcm_path, dtype="<f4")
    if values.size % len(names):
        raise ValueError(f"partial RMS row in {pcm_path}")
    return DtsxEnvelope(values.reshape(-1, len(names)), names)


def write_atmos_envelope(audio_path: Path, output_path: Path) -> None:
    command = [
        "ffmpeg",
        "-loglevel",
        "quiet",
        "-i",
        str(audio_path),
        "-map",
        "0:a:0",
        "-c:a",
        "pcm_f32le",
        "-f",
        "f32le",
        "-",
    ]
    process = subprocess.Popen(command, stdout=subprocess.PIPE)
    assert process.stdout is not None
    frame_bytes = FRAME_SAMPLES * ATMOS_CHANNELS * 4
    pending = bytearray()
    rows: list[np.ndarray] = []
    while chunk := process.stdout.read(1024 * 1024):
        pending.extend(chunk)
        complete = len(pending) // frame_bytes
        if complete == 0:
            continue
        byte_count = complete * frame_bytes
        samples = np.frombuffer(
            memoryview(pending)[:byte_count], dtype="<f4"
        ).copy()
        samples = samples.reshape(complete, FRAME_SAMPLES, ATMOS_CHANNELS)
        rows.append(np.sqrt(np.mean(samples.astype(np.float64) ** 2, axis=1)))
        del pending[:byte_count]
    return_code = process.wait()
    if return_code:
        raise RuntimeError(f"ffmpeg failed for {audio_path}: {return_code}")
    if pending:
        sample_bytes = ATMOS_CHANNELS * 4
        complete_samples = len(pending) // sample_bytes
        if complete_samples:
            samples = np.frombuffer(
                memoryview(pending)[: complete_samples * sample_bytes],
                dtype="<f4",
            ).copy()
            samples = samples.reshape(complete_samples, ATMOS_CHANNELS)
            rows.append(
                np.sqrt(
                    np.mean(samples.astype(np.float64) ** 2, axis=0)
                )[None, :]
            )
    envelope = np.concatenate(rows).astype("<f4", copy=False)
    envelope.tofile(output_path)


def read_atmos_envelope(
    audio_path: Path, cache_path: Path
) -> np.ndarray:
    if not cache_path.is_file():
        print(f"atmos-rms\t{audio_path.stem.removesuffix('.atmos')}")
        write_atmos_envelope(audio_path, cache_path)
    values = np.fromfile(cache_path, dtype="<f4")
    if values.size % ATMOS_CHANNELS:
        raise ValueError(f"partial Atmos RMS row in {cache_path}")
    return values.reshape(-1, ATMOS_CHANNELS)


def overlap_slices(
    atmosphere: np.ndarray, dtsx: np.ndarray, lag: int
) -> tuple[np.ndarray, np.ndarray]:
    if lag >= 0:
        count = min(len(dtsx), len(atmosphere) - lag)
        return atmosphere[lag : lag + count], dtsx[:count]
    count = min(len(atmosphere), len(dtsx) + lag)
    return atmosphere[:count], dtsx[-lag : -lag + count]


def pearson(x: np.ndarray, y: np.ndarray) -> float:
    x = x.astype(np.float64, copy=False)
    y = y.astype(np.float64, copy=False)
    x = x - np.mean(x)
    y = y - np.mean(y)
    denominator = np.linalg.norm(x) * np.linalg.norm(y)
    return float(np.dot(x, y) / denominator) if denominator else float("nan")


def normalized_lag_score(
    atmosphere: np.ndarray, dtsx: np.ndarray, max_lag: int
) -> tuple[int, float]:
    atmosphere = atmosphere.astype(np.float64, copy=False)
    dtsx = dtsx.astype(np.float64, copy=False)
    cross = correlate(atmosphere, dtsx, mode="full", method="fft")
    lags = correlation_lags(len(atmosphere), len(dtsx), mode="full")
    keep = np.flatnonzero(np.abs(lags) <= max_lag)
    best_lag = 0
    best_score = -np.inf
    for index in keep:
        lag = int(lags[index])
        a, d = overlap_slices(atmosphere, dtsx, lag)
        if len(a) < 100:
            continue
        numerator = (
            cross[index]
            - float(np.sum(a)) * float(np.sum(d)) / len(a)
        )
        a_var = float(np.dot(a, a) - np.sum(a) ** 2 / len(a))
        d_var = float(np.dot(d, d) - np.sum(d) ** 2 / len(d))
        if a_var <= 0.0 or d_var <= 0.0:
            continue
        score = numerator / np.sqrt(a_var * d_var)
        if score > best_score:
            best_lag = lag
            best_score = score
    return best_lag, float(best_score)


def correlation_matrix(x: np.ndarray, y: np.ndarray) -> np.ndarray:
    x = x.astype(np.float64, copy=False)
    y = y.astype(np.float64, copy=False)
    x = x - np.mean(x, axis=0)
    y = y - np.mean(y, axis=0)
    denominator = np.sqrt(
        np.sum(x * x, axis=0)[:, None] * np.sum(y * y, axis=0)[None, :]
    )
    return np.divide(
        x.T @ y,
        denominator,
        out=np.zeros((x.shape[1], y.shape[1])),
        where=denominator > 0.0,
    )


def fit_power_mixtures(
    atmosphere_rms: np.ndarray, dtsx_rms: np.ndarray
) -> tuple[np.ndarray, np.ndarray]:
    atmosphere_power = atmosphere_rms.astype(np.float64) ** 2
    dtsx_power = dtsx_rms.astype(np.float64) ** 2
    scale = np.sqrt(np.mean(atmosphere_power * atmosphere_power, axis=0))
    scale[scale == 0.0] = 1.0
    design = atmosphere_power / scale
    weights = np.zeros((dtsx_power.shape[1], ATMOS_CHANNELS))
    r_squared = np.zeros(dtsx_power.shape[1])
    for channel in range(dtsx_power.shape[1]):
        target = dtsx_power[:, channel]
        coefficient, _ = nnls(design, target)
        prediction = design @ coefficient
        residual = np.sum((target - prediction) ** 2)
        total = np.sum((target - np.mean(target)) ** 2)
        r_squared[channel] = 1.0 - residual / total if total > 0 else 0.0
        weights[channel] = coefficient / scale
    return r_squared, weights


def fit_windowed_power_mixtures(
    atmosphere_rms: np.ndarray,
    dtsx_rms: np.ndarray,
    extension_start: int,
    window_seconds: float = 5.0,
) -> tuple[float, float, float]:
    window_frames = max(100, round(window_seconds / FRAME_SECONDS))
    scores = []
    dominants: list[np.ndarray] = []
    for start in range(0, len(dtsx_rms), window_frames):
        end = min(start + window_frames, len(dtsx_rms))
        if end - start < 100:
            continue
        r_squared, weights = fit_power_mixtures(
            atmosphere_rms[start:end], dtsx_rms[start:end]
        )
        scores.extend(np.clip(r_squared[extension_start:], 0.0, 1.0))
        dominants.append(
            np.argmax(weights[extension_start:], axis=1)
        )
    if not dominants:
        return float("nan"), float("nan"), float("nan")
    dominant_matrix = np.stack(dominants)
    unique_dominants = float(
        np.mean(
            [
                len(np.unique(dominant_matrix[:, channel]))
                for channel in range(dominant_matrix.shape[1])
            ]
        )
    )
    if len(dominants) < 2:
        switch_rate = 0.0
    else:
        switch_rate = float(
            np.mean(dominant_matrix[1:] != dominant_matrix[:-1])
        )
    return float(np.mean(scores)), unique_dominants, switch_rate


def read_motion(
    metadata_path: Path, start_sample: int = 0
) -> dict[int, ObjectMotion]:
    data = yaml.safe_load(metadata_path.read_text())
    positions: dict[int, list[tuple[int, tuple[float, float, float]]]] = (
        collections.defaultdict(list)
    )
    for event in data.get("events", []):
        if "pos" not in event:
            continue
        positions[int(event["ID"])].append(
            (
                int(event.get("samplePos", 0)),
                tuple(float(value) for value in event["pos"]),
            )
        )
    result = {}
    for object_id, events in positions.items():
        baseline = None
        points = []
        for sample, point in events:
            if sample <= start_sample:
                baseline = point
            else:
                if baseline is not None and not points:
                    points.append(baseline)
                points.append(point)
        if not points and baseline is not None:
            points = [baseline]
        if not points:
            points = [events[0][1]]
        changes = sum(a != b for a, b in zip(points, points[1:]))
        displacement = sum(
            float(np.linalg.norm(np.subtract(b, a)))
            for a, b in zip(points, points[1:])
        )
        result[object_id] = ObjectMotion(
            object_id=object_id,
            unique_positions=len(set(points)),
            changes=changes,
            displacement=displacement,
        )
    return result


def atmosphere_signal_names(motion: dict[int, ObjectMotion]) -> list[str]:
    # truehdd writes the active elements contiguously: the sole LFE bed signal
    # first, followed by dynamic objects in DAMF ID order. DAMF ID 3 is the
    # semantic speaker ID, not a sparse index into this 16-channel PCM file.
    object_ids = sorted(motion)
    return ["LFE", *(f"O{object_id}" for object_id in object_ids)]


def weight_summary(weights: np.ndarray, names: list[str]) -> str:
    if not np.any(weights > 0.0):
        return "-"
    order = np.argsort(weights)[::-1]
    total = float(np.sum(weights))
    selected = []
    accumulated = 0.0
    for index in order:
        share = float(weights[index] / total)
        if share < 0.05 and selected:
            break
        selected.append(f"{names[index]}:{share:.2f}")
        accumulated += share
        if accumulated >= 0.9:
            break
    return ",".join(selected)


def locate_dtsx_stream(directory: Path, title: str) -> Path:
    expected = re.sub(r"[^a-z0-9]+", "", title.lower())
    candidates = [
        path
        for path in directory.glob("* - DTS-X*.dts")
        if re.sub(
            r"[^a-z0-9]+",
            "",
            path.name.split(" - DTS-X", maxsplit=1)[0].lower(),
        )
        == expected
    ]
    if len(candidates) != 1:
        raise ValueError(f"expected one DTS stream for {title}: {candidates}")
    return candidates[0]


def select_waveform_window(
    atmosphere_rms: np.ndarray,
    dtsx_rms: np.ndarray,
    lag: int,
    duration_seconds: float,
) -> tuple[float, float, float]:
    aligned_atmosphere, aligned_dtsx = overlap_slices(
        atmosphere_rms, dtsx_rms, lag
    )
    frames = max(1, round(duration_seconds / FRAME_SECONDS))
    frames = min(frames, len(aligned_dtsx))
    joint_power = np.sum(aligned_atmosphere.astype(np.float64) ** 2, axis=1)
    joint_power += np.sum(aligned_dtsx.astype(np.float64) ** 2, axis=1)
    if frames == len(joint_power):
        aligned_start = 0
    else:
        cumulative = np.concatenate(([0.0], np.cumsum(joint_power)))
        energy = cumulative[frames:] - cumulative[:-frames]
        aligned_start = int(np.argmax(energy))
    if lag >= 0:
        dtsx_frame = aligned_start
        atmosphere_frame = aligned_start + lag
    else:
        dtsx_frame = aligned_start - lag
        atmosphere_frame = aligned_start
    return (
        dtsx_frame * FRAME_SECONDS,
        atmosphere_frame * FRAME_SECONDS,
        frames * FRAME_SECONDS,
    )


def read_waveform(
    title: str,
    dtsx_stream: Path,
    atmosphere_audio: Path,
    dtsx_channels: int,
    dtsx_start: float,
    atmosphere_start: float,
    duration: float,
    xll_pcm_range: Path,
    temporary_dir: Path,
) -> tuple[np.ndarray, np.ndarray]:
    safe_title = re.sub(r"[^A-Za-z0-9_.-]+", "_", title)
    dtsx_path = temporary_dir / f"{safe_title}.dtsx-wave.f32le"
    atmosphere_path = temporary_dir / f"{safe_title}.atmos-wave.f32le"
    subprocess.run(
        [
            str(xll_pcm_range),
            str(dtsx_stream),
            str(dtsx_path),
            f"{dtsx_start:.9f}",
            f"{dtsx_start + duration:.9f}",
        ],
        check=True,
        stdout=subprocess.DEVNULL,
    )
    subprocess.run(
        [
            "ffmpeg",
            "-loglevel",
            "quiet",
            "-i",
            str(atmosphere_audio),
            "-ss",
            f"{atmosphere_start:.9f}",
            "-t",
            f"{duration:.9f}",
            "-map",
            "0:a:0",
            "-c:a",
            "pcm_f32le",
            "-f",
            "f32le",
            str(atmosphere_path),
            "-y",
        ],
        check=True,
    )
    dtsx = np.fromfile(dtsx_path, dtype="<f4").reshape(-1, dtsx_channels)
    atmosphere = np.fromfile(atmosphere_path, dtype="<f4").reshape(
        -1, ATMOS_CHANNELS
    )
    dtsx_path.unlink()
    atmosphere_path.unlink()
    return atmosphere, dtsx


def fine_waveform_alignment(
    atmosphere: np.ndarray,
    dtsx: np.ndarray,
    rms_correlation: np.ndarray,
) -> tuple[np.ndarray, np.ndarray, int, float, tuple[int, int]]:
    candidates = set()
    for dtsx_channel in range(dtsx.shape[1]):
        order = np.argsort(np.abs(rms_correlation[dtsx_channel]))[::-1]
        for atmosphere_channel in order[:3]:
            candidates.add((dtsx_channel, int(atmosphere_channel)))
    candidates.add((5, 0))

    fine_lag = 0
    peak_correlation = 0.0
    peak_pair = (0, 0)
    for dtsx_channel, atmosphere_channel in sorted(candidates):
        atmosphere_samples = atmosphere[:, atmosphere_channel].astype(
            np.float64
        )
        dtsx_samples = dtsx[:, dtsx_channel].astype(np.float64)
        cross = correlate(
            atmosphere_samples, dtsx_samples, mode="full", method="fft"
        )
        lags = correlation_lags(
            len(atmosphere_samples), len(dtsx_samples), mode="full"
        )
        keep = np.abs(lags) <= FRAME_SAMPLES * 2
        selected = np.flatnonzero(keep)
        denominator = np.linalg.norm(atmosphere_samples) * np.linalg.norm(
            dtsx_samples
        )
        if denominator == 0.0:
            continue
        local = cross[selected] / denominator
        index = int(np.argmax(np.abs(local)))
        if abs(local[index]) > abs(peak_correlation):
            peak_correlation = float(local[index])
            fine_lag = int(lags[selected[index]])
            peak_pair = (dtsx_channel, atmosphere_channel)
    aligned_atmosphere, aligned_dtsx = overlap_slices(
        atmosphere, dtsx, fine_lag
    )
    return (
        aligned_atmosphere,
        aligned_dtsx,
        fine_lag,
        peak_correlation,
        peak_pair,
    )


def energy_fit_score(target: np.ndarray, prediction: np.ndarray) -> np.ndarray:
    residual = np.sum((target - prediction) ** 2, axis=0)
    total = np.sum(target * target, axis=0)
    score = np.divide(
        residual,
        total,
        out=np.ones_like(total),
        where=total > 0.0,
    )
    return np.clip(1.0 - score, 0.0, 1.0)


def ridge_fit(design: np.ndarray, target: np.ndarray) -> np.ndarray:
    covariance = design.T @ design
    ridge = max(float(np.trace(covariance)) / design.shape[1] * 1e-4, 1e-12)
    return np.linalg.solve(
        covariance + np.eye(design.shape[1]) * ridge,
        design.T @ target,
    )


def waveform_static_fit(
    atmosphere: np.ndarray, dtsx: np.ndarray
) -> tuple[np.ndarray, np.ndarray]:
    count = min(len(atmosphere), len(dtsx))
    midpoint = count // 2
    stride = max(1, midpoint // 100_000)
    full_x = atmosphere[::stride].astype(np.float64)
    full_y = dtsx[::stride].astype(np.float64)
    train_x = atmosphere[:midpoint:stride].astype(np.float64)
    train_y = dtsx[:midpoint:stride].astype(np.float64)
    test_x = atmosphere[midpoint::stride].astype(np.float64)
    test_y = dtsx[midpoint::stride].astype(np.float64)
    full_coefficient = ridge_fit(full_x, full_y)
    full_score = energy_fit_score(full_y, full_x @ full_coefficient)
    train_coefficient = ridge_fit(train_x, train_y)
    test_score = energy_fit_score(test_y, test_x @ train_coefficient)
    return full_score, test_score


def main() -> None:
    args = parse_args()
    args.cache_dir.mkdir(parents=True, exist_ok=True)
    titles = sorted(
        path.name.removesuffix(".dtsx-rms.txt")
        for path in args.dtsx_rms_dir.glob("*.dtsx-rms.txt")
    )
    fieldnames = [
        "title",
        "dtsx_channels",
        "dtsx_extensions",
        "atmos_objects",
        "moving_objects",
        "position_changes",
        "position_displacement",
        "offset_seconds",
        "power_envelope_r",
        "mean_best_rms_r",
        "mean_power_fit_r2",
        "extension_power_fit_r2",
        "windowed_extension_power_fit_r2",
        "mean_unique_window_dominants",
        "window_dominant_switch_rate",
        "fine_lag_samples",
        "fine_alignment_peak_r",
        "fine_alignment_pair",
        "mean_best_sample_r",
        "sample_static_fit_r2",
        "extension_sample_fit_r2",
        "sample_static_cv_r2",
        "extension_sample_cv_r2",
        "extension_mappings",
    ]
    with args.output.open("w", newline="") as output:
        writer = csv.DictWriter(
            output, fieldnames=fieldnames, dialect="excel-tab"
        )
        writer.writeheader()
        for title in titles:
            dtsx = read_dtsx_envelope(args.dtsx_rms_dir, title)
            audio_path = args.atmos_dir / f"{title}.atmos.audio"
            metadata_path = args.atmos_dir / f"{title}.atmos.metadata"
            atmosphere = read_atmos_envelope(
                audio_path, args.cache_dir / f"{title}.atmos-rms.f32le"
            )
            all_motion = read_motion(metadata_path)
            atmosphere_names = atmosphere_signal_names(all_motion)

            atmosphere_power = np.sum(atmosphere.astype(np.float64) ** 2, axis=1)
            dtsx_power = np.sum(dtsx.values.astype(np.float64) ** 2, axis=1)
            atmosphere_level = np.log10(
                atmosphere_power
                + max(float(np.percentile(atmosphere_power, 10)), 1e-14)
            )
            dtsx_level = np.log10(
                dtsx_power + max(float(np.percentile(dtsx_power, 10)), 1e-14)
            )
            lag, envelope_r = normalized_lag_score(
                atmosphere_level,
                dtsx_level,
                round(args.max_lag_seconds / FRAME_SECONDS),
            )
            aligned_atmosphere, aligned_dtsx = overlap_slices(
                atmosphere, dtsx.values, lag
            )
            # Ignore the first matched second, which several files use to
            # replace a common placeholder coordinate with their actual
            # programme layout.
            motion = read_motion(
                metadata_path,
                max(0, lag * FRAME_SAMPLES) + SAMPLE_RATE,
            )
            rms_correlation = correlation_matrix(
                aligned_dtsx, aligned_atmosphere
            )
            best_rms = np.max(rms_correlation, axis=1)
            fit_r2, weights = fit_power_mixtures(
                aligned_atmosphere, aligned_dtsx
            )
            extension_start = 8
            (
                windowed_extension_fit,
                unique_window_dominants,
                window_dominant_switch_rate,
            ) = fit_windowed_power_mixtures(
                aligned_atmosphere,
                aligned_dtsx,
                extension_start,
            )
            fine_lag = 0
            fine_peak = float("nan")
            fine_pair = "-"
            mean_best_sample = float("nan")
            sample_fit_mean = float("nan")
            extension_sample_fit = float("nan")
            sample_cv_mean = float("nan")
            extension_sample_cv = float("nan")
            if args.dtsx_audio_dir and args.xll_pcm_range:
                dtsx_start, atmosphere_start, duration = (
                    select_waveform_window(
                        atmosphere,
                        dtsx.values,
                        lag,
                        args.waveform_seconds,
                    )
                )
                waveform_atmosphere, waveform_dtsx = read_waveform(
                    title,
                    locate_dtsx_stream(args.dtsx_audio_dir, title),
                    audio_path,
                    len(dtsx.names),
                    dtsx_start,
                    atmosphere_start,
                    duration,
                    args.xll_pcm_range,
                    args.cache_dir,
                )
                (
                    waveform_atmosphere,
                    waveform_dtsx,
                    fine_lag,
                    fine_peak,
                    fine_pair_indices,
                ) = (
                    fine_waveform_alignment(
                        waveform_atmosphere,
                        waveform_dtsx,
                        rms_correlation,
                    )
                )
                fine_pair = (
                    f"{dtsx.names[fine_pair_indices[0]]}~"
                    f"{atmosphere_names[fine_pair_indices[1]]}"
                )
                sample_correlation = correlation_matrix(
                    waveform_dtsx, waveform_atmosphere
                )
                mean_best_sample = float(
                    np.mean(np.max(np.abs(sample_correlation), axis=1))
                )
                sample_fit, sample_cv = waveform_static_fit(
                    waveform_atmosphere, waveform_dtsx
                )
                sample_fit_mean = float(np.nanmean(sample_fit))
                extension_sample_fit = float(
                    np.nanmean(sample_fit[extension_start:])
                )
                sample_cv_mean = float(np.nanmean(sample_cv))
                extension_sample_cv = float(
                    np.nanmean(sample_cv[extension_start:])
                )
            extension_mappings = ";".join(
                f"{dtsx.names[channel]}="
                f"{weight_summary(weights[channel], atmosphere_names)}"
                for channel in range(extension_start, len(dtsx.names))
            )
            moving = [item for item in motion.values() if item.changes > 0]
            writer.writerow(
                {
                    "title": title,
                    "dtsx_channels": len(dtsx.names),
                    "dtsx_extensions": len(dtsx.names) - extension_start,
                    "atmos_objects": len(motion),
                    "moving_objects": len(moving),
                    "position_changes": sum(
                        item.changes for item in motion.values()
                    ),
                    "position_displacement": f"{sum(item.displacement for item in motion.values()):.3f}",
                    "offset_seconds": f"{lag * FRAME_SECONDS:.3f}",
                    "power_envelope_r": f"{envelope_r:.4f}",
                    "mean_best_rms_r": f"{float(np.mean(best_rms)):.4f}",
                    "mean_power_fit_r2": f"{float(np.mean(fit_r2)):.4f}",
                    "extension_power_fit_r2": f"{float(np.mean(fit_r2[extension_start:])):.4f}",
                    "windowed_extension_power_fit_r2": f"{windowed_extension_fit:.4f}",
                    "mean_unique_window_dominants": f"{unique_window_dominants:.2f}",
                    "window_dominant_switch_rate": f"{window_dominant_switch_rate:.4f}",
                    "fine_lag_samples": fine_lag,
                    "fine_alignment_peak_r": f"{fine_peak:.4f}",
                    "fine_alignment_pair": fine_pair,
                    "mean_best_sample_r": f"{mean_best_sample:.4f}",
                    "sample_static_fit_r2": f"{sample_fit_mean:.4f}",
                    "extension_sample_fit_r2": f"{extension_sample_fit:.4f}",
                    "sample_static_cv_r2": f"{sample_cv_mean:.4f}",
                    "extension_sample_cv_r2": f"{extension_sample_cv:.4f}",
                    "extension_mappings": extension_mappings,
                }
            )
            print(
                f"pair\t{title}\toffset={lag * FRAME_SECONDS:+.3f}s"
                f"\tenvelope_r={envelope_r:.3f}"
                f"\trms_r={np.mean(best_rms):.3f}"
                f"\tfit_r2={np.mean(fit_r2):.3f}"
                f"\tsample_r={mean_best_sample:.3f}"
                f"\tsample_fit={sample_fit_mean:.3f}"
            )


if __name__ == "__main__":
    main()
