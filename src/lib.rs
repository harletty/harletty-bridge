mod eac3_spdif;
mod mat;

use abi_stable::{
    export_root_module,
    prefix_type::PrefixTypeTrait,
    sabi_trait::prelude::TD_Opaque,
    std_types::{RSlice, RStr, RString, RVec},
};
use bridge_api::{
    BridgeLib, BridgeLibRef, FormatBridge, FormatBridge_TO, FormatBridgeBox, RChannelLabel,
    RCoordinateFormat, RDecodedFrame, RInputTransport, RMetadataFrame, RPushResult,
    RVbapCartesianDefaults, RVbapTableMode,
};
use eac3_spdif::Eac3SpdifStream;
use mat::MatStream;
#[cfg(feature = "bridge-perf")]
use std::env;
#[cfg(feature = "bridge-perf")]
use std::time::{Duration, Instant};
#[cfg(feature = "bridge-perf")]
use truehd::process::parse::ParserPerfStats;
use truehd::{
    process::{
        MAX_PRESENTATIONS,
        decode::{DecodedAccessUnit, Decoder},
        extract::Extractor,
        parse::Parser,
    },
    structs::{
        channel::ChannelLabel,
        oamd::{ObjectAudioMetadataPayload, SpeakerLabels},
    },
};

// E-AC3 decoder imports.
use eac3::{
    AccessUnitInfo, CorePcmFrame, OamdPayload, ObjectPcmDecoder, ObjectPcmPushResult,
    ParsedEmdfPayloadData, PcmDecoder,
};

// Silence unused import warning — FormatBridge is used via the proc-macro generated impl.
#[allow(unused_imports)]
use bridge_api::FormatBridge as _FormatBridgeTrait;

fn bridge_diag_log(level: log::Level, message: &str) {
    sys::live_log::emit_external_record(
        level,
        "harletty-bridge::eac3",
        message.trim_end_matches('\n'),
    );
}

fn dbg_log(msg: &str) {
    bridge_diag_log(log::Level::Info, msg);
}

#[derive(Debug, Clone, Copy)]
struct PcmStats {
    max_abs: i32,
    near_clip_count: usize,
}

impl PcmStats {
    fn from_frame(frame: &RDecodedFrame) -> Result<Self, String> {
        if frame.sample_count == 0 {
            return Err("sample_count_zero".to_string());
        }
        if frame.sampling_frequency == 0 {
            return Err("sample_rate_zero".to_string());
        }
        if frame.channel_count == 0 {
            return Err("channel_count_zero".to_string());
        }

        let expected = frame.sample_count as usize * frame.channel_count as usize;
        if frame.pcm.len() != expected {
            return Err(format!(
                "pcm_len_mismatch expected={} actual={}",
                expected,
                frame.pcm.len()
            ));
        }

        let near_clip_threshold = (i32::MAX as f32 * 0.999) as i32;
        let mut max_abs = 0i32;
        let mut near_clip_count = 0usize;
        for &sample in &frame.pcm {
            let abs = sample.saturating_abs();
            max_abs = max_abs.max(abs);
            if abs >= near_clip_threshold {
                near_clip_count += 1;
            }
        }

        if near_clip_count > expected / 16 {
            return Err(format!(
                "too_many_near_clip_samples near_clip={} total={} max_abs={}",
                near_clip_count, expected, max_abs
            ));
        }

        Ok(Self {
            max_abs,
            near_clip_count,
        })
    }
}

/// Plugin entry point: export the root module so the host can load it.
#[export_root_module]
fn get_library() -> BridgeLibRef {
    BridgeLib {
        new_bridge: create_bridge,
    }
    .leak_into_prefix()
}

extern "C" fn create_bridge(strict: bool) -> FormatBridgeBox {
    dbg_log(&format!("create_bridge strict={}\n", strict));
    FormatBridge_TO::from_value(AtmosBridge::new(strict), TD_Opaque)
}

// ---------------------------------------------------------------------------
// Bridge state
// ---------------------------------------------------------------------------

struct AtmosBridge {
    // ── TrueHD pipeline ──────────────────────────────────────────────
    mat_stream: MatStream,
    extractor: Extractor,
    parser: Parser,
    decoder: Decoder,
    // ── E-AC3 pipeline ───────────────────────────────────────────────
    eac3_spdif: Eac3SpdifStream,
    eac3_pcm_decoder: PcmDecoder,
    eac3_object_decoder: ObjectPcmDecoder,
    eac3_frame_count: u64,
    eac3_total_samples: u64,
    /// True when the most recent `push_packet` used the E-AC3 path.
    eac3_active: bool,
    // ── Shared ───────────────────────────────────────────────────────
    presentation: u8,
    strict: bool,
    /// Running total of decoded samples (used for metadata timestamping).
    total_samples: u64,
    /// Current dialogue level from the last major sync.
    current_dialogue_level: Option<i8>,
    /// Substream info tracking for change detection (TrueHD only).
    current_substream_info: Option<u8>,
    current_extended_substream_info: Option<u8>,
    recovering_until_major_sync: bool,
    frame_count: u64,
    object_name_keys_by_id: Vec<Option<ObjectNameKey>>,
    perf: PerfStats,
}

impl AtmosBridge {
    fn new(strict: bool) -> Self {
        // Default to presentation 3 (full Atmos/JOC); overridable via configure().
        let presentation = 3u8;

        let mut parser = Parser::default();
        let mut decoder = Decoder::default();

        let fail_level = if strict {
            log::Level::Warn
        } else {
            log::Level::Error
        };
        parser.set_fail_level(fail_level);
        decoder.set_fail_level(fail_level);

        // Require all presentations up to and including the requested one.
        let mut required_presentations = [false; MAX_PRESENTATIONS];
        required_presentations[..=presentation as usize]
            .iter_mut()
            .for_each(|p| *p = true);
        parser.set_required_presentations(&required_presentations);

        let eac3_log_level = if strict {
            log::Level::Warn
        } else {
            log::Level::Error
        };
        let mut eac3_pcm = PcmDecoder::new();
        eac3_pcm.set_debug_log_level(eac3_log_level);
        let mut eac3_obj = ObjectPcmDecoder::new();
        eac3_obj.set_debug_log_level(eac3_log_level);

        let bridge = Self {
            mat_stream: MatStream::default(),
            extractor: Extractor::default(),
            parser,
            decoder,
            eac3_spdif: Eac3SpdifStream::default(),
            eac3_pcm_decoder: eac3_pcm,
            eac3_object_decoder: eac3_obj,
            eac3_frame_count: 0,
            eac3_total_samples: 0,
            eac3_active: false,
            presentation,
            strict,
            total_samples: 0,
            current_dialogue_level: None,
            current_substream_info: None,
            current_extended_substream_info: None,
            recovering_until_major_sync: false,
            frame_count: 0,
            object_name_keys_by_id: Vec::new(),
            perf: PerfStats::default(),
        };

        #[cfg(feature = "bridge-perf")]
        {
            let enabled = env::var("TRUEHD_BRIDGE_PERF_PROFILE")
                .ok()
                .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "on" | "yes"));
            let interval = env::var("TRUEHD_BRIDGE_PERF_REPORT_EVERY")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|&v| v > 0)
                .unwrap_or(120);
            bridge.perf.configure(enabled, interval);
        }

        bridge
    }

    fn reset_pipeline(&mut self) {
        // TrueHD reset.
        self.mat_stream.reset();
        self.extractor = Extractor::default();
        self.parser = Parser::default();
        self.decoder = Decoder::default();

        // E-AC3 reset.
        self.eac3_spdif.reset();
        self.eac3_pcm_decoder.reset();
        self.eac3_object_decoder.reset();
        self.eac3_frame_count = 0;
        self.eac3_active = false;

        // Re-apply configuration to new parser/decoder instances.
        let fail_level = if self.strict {
            log::Level::Warn
        } else {
            log::Level::Error
        };
        self.parser.set_fail_level(fail_level);
        self.decoder.set_fail_level(fail_level);
        self.eac3_pcm_decoder.set_debug_log_level(fail_level);
        self.eac3_object_decoder.set_debug_log_level(fail_level);
        let mut required_presentations = [false; MAX_PRESENTATIONS];
        required_presentations[..=self.presentation as usize]
            .iter_mut()
            .for_each(|p| *p = true);
        self.parser
            .set_required_presentations(&required_presentations);
        self.object_name_keys_by_id.clear();
        self.recovering_until_major_sync = false;
    }
}

// ---------------------------------------------------------------------------
// PerfStats — unchanged from the TrueHD bridge.
// ---------------------------------------------------------------------------

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
struct PerfStats {
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
    build_metadata_bed_indices: StageStat,
    build_metadata_name_updates: StageStat,
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
    fn configure(&mut self, enabled: bool, report_every_frames: u64) {
        self.enabled = enabled;
        self.report_every_frames = report_every_frames.max(1);
        self.last_report_frame = 0;
        self.first_frame_logged = false;
    }

    fn configure_profile(&mut self, enabled: bool) -> u64 {
        let interval = self.report_every_frames.max(120);
        self.configure(enabled, interval);
        interval
    }

    fn configure_report_every(&mut self, interval: u64) {
        self.configure(self.enabled, interval);
    }

    fn record_mat(&mut self, elapsed: Duration) {
        if self.enabled {
            self.mat.record(elapsed);
        }
    }

    fn record_mat_chunk_extract(&mut self, elapsed: Duration) {
        if self.enabled {
            self.mat_chunk_extract.record(elapsed);
        }
    }

    fn record_extractor_push(&mut self, elapsed: Duration) {
        if self.enabled {
            self.extractor_push.record(elapsed);
        }
    }

    fn record_drain(&mut self, elapsed: Duration) {
        if self.enabled {
            self.drain.record(elapsed);
        }
    }

    fn record_extract(&mut self, elapsed: Duration) {
        if self.enabled {
            self.extract.record(elapsed);
        }
    }

    fn record_parse(&mut self, elapsed: Duration) {
        if self.enabled {
            self.parse.record(elapsed);
        }
    }

    fn record_decode(&mut self, elapsed: Duration) {
        if self.enabled {
            self.decode.record(elapsed);
        }
    }

    #[cfg(feature = "bridge-perf")]
    fn record_parse_substats(&mut self, stats: ParserPerfStats) {
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

    fn record_build(&mut self, elapsed: Duration) {
        if self.enabled {
            self.build.record(elapsed);
        }
    }

    fn record_build_pcm(&mut self, elapsed: Duration) {
        if self.enabled {
            self.build_pcm.record(elapsed);
        }
    }

    fn record_build_labels(&mut self, elapsed: Duration) {
        if self.enabled {
            self.build_labels.record(elapsed);
        }
    }

    fn record_build_metadata(&mut self, elapsed: Duration) {
        if self.enabled {
            self.build_metadata.record(elapsed);
        }
    }

    fn record_build_metadata_events(&mut self, elapsed: Duration) {
        if self.enabled {
            self.build_metadata_events.record(elapsed);
        }
    }

    fn record_build_metadata_bed_indices(&mut self, elapsed: Duration) {
        if self.enabled {
            self.build_metadata_bed_indices.record(elapsed);
        }
    }

    fn record_build_metadata_name_updates(&mut self, elapsed: Duration) {
        if self.enabled {
            self.build_metadata_name_updates.record(elapsed);
        }
    }

    fn note_raw_packet(&mut self, bytes: usize) {
        if self.enabled {
            self.packets_raw += 1;
            self.extractor_input_bytes += bytes as u64;
        }
    }

    fn note_mat_packet(&mut self, bytes: usize) {
        if self.enabled {
            self.packets_iec61937 += 1;
            self.mat_payload_bytes += bytes as u64;
        }
    }

    fn note_mat_chunk(&mut self, bytes: usize) {
        if self.enabled {
            self.mat_chunks += 1;
            self.mat_chunk_bytes += bytes as u64;
            self.extractor_input_bytes += bytes as u64;
        }
    }

    fn note_duplicate_frame(&mut self) {
        if self.enabled {
            self.duplicate_frames += 1;
        }
    }

    fn note_built_frame(&mut self, metadata_frames: usize, metadata_events: usize) {
        if self.enabled {
            self.frames_built += 1;
            self.metadata_frames += metadata_frames as u64;
            self.metadata_events += metadata_events as u64;
        }
    }

    fn maybe_report(&mut self, frame_count: u64) {
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
            "harletty-bridge perf @ frame {}: packets raw={} iec61937={} | bytes mat_payload={} mat_chunks={} avg_chunk={:.1} extractor_in={} | mat total {:.2}/{:.2}/{:.2}us chunk {:.2}/{:.2}/{:.2}us | extractor push {:.2}/{:.2}/{:.2}us drain {:.2}/{:.2}/{:.2}us next {:.2}/{:.2}/{:.2}us | parse {:.2}/{:.2}/{:.2}us au {:.2}/{:.2}/{:.2}us dirs {:.2}/{:.2}/{:.2}us segs {:.2}/{:.2}/{:.2}us blocks {:.2}/{:.2}/{:.2}us bh {:.2}/{:.2}/{:.2}us blsb {:.2}/{:.2}/{:.2}us huff {:.2}/{:.2}/{:.2}us checks {:.2}/{:.2}/{:.2}us tail {:.2}/{:.2}/{:.2}us extra {:.2}/{:.2}/{:.2}us decode {:.2}/{:.2}/{:.2}us | build total {:.2}/{:.2}/{:.2}us pcm {:.2}/{:.2}/{:.2}us labels {:.2}/{:.2}/{:.2}us metadata {:.2}/{:.2}/{:.2}us events {:.2}/{:.2}/{:.2}us beds {:.2}/{:.2}/{:.2}us names {:.2}/{:.2}/{:.2}us | frames built={} dup={} metadata_frames={} avg_events={:.2}",
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
            self.build_metadata_bed_indices.avg_us(),
            self.build_metadata_bed_indices.p95_us(),
            self.build_metadata_bed_indices.max_us(),
            self.build_metadata_name_updates.avg_us(),
            self.build_metadata_name_updates.p95_us(),
            self.build_metadata_name_updates.max_us(),
            self.frames_built,
            self.duplicate_frames,
            self.metadata_frames,
            avg_metadata_events,
        );
    }
}

#[cfg(not(feature = "bridge-perf"))]
#[derive(Default)]
struct PerfStats;

#[cfg(not(feature = "bridge-perf"))]
#[allow(dead_code)]
impl PerfStats {
    fn configure(&mut self, _enabled: bool, _report_every_frames: u64) {}

    fn configure_profile(&mut self, _enabled: bool) -> u64 {
        0
    }

    fn configure_report_every(&mut self, _interval: u64) {}

    fn record_mat(&mut self, _elapsed: ()) {}

    fn record_mat_chunk_extract(&mut self, _elapsed: ()) {}

    fn record_extractor_push(&mut self, _elapsed: ()) {}

    fn record_drain(&mut self, _elapsed: ()) {}

    fn record_extract(&mut self, _elapsed: ()) {}

    fn record_parse(&mut self, _elapsed: ()) {}

    fn record_decode(&mut self, _elapsed: ()) {}

    #[cfg(feature = "bridge-perf")]
    fn record_parse_substats(&mut self, _stats: ParserPerfStats) {}

    fn record_build(&mut self, _elapsed: ()) {}

    fn record_build_pcm(&mut self, _elapsed: ()) {}

    fn record_build_labels(&mut self, _elapsed: ()) {}

    fn record_build_metadata(&mut self, _elapsed: ()) {}

    fn record_build_metadata_events(&mut self, _elapsed: ()) {}

    fn record_build_metadata_bed_indices(&mut self, _elapsed: ()) {}

    fn record_build_metadata_name_updates(&mut self, _elapsed: ()) {}

    fn note_raw_packet(&mut self, _bytes: usize) {}

    fn note_mat_packet(&mut self, _bytes: usize) {}

    fn note_mat_chunk(&mut self, _bytes: usize) {}

    fn note_duplicate_frame(&mut self) {}

    fn note_built_frame(&mut self, _metadata_frames: usize, _metadata_events: usize) {}

    fn maybe_report(&mut self, _frame_count: u64) {}
}

// ---------------------------------------------------------------------------
// Drain context — borrowed fields for TrueHD drain_frames.
// ---------------------------------------------------------------------------

struct DrainContext<'a> {
    extractor: &'a mut Extractor,
    parser: &'a mut Parser,
    decoder: &'a mut Decoder,
    frame_count: &'a mut u64,
    strict: bool,
    presentation: u8,
    current_substream_info: &'a mut Option<u8>,
    current_extended_substream_info: &'a mut Option<u8>,
    current_dialogue_level: &'a mut Option<i8>,
    recovering_until_major_sync: &'a mut bool,
    total_samples: &'a mut u64,
    object_name_keys_by_id: &'a mut Vec<Option<ObjectNameKey>>,
    perf: &'a mut PerfStats,
}

impl DrainContext<'_> {
    fn reset_parse_recovery_state(&mut self) {
        let fail_level = if self.strict {
            log::Level::Warn
        } else {
            log::Level::Error
        };

        *self.parser = Parser::default();
        *self.decoder = Decoder::default();
        self.parser.set_fail_level(fail_level);
        self.decoder.set_fail_level(fail_level);

        let mut required_presentations = [false; MAX_PRESENTATIONS];
        required_presentations[..=self.presentation as usize]
            .iter_mut()
            .for_each(|p| *p = true);
        self.parser
            .set_required_presentations(&required_presentations);

        *self.current_substream_info = None;
        *self.current_extended_substream_info = None;
        *self.current_dialogue_level = None;
        self.object_name_keys_by_id.clear();
        *self.recovering_until_major_sync = true;
    }
}

// ---------------------------------------------------------------------------
// TrueHD drain_frames — unchanged.
// ---------------------------------------------------------------------------

fn drain_frames(ctx: &mut DrainContext<'_>) -> (Vec<RDecodedFrame>, Option<String>) {
    let mut frames = Vec::new();
    let mut error_msg: Option<String> = None;

    loop {
        #[cfg(feature = "bridge-perf")]
        let extract_started = Instant::now();
        let next_frame = ctx.extractor.next();
        #[cfg(feature = "bridge-perf")]
        ctx.perf.record_extract(extract_started.elapsed());

        match next_frame {
            Some(Ok(raw_frame)) => {
                *ctx.frame_count += 1;

                if *ctx.recovering_until_major_sync {
                    if !raw_frame.is_major_sync() {
                        continue;
                    }

                    log::info!(
                        "Major sync found at frame {}; resuming after parse recovery",
                        ctx.frame_count
                    );
                    *ctx.recovering_until_major_sync = false;
                }

                #[cfg(feature = "bridge-perf")]
                let parse_started = Instant::now();
                let access_unit = match ctx.parser.parse(&raw_frame) {
                    Ok(au) => au,
                    Err(e) => {
                        let msg = format!("Parse error at frame {}: {e}", ctx.frame_count);
                        log::error!("{msg}");
                        if ctx.strict {
                            error_msg = Some(msg);
                            return (frames, error_msg);
                        }
                        ctx.reset_parse_recovery_state();
                        continue;
                    }
                };
                #[cfg(feature = "bridge-perf")]
                {
                    ctx.perf.record_parse(parse_started.elapsed());
                    ctx.perf
                        .record_parse_substats(ctx.parser.last_parse_stats());
                }

                // Track substream info changes and extract dialogue level.
                let mut substream_info_changed = false;
                if let Some(major_sync) = &access_unit.major_sync_info {
                    match *ctx.current_substream_info {
                        Some(cur) if cur != major_sync.substream_info => {
                            log::info!(
                                "substream_info changed: {:#02X} -> {:#02X}",
                                cur,
                                major_sync.substream_info
                            );
                            substream_info_changed = true;
                        }
                        None => {
                            *ctx.current_substream_info = Some(major_sync.substream_info);
                        }
                        _ => {}
                    }
                    match *ctx.current_extended_substream_info {
                        Some(cur) if cur != major_sync.extended_substream_info => {
                            log::info!(
                                "extended_substream_info changed: {:#02X} -> {:#02X}",
                                cur,
                                major_sync.extended_substream_info
                            );
                            substream_info_changed = true;
                        }
                        None => {
                            *ctx.current_extended_substream_info =
                                Some(major_sync.extended_substream_info);
                        }
                        _ => {}
                    }
                    *ctx.current_substream_info = Some(major_sync.substream_info);
                    *ctx.current_extended_substream_info = Some(major_sync.extended_substream_info);

                    // Extract dialogue level.
                    let cm = &major_sync.channel_meaning;
                    let dialogue_level: i8 = match ctx.presentation {
                        0 => -(cm.twoch_dialogue_norm as i8),
                        1 => -(cm.sixch_dialogue_norm as i8),
                        2 => -(cm.eightch_dialogue_norm as i8),
                        3 => {
                            if let Some(ref extra) = cm.extra_channel_meaning {
                                -(extra.sixteench_dialogue_norm as i8)
                            } else {
                                -(cm.eightch_dialogue_norm as i8)
                            }
                        }
                        _ => -(cm.eightch_dialogue_norm as i8),
                    };
                    *ctx.current_dialogue_level = Some(dialogue_level);
                    log::debug!(
                        "Dialogue level: {} dBFS (presentation {})",
                        dialogue_level,
                        ctx.presentation
                    );
                }

                #[cfg(feature = "bridge-perf")]
                let decode_started = Instant::now();
                let mut decoded = match ctx
                    .decoder
                    .decode_presentation(&access_unit, ctx.presentation as usize)
                {
                    Ok(d) => d,
                    Err(e) => {
                        let msg = format!("Decode error at frame {}: {e}", ctx.frame_count);
                        log::error!("{msg}");
                        if ctx.strict {
                            error_msg = Some(msg);
                        }
                        return (frames, error_msg);
                    }
                };
                #[cfg(feature = "bridge-perf")]
                ctx.perf.record_decode(decode_started.elapsed());

                if substream_info_changed {
                    decoded.substream_info_changed = true;
                }

                // Advance the timeline regardless; duplicate frames represent real
                // audio time and must be counted for accurate metadata timestamping.
                let base_sample_pos = *ctx.total_samples;
                *ctx.total_samples += decoded.sample_length as u64;

                if decoded.is_duplicate {
                    #[cfg(feature = "bridge-perf")]
                    ctx.perf.note_duplicate_frame();
                    ctx.perf.maybe_report(*ctx.frame_count);
                    continue;
                }

                #[cfg(feature = "bridge-perf")]
                let build_started = Instant::now();
                frames.push(build_thd_frame(ctx, &decoded, base_sample_pos));
                #[cfg(feature = "bridge-perf")]
                ctx.perf.record_build(build_started.elapsed());
                ctx.perf.maybe_report(*ctx.frame_count);
            }

            Some(Err(ref e))
                if matches!(e, truehd::utils::errors::ExtractError::InsufficientData) =>
            {
                break;
            }

            Some(Err(extract_error)) => {
                let msg = format!(
                    "Extract error at frame {}: {extract_error}",
                    ctx.frame_count
                );
                log::error!("{msg}");
                if ctx.strict {
                    error_msg = Some(msg);
                }
                return (frames, error_msg);
            }

            None => break,
        }
    }

    (frames, None)
}

/// Build an [`RDecodedFrame`] from a TrueHD decoded access unit.
fn build_thd_frame(
    ctx: &mut DrainContext<'_>,
    decoded: &DecodedAccessUnit,
    base_sample_pos: u64,
) -> RDecodedFrame {
    #[cfg(feature = "bridge-perf")]
    let pcm_started = Instant::now();
    // Build interleaved PCM: [s0c0, s0c1, …, s(M-1)c(N-1)].
    // Manual loops avoid iterator/closure overhead on this hot path.
    let mut pcm: RVec<i32> = RVec::with_capacity(decoded.sample_length * decoded.channel_count);
    for s in 0..decoded.sample_length {
        for c in 0..decoded.channel_count {
            pcm.push(decoded.pcm_data[s][c]);
        }
    }
    #[cfg(feature = "bridge-perf")]
    ctx.perf.record_build_pcm(pcm_started.elapsed());

    // Channel labels.
    #[cfg(feature = "bridge-perf")]
    let labels_started = Instant::now();
    let channel_labels: RVec<RChannelLabel> = decoded
        .channel_labels
        .iter()
        .map(channel_label_to_r)
        .collect();
    #[cfg(feature = "bridge-perf")]
    ctx.perf.record_build_labels(labels_started.elapsed());

    // Metadata: one RMetadataFrame per parsed OAMD payload.
    #[cfg(feature = "bridge-perf")]
    let metadata_started = Instant::now();
    let mut metadata: RVec<RMetadataFrame> = RVec::new();
    if decoded.substream_info_changed {
        ctx.object_name_keys_by_id.clear();
    }
    #[cfg(feature = "bridge-perf")]
    let mut metadata_events = 0usize;
    for oamd in &decoded.oamd {
        let evo_base = base_sample_pos + oamd.evo_sample_offset;
        let meta = build_metadata_frame_from_oamd(
            oamd,
            evo_base,
            base_sample_pos,
            ctx.object_name_keys_by_id,
            ctx.perf,
        );
        #[cfg(feature = "bridge-perf")]
        {
            metadata_events += meta.events.len();
        }
        metadata.push(meta);
    }
    #[cfg(feature = "bridge-perf")]
    {
        ctx.perf.record_build_metadata(metadata_started.elapsed());
        ctx.perf.note_built_frame(metadata.len(), metadata_events);
    }

    RDecodedFrame {
        sampling_frequency: decoded.sampling_frequency,
        sample_count: decoded.sample_length as u32,
        channel_count: decoded.channel_count as u32,
        pcm,
        channel_labels,
        metadata,
        dialogue_level: (*ctx.current_dialogue_level).into(),
        is_new_segment: decoded.substream_info_changed,
    }
}

// ---------------------------------------------------------------------------
// E-AC3 processing
// ---------------------------------------------------------------------------

/// Process a raw E-AC3 access unit (one complete syncframe).
///
/// Attempts object-level decode first (JOC + OAMD), then falls back to
/// core PCM decode.  Converts the result into one [`RDecodedFrame`].
fn process_eac3_frame(bridge: &mut AtmosBridge, frame: &[u8]) -> Result<RDecodedFrame, String> {
    // Write first frame to a file for external inspection.
    if bridge.eac3_frame_count == 1 {
        let _ = std::fs::write("/tmp/harletty-eac3-frame.bin", frame);
        dbg_log("eac3_first_frame_dumped_to_/tmp/harletty-eac3-frame.bin\n");
    }

    match bridge.eac3_object_decoder.push_access_unit(frame) {
        Ok(Some(result)) => {
            let sample_count = result.pcm.samples_per_channel();
            let frame_ms = sample_count as f64 / result.pcm.core.sample_rate.max(1) as f64 * 1000.0;
            dbg_log(&format!(
                "eac3_object_decode ok sr={} samples={} frame_ms={:.3} bed_ch={} object_ch={} active={} frames_seen={}\n",
                result.pcm.core.sample_rate,
                sample_count,
                frame_ms,
                result.pcm.core.total_channels(),
                result.pcm.object_channels.len(),
                result
                    .pcm
                    .object_active
                    .iter()
                    .filter(|active| **active)
                    .count(),
                result.frames_seen
            ));

            let base_sample_pos = bridge.eac3_total_samples;
            bridge.eac3_total_samples += sample_count as u64;
            let rf = build_eac3_frame_from_object(result, base_sample_pos, bridge);
            bridge.perf.maybe_report(bridge.eac3_frame_count);
            return Ok(rf);
        }
        Ok(None) => {
            dbg_log("eac3_object_decode no_joc_payload fallback=core\n");
        }
        Err(e) => {
            dbg_log(&format!("eac3_object_decode_error={e} fallback=core\n"));
        }
    }

    match bridge.eac3_pcm_decoder.push_access_unit(frame) {
        Ok(result) => {
            let info = result.info;
            let pcm = result.pcm;
            let decoded_samples = pcm.samples_per_channel();
            let frame_ms = decoded_samples as f64 / pcm.sample_rate.max(1) as f64 * 1000.0;
            dbg_log(&format!(
                "eac3_inspect_ok frame={}B blocks={} expected_samples={} body_offset={} block_start={}\n",
                info.frame_size,
                info.num_blocks,
                info.num_blocks as usize * 256,
                info.body_start_bit_offset,
                info.audio_frame.block_payload_start_bit_offset
            ));

            // Log the frame bytes near the end to diagnose footer.
            let tail_start = frame.len().saturating_sub(16);
            let tail: Vec<String> = frame[tail_start..]
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect();
            dbg_log(&format!("eac3_frame_tail_last16=[{}]\n", tail.join(" ")));

            dbg_log(&format!(
                "eac3_decode ok sr={} samples={} frame_ms={:.3} ch={} frames_seen={}\n",
                pcm.sample_rate,
                decoded_samples,
                frame_ms,
                pcm.total_channels(),
                result.frames_seen
            ));
            if decoded_samples != info.num_blocks as usize * 256 {
                dbg_log(&format!(
                    "eac3_decode_sample_count_mismatch decoded={} expected={} blocks={}\n",
                    decoded_samples,
                    info.num_blocks as usize * 256,
                    info.num_blocks
                ));
            }
            let sample_count = decoded_samples as u32;
            let base_sample_pos = bridge.eac3_total_samples;
            bridge.eac3_total_samples += sample_count as u64;
            let rf = build_eac3_frame_from_core(&pcm, &info, base_sample_pos, bridge);
            bridge.perf.maybe_report(bridge.eac3_frame_count);
            Ok(rf)
        }
        Err(e) => {
            dbg_log(&format!("eac3_decode_error={e}\n"));
            Err(format!("E-AC3 decode error: {e}"))
        }
    }
}

/// Build an [`RDecodedFrame`] from an E-AC3 object decode result.
fn build_eac3_frame_from_object(
    result: ObjectPcmPushResult,
    base_sample_pos: u64,
    bridge: &mut AtmosBridge,
) -> RDecodedFrame {
    let pcm_frame = &result.pcm;
    let core = &pcm_frame.core;
    let sample_count = pcm_frame.samples_per_channel();
    let sampling_frequency = core.sample_rate;
    let bed_channels = core.fullband_channels.len() + usize::from(core.lfe_channel.is_some());
    let object_channels = pcm_frame.object_channels.len();
    let total_channel_count = bed_channels + object_channels;

    // Build interleaved PCM: bed channels first, then dynamic-object channels.
    // JOC output is dynamic-only — `object_channels[i]` corresponds to OAMD
    // object[bed_or_isf_objects + i] (cf. libstarmine_ad render.rs:881).
    let pcm_capacity = sample_count * total_channel_count;
    let mut pcm: RVec<i32> = RVec::with_capacity(pcm_capacity);

    for s in 0..sample_count {
        for ch in &core.fullband_channels {
            pcm.push(float_to_pcm_i32(ch[s]));
        }
        if let Some(lfe) = &core.lfe_channel {
            pcm.push(float_to_pcm_i32(lfe[s]));
        }
        for obj_ch in &pcm_frame.object_channels {
            pcm.push(float_to_pcm_i32(obj_ch[s]));
        }
    }

    // Channel labels.
    let mut channel_labels: RVec<RChannelLabel> = RVec::with_capacity(total_channel_count);
    for bed in &core.fullband_channel_order {
        channel_labels.push(bed_channel_to_r(*bed));
    }
    if core.lfe_channel.is_some() {
        channel_labels.push(RChannelLabel::LFE);
    }
    for _ in 0..object_channels {
        channel_labels.push(RChannelLabel::Unknown);
    }

    // Metadata from OAMD payloads.
    #[cfg(feature = "bridge-perf")]
    let metadata_started = Instant::now();
    let mut metadata: RVec<RMetadataFrame> = RVec::new();
    #[cfg(feature = "bridge-perf")]
    let mut metadata_events = 0usize;
    let bed_indices = eac3_core_bed_indices(core);
    for (oamd, sample_offset) in &pcm_frame.oamd_payloads {
        let evo_base = base_sample_pos + sample_offset.unwrap_or(0) as u64;
        let oamd_ref: &OamdPayload = oamd;
        let meta = build_eac3_metadata_frame(
            oamd_ref,
            evo_base,
            base_sample_pos,
            &bed_indices,
            object_channels,
            bridge,
        );
        #[cfg(feature = "bridge-perf")]
        {
            metadata_events += meta.events.len();
        }
        metadata.push(meta);
    }
    #[cfg(feature = "bridge-perf")]
    {
        let elapsed = metadata_started.elapsed();
        bridge.perf.record_build_metadata(elapsed);
        bridge
            .perf
            .note_built_frame(metadata.len(), metadata_events);
    }

    RDecodedFrame {
        sampling_frequency,
        sample_count: sample_count as u32,
        channel_count: total_channel_count as u32,
        pcm,
        channel_labels,
        metadata,
        dialogue_level: bridge.current_dialogue_level.into(),
        is_new_segment: false,
    }
}

/// Build an [`RDecodedFrame`] from a core-PCM-only E-AC3 decode result.
fn build_eac3_frame_from_core(
    core: &CorePcmFrame,
    info: &AccessUnitInfo,
    base_sample_pos: u64,
    bridge: &mut AtmosBridge,
) -> RDecodedFrame {
    let sampling_frequency = core.sample_rate;
    let sample_count = core.samples_per_channel();
    let total_channel_count = core.total_channels();

    // Build interleaved PCM.
    let pcm_capacity = sample_count * total_channel_count;
    let mut pcm: RVec<i32> = RVec::with_capacity(pcm_capacity);

    for s in 0..sample_count {
        for ch in &core.fullband_channels {
            pcm.push(float_to_pcm_i32(ch[s]));
        }
        if let Some(lfe) = &core.lfe_channel {
            pcm.push(float_to_pcm_i32(lfe[s]));
        }
    }

    // Channel labels.
    let mut channel_labels: RVec<RChannelLabel> = RVec::with_capacity(total_channel_count);
    for bed in &core.fullband_channel_order {
        channel_labels.push(bed_channel_to_r(*bed));
    }
    if core.lfe_channel.is_some() {
        channel_labels.push(RChannelLabel::LFE);
    }

    // Extract metadata from OAMD payloads in the access unit info.
    let mut metadata: RVec<RMetadataFrame> = RVec::new();
    let bed_indices = eac3_core_bed_indices(core);
    for payload in info.payloads() {
        if let ParsedEmdfPayloadData::Oamd(oamd) = &payload.parsed {
            let evo_base = base_sample_pos + payload.info.sample_offset.unwrap_or(0) as u64;
            let meta =
                build_eac3_metadata_frame(oamd, evo_base, base_sample_pos, &bed_indices, 0, bridge);
            metadata.push(meta);
        }
    }

    RDecodedFrame {
        sampling_frequency,
        sample_count: sample_count as u32,
        channel_count: total_channel_count as u32,
        pcm,
        channel_labels,
        metadata,
        dialogue_level: bridge.current_dialogue_level.into(),
        is_new_segment: false,
    }
}

// ---------------------------------------------------------------------------
// TrueHD / E-AC3 metadata frame builders
// ---------------------------------------------------------------------------

/// Build an [`RMetadataFrame`] from an OAMD payload parsed from E-AC3.
fn build_eac3_metadata_frame(
    oamd: &OamdPayload,
    evo_base: u64,
    frame_sample_pos: u64,
    bed_indices: &[usize],
    object_channel_count: usize,
    bridge: &mut AtmosBridge,
) -> RMetadataFrame {
    let events = extract_eac3_events(oamd, evo_base, bed_indices, object_channel_count);
    let bed_indices: RVec<usize> = bed_indices.iter().copied().collect();

    let mut name_updates = RVec::new();
    let dynamic_objects = oamd
        .object_count
        .saturating_sub(oamd.bed_or_isf_objects)
        .min(object_channel_count);
    for dynamic_idx in 0..dynamic_objects {
        let id = (10 + dynamic_idx) as u32;
        let key = ObjectNameKey::Dynamic(id as usize);
        if name_key_changed(&mut bridge.object_name_keys_by_id, id, &key) {
            name_updates.push(bridge_api::RNameUpdate {
                id,
                name: object_name_from_key(&key).into(),
            });
        }
    }

    dbg_log(&format!(
        "eac3_metadata bed_indices={:?} bed_ch={} object_ch={} object_count={} bed_or_isf={} dynamic_events={}\n",
        bed_indices,
        bed_indices.len(),
        object_channel_count,
        oamd.object_count,
        oamd.bed_or_isf_objects,
        events.len()
    ));

    RMetadataFrame {
        events,
        bed_indices,
        name_updates,
        sample_pos: frame_sample_pos,
        ramp_duration: 0,
    }
}

/// Convert an E-AC3 OAMD payload into the bridge event list.
///
/// Mirrors TrueHD's structure: bed events first (one per PCM bed channel,
/// `has_pos=false`, `id=speaker_id`), then dynamic events (`id=10+dynamic_idx`).
/// Studio displays slots ordered by `events[]` index, so the bed events keep
/// the dynamic slots aligned with their position metadata.
fn extract_eac3_events(
    oamd: &OamdPayload,
    base_sample_pos: u64,
    bed_indices: &[usize],
    object_channel_count: usize,
) -> RVec<bridge_api::REvent> {
    let dynamic_objects = oamd
        .object_count
        .saturating_sub(oamd.bed_or_isf_objects)
        .min(object_channel_count);
    let mut events: RVec<bridge_api::REvent> =
        RVec::with_capacity(bed_indices.len() + dynamic_objects);

    for &speaker_id in bed_indices {
        events.push(bridge_api::REvent {
            id: speaker_id as u32,
            sample_pos: base_sample_pos,
            has_pos: false,
            pos: [0.0; 3],
            gain_db: 0,
            spread: 0.0,
            ramp_duration: 0,
        });
    }

    for element in &oamd.elements {
        let eac3::OamdElementKind::Object(ref obj_element) = element.kind else {
            continue;
        };

        for obj_idx in 0..obj_element.object_blocks.len() {
            let Some(blocks) = obj_element.object_blocks.get(obj_idx) else {
                continue;
            };
            let Some(block) = blocks.first() else {
                continue;
            };

            if obj_idx < oamd.bed_or_isf_objects {
                continue;
            }
            let dynamic_idx = obj_idx - oamd.bed_or_isf_objects;
            if dynamic_idx >= object_channel_count {
                continue;
            }
            let id = (10 + dynamic_idx) as u32;
            let has_pos = block.valid_position;
            let pos: [f64; 3] = if has_pos {
                match block.position.as_ref() {
                    Some(p) if !block.differential_position => [
                        ((p.x as f64).clamp(0.0, 1.0) - 0.5) * 2.0,
                        (0.5 - (p.y as f64).clamp(0.0, 1.0)) * 2.0,
                        (p.z as f64).clamp(-1.0, 1.0),
                    ],
                    Some(p) => [
                        (p.x as f64).clamp(-1.0, 1.0),
                        (-(p.y as f64)).clamp(-1.0, 1.0),
                        (p.z as f64).clamp(-1.0, 1.0),
                    ],
                    None => [0.0; 3],
                }
            } else {
                [0.0; 3]
            };
            let spread = (block.size.unwrap_or(0.0) as f64 * 180.0).clamp(0.0, 180.0);

            let sample_offset = obj_element
                .block_updates
                .first()
                .map(|u| u.offset as u64)
                .unwrap_or(0);
            let ramp_duration = obj_element
                .block_updates
                .first()
                .map(|u| u.ramp_duration as u32)
                .unwrap_or(0);

            events.push(bridge_api::REvent {
                id,
                sample_pos: base_sample_pos + sample_offset,
                has_pos,
                pos,
                gain_db: block.gain.unwrap_or(0.0) as i8,
                spread,
                ramp_duration,
            });
        }
    }

    events
}

// ---------------------------------------------------------------------------
// TrueHD extractor input processing — unchanged.
// ---------------------------------------------------------------------------

fn process_extractor_input(bridge: &mut AtmosBridge, input: &[u8], result: &mut RPushResult) {
    if input.is_empty() {
        return;
    }

    #[cfg(feature = "bridge-perf")]
    let push_started = Instant::now();
    bridge.extractor.push_bytes(input);
    #[cfg(feature = "bridge-perf")]
    bridge.perf.record_extractor_push(push_started.elapsed());

    let panic_result = {
        let mut ctx = DrainContext {
            extractor: &mut bridge.extractor,
            parser: &mut bridge.parser,
            decoder: &mut bridge.decoder,
            frame_count: &mut bridge.frame_count,
            strict: bridge.strict,
            presentation: bridge.presentation,
            current_substream_info: &mut bridge.current_substream_info,
            current_extended_substream_info: &mut bridge.current_extended_substream_info,
            current_dialogue_level: &mut bridge.current_dialogue_level,
            recovering_until_major_sync: &mut bridge.recovering_until_major_sync,
            total_samples: &mut bridge.total_samples,
            object_name_keys_by_id: &mut bridge.object_name_keys_by_id,
            perf: &mut bridge.perf,
        };
        #[cfg(feature = "bridge-perf")]
        let drain_started = Instant::now();
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drain_frames(&mut ctx)));
        #[cfg(feature = "bridge-perf")]
        bridge.perf.record_drain(drain_started.elapsed());
        result
    };

    match panic_result {
        Ok((frames, err)) => {
            result.frames.extend(frames);
            if let Some(msg) = err {
                result.error_message = msg.into();
                bridge.reset_pipeline();
                result.did_reset = true;
            }
        }
        Err(panic_info) => {
            let msg = panic_message(&panic_info);
            log::warn!(
                "Panic caught during frame processing: {}. Resetting pipeline.",
                msg
            );
            bridge.reset_pipeline();
            result.did_reset = true;
            if bridge.strict {
                result.error_message = msg.into();
            }
        }
    }
}

/// Extract a panic message from the payload returned by catch_unwind.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "Unknown panic during frame processing".to_string()
    }
}

// ---------------------------------------------------------------------------
// FormatBridge implementation
// ---------------------------------------------------------------------------

impl FormatBridge for AtmosBridge {
    fn push_packet(
        &mut self,
        data: RSlice<'_, u8>,
        transport: RInputTransport,
        data_type: u8,
    ) -> RPushResult {
        dbg_log(&format!(
            "push_packet dt=0x{data_type:02X} len={}\n",
            data.len()
        ));
        let mut result = RPushResult {
            frames: RVec::new(),
            error_message: RString::new(),
            did_reset: false,
        };

        match transport {
            RInputTransport::Raw => {
                #[cfg(feature = "bridge-perf")]
                self.perf.note_raw_packet(data.len());
                process_extractor_input(self, data.as_slice(), &mut result);
                result
            }
            RInputTransport::Iec61937 => {
                // ── TrueHD (data type 0x16) ───────────────────────────
                if MatStream::accepts_data_type(data_type) {
                    self.eac3_active = false;

                    #[cfg(feature = "bridge-perf")]
                    let mat_started = Instant::now();
                    #[cfg(feature = "bridge-perf")]
                    self.perf.note_mat_packet(data.len());
                    self.mat_stream.push_payload(data.as_slice());
                    loop {
                        #[cfg(feature = "bridge-perf")]
                        let chunk_extract_started = Instant::now();
                        match self.mat_stream.next_chunk() {
                            Ok(Some(chunk)) => {
                                #[cfg(feature = "bridge-perf")]
                                {
                                    self.perf
                                        .record_mat_chunk_extract(chunk_extract_started.elapsed());
                                    self.perf.note_mat_chunk(chunk.len());
                                }
                                process_extractor_input(self, &chunk, &mut result);
                                if result.did_reset {
                                    break;
                                }
                            }
                            Ok(None) => {
                                #[cfg(feature = "bridge-perf")]
                                self.perf
                                    .record_mat_chunk_extract(chunk_extract_started.elapsed());
                                break;
                            }
                            Err(msg) => {
                                #[cfg(feature = "bridge-perf")]
                                self.perf
                                    .record_mat_chunk_extract(chunk_extract_started.elapsed());
                                log::warn!("{msg}");
                                self.reset_pipeline();
                                result.did_reset = true;
                                if self.strict {
                                    result.error_message = msg.into();
                                }
                                return result;
                            }
                        }
                    }
                    #[cfg(feature = "bridge-perf")]
                    self.perf.record_mat(mat_started.elapsed());
                    return result;
                }

                // ── E-AC3 (data type 0x15) ────────────────────────────
                if Eac3SpdifStream::accepts_data_type(data_type) {
                    bridge_diag_log(
                        log::Level::Info,
                        &format!("eac3_branch dt=0x{data_type:02X} len={}", data.len()),
                    );
                    self.eac3_active = true;
                    let preview_len = data.len().min(16);
                    let preview: String = data[..preview_len]
                        .iter()
                        .map(|b| format!("{b:02X}"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    bridge_diag_log(log::Level::Info, &format!("eac3_preview=[{preview}]"));

                    self.eac3_spdif.push_payload(data.as_slice());
                    let mut frame_count_in_packet = 0u32;
                    loop {
                        match self.eac3_spdif.next_frame() {
                            Ok(Some(frame)) => {
                                frame_count_in_packet += 1;
                                self.eac3_frame_count += 1;
                                bridge_diag_log(
                                    log::Level::Info,
                                    &format!(
                                        "eac3_frame_extracted index={} bytes={} sync=0x{:02X}{:02X} first6={:02X?}",
                                        self.eac3_frame_count,
                                        frame.len(),
                                        frame.first().copied().unwrap_or(0),
                                        frame.get(1).copied().unwrap_or(0),
                                        &frame.get(..6).unwrap_or(&[])
                                    ),
                                );
                                let decode_result = process_eac3_frame(self, &frame);
                                match decode_result {
                                    Ok(decoded_frame) => {
                                        let stats = match PcmStats::from_frame(&decoded_frame) {
                                            Ok(stats) => stats,
                                            Err(reason) => {
                                                bridge_diag_log(
                                                    log::Level::Warn,
                                                    &format!(
                                                        "eac3_frame_rejected index={} reason={} sr={} samples={} ch={} pcm_len={}",
                                                        self.eac3_frame_count,
                                                        reason,
                                                        decoded_frame.sampling_frequency,
                                                        decoded_frame.sample_count,
                                                        decoded_frame.channel_count,
                                                        decoded_frame.pcm.len()
                                                    ),
                                                );
                                                continue;
                                            }
                                        };
                                        bridge_diag_log(
                                            log::Level::Info,
                                            &format!(
                                                "eac3_frame_ready index={} sr={} samples={} ch={} max_abs={} near_clip={}",
                                                self.eac3_frame_count,
                                                decoded_frame.sampling_frequency,
                                                decoded_frame.sample_count,
                                                decoded_frame.channel_count,
                                                stats.max_abs,
                                                stats.near_clip_count
                                            ),
                                        );
                                        log::debug!(
                                            "E-AC3 decoded frame: sr={} samples={} ch={}",
                                            decoded_frame.sampling_frequency,
                                            decoded_frame.sample_count,
                                            decoded_frame.channel_count
                                        );
                                        result.frames.push(decoded_frame);
                                    }
                                    Err(msg) => {
                                        log::warn!("{msg}");
                                        self.reset_pipeline();
                                        result.did_reset = true;
                                        if self.strict {
                                            result.error_message = msg.into();
                                        }
                                        return result;
                                    }
                                }
                            }
                            Ok(None) => {
                                if frame_count_in_packet == 0 {
                                    bridge_diag_log(
                                        log::Level::Info,
                                        &format!("eac3_no_frame_extracted len={}", data.len()),
                                    );
                                }
                                break;
                            }
                            Err(msg) => {
                                bridge_diag_log(log::Level::Warn, &format!("eac3_error={msg}"));
                                log::warn!("{msg}");
                                self.reset_pipeline();
                                result.did_reset = true;
                                if self.strict {
                                    result.error_message = msg.into();
                                }
                                return result;
                            }
                        }
                    }
                    return result;
                }

                // Unsupported data type.
                let msg =
                    format!("Unsupported IEC 61937 data type for this bridge: 0x{data_type:02X}");
                log::warn!("{msg}");
                if self.strict {
                    result.error_message = msg.into();
                    self.reset_pipeline();
                    result.did_reset = true;
                }
                result
            }
        }
    }

    fn reset(&mut self) {
        log::info!("Bridge reset requested");
        self.reset_pipeline();
        // Note: total_samples is NOT reset — it tracks the global position for
        // continuous-mode timestamping. The handler manages segment offsets.
    }

    fn is_ready(&self) -> bool {
        self.frame_count > 0 || self.eac3_frame_count > 0
    }

    fn is_spatial(&self) -> bool {
        if self.eac3_active {
            // E-AC3/JOC is spatial when we've decoded object channels.
            // The object decoder count acts as a proxy.
            self.eac3_object_decoder.frames_seen() > 0
        } else {
            // Presentations 0–(MAX-2) are pure downmixes; the top presentation carries objects.
            self.presentation >= (MAX_PRESENTATIONS as u8) - 1
        }
    }

    fn configure(&mut self, key: RStr<'_>, value: RStr<'_>) -> bool {
        match key.as_str() {
            "presentation" => {
                let p = match value.as_str() {
                    "best" => (MAX_PRESENTATIONS as u8) - 1,
                    s => match s.parse::<u8>() {
                        Ok(p) if p < MAX_PRESENTATIONS as u8 => p,
                        Ok(p) => {
                            log::warn!(
                                "atmos-bridge: presentation {p} out of range (0–{})",
                                MAX_PRESENTATIONS - 1
                            );
                            return false;
                        }
                        Err(_) => {
                            log::warn!("atmos-bridge: cannot parse presentation value {:?}", s);
                            return false;
                        }
                    },
                };
                self.presentation = p;
                let mut required_presentations = [false; MAX_PRESENTATIONS];
                required_presentations[..=p as usize]
                    .iter_mut()
                    .for_each(|v| *v = true);
                self.parser
                    .set_required_presentations(&required_presentations);
                log::debug!("atmos-bridge: presentation set to {p}");
                true
            }
            #[cfg(feature = "bridge-perf")]
            "perf_profile" => {
                let enabled = matches!(value.as_str(), "1" | "true" | "on" | "yes");
                let report_every = self.perf.configure_profile(enabled);
                eprintln!(
                    "harletty-bridge perf profiling {} (report_every_frames={})",
                    if enabled { "enabled" } else { "disabled" },
                    report_every
                );
                true
            }
            #[cfg(feature = "bridge-perf")]
            "perf_report_every" => match value.as_str().parse::<u64>() {
                Ok(interval) if interval > 0 => {
                    self.perf.configure_report_every(interval);
                    eprintln!(
                        "harletty-bridge perf reporting interval set to {} frames",
                        interval
                    );
                    true
                }
                _ => {
                    log::warn!(
                        "atmos-bridge: invalid perf_report_every value {:?}",
                        value.as_str()
                    );
                    false
                }
            },
            #[cfg(not(feature = "bridge-perf"))]
            "perf_profile" | "perf_report_every" => false,
            _ => {
                log::debug!("atmos-bridge: unknown configuration key {:?}", key.as_str());
                false
            }
        }
    }

    fn coordinate_format(&self) -> RCoordinateFormat {
        RCoordinateFormat::Cartesian
    }

    fn vbap_cartesian_defaults(&self) -> RVbapCartesianDefaults {
        // Balanced default grid size for runtime cartesian VBAP table generation.
        RVbapCartesianDefaults {
            x_size: 62,
            y_size: 62,
            z_size: 15,
            allow_negative_z: false,
        }
    }

    fn preferred_vbap_table_mode(&self) -> RVbapTableMode {
        RVbapTableMode::Cartesian
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Convert a TrueHD `ChannelLabel` to its ABI-stable counterpart.
fn channel_label_to_r(label: &ChannelLabel) -> RChannelLabel {
    match label {
        ChannelLabel::L => RChannelLabel::L,
        ChannelLabel::R => RChannelLabel::R,
        ChannelLabel::C => RChannelLabel::C,
        ChannelLabel::LFE => RChannelLabel::LFE,
        ChannelLabel::Ls => RChannelLabel::Ls,
        ChannelLabel::Rs => RChannelLabel::Rs,
        ChannelLabel::Tfl => RChannelLabel::Tfl,
        ChannelLabel::Tfr => RChannelLabel::Tfr,
        ChannelLabel::Tsl => RChannelLabel::Tsl,
        ChannelLabel::Tsr => RChannelLabel::Tsr,
        ChannelLabel::Tbl => RChannelLabel::Tbl,
        ChannelLabel::Tbr => RChannelLabel::Tbr,
        ChannelLabel::Lsc => RChannelLabel::Lsc,
        ChannelLabel::Rsc => RChannelLabel::Rsc,
        ChannelLabel::Lb => RChannelLabel::Lb,
        ChannelLabel::Rb => RChannelLabel::Rb,
        ChannelLabel::Cb => RChannelLabel::Cb,
        ChannelLabel::Tc => RChannelLabel::Tc,
        ChannelLabel::Lsd => RChannelLabel::Lsd,
        ChannelLabel::Rsd => RChannelLabel::Rsd,
        ChannelLabel::Lw => RChannelLabel::Lw,
        ChannelLabel::Rw => RChannelLabel::Rw,
        ChannelLabel::Tfc => RChannelLabel::Tfc,
        ChannelLabel::LFE2 => RChannelLabel::LFE2,
    }
}

/// Convert an E-AC3 `BedChannel` to its ABI-stable counterpart.
fn bed_channel_to_r(ch: eac3::BedChannel) -> RChannelLabel {
    use eac3::BedChannel;
    match ch {
        BedChannel::FrontLeft => RChannelLabel::L,
        BedChannel::FrontRight => RChannelLabel::R,
        BedChannel::Center => RChannelLabel::C,
        BedChannel::LowFrequencyEffects => RChannelLabel::LFE,
        BedChannel::SurroundLeft => RChannelLabel::Ls,
        BedChannel::SurroundRight => RChannelLabel::Rs,
        BedChannel::RearCenter => RChannelLabel::Cb,
        BedChannel::RearLeft => RChannelLabel::Lb,
        BedChannel::RearRight => RChannelLabel::Rb,
        BedChannel::TopFrontLeft => RChannelLabel::Tfl,
        BedChannel::TopFrontRight => RChannelLabel::Tfr,
        BedChannel::TopSurroundLeft => RChannelLabel::Tsl,
        BedChannel::TopSurroundRight => RChannelLabel::Tsr,
        BedChannel::TopRearLeft => RChannelLabel::Tbl,
        BedChannel::TopRearRight => RChannelLabel::Tbr,
        BedChannel::WideLeft => RChannelLabel::Lw,
        BedChannel::WideRight => RChannelLabel::Rw,
        BedChannel::LowFrequencyEffects2 => RChannelLabel::LFE2,
    }
}

/// Map a `BedChannel` to the speaker ID space used by the bridge.
fn bed_channel_to_speaker_id(ch: eac3::BedChannel) -> usize {
    use eac3::BedChannel;
    match ch {
        BedChannel::FrontLeft => 0,
        BedChannel::FrontRight => 1,
        BedChannel::Center => 2,
        BedChannel::LowFrequencyEffects => 3,
        BedChannel::SurroundLeft => 4,
        BedChannel::SurroundRight => 5,
        BedChannel::RearCenter => 6,
        BedChannel::RearLeft => 6,
        BedChannel::RearRight => 7,
        BedChannel::TopFrontLeft => 8,
        BedChannel::TopFrontRight => 9,
        BedChannel::TopSurroundLeft => 8,
        BedChannel::TopSurroundRight => 9,
        BedChannel::TopRearLeft => 8,
        BedChannel::TopRearRight => 9,
        BedChannel::WideLeft => 0,
        BedChannel::WideRight => 1,
        BedChannel::LowFrequencyEffects2 => 3,
    }
}

fn eac3_core_bed_indices(core: &CorePcmFrame) -> Vec<usize> {
    let mut bed_indices = Vec::with_capacity(core.total_channels());
    bed_indices.extend(
        core.fullband_channel_order
            .iter()
            .copied()
            .map(bed_channel_to_speaker_id),
    );
    if core.lfe_channel.is_some() {
        bed_indices.push(bed_channel_to_speaker_id(
            eac3::BedChannel::LowFrequencyEffects,
        ));
    }
    bed_indices
}

/// Remap a speaker index to the Atmos channel-ID space.
/// IDs 0–9 are bed channels; 10+ are dynamic objects.
fn speaker_to_id(speaker_index: usize) -> usize {
    match speaker_index {
        0..8 => speaker_index,
        8..10 => speaker_index + 122,
        10..12 => speaker_index - 2,
        _ => speaker_index + 120,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ObjectNameKey {
    Bed(u8),
    Isf(usize),
    Dynamic(usize),
}

/// Build an [`RMetadataFrame`] from a parsed OAMD payload.
///
/// - `evo_base` = `total_samples + evo_sample_offset` (used for event sample_pos).
/// - `frame_sample_pos` = `total_samples` (used for OSC timing, no evo offset).
fn object_name_from_key(key: &ObjectNameKey) -> String {
    match key {
        ObjectNameKey::Bed(idx) => SpeakerLabels::from_u8(*idx)
            .map(|l| format!("{:?}", l))
            .unwrap_or_else(|| format!("Bed_{}", idx)),
        ObjectNameKey::Isf(i) => format!("ISF_{}", i),
        ObjectNameKey::Dynamic(i) => format!("Obj_{}", i),
    }
}

#[inline]
fn name_key_changed(cache: &mut Vec<Option<ObjectNameKey>>, id: u32, key: &ObjectNameKey) -> bool {
    let idx = id as usize;
    if idx >= cache.len() {
        cache.resize(idx + 1, None);
        cache[idx] = Some(key.clone());
        return true;
    }
    match &cache[idx] {
        Some(prev) if prev == key => false,
        _ => {
            cache[idx] = Some(key.clone());
            true
        }
    }
}

fn build_metadata_frame_from_oamd(
    oamd: &ObjectAudioMetadataPayload,
    evo_base: u64,
    frame_sample_pos: u64,
    name_key_cache: &mut Vec<Option<ObjectNameKey>>,
    _perf: &mut PerfStats,
) -> RMetadataFrame {
    #[cfg(feature = "bridge-perf")]
    let perf = _perf;
    #[cfg(feature = "bridge-perf")]
    let events_started = Instant::now();
    let events = extract_events(oamd, evo_base);
    #[cfg(feature = "bridge-perf")]
    perf.record_build_metadata_events(events_started.elapsed());

    #[cfg(feature = "bridge-perf")]
    let bed_indices_started = Instant::now();
    let bed_index_vec: Vec<usize> = oamd
        .program_assignment
        .bed_assignment
        .first()
        .map(|bed| bed.to_index_vec())
        .unwrap_or_default();
    let bed_indices: RVec<usize> = bed_index_vec.iter().map(|&i| speaker_to_id(i)).collect();
    #[cfg(feature = "bridge-perf")]
    perf.record_build_metadata_bed_indices(bed_indices_started.elapsed());

    #[cfg(feature = "bridge-perf")]
    let name_updates_started = Instant::now();
    let mut name_updates = RVec::new();
    let num_isf_objects = oamd.program_assignment.num_isf_objects;
    let num_dynamic_objects = oamd.program_assignment.num_dynamic_objects;
    for (idx, event) in events.iter().enumerate() {
        let id = event.id;
        let key = object_name_key_for_index(
            idx,
            id,
            &bed_index_vec,
            num_isf_objects,
            num_dynamic_objects,
        );
        if name_key_changed(name_key_cache, id, &key) {
            name_updates.push(bridge_api::RNameUpdate {
                id,
                name: object_name_from_key(&key).into(),
            });
        }
    }
    #[cfg(feature = "bridge-perf")]
    perf.record_build_metadata_name_updates(name_updates_started.elapsed());

    let ramp_duration = oamd
        .object_element
        .as_ref()
        .and_then(|e| e.md_update_info.block_update_info.first())
        .map(|b| b.ramp_duration as u32)
        .unwrap_or(0);

    RMetadataFrame {
        events,
        bed_indices,
        name_updates,
        sample_pos: frame_sample_pos,
        ramp_duration,
    }
}

#[inline]
fn object_name_key_for_index(
    object_index: usize,
    object_id: u32,
    bed_index_vec: &[usize],
    num_isf_objects: usize,
    num_dynamic_objects: usize,
) -> ObjectNameKey {
    let bed_count = bed_index_vec.len();
    if object_index < bed_count {
        return ObjectNameKey::Bed(bed_index_vec[object_index] as u8);
    }
    let isf_start = bed_count;
    let isf_end = isf_start + num_isf_objects;
    if object_index < isf_end {
        return ObjectNameKey::Isf(object_index - isf_start);
    }
    let dyn_start = isf_end;
    let dyn_end = dyn_start + num_dynamic_objects;
    if object_index < dyn_end {
        return ObjectNameKey::Dynamic(object_id as usize);
    }
    ObjectNameKey::Dynamic(object_id as usize)
}

/// Extract spatial events from a TrueHD OAMD frame.
fn extract_events(
    oamd: &ObjectAudioMetadataPayload,
    base_sample_pos: u64,
) -> RVec<bridge_api::REvent> {
    let object_count = oamd.object_count;
    let Some(object_element) = &oamd.object_element else {
        return RVec::new();
    };

    if object_element.md_update_info.num_obj_info_blocks != 1 {
        log::warn!(
            "atmos-bridge: unsupported OAMD with num_obj_info_blocks={} (expected 1); skipping metadata frame",
            object_element.md_update_info.num_obj_info_blocks
        );
        return RVec::new();
    }
    if oamd.program_assignment.bed_assignment.len() != 1 {
        log::warn!(
            "atmos-bridge: unsupported OAMD with bed_assignment_count={} (expected 1); skipping metadata frame",
            oamd.program_assignment.bed_assignment.len()
        );
        return RVec::new();
    }
    if oamd.program_assignment.num_isf_objects != 0 {
        log::warn!(
            "atmos-bridge: unsupported OAMD with num_isf_objects={} (expected 0); skipping metadata frame",
            oamd.program_assignment.num_isf_objects
        );
        return RVec::new();
    }

    let sample_offset = object_element.md_update_info.sample_offset as u64;
    let ramp_duration = object_element.md_update_info.block_update_info[0].ramp_duration as u32;
    let sample_pos = base_sample_pos + sample_offset;

    let pos_vec = oamd.get_damf_pos();
    let bed_index_vec = oamd
        .program_assignment
        .bed_assignment
        .first()
        .map(|b| b.to_index_vec())
        .unwrap_or_default();

    let mut events: RVec<bridge_api::REvent> = RVec::with_capacity(object_count);
    let mut missing_object_data = 0usize;
    let mut empty_object_blocks = 0usize;
    let mut bed_index_oob = 0usize;
    let mut missing_damf_pos = 0usize;

    for i in 0..object_count {
        let Some(object_blocks) = object_element.object_data.get(i) else {
            missing_object_data += 1;
            continue;
        };
        let Some(object_data) = object_blocks.first() else {
            empty_object_blocks += 1;
            continue;
        };

        let id = if object_data.b_object_in_bed_or_isf {
            let Some(&bed_idx) = bed_index_vec.get(i) else {
                bed_index_oob += 1;
                continue;
            };
            speaker_to_id(bed_idx) as u32
        } else {
            (i + 10 - bed_index_vec.len()) as u32
        };
        let (has_pos, pos, spread) = if !object_data.b_object_in_bed_or_isf {
            let render = &object_data.object_render_info;
            match pos_vec.get(i).and_then(|raw_blocks| raw_blocks.first()) {
                Some(raw) if raw.len() >= 3 => (
                    true,
                    [raw[0], raw[1], raw[2]],
                    (render.object_size[0] * 180.0).clamp(0.0, 180.0),
                ),
                Some(_) => (false, [0.0; 3], 0.0),
                None => {
                    missing_damf_pos += 1;
                    (false, [0.0; 3], 0.0)
                }
            }
        } else {
            (false, [0.0; 3], 0.0)
        };

        events.push(bridge_api::REvent {
            id,
            sample_pos,
            has_pos,
            pos,
            gain_db: object_data.object_basic_info.object_gain,
            spread,
            ramp_duration,
        });
    }

    if missing_object_data > 0 {
        log::warn!(
            "atmos-bridge: missing object_data for {} object(s) (object_count={}); skipped",
            missing_object_data,
            object_count
        );
    }
    if empty_object_blocks > 0 {
        log::warn!(
            "atmos-bridge: empty object_data blocks for {} object(s); skipped",
            empty_object_blocks
        );
    }
    if bed_index_oob > 0 {
        log::warn!(
            "atmos-bridge: bed index out-of-range for {} object(s) (bed_index_len={}); skipped",
            bed_index_oob,
            bed_index_vec.len()
        );
    }
    if missing_damf_pos > 0 {
        log::warn!(
            "atmos-bridge: missing DAMF position for {} object(s); positions omitted",
            missing_damf_pos
        );
    }

    events
}

/// Convert a floating-point PCM sample to the bridge's i32 PCM convention
/// (24-bit signed, stored in i32, matching the TrueHD decoder's range).
fn float_to_pcm_i32(sample: f32) -> i32 {
    if !sample.is_finite() {
        return 0;
    }
    const SCALE: f32 = 8_388_607.0; // 2^23 - 1
    (sample.clamp(-1.0, 1.0) * SCALE) as i32
}

#[cfg(test)]
mod tests {
    use super::PcmStats;
    use abi_stable::std_types::RVec;
    use bridge_api::{RChannelLabel, RDecodedFrame};

    fn decoded_frame(sample_count: u32, channel_count: u32, pcm: Vec<i32>) -> RDecodedFrame {
        RDecodedFrame {
            sampling_frequency: 48_000,
            sample_count,
            channel_count,
            pcm: pcm.into(),
            channel_labels: (0..channel_count)
                .map(|_| RChannelLabel::Unknown)
                .collect::<Vec<_>>()
                .into(),
            metadata: RVec::new(),
            dialogue_level: None.into(),
            is_new_segment: false,
        }
    }

    #[test]
    fn pcm_stats_accepts_matching_frame() {
        let frame = decoded_frame(2, 2, vec![0, 10, -20, 30]);
        let stats = PcmStats::from_frame(&frame).expect("frame should be valid");
        assert_eq!(stats.max_abs, 30);
        assert_eq!(stats.near_clip_count, 0);
    }

    #[test]
    fn pcm_stats_rejects_length_mismatch() {
        let frame = decoded_frame(2, 2, vec![0, 10, -20]);
        let err = PcmStats::from_frame(&frame).expect_err("frame should be rejected");
        assert!(err.starts_with("pcm_len_mismatch"));
    }

    #[test]
    fn pcm_stats_rejects_many_clipped_samples() {
        let frame = decoded_frame(16, 2, vec![i32::MAX; 32]);
        let err = PcmStats::from_frame(&frame).expect_err("frame should be rejected");
        assert!(err.starts_with("too_many_near_clip_samples"));
    }
}
