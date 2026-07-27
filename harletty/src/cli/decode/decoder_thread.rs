use super::processor::{AccessUnitShape, ProcessFramesContext, process_frames};
use crate::input::InputReader;
use anyhow::Result;
use indicatif::ProgressBar;
use std::sync::mpsc;
use std::thread;
use truehd::process::{decode::Decoder, extract::Extractor, parse::Parser};

pub struct DecoderThreadConfig {
    pub input_path: std::path::PathBuf,
    pub presentation: u8,
    pub strict_mode: bool,
    pub tx: mpsc::Sender<Result<truehd::process::decode::DecodedAccessUnit>>,
    pub pb_clone: Option<ProgressBar>,
    pub extractor: Extractor,
    pub parser: Parser,
    pub decoder: Decoder,
    pub prefix: Vec<u8>,
}

pub fn spawn_decoder_thread(config: DecoderThreadConfig) -> thread::JoinHandle<Result<()>> {
    thread::spawn(move || -> Result<()> {
        let DecoderThreadConfig {
            input_path,
            presentation,
            strict_mode,
            tx,
            pb_clone,
            mut extractor,
            mut parser,
            mut decoder,
            prefix,
        } = config;

        let mut frame_count: u64 = 0;
        let mut total_samples = 0u64;
        let mut frames_processed = 0;
        let mut current_substream_info: Option<u8> = None;
        let mut current_extended_substream_info: Option<u8> = None;
        let mut recovering_until_major_sync = false;
        let mut last_shape: Option<AccessUnitShape> = None;
        let mut gap_access_units = 0u64;
        let mut gap_samples = 0u64;

        if !prefix.is_empty() {
            extractor.push_bytes(&prefix);
        }

        let mut input_reader = InputReader::new(&input_path)?;

        input_reader.process_chunks(64 * 1024, |chunk| {
            extractor.push_bytes(chunk);

            let mut ctx = ProcessFramesContext {
                extractor: &mut extractor,
                parser: &mut parser,
                decoder: &mut decoder,
                frames_processed: &mut frames_processed,
                frame_count: &mut frame_count,
                total_samples: &mut total_samples,
                presentation,
                strict_mode,
                tx: &tx,
                pb_clone: &pb_clone,
                current_substream_info: &mut current_substream_info,
                current_extended_substream_info: &mut current_extended_substream_info,
                recovering_until_major_sync: &mut recovering_until_major_sync,
                last_shape: &mut last_shape,
                gap_access_units: &mut gap_access_units,
                gap_samples: &mut gap_samples,
            };

            let should_exit = process_frames(&mut ctx)?;

            Ok(!should_exit) // Convert exit signal to continue signal
        })?;

        log::info!("Processing complete: {frame_count} frames, {total_samples} samples");

        if gap_access_units > 0 {
            // Worth stating plainly: the output is not lossless any more, but
            // it is still in sync, which is the trade the alternative gets
            // wrong in the other direction.
            let sample_rate = last_shape
                .as_ref()
                .map_or(48000, AccessUnitShape::sampling_frequency)
                .max(1) as f64;
            log::warn!(
                "{gap_access_units} access units could not be decoded and were replaced with \
                 silence ({gap_samples} samples, {:.0} ms). Output length is preserved; \
                 audio stays aligned with the source timeline.",
                gap_samples as f64 / sample_rate * 1000.0
            );
        }

        Ok(())
    })
}
