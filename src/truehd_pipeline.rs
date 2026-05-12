use abi_stable::std_types::RVec;
use bridge_api::{RChannelLabel, RDecodedFrame, RMetadataFrame, RPushResult};
#[cfg(feature = "bridge-perf")]
use std::time::Instant;
use truehd::process::{
    MAX_PRESENTATIONS,
    decode::{DecodedAccessUnit, Decoder},
    extract::Extractor,
    parse::Parser,
};

use crate::bridge::{AtmosBridge, DrcMode};
use crate::labels::channel_label_to_r;
use crate::logging::panic_message;
use crate::metadata::{ObjectNameKey, build_metadata_frame_from_oamd};
use crate::perf::PerfStats;

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
    drc_mode: DrcMode,
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

                // Extract DRC parameters based on active mode. DRC updates are
                // typically authored on substream 0 (the 2-channel downmix),
                // even for full Atmos presentations, so scan from the active
                // presentation down to 0 and use the first substream that has
                // a DRC update for the selected mode.
                let mut drc_gain = 1.0f32;
                let mut drc_ramp_duration = 0u32;
                let mut drc_source_ss: Option<usize> = None;
                for i in (0..=ctx.presentation as usize).rev() {
                    let Some(ss_state) = ctx.parser.substream_state(i) else {
                        continue;
                    };
                    match ctx.drc_mode {
                        DrcMode::Standard if ss_state.drc_active => {
                            drc_gain = (ss_state.drc_gain_update as f32 * 0.015625).exp2();
                            drc_ramp_duration =
                                ((1 << ss_state.drc_time_update) * decoded.sample_length) as u32;
                            drc_source_ss = Some(i);
                            break;
                        }
                        DrcMode::Heavy if ss_state.heavy_drc_active => {
                            drc_gain = (ss_state.heavy_drc_gain_update as f32 * 0.03125).exp2();
                            drc_ramp_duration = ((1 << ss_state.heavy_drc_time_update)
                                * decoded.sample_length) as u32;
                            drc_source_ss = Some(i);
                            break;
                        }
                        _ => {}
                    }
                }
                if *ctx.frame_count % 96 == 0 {
                    let probe: Vec<_> = (0..=ctx.presentation as usize)
                        .filter_map(|i| {
                            ctx.parser.substream_state(i).map(|s| {
                                (
                                    i,
                                    s.drc_active,
                                    s.drc_gain_update,
                                    s.heavy_drc_active,
                                    s.heavy_drc_gain_update,
                                )
                            })
                        })
                        .collect();
                    eprintln!(
                        "[harletty][drc] mode={:?} src_ss={:?} gain={:.4} ramp={} state={:?}",
                        ctx.drc_mode, drc_source_ss, drc_gain, drc_ramp_duration, probe
                    );
                }

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
                frames.push(build_thd_frame(
                    ctx,
                    &decoded,
                    base_sample_pos,
                    drc_gain,
                    drc_ramp_duration,
                ));
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
    drc_gain: f32,
    drc_ramp_duration: u32,
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
        drc_gain,
        drc_ramp_duration,
        dialogue_level: (*ctx.current_dialogue_level).into(),
        is_new_segment: decoded.substream_info_changed,
    }
}

pub(crate) fn process_extractor_input(
    bridge: &mut AtmosBridge,
    input: &[u8],
    result: &mut RPushResult,
) {
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
            drc_mode: bridge.drc_mode,
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
