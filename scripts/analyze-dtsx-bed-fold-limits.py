#!/usr/bin/env python3
"""Measure conservative extension-to-bed subtraction limits.

For every extension source X and compatible-bed channel B, this tool finds
the largest positive gain g for which

    energy(B - g * X) < energy(B)

in at least the requested fraction of blocks where X is active.  The block
condition is solved analytically; no gain grid or regression is involved.
The result is evidence about a possible fold, not a normative decoder matrix.

Inputs are CAF ``*.dtsx.audio`` files containing the eight compatible-bed
channels in DCA order followed by extension sources.
"""

from __future__ import annotations

import argparse
import csv
import json
import math
import re
import subprocess
from dataclasses import dataclass
from pathlib import Path

import numpy as np


BED_NAMES = ("C", "L", "R", "Ls", "Rs", "LFE", "Lb", "Rb")
BED_COORDINATES = {
    "C": (0.0, 1.0),
    "L": (-1.0, 1.0),
    "R": (1.0, 1.0),
    "Ls": (-1.0, 0.0),
    "Rs": (1.0, 0.0),
    "Lb": (-1.0, -1.0),
    "Rb": (1.0, -1.0),
}
FIXED_GAINS = (0.5, 1.0 / math.sqrt(2.0), 1.0)


@dataclass(frozen=True)
class AudioInfo:
    channels: int
    sample_rate: int
    frames: int


@dataclass(frozen=True)
class VideoInfo:
    path: Path
    duration: float
    audio_start: float


@dataclass(frozen=True)
class BlockStatistics:
    bed_energy: np.ndarray
    extension_energy: np.ndarray
    cross_energy: np.ndarray


@dataclass(frozen=True)
class FoldLimit:
    title: str
    path: Path
    bed: str
    extension: str
    blocks: int
    active_blocks: int
    positive_fraction: float
    max_gain: float
    max_gain_db: float
    projection_gain: float
    projection_gain_db: float
    fixed_coverages: tuple[float, ...]


@dataclass(frozen=True)
class BedBarycenter:
    title: str
    extension: str
    x: float
    y: float
    weight_sum: float
    maximum_coverage: float
    dominant_bed: str


@dataclass(frozen=True)
class TemporalBarycenters:
    title: str
    video: VideoInfo
    fps: float
    encoding_fps: float
    times: np.ndarray
    positions: np.ndarray
    coverages: np.ndarray
    active_blocks: np.ndarray
    activity: np.ndarray
    valid: np.ndarray


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "inputs",
        type=Path,
        nargs="+",
        help="CAF files or directories searched for *.dtsx.audio",
    )
    parser.add_argument(
        "--output",
        type=Path,
        help="optional long-form TSV result",
    )
    parser.add_argument(
        "--plot-dir",
        type=Path,
        help="optional directory for one fixed-gain coverage PNG per stream",
    )
    parser.add_argument(
        "--plot-gain",
        type=float,
        default=0.5,
        help="plotted subtraction gain: 0.5, 0.707107 or 1.0",
    )
    parser.add_argument(
        "--position-plot-dir",
        type=Path,
        help=(
            "optional directory for coverage-weighted bed barycentre "
            "PNGs and positions.tsv"
        ),
    )
    parser.add_argument(
        "--animation-dir",
        type=Path,
        help="optional directory for real-time temporal-position MP4s",
    )
    parser.add_argument(
        "--temporal-coverage-output",
        type=Path,
        help=(
            "optional long-form TSV containing every temporal X-to-bed "
            "fixed-gain coverage"
        ),
    )
    parser.add_argument(
        "--video-dir",
        type=Path,
        help="original MKV directory used for animation PTS and duration",
    )
    parser.add_argument(
        "--animation-fps",
        type=float,
        default=5.0,
        help="target animation frame rate (default: 5)",
    )
    parser.add_argument(
        "--temporal-window-seconds",
        type=float,
        default=1.0,
        help="sliding coverage window duration (default: 1 second)",
    )
    parser.add_argument(
        "--trail-seconds",
        type=float,
        default=3.0,
        help="visible temporal-position trail (default: 3 seconds)",
    )
    parser.add_argument(
        "--minimum-active-blocks",
        type=int,
        default=3,
        help="minimum active X blocks needed for a temporal position",
    )
    parser.add_argument(
        "--block-samples",
        type=int,
        default=512,
        help="non-overlapping energy block size (default: 512)",
    )
    parser.add_argument(
        "--coverage",
        type=float,
        default=0.95,
        help="required active-block energy-decrease fraction",
    )
    parser.add_argument(
        "--activity-db",
        type=float,
        default=-60.0,
        help="X block-energy gate relative to that source's peak",
    )
    parser.add_argument(
        "--extensions",
        type=int,
        default=8,
        help="required extension-source count (default: D3 4+4 = 8)",
    )
    return parser.parse_args()


def discover(inputs: list[Path]) -> list[Path]:
    paths: set[Path] = set()
    for item in inputs:
        if item.is_dir():
            paths.update(item.glob("*.dtsx.audio"))
        elif item.is_file():
            paths.add(item)
        else:
            raise FileNotFoundError(item)
    return sorted(paths)


def probe(path: Path) -> AudioInfo:
    process = subprocess.run(
        [
            "ffprobe",
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=channels,sample_rate,nb_frames",
            "-of",
            "json",
            str(path),
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    streams = json.loads(process.stdout).get("streams", [])
    if len(streams) != 1:
        raise ValueError(f"{path}: expected one audio stream")
    stream = streams[0]
    return AudioInfo(
        channels=int(stream["channels"]),
        sample_rate=int(stream["sample_rate"]),
        frames=int(stream["nb_frames"]),
    )


def probe_video(directory: Path, title: str) -> VideoInfo:
    candidates = sorted(directory.glob(f"{title} - DTS-X*.mkv"))
    if len(candidates) != 1:
        raise ValueError(
            f"{title}: expected one original MKV in {directory}, "
            f"found {candidates}"
        )
    path = candidates[0]
    process = subprocess.run(
        [
            "ffprobe",
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=start_time:format=duration",
            "-of",
            "json",
            str(path),
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    data = json.loads(process.stdout)
    streams = data.get("streams", [])
    if len(streams) != 1:
        raise ValueError(f"{path}: expected one selected audio stream")
    duration = float(data["format"]["duration"])
    audio_start = float(streams[0].get("start_time", 0.0))
    if duration <= 0.0 or audio_start < 0.0:
        raise ValueError(
            f"{path}: invalid duration/start {duration}/{audio_start}"
        )
    return VideoInfo(
        path=path,
        duration=duration,
        audio_start=audio_start,
    )


def append_block_statistics(
    samples: np.ndarray,
    bed_energy: list[np.ndarray],
    extension_energy: list[np.ndarray],
    cross_energy: list[np.ndarray],
) -> None:
    bed = samples[:, :, : len(BED_NAMES)].astype(np.float64)
    extension = samples[:, :, len(BED_NAMES) :].astype(np.float64)
    bed_energy.append(np.einsum("nsi,nsi->ni", bed, bed))
    extension_energy.append(
        np.einsum("nsj,nsj->nj", extension, extension)
    )
    cross_energy.append(np.einsum("nsi,nsj->nij", bed, extension))


def read_block_statistics(
    path: Path,
    channels: int,
    block_samples: int,
) -> BlockStatistics:
    process = subprocess.Popen(
        [
            "ffmpeg",
            "-v",
            "error",
            "-i",
            str(path),
            "-map",
            "0:a:0",
            "-c:a",
            "pcm_f32le",
            "-f",
            "f32le",
            "-",
        ],
        stdout=subprocess.PIPE,
    )
    assert process.stdout is not None
    block_bytes = block_samples * channels * 4
    pending = bytearray()
    bed_energy: list[np.ndarray] = []
    extension_energy: list[np.ndarray] = []
    cross_energy: list[np.ndarray] = []
    while chunk := process.stdout.read(1024 * 1024):
        pending.extend(chunk)
        blocks = len(pending) // block_bytes
        if blocks == 0:
            continue
        byte_count = blocks * block_bytes
        samples = np.frombuffer(
            memoryview(pending)[:byte_count], dtype="<f4"
        ).copy()
        samples = samples.reshape(blocks, block_samples, channels)
        append_block_statistics(
            samples, bed_energy, extension_energy, cross_energy
        )
        del pending[:byte_count]
    return_code = process.wait()
    if return_code:
        raise RuntimeError(f"ffmpeg failed for {path}: {return_code}")
    sample_bytes = channels * 4
    tail_samples = len(pending) // sample_bytes
    if tail_samples:
        samples = np.frombuffer(
            memoryview(pending)[: tail_samples * sample_bytes],
            dtype="<f4",
        ).copy()
        samples = samples.reshape(1, tail_samples, channels)
        append_block_statistics(
            samples, bed_energy, extension_energy, cross_energy
        )
    return BlockStatistics(
        bed_energy=np.concatenate(bed_energy),
        extension_energy=np.concatenate(extension_energy),
        cross_energy=np.concatenate(cross_energy),
    )


def maximum_gain(
    thresholds: np.ndarray, coverage: float
) -> tuple[float, float]:
    positive_fraction = float(np.mean(thresholds > 0.0))
    required = math.ceil(coverage * len(thresholds))
    index = len(thresholds) - required
    boundary = float(np.partition(thresholds, index)[index])
    if boundary <= 0.0:
        return 0.0, positive_fraction
    # The mathematical maximum is an open boundary for strict energy
    # decrease. Return the closest representable value below it.
    return float(np.nextafter(boundary, 0.0)), positive_fraction


def analyze_file(
    path: Path,
    info: AudioInfo,
    block_samples: int,
    coverage: float,
    activity_db: float,
    statistics: BlockStatistics | None = None,
) -> list[FoldLimit]:
    if statistics is None:
        statistics = read_block_statistics(
            path, info.channels, block_samples
        )
    extension_energy = statistics.extension_energy
    cross_energy = statistics.cross_energy
    extension_count = info.channels - len(BED_NAMES)
    title = path.name.removesuffix(".dtsx.audio")
    results: list[FoldLimit] = []
    activity_ratio = 10.0 ** (activity_db / 10.0)
    for extension_index in range(extension_count):
        x_energy = extension_energy[:, extension_index]
        peak = float(np.max(x_energy))
        active = (x_energy > 0.0) & (x_energy >= peak * activity_ratio)
        active_count = int(np.count_nonzero(active))
        if active_count == 0:
            continue
        selected_x_energy = x_energy[active]
        for bed_index, bed_name in enumerate(BED_NAMES):
            thresholds = (
                2.0
                * cross_energy[active, bed_index, extension_index]
                / selected_x_energy
            )
            gain, positive_fraction = maximum_gain(
                thresholds, coverage
            )
            gain_db = (
                20.0 * math.log10(gain)
                if gain > 0.0
                else float("-inf")
            )
            projection_gain = gain / 2.0
            projection_gain_db = (
                20.0 * math.log10(projection_gain)
                if projection_gain > 0.0
                else float("-inf")
            )
            fixed_coverages = tuple(
                float(np.mean(thresholds > fixed_gain))
                for fixed_gain in FIXED_GAINS
            )
            results.append(
                FoldLimit(
                    title=title,
                    path=path,
                    bed=bed_name,
                    extension=f"X{extension_index}",
                    blocks=len(x_energy),
                    active_blocks=active_count,
                    positive_fraction=positive_fraction,
                    max_gain=gain,
                    max_gain_db=gain_db,
                    projection_gain=projection_gain,
                    projection_gain_db=projection_gain_db,
                    fixed_coverages=fixed_coverages,
                )
            )
    return results


def format_gain(gain: float, gain_db: float) -> str:
    if gain <= 0.0:
        return "-"
    return f"{gain:.4f} ({gain_db:+.2f} dB)"


def print_file_summary(
    title: str, results: list[FoldLimit]
) -> None:
    print(f"file\t{title}")
    extensions = sorted({result.extension for result in results})
    for extension in extensions:
        candidates = [
            result
            for result in results
            if result.extension == extension and result.max_gain > 0.0
        ]
        if not candidates:
            print(f"  {extension}: no bed reaches requested coverage")
            continue
        candidates.sort(key=lambda result: result.max_gain, reverse=True)
        text = ", ".join(
            f"{item.bed} limit="
            f"{format_gain(item.max_gain, item.max_gain_db)} "
            f"p05_beta="
            f"{format_gain(item.projection_gain, item.projection_gain_db)}"
            for item in candidates[:3]
        )
        print(f"  {extension}: {text}")


def print_corpus_summary(
    results: list[FoldLimit], title_count: int, coverage: float
) -> None:
    print("corpus_consensus:")
    extensions = sorted({result.extension for result in results})
    for extension in extensions:
        candidates = []
        for bed in BED_NAMES:
            selected = [
                result
                for result in results
                if result.extension == extension
                and result.bed == bed
                and result.max_gain > 0.0
            ]
            if not selected:
                continue
            gains = np.array(
                [result.max_gain for result in selected], dtype=np.float64
            )
            at_minus_3 = sum(
                result.fixed_coverages[1] >= coverage
                for result in selected
            )
            at_minus_6 = sum(
                result.fixed_coverages[0] >= coverage
                for result in selected
            )
            candidates.append(
                (
                    len(selected),
                    float(np.median(gains)),
                    bed,
                    float(np.min(gains)),
                    at_minus_3,
                    at_minus_6,
                )
            )
        candidates.sort(reverse=True)
        if not candidates:
            print(f"  {extension}: no positive candidate")
            continue
        text = "; ".join(
            f"{bed} valid={valid}/{title_count} "
            f"median_limit={median:.4f} "
            f"median_p05_beta={median / 2.0:.4f} "
            f"min_limit={minimum:.4f} "
            f"-3dB={at_minus_3}/{title_count} "
            f"-6dB={at_minus_6}/{title_count}"
            for valid, median, bed, minimum, at_minus_3, at_minus_6
            in candidates[:3]
        )
        print(f"  {extension}: {text}")


def write_tsv(path: Path, results: list[FoldLimit]) -> None:
    fields = [
        "title",
        "path",
        "bed",
        "extension",
        "blocks",
        "active_blocks",
        "positive_fraction",
        "max_gain",
        "max_gain_db",
        "p05_projection_gain",
        "p05_projection_gain_db",
        "coverage_at_0.5",
        "coverage_at_0.707107",
        "coverage_at_1.0",
    ]
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", newline="") as output:
        writer = csv.DictWriter(
            output, fieldnames=fields, dialect="excel-tab"
        )
        writer.writeheader()
        for result in results:
            writer.writerow(
                {
                    "title": result.title,
                    "path": result.path,
                    "bed": result.bed,
                    "extension": result.extension,
                    "blocks": result.blocks,
                    "active_blocks": result.active_blocks,
                    "positive_fraction": (
                        f"{result.positive_fraction:.6f}"
                    ),
                    "max_gain": f"{result.max_gain:.9g}",
                    "max_gain_db": (
                        f"{result.max_gain_db:.6f}"
                        if math.isfinite(result.max_gain_db)
                        else "-inf"
                    ),
                    "p05_projection_gain": (
                        f"{result.projection_gain:.9g}"
                    ),
                    "p05_projection_gain_db": (
                        f"{result.projection_gain_db:.6f}"
                        if math.isfinite(result.projection_gain_db)
                        else "-inf"
                    ),
                    "coverage_at_0.5": (
                        f"{result.fixed_coverages[0]:.6f}"
                    ),
                    "coverage_at_0.707107": (
                        f"{result.fixed_coverages[1]:.6f}"
                    ),
                    "coverage_at_1.0": (
                        f"{result.fixed_coverages[2]:.6f}"
                    ),
                }
            )


def write_coverage_plots(
    directory: Path,
    results: list[FoldLimit],
    required_coverage: float,
    block_samples: int,
    activity_db: float,
    plot_gain: float,
    fixed_gain_index: int,
) -> None:
    import matplotlib

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    from matplotlib.patches import Rectangle
    from matplotlib.ticker import PercentFormatter

    directory.mkdir(parents=True, exist_ok=True)
    gain_label = (
        f"{plot_gain:.1f}"
        if plot_gain.is_integer()
        else f"{plot_gain:.6f}".rstrip("0")
    )
    gain_db = 20.0 * math.log10(plot_gain)
    titles = sorted({result.title for result in results})
    for title in titles:
        selected = [
            result for result in results if result.title == title
        ]
        extensions = sorted(
            {result.extension for result in selected},
            key=lambda name: int(name.removeprefix("X")),
        )
        matrix = np.full((len(extensions), len(BED_NAMES)), np.nan)
        for result in selected:
            row = extensions.index(result.extension)
            column = BED_NAMES.index(result.bed)
            matrix[row, column] = result.fixed_coverages[
                fixed_gain_index
            ]

        figure, axis = plt.subplots(figsize=(10.0, 8.0))
        image = axis.imshow(
            matrix,
            vmin=0.0,
            vmax=1.0,
            cmap="viridis",
            interpolation="nearest",
            aspect="auto",
        )
        axis.set_xticks(range(len(BED_NAMES)), BED_NAMES)
        axis.set_yticks(range(len(extensions)), extensions)
        axis.set_xlabel("Compatible bed channel")
        axis.set_ylabel("Extension source")
        axis.set_title(
            f"{title}\n"
            "Energy-decrease coverage for bed - "
            f"{gain_label} X ({gain_db:+.2f} dB)"
        )
        axis.set_xticks(
            np.arange(-0.5, len(BED_NAMES), 1.0), minor=True
        )
        axis.set_yticks(
            np.arange(-0.5, len(extensions), 1.0), minor=True
        )
        axis.grid(which="minor", color="white", linewidth=0.6, alpha=0.5)
        axis.tick_params(which="minor", bottom=False, left=False)

        for row in range(len(extensions)):
            for column in range(len(BED_NAMES)):
                value = matrix[row, column]
                if not np.isfinite(value):
                    continue
                text_color = "white" if value < 0.55 else "black"
                axis.text(
                    column,
                    row,
                    f"{value:.1%}",
                    ha="center",
                    va="center",
                    color=text_color,
                    fontsize=9,
                )
                if value >= required_coverage:
                    axis.add_patch(
                        Rectangle(
                            (column - 0.48, row - 0.48),
                            0.96,
                            0.96,
                            fill=False,
                            edgecolor="#ff2d55",
                            linewidth=2.5,
                        )
                    )

        colorbar = figure.colorbar(image, ax=axis, pad=0.025)
        colorbar.set_label("Active-block coverage")
        colorbar.ax.yaxis.set_major_formatter(PercentFormatter(1.0))
        figure.text(
            0.5,
            0.015,
            f"{block_samples}-sample blocks; X activity gate "
            f"{activity_db:g} dB; red outline >= "
            f"{required_coverage:.0%}",
            ha="center",
            fontsize=9,
        )
        figure.tight_layout(rect=(0.0, 0.035, 1.0, 1.0))
        safe_title = re.sub(r"[^A-Za-z0-9_.-]+", "_", title)
        output = (
            directory
            / f"{safe_title}.coverage-{gain_label}.png"
        )
        figure.savefig(output, dpi=160, facecolor="white")
        plt.close(figure)
        print(f"plot\t{output}")


def calculate_bed_barycenters(
    results: list[FoldLimit],
    fixed_gain_index: int,
) -> list[BedBarycenter]:
    barycenters = []
    titles = sorted({result.title for result in results})
    for title in titles:
        extensions = sorted(
            {
                result.extension
                for result in results
                if result.title == title
            },
            key=lambda name: int(name.removeprefix("X")),
        )
        for extension in extensions:
            selected = [
                result
                for result in results
                if result.title == title
                and result.extension == extension
                and result.bed in BED_COORDINATES
            ]
            weights = np.array(
                [
                    result.fixed_coverages[fixed_gain_index]
                    for result in selected
                ],
                dtype=np.float64,
            )
            weight_sum = float(np.sum(weights))
            if weight_sum <= 0.0:
                continue
            coordinates = np.array(
                [BED_COORDINATES[result.bed] for result in selected],
                dtype=np.float64,
            )
            position = np.sum(
                coordinates * weights[:, None], axis=0
            ) / weight_sum
            dominant = selected[int(np.argmax(weights))]
            barycenters.append(
                BedBarycenter(
                    title=title,
                    extension=extension,
                    x=float(position[0]),
                    y=float(position[1]),
                    weight_sum=weight_sum,
                    maximum_coverage=float(np.max(weights)),
                    dominant_bed=dominant.bed,
                )
            )
    return barycenters


def write_barycenter_tsv(
    path: Path, barycenters: list[BedBarycenter]
) -> None:
    fields = [
        "title",
        "extension",
        "x",
        "y",
        "spatial_weight_sum",
        "maximum_coverage",
        "dominant_bed",
    ]
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", newline="") as output:
        writer = csv.DictWriter(
            output, fieldnames=fields, dialect="excel-tab"
        )
        writer.writeheader()
        for item in barycenters:
            writer.writerow(
                {
                    "title": item.title,
                    "extension": item.extension,
                    "x": f"{item.x:.9f}",
                    "y": f"{item.y:.9f}",
                    "spatial_weight_sum": f"{item.weight_sum:.9f}",
                    "maximum_coverage": (
                        f"{item.maximum_coverage:.9f}"
                    ),
                    "dominant_bed": item.dominant_bed,
                }
            )


def write_barycenter_plots(
    directory: Path,
    barycenters: list[BedBarycenter],
    plot_gain: float,
) -> None:
    import matplotlib

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    from matplotlib.patches import Polygon

    directory.mkdir(parents=True, exist_ok=True)
    write_barycenter_tsv(directory / "positions.tsv", barycenters)
    gain_label = (
        f"{plot_gain:.1f}"
        if plot_gain.is_integer()
        else f"{plot_gain:.6f}".rstrip("0")
    )
    colors = plt.get_cmap("tab10").colors
    label_offsets = (
        (-24, 14),
        (24, 14),
        (-24, -16),
        (24, -16),
        (-24, 14),
        (24, 14),
        (-24, -16),
        (24, -16),
    )
    titles = sorted({item.title for item in barycenters})
    for title in titles:
        selected = [
            item for item in barycenters if item.title == title
        ]
        figure, axis = plt.subplots(figsize=(9.0, 9.0))
        axis.add_patch(
            Polygon(
                [(-1.0, 1.0), (1.0, 1.0), (1.0, -1.0), (-1.0, -1.0)],
                closed=True,
                fill=False,
                edgecolor="#666666",
                linewidth=1.5,
            )
        )
        for bed, (x, y) in BED_COORDINATES.items():
            axis.scatter(
                x,
                y,
                marker="s",
                s=115,
                color="#222222",
                edgecolor="white",
                linewidth=1.0,
                zorder=4,
            )
            horizontal = "right" if x < 0 else "left" if x > 0 else "center"
            x_offset = -0.04 if x < 0 else 0.04 if x > 0 else 0.0
            y_offset = 0.075 if y >= 0 else -0.09
            axis.text(
                x + x_offset,
                y + y_offset,
                bed,
                ha=horizontal,
                va="center",
                fontsize=10,
                fontweight="bold",
                color="#222222",
            )
        axis.scatter(
            0.0,
            0.0,
            marker="+",
            s=90,
            color="#777777",
            linewidth=1.5,
            zorder=2,
        )
        for item in selected:
            index = int(item.extension.removeprefix("X"))
            color = colors[index % len(colors)]
            size = 90.0 + 150.0 * item.maximum_coverage
            axis.scatter(
                item.x,
                item.y,
                s=size,
                color=color,
                edgecolor="white",
                linewidth=1.4,
                zorder=5,
            )
            axis.annotate(
                item.extension,
                xy=(item.x, item.y),
                xytext=label_offsets[index],
                textcoords="offset points",
                ha="center",
                va="center",
                fontsize=10,
                fontweight="bold",
                color=color,
                bbox={
                    "boxstyle": "round,pad=0.18",
                    "facecolor": "white",
                    "edgecolor": color,
                    "alpha": 0.9,
                },
                arrowprops={
                    "arrowstyle": "-",
                    "color": color,
                    "linewidth": 0.9,
                    "alpha": 0.8,
                },
                zorder=6,
            )
        axis.set_xlim(-1.18, 1.18)
        axis.set_ylim(-1.18, 1.18)
        axis.set_aspect("equal", adjustable="box")
        axis.set_xlabel("x (left - / right +)")
        axis.set_ylabel("y (rear - / front +)")
        axis.set_title(
            f"{title}\n"
            f"Bed-coverage barycentres at g = {gain_label}"
        )
        axis.grid(True, color="#dddddd", linewidth=0.7)
        axis.axhline(0.0, color="#bbbbbb", linewidth=0.8, zorder=1)
        axis.axvline(0.0, color="#bbbbbb", linewidth=0.8, zorder=1)
        figure.text(
            0.5,
            0.025,
            "Weights: energy-decrease coverage; LFE excluded "
            "(no spatial coordinate). Marker size: maximum bed coverage.",
            ha="center",
            fontsize=9,
        )
        figure.tight_layout(rect=(0.0, 0.045, 1.0, 1.0))
        safe_title = re.sub(r"[^A-Za-z0-9_.-]+", "_", title)
        output = (
            directory
            / f"{safe_title}.position-{gain_label}.png"
        )
        figure.savefig(output, dpi=160, facecolor="white")
        plt.close(figure)
        print(f"position_plot\t{output}")
    print(f"positions\t{directory / 'positions.tsv'}")


def calculate_temporal_barycenters(
    title: str,
    video: VideoInfo,
    info: AudioInfo,
    statistics: BlockStatistics,
    block_samples: int,
    activity_db: float,
    plot_gain: float,
    target_fps: float,
    window_seconds: float,
    minimum_active_blocks: int,
) -> TemporalBarycenters:
    extension_energy = statistics.extension_energy
    cross_energy = statistics.cross_energy
    blocks, extension_count = extension_energy.shape
    block_centres = (
        np.arange(blocks, dtype=np.float64) + 0.5
    ) * block_samples / info.sample_rate
    activity_ratio = 10.0 ** (activity_db / 10.0)
    peaks = np.max(extension_energy, axis=0)
    active = (
        (extension_energy > 0.0)
        & (extension_energy >= peaks[None, :] * activity_ratio)
    )
    delta = (
        plot_gain * plot_gain * extension_energy[:, None, :]
        - 2.0 * plot_gain * cross_energy
    )
    passed = (delta < 0.0) & active[:, None, :]
    cumulative_active = np.concatenate(
        (
            np.zeros((1, extension_count), dtype=np.int64),
            np.cumsum(active, axis=0, dtype=np.int64),
        ),
        axis=0,
    )
    cumulative_passed = np.concatenate(
        (
            np.zeros(
                (1, len(BED_NAMES), extension_count),
                dtype=np.int64,
            ),
            np.cumsum(passed, axis=0, dtype=np.int64),
        ),
        axis=0,
    )
    cumulative_energy = np.concatenate(
        (
            np.zeros((1, extension_count), dtype=np.float64),
            np.cumsum(extension_energy, axis=0, dtype=np.float64),
        ),
        axis=0,
    )

    frame_count = max(1, round(video.duration * target_fps))
    effective_fps = frame_count / video.duration
    times = np.arange(frame_count, dtype=np.float64) / effective_fps
    positions = np.full(
        (frame_count, extension_count, 2), np.nan, dtype=np.float64
    )
    coverages = np.full(
        (frame_count, len(BED_NAMES), extension_count),
        np.nan,
        dtype=np.float64,
    )
    active_blocks = np.zeros(
        (frame_count, extension_count), dtype=np.int64
    )
    window_energy = np.zeros(
        (frame_count, extension_count), dtype=np.float64
    )
    valid = np.zeros(
        (frame_count, extension_count), dtype=np.bool_
    )
    spatial_beds = [
        BED_NAMES.index(name) for name in BED_COORDINATES
    ]
    coordinates = np.array(
        list(BED_COORDINATES.values()), dtype=np.float64
    )
    audio_duration = info.frames / info.sample_rate
    half_window = window_seconds / 2.0
    for frame, video_time in enumerate(times):
        audio_time = video_time - video.audio_start
        if audio_time < 0.0 or audio_time >= audio_duration:
            continue
        start = int(
            np.searchsorted(
                block_centres, audio_time - half_window, side="left"
            )
        )
        end = int(
            np.searchsorted(
                block_centres, audio_time + half_window, side="right"
            )
        )
        if end <= start:
            continue
        active_count = (
            cumulative_active[end] - cumulative_active[start]
        )
        active_blocks[frame] = active_count
        pass_count = (
            cumulative_passed[end] - cumulative_passed[start]
        )
        energy = (
            cumulative_energy[end] - cumulative_energy[start]
        )
        window_energy[frame] = energy
        for extension in range(extension_count):
            if active_count[extension] < minimum_active_blocks:
                continue
            coverage = (
                pass_count[:, extension] / active_count[extension]
            )
            coverages[frame, :, extension] = coverage
            weights = coverage[spatial_beds]
            weight_sum = float(np.sum(weights))
            if weight_sum <= 0.0:
                continue
            positions[frame, extension] = (
                np.sum(coordinates * weights[:, None], axis=0)
                / weight_sum
            )
            valid[frame, extension] = True

    activity = np.zeros_like(window_energy)
    for extension in range(extension_count):
        selected = window_energy[
            window_energy[:, extension] > 0.0, extension
        ]
        if selected.size == 0:
            continue
        scale = float(np.percentile(selected, 95.0))
        if scale > 0.0:
            activity[:, extension] = np.clip(
                window_energy[:, extension] / scale, 0.0, 1.0
            )
    activity[~valid] = 0.0
    return TemporalBarycenters(
        title=title,
        video=video,
        fps=effective_fps,
        encoding_fps=target_fps,
        times=times,
        positions=positions,
        coverages=coverages,
        active_blocks=active_blocks,
        activity=activity,
        valid=valid,
    )


def format_clock(seconds: float) -> str:
    milliseconds = round(seconds * 1000.0)
    hours, remainder = divmod(milliseconds, 3_600_000)
    minutes, remainder = divmod(remainder, 60_000)
    seconds, milliseconds = divmod(remainder, 1000)
    return (
        f"{hours:02d}:{minutes:02d}:{seconds:02d}."
        f"{milliseconds:03d}"
    )


def write_temporal_positions(
    path: Path, tracks: list[TemporalBarycenters]
) -> None:
    fields = [
        "title",
        "video_time",
        "audio_time",
        "extension",
        "x",
        "y",
        "activity",
        "valid",
    ]
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", newline="") as output:
        writer = csv.DictWriter(
            output, fieldnames=fields, dialect="excel-tab"
        )
        writer.writeheader()
        for track in tracks:
            extension_count = track.positions.shape[1]
            for frame, video_time in enumerate(track.times):
                audio_time = video_time - track.video.audio_start
                for extension in range(extension_count):
                    is_valid = bool(track.valid[frame, extension])
                    writer.writerow(
                        {
                            "title": track.title,
                            "video_time": f"{video_time:.9f}",
                            "audio_time": f"{audio_time:.9f}",
                            "extension": f"X{extension}",
                            "x": (
                                f"{track.positions[frame, extension, 0]:.9f}"
                                if is_valid
                                else ""
                            ),
                            "y": (
                                f"{track.positions[frame, extension, 1]:.9f}"
                                if is_valid
                                else ""
                            ),
                            "activity": (
                                f"{track.activity[frame, extension]:.9f}"
                            ),
                            "valid": int(is_valid),
                        }
                    )


def write_temporal_coverages(
    path: Path, tracks: list[TemporalBarycenters]
) -> None:
    fields = [
        "title",
        "video_time",
        "audio_time",
        "extension",
        "bed",
        "coverage",
        "active_blocks",
        "valid",
    ]
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", newline="") as output:
        writer = csv.DictWriter(
            output, fieldnames=fields, dialect="excel-tab"
        )
        writer.writeheader()
        for track in tracks:
            extension_count = track.coverages.shape[2]
            for frame, video_time in enumerate(track.times):
                audio_time = video_time - track.video.audio_start
                for extension in range(extension_count):
                    active_blocks = int(
                        track.active_blocks[frame, extension]
                    )
                    for bed_index, bed in enumerate(BED_NAMES):
                        coverage = track.coverages[
                            frame, bed_index, extension
                        ]
                        is_valid = bool(np.isfinite(coverage))
                        writer.writerow(
                            {
                                "title": track.title,
                                "video_time": f"{video_time:.9f}",
                                "audio_time": f"{audio_time:.9f}",
                                "extension": f"X{extension}",
                                "bed": bed,
                                "coverage": (
                                    f"{coverage:.9f}"
                                    if is_valid
                                    else ""
                                ),
                                "active_blocks": active_blocks,
                                "valid": int(is_valid),
                            }
                        )


def write_barycenter_animations(
    directory: Path,
    tracks: list[TemporalBarycenters],
    plot_gain: float,
    window_seconds: float,
    trail_seconds: float,
) -> None:
    import matplotlib

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    from matplotlib.animation import FFMpegWriter
    from matplotlib.patches import Polygon

    directory.mkdir(parents=True, exist_ok=True)
    write_temporal_positions(
        directory / "temporal-positions.tsv", tracks
    )
    write_temporal_coverages(
        directory / "temporal-coverages.tsv", tracks
    )
    gain_label = (
        f"{plot_gain:.1f}"
        if plot_gain.is_integer()
        else f"{plot_gain:.6f}".rstrip("0")
    )
    colors = plt.get_cmap("tab10").colors
    for track in tracks:
        figure, axis = plt.subplots(figsize=(6.0, 6.0))
        axis.add_patch(
            Polygon(
                [(-1.0, 1.0), (1.0, 1.0), (1.0, -1.0), (-1.0, -1.0)],
                closed=True,
                fill=False,
                edgecolor="#666666",
                linewidth=1.3,
            )
        )
        for bed, (x, y) in BED_COORDINATES.items():
            axis.scatter(
                x,
                y,
                marker="s",
                s=65,
                color="#222222",
                edgecolor="white",
                linewidth=0.8,
                zorder=4,
            )
            horizontal = (
                "right" if x < 0 else "left" if x > 0 else "center"
            )
            x_offset = -0.04 if x < 0 else 0.04 if x > 0 else 0.0
            y_offset = 0.075 if y >= 0 else -0.09
            axis.text(
                x + x_offset,
                y + y_offset,
                bed,
                ha=horizontal,
                va="center",
                fontsize=8,
                fontweight="bold",
                color="#222222",
            )
        axis.scatter(
            0.0,
            0.0,
            marker="+",
            s=55,
            color="#777777",
            linewidth=1.2,
            zorder=2,
        )
        axis.set_xlim(-1.18, 1.18)
        axis.set_ylim(-1.18, 1.18)
        axis.set_aspect("equal", adjustable="box")
        axis.set_xlabel("x (left - / right +)")
        axis.set_ylabel("y (rear - / front +)")
        axis.set_title(
            f"{track.title}\n"
            f"Temporal bed-coverage barycentres at g = {gain_label}"
        )
        axis.grid(True, color="#dddddd", linewidth=0.6)
        axis.axhline(0.0, color="#bbbbbb", linewidth=0.7, zorder=1)
        axis.axvline(0.0, color="#bbbbbb", linewidth=0.7, zorder=1)
        time_text = axis.text(
            0.02,
            0.98,
            "",
            transform=axis.transAxes,
            ha="left",
            va="top",
            fontsize=9,
            family="monospace",
            bbox={
                "boxstyle": "round,pad=0.25",
                "facecolor": "white",
                "edgecolor": "#aaaaaa",
                "alpha": 0.9,
            },
            zorder=10,
        )
        status_text = axis.text(
            0.5,
            0.04,
            "",
            transform=axis.transAxes,
            ha="center",
            va="bottom",
            fontsize=8,
            color="#666666",
            zorder=10,
        )
        figure.text(
            0.5,
            0.015,
            f"{window_seconds:g} s coverage window; LFE excluded; "
            "marker size/opacity = X activity.",
            ha="center",
            fontsize=8,
        )
        figure.tight_layout(rect=(0.0, 0.035, 1.0, 1.0))

        extension_count = track.positions.shape[1]
        points = []
        labels = []
        trails = []
        for extension in range(extension_count):
            color = colors[extension % len(colors)]
            trail, = axis.plot(
                [],
                [],
                color=color,
                linewidth=1.2,
                alpha=0.35,
                zorder=3,
            )
            point, = axis.plot(
                [],
                [],
                marker="o",
                linestyle="none",
                color=color,
                markeredgecolor="white",
                markeredgewidth=1.0,
                zorder=5,
            )
            label = axis.text(
                0.0,
                0.0,
                f"X{extension}",
                fontsize=8,
                fontweight="bold",
                color=color,
                zorder=6,
            )
            trails.append(trail)
            points.append(point)
            labels.append(label)

        safe_title = re.sub(
            r"[^A-Za-z0-9_.-]+", "_", track.title
        )
        output = (
            directory
            / f"{safe_title}.positions-{gain_label}.mp4"
        )
        temporary_output = (
            directory
            / f".{safe_title}.positions-{gain_label}.unscaled.mp4"
        )
        writer = FFMpegWriter(
            fps=track.encoding_fps,
            codec="libx264",
            metadata={
                "title": track.title,
                "comment": (
                    "Coverage-weighted research visualization; "
                    "not decoded object metadata"
                ),
            },
            extra_args=[
                "-pix_fmt",
                "yuv420p",
                "-crf",
                "22",
                "-movflags",
                "+faststart",
            ],
        )
        trail_frames = max(1, round(trail_seconds * track.fps))
        print(
            f"animation_start\t{track.title}\t"
            f"frames={len(track.times)} fps={track.encoding_fps:.6f} "
            f"duration={track.video.duration:.6f}"
        )
        with writer.saving(figure, str(temporary_output), dpi=120):
            for frame, video_time in enumerate(track.times):
                any_valid = False
                trail_start = max(0, frame - trail_frames + 1)
                for extension in range(extension_count):
                    is_valid = bool(track.valid[frame, extension])
                    if not is_valid:
                        points[extension].set_data([], [])
                        labels[extension].set_visible(False)
                    else:
                        any_valid = True
                        x, y = track.positions[frame, extension]
                        level = math.sqrt(
                            float(track.activity[frame, extension])
                        )
                        points[extension].set_data([x], [y])
                        points[extension].set_markersize(
                            4.0 + 9.0 * level
                        )
                        points[extension].set_alpha(
                            0.25 + 0.75 * level
                        )
                        side = -1.0 if extension % 2 == 0 else 1.0
                        vertical = (
                            0.035
                            if extension % 4 < 2
                            else -0.045
                        )
                        labels[extension].set_position(
                            (x + side * 0.035, y + vertical)
                        )
                        labels[extension].set_ha(
                            "right" if side < 0.0 else "left"
                        )
                        labels[extension].set_visible(True)
                    history = track.positions[
                        trail_start : frame + 1, extension
                    ].copy()
                    history_valid = track.valid[
                        trail_start : frame + 1, extension
                    ]
                    history[~history_valid] = np.nan
                    trails[extension].set_data(
                        history[:, 0], history[:, 1]
                    )
                time_text.set_text(
                    f"{format_clock(video_time)} / "
                    f"{format_clock(track.video.duration)}"
                )
                if any_valid:
                    status_text.set_text("")
                else:
                    status_text.set_text(
                        "No active decoded DTS:X window"
                    )
                writer.grab_frame()
        plt.close(figure)
        encoded_duration = len(track.times) / track.encoding_fps
        timestamp_scale = track.video.duration / encoded_duration
        subprocess.run(
            [
                "ffmpeg",
                "-hide_banner",
                "-loglevel",
                "error",
                "-itsscale",
                f"{timestamp_scale:.12f}",
                "-i",
                str(temporary_output),
                "-map",
                "0:v:0",
                "-c",
                "copy",
                "-movflags",
                "+faststart",
                "-y",
                str(output),
            ],
            check=True,
        )
        temporary_output.unlink()
        print(f"animation\t{output}")
    print(
        f"temporal_positions\t"
        f"{directory / 'temporal-positions.tsv'}"
    )


def main() -> None:
    args = parse_args()
    if args.block_samples <= 0:
        raise SystemExit("--block-samples must be positive")
    if not 0.0 < args.coverage <= 1.0:
        raise SystemExit("--coverage must be in (0, 1]")
    if args.extensions <= 0:
        raise SystemExit("--extensions must be positive")
    if args.activity_db > 0.0:
        raise SystemExit("--activity-db must be at most 0 dB")
    if args.animation_fps <= 0.0:
        raise SystemExit("--animation-fps must be positive")
    if args.temporal_window_seconds <= 0.0:
        raise SystemExit("--temporal-window-seconds must be positive")
    if args.trail_seconds < 0.0:
        raise SystemExit("--trail-seconds must be non-negative")
    if args.minimum_active_blocks <= 0:
        raise SystemExit("--minimum-active-blocks must be positive")
    needs_temporal = bool(
        args.animation_dir or args.temporal_coverage_output
    )
    if needs_temporal and args.video_dir is None:
        raise SystemExit(
            "--video-dir is required with temporal coverage or animation"
        )
    gain_distances = [
        abs(fixed_gain - args.plot_gain)
        for fixed_gain in FIXED_GAINS
    ]
    fixed_gain_index = int(np.argmin(gain_distances))
    if gain_distances[fixed_gain_index] > 5e-6:
        raise SystemExit(
            "--plot-gain must be 0.5, 0.707107 or 1.0"
        )
    plot_gain = float(FIXED_GAINS[fixed_gain_index])
    paths = discover(args.inputs)
    all_results: list[FoldLimit] = []
    temporal_tracks: list[TemporalBarycenters] = []
    analyzed_titles = []
    for path in paths:
        info = probe(path)
        extension_count = info.channels - len(BED_NAMES)
        if extension_count != args.extensions:
            print(
                f"skip\t{path.name}\textensions={extension_count}, "
                f"requested={args.extensions}"
            )
            continue
        if info.sample_rate != 48_000:
            raise ValueError(
                f"{path}: expected 48000 Hz, got {info.sample_rate}"
            )
        print(
            f"analyze\t{path.name}\tchannels={info.channels} "
            f"frames={info.frames}"
        )
        statistics = None
        if needs_temporal:
            statistics = read_block_statistics(
                path, info.channels, args.block_samples
            )
        results = analyze_file(
            path,
            info,
            args.block_samples,
            args.coverage,
            args.activity_db,
            statistics,
        )
        title = path.name.removesuffix(".dtsx.audio")
        if needs_temporal:
            assert statistics is not None
            assert args.video_dir is not None
            video = probe_video(args.video_dir, title)
            temporal_tracks.append(
                calculate_temporal_barycenters(
                    title=title,
                    video=video,
                    info=info,
                    statistics=statistics,
                    block_samples=args.block_samples,
                    activity_db=args.activity_db,
                    plot_gain=plot_gain,
                    target_fps=args.animation_fps,
                    window_seconds=args.temporal_window_seconds,
                    minimum_active_blocks=(
                        args.minimum_active_blocks
                    ),
                )
            )
        print_file_summary(
            title, results
        )
        all_results.extend(results)
        analyzed_titles.append(title)
    if not all_results:
        raise SystemExit("no matching streams analyzed")
    print_corpus_summary(
        all_results, len(analyzed_titles), args.coverage
    )
    if args.output:
        write_tsv(args.output, all_results)
        print(f"output\t{args.output}")
    if args.plot_dir:
        write_coverage_plots(
            args.plot_dir,
            all_results,
            args.coverage,
            args.block_samples,
            args.activity_db,
            plot_gain,
            fixed_gain_index,
        )
    if args.position_plot_dir:
        barycenters = calculate_bed_barycenters(
            all_results, fixed_gain_index
        )
        write_barycenter_plots(
            args.position_plot_dir,
            barycenters,
            plot_gain,
        )
    if args.temporal_coverage_output:
        write_temporal_coverages(
            args.temporal_coverage_output,
            temporal_tracks,
        )
        print(
            f"temporal_coverages\t"
            f"{args.temporal_coverage_output}"
        )
    if args.animation_dir:
        write_barycenter_animations(
            args.animation_dir,
            temporal_tracks,
            plot_gain,
            args.temporal_window_seconds,
            args.trail_seconds,
        )


if __name__ == "__main__":
    main()
