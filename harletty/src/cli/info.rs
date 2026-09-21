use anyhow::Result;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use log::Level;

use super::command::{Cli, InfoArgs};
use super::info_report::{Eac3Facts, InfoReport, Signature, Spatial, TrueHdFacts};
use crate::codec_probe::{Codec, probe_codec};
use crate::input::InputReader;
use crate::settings::Settings;
use crate::signature::{Outcome, State, Verifier};
use crate::timestamp::time_str;
use truehd::process::{
    PresentationMap, PresentationType,
    extract::{Extractor, Frame},
    parse::Parser,
};
use truehd::structs::access_unit::AccessUnit;
use truehd::structs::channel::{ChannelGroup, ChannelLabel};

pub fn cmd_info(args: &InfoArgs, cli: &Cli, multi: Option<&MultiProgress>) -> Result<()> {
    let mut probe_reader = InputReader::new(&args.input)?;
    let (codec, prefix) = probe_codec(&mut probe_reader, cli.codec)?;

    if codec == Codec::Dts {
        // Keeps reading the same handle: the input may be a pipe.
        return super::dts_info::cmd_info_dts(&mut probe_reader, prefix, args);
    }

    if args.matrices || args.params || args.filters || args.stats {
        drop(probe_reader);
        return super::inspect::run(args, cli);
    }

    // The report reads the input from its first byte. The probe already
    // took up to eight kilobytes of it: a file is simply opened again, but a
    // pipe cannot be, and a short head piped in — a catalogue probe sends a
    // few hundred bytes of TrueHD — would otherwise be gone entirely.
    let reader = if probe_reader.is_pipe() {
        probe_reader.replaying(prefix)
    } else {
        drop(probe_reader);
        InputReader::new(&args.input)?
    };

    if codec == Codec::Eac3 {
        return cmd_info_eac3(args, reader);
    }

    log::info!("Analyzing TrueHD stream: {}", args.input.display());

    // No key on this machine: the stream is read as it always was, and the
    // report says the signature was not checked rather than claiming
    // anything about it.
    let verifier = if args.no_signature {
        None
    } else {
        let settings = Settings::load(cli.config.as_deref())?;
        settings
            .evolution_key
            .map(|key| Verifier::new(key, settings.from.as_deref()))
    };

    let analysis_result =
        analyze_stream(reader, cli, multi, args.json, args.max_seconds, verifier)?;

    if args.json {
        return truehd_report(analysis_result.as_ref()).print();
    }

    match analysis_result {
        Some((stream_info, _timestamp, frame_count, total_bytes, signature)) => {
            // Final update with total frames and duration
            update_final_stats(&stream_info, frame_count, total_bytes, &signature);
        }
        None => {
            println!("No TrueHD major sync found in the file.");
            println!("This doesn't appear to be a valid TrueHD stream.");
        }
    }

    Ok(())
}

fn cmd_info_eac3(args: &InfoArgs, mut reader: InputReader) -> Result<()> {
    log::info!("Analyzing EAC3 stream: {}", args.input.display());

    let mut extractor = eac3::Extractor::default();
    // Spectral Extension shows only in a decoded frame; the text report
    // never paid for that decode and still does not.
    let mut decoder = args.json.then(eac3::PcmDecoder::new);
    let mut joc_seen = false;
    let mut oamd_seen = false;
    let mut spx_seen = false;
    let mut first_info: Option<eac3::AccessUnitInfo> = None;
    // The header of the first independent frame, for the report: a dependent
    // frame describes only the channels it adds to the programme.
    let mut first_header: Option<eac3::FrameInfo> = None;
    let mut frames = 0u64;
    // Samples of the independent frames, for the bound; a dependent frame
    // extends the same audio and is not counted twice.
    let mut samples = 0u64;

    reader.process_chunks(64 * 1024, |chunk| {
        extractor.push_bytes(chunk);
        while let Some(frame) = match extractor.next_frame() {
            Ok(f) => f,
            Err(_) => None,
        } {
            frames += 1;
            let header = frame.info();
            if header.stream_type != eac3::StreamType::Dependent {
                samples += u64::from(header.samples);
                if first_header.is_none() {
                    first_header = Some(header);
                }
            }
            if let Ok(info) = eac3::inspect_access_unit(frame.as_bytes()) {
                for payload in info.payloads() {
                    match payload.parsed {
                        eac3::ParsedEmdfPayloadData::Joc(_) => joc_seen = true,
                        eac3::ParsedEmdfPayloadData::Oamd(_) => oamd_seen = true,
                        _ => {}
                    }
                }
                if first_info.is_none() {
                    first_info = Some(info);
                }
            }
            if let Some(decoder) = decoder.as_mut() {
                if decoder.push_access_unit(frame.as_bytes()).is_ok() && decoder.last_spx_in_use() {
                    spx_seen = true;
                }
            }
            if let (Some(max), Some(header)) = (args.max_seconds, first_header.as_ref()) {
                if header.sample_rate > 0 && samples as f64 / f64::from(header.sample_rate) >= max {
                    return Ok(false);
                }
            }
            if frames > 200 && joc_seen && oamd_seen {
                return Ok(false);
            }
        }
        Ok(true)
    })?;

    if args.json {
        let Some(header) = first_header else {
            return InfoReport::not_found("no independent EAC3 frame found in the input").print();
        };
        let mut report = InfoReport::new();
        report.codec = Some("EAC3");
        report.channels = Some(u32::from(header.channels()));
        report.sample_rate = Some(header.sample_rate);
        report.spatial = joc_seen.then(|| Spatial {
            label: damf::SourceCodec::Eac3Joc.label().to_string(),
            kind: "joc",
            objects: None,
            fixed: None,
            experimental: false,
            presentation: None,
        });
        report.eac3 = Some(Eac3Facts {
            oamd: oamd_seen,
            joc: joc_seen,
            spx: spx_seen,
            bitstream_id: header.bitstream_id,
        });
        report.frames_seen = frames;
        report.seconds_seen = samples as f64 / f64::from(header.sample_rate.max(1));
        return report.print();
    }

    if let Some(info) = first_info {
        println!("Codec        : EAC3 (Dolby Digital Plus)");
        println!("Bitstream ID : {}", info.bitstream_id);
        println!("Frame type   : {}", info.frame_type);
        println!("Sample rate  : {} Hz", info.sample_rate);
        println!(
            "Channel mode : {} ch + {}",
            info.fullband_channels,
            if info.lfe_on { "LFE" } else { "no LFE" }
        );
        println!("OAMD         : {}", if oamd_seen { "yes" } else { "no" });
        println!("JOC          : {}", if joc_seen { "yes" } else { "no" });
        println!("Frames seen  : {frames}");
    } else {
        println!("No EAC3 syncframe found in the file.");
    }
    Ok(())
}

type AnalysisResultTuple = (
    AnalysisResult,
    Option<truehd::structs::timestamp::Timestamp>,
    usize,
    usize,
    Outcome,
);

/// The TrueHD facts of `analysis`, for `--json`: the bed's channel count is
/// that of the highest channel-based presentation the stream declares
/// (presentation 2 when there are three or more substreams), the spatial
/// block is present when the major sync flags Atmos.
fn truehd_report(analysis: Option<&AnalysisResultTuple>) -> InfoReport {
    let Some((analysis, _, frame_count, _, signature)) = analysis else {
        return InfoReport::not_found("no TrueHD major sync found in the input");
    };
    let major_sync = analysis
        .access_unit
        .major_sync_info
        .as_ref()
        .expect("an analysis result carries its major sync");
    let map = PresentationMap::for_format_sync(
        major_sync.format_sync,
        major_sync.substream_info,
        major_sync.extended_substream_info,
    );
    let presentations =
        PresentationBuilder::new(major_sync, &analysis.access_unit).build_all_presentations();
    let channel_presentation = major_sync.substreams.min(3).saturating_sub(1);
    let seconds = major_sync
        .format_info
        .samples_per_au()
        .map(|samples_per_au| {
            (*frame_count * samples_per_au) as f64
                / f64::from(analysis.stream_info.sampling_frequency.max(1))
        })
        .unwrap_or(0.0);

    let mut report = InfoReport::new();
    report.codec = Some("TrueHD");
    report.channels = presentations
        .get(channel_presentation)
        .map(|presentation| u32::from(presentation.channels));
    report.sample_rate = Some(analysis.stream_info.sampling_frequency);
    report.spatial = analysis.stream_info.is_atmos.then(|| Spatial {
        label: damf::SourceCodec::TrueHD.label().to_string(),
        kind: "atmos",
        objects: None,
        fixed: None,
        experimental: false,
        presentation: None,
    });
    report.truehd = Some(TrueHdFacts {
        max_presentation: map
            .max_independent_presentation()
            .and_then(|presentation| u8::try_from(presentation).ok()),
        atmos: analysis.stream_info.is_atmos,
        substreams: major_sync.substreams as u32,
    });
    report.signature = Some(Signature {
        state: signature.state().as_str(),
        units: signature.tally.units,
        frames: signature.tally.frames,
        checked: signature.tally.checked,
        verified: signature.tally.verified,
        mismatched: signature.tally.mismatched,
    });
    report.frames_seen = *frame_count as u64;
    report.seconds_seen = seconds;
    report
}

/// Read the stream: every frame when `max_seconds` is `None`, otherwise
/// about that much audio past the major sync. `quiet` keeps the running
/// report off stdout, for a caller that wants the JSON alone.
fn analyze_stream(
    mut input_reader: InputReader,
    cli: &Cli,
    multi: Option<&MultiProgress>,
    quiet: bool,
    max_seconds: Option<f64>,
    verifier: Option<Verifier>,
) -> Result<Option<AnalysisResultTuple>> {
    let mut extractor = Extractor::default();
    let mut parser = Parser::default();

    // Configure fail level based on strict mode
    let fail_level = if cli.strict {
        Level::Warn
    } else {
        Level::Error
    };
    parser.set_fail_level(fail_level);

    let mut context = AnalysisContext {
        quiet,
        max_seconds,
        verifier,
        ..AnalysisContext::default()
    };

    // Create progress bar for frame counting if enabled
    if let Some(multi) = multi {
        let pb = multi.add(ProgressBar::new_spinner());
        pb.set_style(ProgressStyle::with_template("{spinner:.green} {msg}")?);
        pb.enable_steady_tick(std::time::Duration::from_millis(100));
        pb.set_message("Analyzing frames...");
        context.pb = Some(pb);
    }

    input_reader.process_chunks(64 * 1024, |chunk| {
        context.total_bytes += chunk.len();
        extractor.push_bytes(chunk);

        for frame_result in extractor.by_ref() {
            let frame = match frame_result {
                Ok(frame) => frame,
                Err(_) => continue,
            };

            context.process_frame(&frame, &mut parser, cli)?;
            if context.stop {
                return Ok(false);
            }
        }

        Ok(true)
    })?;

    Ok(context.into_result())
}

#[derive(Default)]
struct AnalysisContext {
    timestamp: Option<truehd::structs::timestamp::Timestamp>,
    analysis_result: Option<AnalysisResult>,
    hires_timing_displayed: bool,
    frame_count: usize,
    info_displayed: bool,
    pb: Option<ProgressBar>,
    total_bytes: usize,
    /// Nothing on stdout while reading.
    quiet: bool,
    /// Stop once this much audio has gone by, counted from the first frame.
    max_seconds: Option<f64>,
    /// Set once the bound is reached.
    stop: bool,
    /// The signature check, when this machine has a key. Its presence is
    /// what makes every access unit parsed rather than only the first few:
    /// the digest is over one unit each, so there is nothing to check in a
    /// unit that was not parsed.
    verifier: Option<Verifier>,
}

struct AnalysisResult {
    stream_info: StreamInfo,
    access_unit: AccessUnit,
    hires_timing: Option<u32>,
}

impl AnalysisContext {
    fn process_frame(&mut self, frame: &Frame, parser: &mut Parser, cli: &Cli) -> Result<()> {
        if self.analysis_result.is_none() || !self.hires_timing_displayed || self.verifier.is_some()
        {
            match parser.parse(frame) {
                Ok(access_unit) => {
                    if let Some(verifier) = self.verifier.as_mut() {
                        verifier.check(&access_unit, frame.as_ref(), frame.index);
                    }

                    if let Some(ts) = &frame.timestamp {
                        if self.timestamp.is_none() {
                            self.timestamp = Some(ts.clone());
                        }
                    }

                    if let Some(major_sync) = &access_unit.major_sync_info {
                        if self.analysis_result.is_none() {
                            let stream_info = StreamInfo::from_major_sync(major_sync)?;
                            self.analysis_result = Some(AnalysisResult {
                                stream_info,
                                access_unit,
                                hires_timing: None,
                            });

                            // Display immediate info now that we have the major sync
                            if !self.info_displayed {
                                if !self.quiet {
                                    self.display_immediate_info();
                                }
                                self.info_displayed = true;
                            }
                        }

                        if !self.hires_timing_displayed {
                            if let Some(timing) = parser.hires_output_timing() {
                                if let Some(result) = &mut self.analysis_result {
                                    result.hires_timing = Some(timing as u32);
                                }

                                // Print trim detection immediately when available
                                // Temporarily pause progress bar for clean output
                                if self.quiet {
                                } else if let Some(ref pb) = self.pb {
                                    pb.suspend(|| {
                                        print!("Trim detection              ");
                                        if timing != 0 {
                                            println!("{timing} samples are trimmed from the beginning of the stream");
                                        } else {
                                            println!("No trimmed samples detected");
                                        }
                                        println!();
                                    });
                                } else {
                                    print!("Trim detection              ");
                                    if timing != 0 {
                                        println!(
                                            "{timing} samples are trimmed from the beginning of the stream"
                                        );
                                    } else {
                                        println!("No trimmed samples detected");
                                    }
                                    println!();
                                }

                                self.hires_timing_displayed = true;
                            }
                        }
                    }
                }
                Err(e) => {
                    if cli.strict {
                        return Err(e);
                    }
                    log::warn!("Parse error at frame {}: {e}", self.frame_count);
                }
            }
        }

        self.frame_count += 1;

        if let (Some(max), Some(result)) = (self.max_seconds, &self.analysis_result) {
            let major_sync = result
                .access_unit
                .major_sync_info
                .as_ref()
                .expect("an analysis result carries its major sync");
            if let Ok(samples_per_au) = major_sync.format_info.samples_per_au() {
                let seconds = (self.frame_count * samples_per_au) as f64
                    / f64::from(result.stream_info.sampling_frequency.max(1));
                if seconds >= max {
                    self.stop = true;
                }
            }
        }

        if self.frame_count.is_multiple_of(100) {
            if let Some(ref pb) = self.pb {
                pb.set_message(format!("Analyzing frames...       {}", self.frame_count));
                pb.tick();
            }
        }

        Ok(())
    }

    fn display_immediate_info(&self) {
        if let Some(ref analysis) = self.analysis_result {
            if let Some(ref pb) = self.pb {
                pb.suspend(|| {
                    println!();
                    println!("TrueHD Stream Information");
                    println!("=========================");
                    println!();

                    if let Some(ts) = &self.timestamp {
                        println!("SMPTE Timestamp             {ts}");
                        println!();
                    }

                    display_stream_info(&analysis.stream_info);
                    display_presentations(&analysis.access_unit);
                });
            } else {
                println!();
                println!("TrueHD Stream Information");
                println!("=========================");
                println!();

                if let Some(ts) = &self.timestamp {
                    println!("SMPTE Timestamp             {ts}");
                    println!();
                }

                display_stream_info(&analysis.stream_info);
                display_presentations(&analysis.access_unit);
            }
        }
    }

    fn into_result(self) -> Option<AnalysisResultTuple> {
        // Finish progress bar
        if let Some(ref pb) = self.pb {
            pb.finish_and_clear();
        }

        let signature = self.verifier.map(Verifier::finish).unwrap_or_default();

        self.analysis_result.map(|result| {
            (
                result,
                self.timestamp,
                self.frame_count,
                self.total_bytes,
                signature,
            )
        })
    }
}

fn update_final_stats(
    analysis: &AnalysisResult,
    frame_count: usize,
    total_bytes: usize,
    signature: &Outcome,
) {
    println!("Analysis Summary");
    println!("  Frames processed          {frame_count}");

    // Format file size
    let size_mb = total_bytes as f64 / 1_000_000.0;
    println!("  Size                      {size_mb:.2} MB ({total_bytes} bytes)");

    // Calculate and display duration
    if let Ok(samples_per_au) = analysis
        .access_unit
        .major_sync_info
        .as_ref()
        .unwrap()
        .format_info
        .samples_per_au()
    {
        let total_samples = frame_count * samples_per_au;
        let duration_secs = total_samples as f64 / analysis.stream_info.sampling_frequency as f64;
        let duration_str = time_str(duration_secs);
        println!("  Duration                  {duration_str}");

        // Calculate average data rate
        if duration_secs > 0.0 {
            let avg_data_rate_kbps = (total_bytes as f64 * 8.0) / (duration_secs * 1000.0);
            println!("  Average data rate         {avg_data_rate_kbps:.1} kbps");
        }
    }

    print!("  Signature                 ");
    println!("{}", describe(signature));

    println!();
}

/// The signature line of the summary: what was asked, and of how much.
fn describe(signature: &Outcome) -> String {
    let tally = &signature.tally;
    let from = signature
        .from
        .as_ref()
        .map(|path| format!(" (key from {})", path.display()))
        .unwrap_or_default();
    match signature.state() {
        State::Unchecked => "not checked: no key configured, see `--config`".to_string(),
        State::Absent => format!(
            "absent: no Evolution frame in {} access unit(s), so there is nothing to sign",
            tally.units
        ),
        State::Unsigned => format!(
            "none: {} Evolution frame(s) carry no protection word{from}",
            tally.frames
        ),
        State::Verified => format!(
            "verified: {} of {} protection word(s) are the key's digest{from}",
            tally.verified, tally.checked
        ),
        State::Mismatch => format!(
            "MISMATCH: {} of {} protection word(s) are not the key's digest{from}",
            tally.mismatched, tally.checked
        ),
    }
}

struct StreamInfo {
    format_sync: String,
    sampling_frequency: u32,
    variable_rate: bool,
    peak_data_rate: u32,
    substreams: usize,
    is_atmos: bool,
}

impl StreamInfo {
    fn from_major_sync(major_sync: &truehd::structs::sync::MajorSyncInfo) -> Result<Self> {
        Ok(Self {
            format_sync: format!("{:08X}", major_sync.format_sync),
            sampling_frequency: major_sync.format_info.sampling_frequency_1()?,
            variable_rate: major_sync.variable_rate,
            peak_data_rate: (major_sync.peak_data_rate as u32
                * major_sync.format_info.sampling_frequency_1()?)
                / 16000,
            substreams: major_sync.substreams,
            is_atmos: major_sync.substream_info >> 7 != 0,
        })
    }
}

fn display_stream_info(info: &StreamInfo) {
    println!("Stream Information");
    println!("  Format Sync               {}", info.format_sync);
    println!("  Sampling rate             {} Hz", info.sampling_frequency);
    println!("  Variable rate             {}", info.variable_rate);
    println!("  Peak data rate            {} kbps", info.peak_data_rate);
    println!("  Number of substreams      {}", info.substreams);
    println!("  Dolby Atmos               {}", info.is_atmos);
    println!();
}

#[derive(Default, Clone)]
struct PresentationInfo {
    index: usize,
    channels: u8,
    presentation_type: Option<PresentationType>,
    twoch_format: Option<ChannelGroup>,
    sixch_ex: Option<String>,
    assignments: Vec<ChannelLabel>,
    control: Option<bool>,
    dialogue_level: i8,
    mix_level: u8,
    // 16ch
    chan_distribution: Option<bool>,
}

fn display_presentation_info(info: &PresentationInfo) {
    println!("  Presentation {}", info.index);

    display_basic_info(info);
    display_format_info(info);
    display_channel_info(info);
    display_audio_control_info(info);
}

fn display_basic_info(info: &PresentationInfo) {
    let entity_type = if info.index == 3 {
        "elements"
    } else {
        "channels"
    };
    println!("    Number of {entity_type:10}    {}", info.channels);

    if let Some(presentation_type) = &info.presentation_type {
        println!("    Presentation type       {presentation_type}");
    }
}

fn display_format_info(info: &PresentationInfo) {
    if let Some(format) = &info.twoch_format {
        println!("    Channel format          {format}");
    }

    if let Some(ex) = &info.sixch_ex {
        println!("    Dolby Surround EX       {ex}");
    }
}

fn display_channel_info(info: &PresentationInfo) {
    if !info.assignments.is_empty() {
        let label = if info.index == 3 {
            "Bed configuration "
        } else {
            "Channel assignment"
        };
        let assignments = info
            .assignments
            .iter()
            .map(|c| format!("{c:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!("    {label:20}    {assignments}");
    }
}

fn display_audio_control_info(info: &PresentationInfo) {
    if let Some(control) = info.control {
        println!("    DRC on by default       {control}");
    }

    println!(
        "    Dialogue Level          {:>3} dBFS",
        info.dialogue_level
    );
    println!("    Mix Level               {:>3} dB", info.mix_level);

    if let Some(chan_distribution) = &info.chan_distribution {
        println!("    Channel distribution    {chan_distribution}");
    }
}

fn display_presentations(access_unit: &AccessUnit) {
    println!("Presentation Information");
    let major_sync = access_unit.major_sync_info.as_ref().unwrap();

    let presentation_builder = PresentationBuilder::new(major_sync, access_unit);
    let presentations = presentation_builder.build_all_presentations();

    for presentation in presentations {
        display_presentation_info(&presentation);
    }
    println!();
}

struct PresentationBuilder<'a> {
    major_sync: &'a truehd::structs::sync::MajorSyncInfo,
    access_unit: &'a AccessUnit,
    presentation_map: PresentationMap,
}

impl<'a> PresentationBuilder<'a> {
    fn new(
        major_sync: &'a truehd::structs::sync::MajorSyncInfo,
        access_unit: &'a AccessUnit,
    ) -> Self {
        // `with_substream_info` is the derivation of one syntax alone, and applying it
        // to the other invents presentations the stream does not declare.
        let presentation_map = PresentationMap::for_format_sync(
            major_sync.format_sync,
            major_sync.substream_info,
            major_sync.extended_substream_info,
        );

        Self {
            major_sync,
            access_unit,
            presentation_map,
        }
    }

    fn build_all_presentations(&self) -> Vec<PresentationInfo> {
        let mut presentations = Vec::new();
        let mut last_presentation = PresentationInfo::default();

        for index in 0..self.major_sync.substreams.max(3) {
            let presentation = if index < self.major_sync.substreams {
                let info = self.build_presentation_for_substream(index);
                last_presentation = info.clone();
                info
            } else {
                last_presentation.clone()
            };

            presentations.push(self.finalize_presentation(presentation, index));
        }

        presentations
    }

    fn build_presentation_for_substream(&self, index: usize) -> PresentationInfo {
        let mut presentation = PresentationInfo {
            channels: self.access_unit.substream_segment[index].block[0]
                .restart_header
                .as_ref()
                .unwrap()
                .max_matrix_chan
                + 1,
            ..Default::default()
        };

        match index {
            0 => self.configure_twoch_presentation(&mut presentation),
            1 => self.configure_sixch_presentation(&mut presentation),
            2 => self.configure_eightch_presentation(&mut presentation),
            3 => self.configure_sixteench_presentation(&mut presentation),
            _ => unreachable!(),
        }

        presentation
    }

    fn configure_twoch_presentation(&self, presentation: &mut PresentationInfo) {
        let format_info = &self.major_sync.format_info;

        presentation.twoch_format =
            Some(ChannelGroup::from_modifier(format_info.twoch_decoder_channel_modifier).unwrap());

        let Some(channel_meaning) = self.major_sync.channel_meaning.fba() else {
            return;
        };

        presentation.control = Some(channel_meaning.twoch_control_enabled);
        presentation.dialogue_level = -(channel_meaning.twoch_dialogue_norm as i8);
        presentation.mix_level = channel_meaning.twoch_mix_level + 70;
    }

    fn configure_sixch_presentation(&self, presentation: &mut PresentationInfo) {
        let format_info = &self.major_sync.format_info;

        let assignment = format_info.sixch_decoder_channel_assignment;
        if assignment == 1 {
            presentation.twoch_format = Some(
                ChannelGroup::from_modifier(format_info.twoch_decoder_channel_modifier).unwrap(),
            );
        }

        if assignment & 8 != 0 {
            presentation.sixch_ex = Some(
                match format_info.twoch_decoder_channel_modifier {
                    0 => "Not indicated",
                    1 => "Not encoded",
                    2 => "Encoded",
                    _ => "Reserved",
                }
                .to_string(),
            );
        }

        presentation.assignments =
            ChannelLabel::from_sixch_channel(format_info.sixch_decoder_channel_assignment).unwrap();

        let Some(channel_meaning) = self.major_sync.channel_meaning.fba() else {
            return;
        };

        presentation.control = Some(channel_meaning.sixch_control_enabled);
        presentation.dialogue_level = -(channel_meaning.sixch_dialogue_norm as i8);
        presentation.mix_level = channel_meaning.sixch_mix_level + 70;
    }

    fn configure_eightch_presentation(&self, presentation: &mut PresentationInfo) {
        let format_info = &self.major_sync.format_info;

        presentation.assignments = ChannelLabel::from_eightch_channel(
            format_info.eightch_decoder_channel_assignment,
            self.major_sync.flags,
        )
        .unwrap();

        let Some(channel_meaning) = self.major_sync.channel_meaning.fba() else {
            return;
        };

        presentation.control = Some(channel_meaning.eightch_control_enabled);
        presentation.dialogue_level = -(channel_meaning.eightch_dialogue_norm as i8);
        presentation.mix_level = channel_meaning.eightch_mix_level + 70;
    }

    fn configure_sixteench_presentation(&self, presentation: &mut PresentationInfo) {
        let Some(extra) = self.major_sync.channel_meaning.extra_channel_meaning() else {
            return;
        };

        presentation.dialogue_level = -(extra.sixteench_dialogue_norm as i8);
        presentation.mix_level = extra.sixteench_mix_level + 70;

        if extra.dyn_object_only && extra.lfe_present {
            presentation.assignments = vec![ChannelLabel::LFE];
        } else {
            let desc = extra.sixteench_content_description;

            if desc & 1 != 0 {
                presentation.chan_distribution = Some(extra.chan_distribute);
                if !extra.lfe_only {
                    presentation.assignments =
                        ChannelLabel::from_sixteenth_channel(extra.sixteench_channel_assignment)
                            .unwrap();
                }
            }
        }
    }

    fn finalize_presentation(
        &self,
        mut presentation: PresentationInfo,
        index: usize,
    ) -> PresentationInfo {
        presentation.index = index;
        presentation.presentation_type =
            Some(self.presentation_map.presentation_type_by_index(index));
        presentation
    }
}
