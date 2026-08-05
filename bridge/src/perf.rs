#[cfg(feature = "bridge-perf")]
use std::time::Duration;
#[cfg(feature = "bridge-perf")]
use truehd::process::parse::ParserPerfStats;

#[cfg(feature = "bridge-perf")]
#[derive(Default)]
struct StageStat {
    total: Duration,
    max: Duration,
    calls: u64,
    samples_us: Vec<f64>,
}

#[cfg(feature = "bridge-perf")]
impl StageStat {
    const MAX_SAMPLES: usize = 2048;

    fn record(&mut self, elapsed: Duration) {
        self.total += elapsed;
        self.max = self.max.max(elapsed);
        self.calls += 1;
        if self.samples_us.len() == Self::MAX_SAMPLES {
            self.samples_us.remove(0);
        }
        self.samples_us.push(elapsed.as_secs_f64() * 1_000_000.0);
    }

    fn avg_us(&self) -> f64 {
        if self.calls == 0 {
            0.0
        } else {
            self.total.as_secs_f64() * 1_000_000.0 / self.calls as f64
        }
    }

    fn max_us(&self) -> f64 {
        self.max.as_secs_f64() * 1_000_000.0
    }

    fn p95_us(&self) -> f64 {
        self.percentile_us(0.95)
    }

    fn percentile_us(&self, percentile: f64) -> f64 {
        if self.samples_us.is_empty() {
            return 0.0;
        }

        let mut sorted = self.samples_us.clone();
        sorted.sort_by(f64::total_cmp);
        let idx = ((sorted.len() - 1) as f64 * percentile).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    }
}

#[cfg(feature = "bridge-perf")]
#[derive(Default)]
pub(crate) struct PerfStats {
    enabled: bool,
    report_every_frames: u64,
    last_report_frame: u64,
    first_frame_logged: bool,
    mat: StageStat,
    mat_chunk_extract: StageStat,
    extractor_push: StageStat,
    drain: StageStat,
    extract: StageStat,
    parse: StageStat,
    decode: StageStat,
    parse_access_unit: StageStat,
    parse_substream_directories: StageStat,
    parse_substream_segments: StageStat,
    parse_substream_segment_blocks: StageStat,
    parse_substream_segment_tail: StageStat,
    parse_extra_data: StageStat,
    parse_block_header_setup: StageStat,
    parse_block_bypassed_lsb: StageStat,
    parse_block_huffman_decode: StageStat,
    parse_block_checks: StageStat,
    build: StageStat,
    build_pcm: StageStat,
    build_labels: StageStat,
    build_metadata: StageStat,
    build_metadata_events: StageStat,
    packets_raw: u64,
    packets_iec61937: u64,
    mat_payload_bytes: u64,
    mat_chunks: u64,
    mat_chunk_bytes: u64,
    extractor_input_bytes: u64,
    frames_built: u64,
    duplicate_frames: u64,
    metadata_frames: u64,
    metadata_events: u64,
}

#[cfg(feature = "bridge-perf")]
impl PerfStats {
    pub(crate) fn configure(&mut self, enabled: bool, report_every_frames: u64) {
        self.enabled = enabled;
        self.report_every_frames = report_every_frames.max(1);
        self.last_report_frame = 0;
        self.first_frame_logged = false;
    }

    pub(crate) fn configure_profile(&mut self, enabled: bool) -> u64 {
        let interval = self.report_every_frames.max(120);
        self.configure(enabled, interval);
        interval
    }

    pub(crate) fn configure_report_every(&mut self, interval: u64) {
        self.configure(self.enabled, interval);
    }

    pub(crate) fn record_mat(&mut self, elapsed: Duration) {
        if self.enabled {
            self.mat.record(elapsed);
        }
    }

    pub(crate) fn record_mat_chunk_extract(&mut self, elapsed: Duration) {
        if self.enabled {
            self.mat_chunk_extract.record(elapsed);
        }
    }

    pub(crate) fn record_extractor_push(&mut self, elapsed: Duration) {
        if self.enabled {
            self.extractor_push.record(elapsed);
        }
    }

    pub(crate) fn record_drain(&mut self, elapsed: Duration) {
        if self.enabled {
            self.drain.record(elapsed);
        }
    }

    pub(crate) fn record_extract(&mut self, elapsed: Duration) {
        if self.enabled {
            self.extract.record(elapsed);
        }
    }

    pub(crate) fn record_parse(&mut self, elapsed: Duration) {
        if self.enabled {
            self.parse.record(elapsed);
        }
    }

    pub(crate) fn record_decode(&mut self, elapsed: Duration) {
        if self.enabled {
            self.decode.record(elapsed);
        }
    }

    #[cfg(feature = "bridge-perf")]
    pub(crate) fn record_parse_substats(&mut self, stats: ParserPerfStats) {
        if self.enabled {
            self.parse_access_unit.record(stats.access_unit_total);
            self.parse_substream_directories
                .record(stats.substream_directories);
            self.parse_substream_segments
                .record(stats.substream_segments);
            self.parse_substream_segment_blocks
                .record(stats.substream_segment_blocks);
            self.parse_substream_segment_tail
                .record(stats.substream_segment_tail);
            self.parse_extra_data.record(stats.extra_data);
            self.parse_block_header_setup
                .record(stats.block_header_setup);
            self.parse_block_bypassed_lsb
                .record(stats.block_bypassed_lsb);
            self.parse_block_huffman_decode
                .record(stats.block_huffman_decode);
            self.parse_block_checks.record(stats.block_checks);
        }
    }

    pub(crate) fn record_build(&mut self, elapsed: Duration) {
        if self.enabled {
            self.build.record(elapsed);
        }
    }

    pub(crate) fn record_build_pcm(&mut self, elapsed: Duration) {
        if self.enabled {
            self.build_pcm.record(elapsed);
        }
    }

    pub(crate) fn record_build_labels(&mut self, elapsed: Duration) {
        if self.enabled {
            self.build_labels.record(elapsed);
        }
    }

    pub(crate) fn record_build_metadata(&mut self, elapsed: Duration) {
        if self.enabled {
            self.build_metadata.record(elapsed);
        }
    }

    pub(crate) fn record_build_metadata_events(&mut self, elapsed: Duration) {
        if self.enabled {
            self.build_metadata_events.record(elapsed);
        }
    }

    pub(crate) fn note_raw_packet(&mut self, bytes: usize) {
        if self.enabled {
            self.packets_raw += 1;
            self.extractor_input_bytes += bytes as u64;
        }
    }

    pub(crate) fn note_mat_packet(&mut self, bytes: usize) {
        if self.enabled {
            self.packets_iec61937 += 1;
            self.mat_payload_bytes += bytes as u64;
        }
    }

    pub(crate) fn note_mat_chunk(&mut self, bytes: usize) {
        if self.enabled {
            self.mat_chunks += 1;
            self.mat_chunk_bytes += bytes as u64;
            self.extractor_input_bytes += bytes as u64;
        }
    }

    pub(crate) fn note_duplicate_frame(&mut self) {
        if self.enabled {
            self.duplicate_frames += 1;
        }
    }

    pub(crate) fn note_built_frame(&mut self, metadata_frames: usize, metadata_events: usize) {
        if self.enabled {
            self.frames_built += 1;
            self.metadata_frames += metadata_frames as u64;
            self.metadata_events += metadata_events as u64;
        }
    }

    pub(crate) fn maybe_report(&mut self, frame_count: u64) {
        if !self.enabled {
            return;
        }
        if !self.first_frame_logged && frame_count > 0 {
            self.first_frame_logged = true;
            eprintln!(
                "harletty-bridge perf first frame seen at frame {}",
                frame_count
            );
        }
        let interval = self.report_every_frames.max(1);
        if frame_count.saturating_sub(self.last_report_frame) < interval {
            return;
        }

        self.last_report_frame = frame_count;
        let avg_mat_chunk_bytes = if self.mat_chunks == 0 {
            0.0
        } else {
            self.mat_chunk_bytes as f64 / self.mat_chunks as f64
        };
        let avg_metadata_events = if self.metadata_frames == 0 {
            0.0
        } else {
            self.metadata_events as f64 / self.metadata_frames as f64
        };
        eprintln!(
            "harletty-bridge perf @ frame {}: packets raw={} iec61937={} | bytes mat_payload={} mat_chunks={} avg_chunk={:.1} extractor_in={} | mat total {:.2}/{:.2}/{:.2}us chunk {:.2}/{:.2}/{:.2}us | extractor push {:.2}/{:.2}/{:.2}us drain {:.2}/{:.2}/{:.2}us next {:.2}/{:.2}/{:.2}us | parse {:.2}/{:.2}/{:.2}us au {:.2}/{:.2}/{:.2}us dirs {:.2}/{:.2}/{:.2}us segs {:.2}/{:.2}/{:.2}us blocks {:.2}/{:.2}/{:.2}us bh {:.2}/{:.2}/{:.2}us blsb {:.2}/{:.2}/{:.2}us huff {:.2}/{:.2}/{:.2}us checks {:.2}/{:.2}/{:.2}us tail {:.2}/{:.2}/{:.2}us extra {:.2}/{:.2}/{:.2}us decode {:.2}/{:.2}/{:.2}us | build total {:.2}/{:.2}/{:.2}us pcm {:.2}/{:.2}/{:.2}us labels {:.2}/{:.2}/{:.2}us metadata {:.2}/{:.2}/{:.2}us events {:.2}/{:.2}/{:.2}us | frames built={} dup={} metadata_frames={} avg_events={:.2}",
            frame_count,
            self.packets_raw,
            self.packets_iec61937,
            self.mat_payload_bytes,
            self.mat_chunks,
            avg_mat_chunk_bytes,
            self.extractor_input_bytes,
            self.mat.avg_us(),
            self.mat.p95_us(),
            self.mat.max_us(),
            self.mat_chunk_extract.avg_us(),
            self.mat_chunk_extract.p95_us(),
            self.mat_chunk_extract.max_us(),
            self.extractor_push.avg_us(),
            self.extractor_push.p95_us(),
            self.extractor_push.max_us(),
            self.drain.avg_us(),
            self.drain.p95_us(),
            self.drain.max_us(),
            self.extract.avg_us(),
            self.extract.p95_us(),
            self.extract.max_us(),
            self.parse.avg_us(),
            self.parse.p95_us(),
            self.parse.max_us(),
            self.parse_access_unit.avg_us(),
            self.parse_access_unit.p95_us(),
            self.parse_access_unit.max_us(),
            self.parse_substream_directories.avg_us(),
            self.parse_substream_directories.p95_us(),
            self.parse_substream_directories.max_us(),
            self.parse_substream_segments.avg_us(),
            self.parse_substream_segments.p95_us(),
            self.parse_substream_segments.max_us(),
            self.parse_substream_segment_blocks.avg_us(),
            self.parse_substream_segment_blocks.p95_us(),
            self.parse_substream_segment_blocks.max_us(),
            self.parse_block_header_setup.avg_us(),
            self.parse_block_header_setup.p95_us(),
            self.parse_block_header_setup.max_us(),
            self.parse_block_bypassed_lsb.avg_us(),
            self.parse_block_bypassed_lsb.p95_us(),
            self.parse_block_bypassed_lsb.max_us(),
            self.parse_block_huffman_decode.avg_us(),
            self.parse_block_huffman_decode.p95_us(),
            self.parse_block_huffman_decode.max_us(),
            self.parse_block_checks.avg_us(),
            self.parse_block_checks.p95_us(),
            self.parse_block_checks.max_us(),
            self.parse_substream_segment_tail.avg_us(),
            self.parse_substream_segment_tail.p95_us(),
            self.parse_substream_segment_tail.max_us(),
            self.parse_extra_data.avg_us(),
            self.parse_extra_data.p95_us(),
            self.parse_extra_data.max_us(),
            self.decode.avg_us(),
            self.decode.p95_us(),
            self.decode.max_us(),
            self.build.avg_us(),
            self.build.p95_us(),
            self.build.max_us(),
            self.build_pcm.avg_us(),
            self.build_pcm.p95_us(),
            self.build_pcm.max_us(),
            self.build_labels.avg_us(),
            self.build_labels.p95_us(),
            self.build_labels.max_us(),
            self.build_metadata.avg_us(),
            self.build_metadata.p95_us(),
            self.build_metadata.max_us(),
            self.build_metadata_events.avg_us(),
            self.build_metadata_events.p95_us(),
            self.build_metadata_events.max_us(),
            self.frames_built,
            self.duplicate_frames,
            self.metadata_frames,
            avg_metadata_events,
        );
    }
}

#[cfg(not(feature = "bridge-perf"))]
#[derive(Default)]
pub(crate) struct PerfStats;

#[cfg(not(feature = "bridge-perf"))]
#[allow(dead_code)]
impl PerfStats {
    pub(crate) fn configure(&mut self, _enabled: bool, _report_every_frames: u64) {}

    pub(crate) fn configure_profile(&mut self, _enabled: bool) -> u64 {
        0
    }

    pub(crate) fn configure_report_every(&mut self, _interval: u64) {}

    pub(crate) fn record_mat(&mut self, _elapsed: ()) {}

    pub(crate) fn record_mat_chunk_extract(&mut self, _elapsed: ()) {}

    pub(crate) fn record_extractor_push(&mut self, _elapsed: ()) {}

    pub(crate) fn record_drain(&mut self, _elapsed: ()) {}

    pub(crate) fn record_extract(&mut self, _elapsed: ()) {}

    pub(crate) fn record_parse(&mut self, _elapsed: ()) {}

    pub(crate) fn record_decode(&mut self, _elapsed: ()) {}

    #[cfg(feature = "bridge-perf")]
    pub(crate) fn record_parse_substats(&mut self, _stats: ParserPerfStats) {}

    pub(crate) fn record_build(&mut self, _elapsed: ()) {}

    pub(crate) fn record_build_pcm(&mut self, _elapsed: ()) {}

    pub(crate) fn record_build_labels(&mut self, _elapsed: ()) {}

    pub(crate) fn record_build_metadata(&mut self, _elapsed: ()) {}

    pub(crate) fn record_build_metadata_events(&mut self, _elapsed: ()) {}

    pub(crate) fn note_raw_packet(&mut self, _bytes: usize) {}

    pub(crate) fn note_mat_packet(&mut self, _bytes: usize) {}

    pub(crate) fn note_mat_chunk(&mut self, _bytes: usize) {}

    pub(crate) fn note_duplicate_frame(&mut self) {}

    pub(crate) fn note_built_frame(&mut self, _metadata_frames: usize, _metadata_events: usize) {}

    pub(crate) fn maybe_report(&mut self, _frame_count: u64) {}
}
