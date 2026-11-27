// `harletty info` on PCM input: a WAV a front end piped in, say from
// `ffmpeg -i film.flac -c:a pcm_s24le -f wav - | harletty --codec wav info -`.
//
// PCM says little about itself: the report is the format, and whether
// Auro-3D rides in its low bits. It stops as soon as that is decided, as the
// DTS report does: when a carrier is found, or when the carrier verdict is
// in and two seconds of audio went by without one.

use anyhow::Result;

use super::command::InfoArgs;
use super::decode::auro_stage::HOLD_LIMIT;
use super::decode::pcm_auro::Carrier;
use super::dts_info::{DEFAULT_MAX_SECONDS, auro_settled};
use super::info_report::{AuroFacts, InfoReport, Spatial};
use crate::input::InputReader;
use crate::pcm::{PcmFormat, PcmReader};

/// Sample frames read at a time: what the unfolder takes in one push.
const CHUNK_FRAMES: usize = auro::unfold::MAX_PUSH;

struct Survey {
    format: PcmFormat,
    max_seconds: f64,
    /// Sample frames read so far.
    frames: u64,
    /// Whether the carrier verdict is in: no carrier can still show up.
    auro_decided: bool,
    auro: Option<auro::Detection>,
}

impl Survey {
    fn seconds(&self) -> f64 {
        self.frames as f64 / f64::from(self.format.sample_rate.max(1))
    }

    fn done(&self) -> bool {
        auro_settled(
            self.auro.is_some(),
            self.auro_decided,
            self.seconds(),
            self.max_seconds,
        )
    }

    fn report(&self) -> InfoReport {
        let mut report = InfoReport::new();
        report.codec = Some("PCM");
        report.channels = Some(u32::from(self.format.channels));
        report.sample_rate = Some(self.format.sample_rate);
        report.frames_seen = self.frames;
        report.seconds_seen = self.seconds();
        if let Some(detection) = &self.auro {
            report.spatial = Some(Spatial {
                label: detection.original.source_codec_label().to_string(),
                kind: "auro",
                objects: Some(0),
                fixed: None,
                experimental: false,
                presentation: None,
            });
            report.auro = Some(AuroFacts {
                carrier: detection.carrier.name(),
                original: detection.original.name(),
            });
        }
        report
    }

    fn print(&self) {
        println!("Codec        : PCM");
        println!("Channels     : {}", self.format.channels);
        println!("Sample rate  : {} Hz", self.format.sample_rate);
        println!("Bit depth    : {}", self.format.bits);
        let spatial = match &self.auro {
            Some(detection) => format!(
                "{} ({} carried in {})",
                detection.original.source_codec_label(),
                detection.original.name().unwrap_or("?"),
                detection.carrier.name().unwrap_or("?")
            ),
            None => "none".to_string(),
        };
        println!("Spatial      : {spatial}");
        println!("Frames seen  : {} ({:.1} s)", self.frames, self.seconds());
    }
}

pub fn cmd_info_pcm(reader: InputReader, args: &InfoArgs) -> Result<()> {
    log::info!("Analyzing PCM stream: {}", args.input.display());
    let mut pcm = match PcmReader::new(reader) {
        Ok(pcm) => pcm,
        Err(e) => {
            if args.json {
                return InfoReport::not_found(&format!("{e:#}")).print();
            }
            println!("No PCM found in the input: {e:#}.");
            return Ok(());
        }
    };
    let format = pcm.format();
    let mut survey = Survey {
        format,
        max_seconds: args.max_seconds.unwrap_or(DEFAULT_MAX_SECONDS),
        frames: 0,
        auro_decided: false,
        auro: None,
    };
    // A carrier needs 24-bit samples of a known channel order: anything else
    // is a plain track from its first sample.
    let mut carrier = Carrier::new(format).ok();
    survey.auro_decided = carrier.is_none();
    let mut detector = carrier.as_ref().map(Carrier::detector);
    let mut bytes = Vec::new();
    while !survey.done() {
        let frames = pcm.read_frames(CHUNK_FRAMES, &mut bytes)?;
        if frames == 0 {
            break;
        }
        survey.frames += frames as u64;
        if let (Some(carrier), Some(detector), None) = (&mut carrier, &mut detector, survey.auro) {
            carrier.split(&bytes);
            for (channel, samples) in carrier.columns().iter().enumerate() {
                if let Some(detection) = detector.push(channel, samples) {
                    survey.auro = Some(detection);
                }
            }
            // The stage that unfolds gives a carrier this long to declare
            // itself; past that it is no carrier.
            if survey.frames > HOLD_LIMIT as u64 {
                survey.auro_decided = true;
            }
        }
    }
    if args.json {
        return survey.report().print();
    }
    survey.print();
    Ok(())
}
