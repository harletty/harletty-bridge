// `harletty info` on a DTS stream: what the first seconds establish about
// it, without decoding the rest. Reads a pipe as well as a file, so a
// player or a front end can ask `ffmpeg … -f dts - | harletty info -` what
// a track carries before committing to a full extraction.
//
// Reported: the codec (core only, or DTS-HD MA when an XLL substream
// follows), the bed, the spatial presentation the extension declares (the
// DTS:X profile, or an Auro-3D carrier found in the low bits), and how many
// frames were needed to say so.

use anyhow::Result;
use dca::{
    HdDecoder, HdError, PcmDecoder, XPresentation, exss_has_xll, exss_substream_size, parse_header,
};

use super::command::InfoArgs;
use super::decode::dts_handler::presentation_label;
use crate::input::InputReader;

const CORE_SYNC: [u8; 4] = dca::SYNCWORD_CORE_BE.to_be_bytes();
const SUBSTREAM_SYNC: [u8; 4] = dca::SYNCWORD_SUBSTREAM.to_be_bytes();
/// Seconds of audio inspected at most, for a stream whose lossless frames
/// are slow to come (peak-bitrate buffering).
const MAX_SECONDS: u64 = 20;
/// Bytes read at most, whatever the sample rate says.
const MAX_BYTES: usize = 96 * 1024 * 1024;

#[derive(Default)]
struct Survey {
    frames: u64,
    samples: u64,
    sample_rate: u32,
    lossless: bool,
    bed_channels: usize,
    presentation: Option<XPresentation>,
    auro: Option<auro::Detection>,
    auro_decided: bool,
    detector: Option<auro::detect::Detector>,
}

impl Survey {
    /// A DTS:X presentation shows on the first lossless frame and an Auro-3D
    /// carrier declares itself within its first blocks, and a stream is one
    /// or the other; a plain track is called plain once the carrier verdict
    /// is in and two seconds went by without a presentation.
    fn done(&self) -> bool {
        let seconds = self.samples / u64::from(self.sample_rate.max(1));
        self.presentation.is_some()
            || self.auro.is_some()
            || (self.auro_decided && seconds >= 2)
            || seconds >= MAX_SECONDS
    }

    fn hd_frame(&mut self, frame: &dca::HdFrame, decoder: &HdDecoder) {
        self.frames += 1;
        self.lossless = true;
        self.sample_rate = frame.sample_rate;
        self.bed_channels = frame.samples.iter().filter(|s| s.is_some()).count();
        let n = frame.bed_sample_count() as u64;
        if let Some(presentation) = XPresentation::detect(frame) {
            self.presentation = Some(presentation);
        }
        if !self.auro_decided {
            let detector = self
                .detector
                .get_or_insert_with(|| auro::detect::Detector::new(frame.samples.len()));
            for (speaker, samples) in decoder.lossless_samples() {
                if let Some(detection) = detector.push(speaker, samples) {
                    self.auro = Some(detection);
                    self.auro_decided = true;
                }
            }
            // The stage that unfolds gives a carrier this long to declare
            // itself; past that it is played as it is.
            if self.samples + n > (3 * auro::block::MAX_BLOCK + 4096) as u64 {
                self.auro_decided = true;
            }
        }
        self.samples += n;
    }

    fn core_frame(&mut self, push: &dca::PcmPushResult) {
        self.frames += 1;
        self.auro_decided = true;
        if self.sample_rate == 0 {
            self.sample_rate = push.pcm.sample_rate;
        }
        if self.bed_channels == 0 {
            self.bed_channels =
                push.pcm.fullband_channels.len() + usize::from(push.pcm.lfe_channel.is_some());
        }
        self.samples += push
            .pcm
            .fullband_channels
            .first()
            .map_or(0, |channel| channel.len() as u64);
    }

    fn print(&self) {
        println!(
            "Codec        : {}",
            if self.lossless { "DTS-HD MA" } else { "DTS" }
        );
        println!("Channels     : {}", self.bed_channels);
        println!("Sample rate  : {} Hz", self.sample_rate);
        let spatial = if let Some(presentation) = self.presentation {
            let objects = presentation.object_feeds().len();
            let fixed = presentation.fixed_feeds().len();
            format!(
                "{} ({presentation:?}: {objects} object{}, {fixed} fixed height{})",
                presentation_label(presentation),
                if objects == 1 { "" } else { "s" },
                if fixed == 1 { "" } else { "s" }
            )
        } else if let Some(detection) = &self.auro {
            format!(
                "{} ({} carried in {})",
                detection.original.source_codec_label(),
                detection.original.name().unwrap_or("?"),
                detection.carrier.name().unwrap_or("?")
            )
        } else {
            "none".to_string()
        };
        println!("Spatial      : {spatial}");
        println!(
            "Frames seen  : {} ({:.1} s)",
            self.frames,
            self.samples as f64 / f64::from(self.sample_rate.max(1))
        );
    }
}

pub fn cmd_info_dts(reader: &mut InputReader, prefix: Vec<u8>, args: &InfoArgs) -> Result<()> {
    log::info!("Analyzing DTS stream: {}", args.input.display());
    let mut buffer = prefix;
    let mut core = PcmDecoder::new();
    let mut hd = HdDecoder::new();
    let mut survey = Survey::default();
    let mut total = buffer.len();
    let mut chunk = vec![0u8; 256 * 1024];
    loop {
        drain(&mut buffer, &mut core, &mut hd, &mut survey);
        if survey.done() || total >= MAX_BYTES {
            break;
        }
        let read = reader.read_chunk(&mut chunk)?;
        if read == 0 {
            break;
        }
        total += read;
        buffer.extend_from_slice(&chunk[..read]);
    }
    if survey.frames == 0 {
        println!("No DTS frame found in the input.");
        return Ok(());
    }
    survey.print();
    Ok(())
}

/// Decode every complete frame in `buffer`, then drop the consumed bytes.
fn drain(buffer: &mut Vec<u8>, core: &mut PcmDecoder, hd: &mut HdDecoder, survey: &mut Survey) {
    let mut consumed = 0usize;
    loop {
        let rest = &buffer[consumed..];
        let Some(offset) = rest.windows(4).position(|w| w == CORE_SYNC) else {
            consumed += rest.len().saturating_sub(3);
            break;
        };
        consumed += offset;
        let rest = &buffer[consumed..];
        let info = match parse_header(rest) {
            Ok(info) => info,
            Err(dca::HeaderParseError::InsufficientData) => break,
            Err(_) => {
                consumed += 4;
                continue;
            }
        };
        let core_size = info.frame_size;
        if rest.len() < core_size + 4 {
            break;
        }
        let mut frame_size = core_size;
        let mut exss = None;
        if rest[core_size..core_size + 4] == SUBSTREAM_SYNC {
            let Some(exss_size) = exss_substream_size(&rest[core_size..]) else {
                break;
            };
            if rest.len() < core_size + exss_size {
                break;
            }
            frame_size = core_size + exss_size;
            let candidate = &rest[core_size..frame_size];
            if exss_has_xll(candidate) {
                exss = Some(candidate);
            }
        }
        let decoded_lossless = match exss.map(|exss| hd.decode(&rest[..core_size], exss)) {
            Some(Ok(frame)) => {
                survey.hd_frame(&frame, hd);
                true
            }
            Some(Err(HdError::Pending)) => true,
            Some(Err(_)) | None => false,
        };
        if !decoded_lossless {
            if let Ok(push) = core.push_access_unit(&rest[..core_size]) {
                survey.core_frame(&push);
            }
        }
        consumed += frame_size;
        if survey.done() {
            break;
        }
    }
    buffer.drain(..consumed);
}
