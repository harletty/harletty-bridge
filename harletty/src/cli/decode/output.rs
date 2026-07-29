use damf::caf::CAFWriter;
use damf::wav::WAVWriter;
use anyhow::Result;
use std::fs::File;
use std::io::{BufWriter, Seek, Write};
use std::path::{Path, PathBuf};

use super::super::command::AudioFormat;

pub fn create_path_with_suffix(base_path: &Path, suffix: &str) -> PathBuf {
    let mut path = base_path.to_path_buf();
    let new_name = format!(
        "{}.{}",
        base_path.file_name().unwrap().to_string_lossy(),
        suffix
    );
    path.set_file_name(new_name);
    path
}

pub fn create_path_with_extension(base_path: &Path, expected_ext: &str) -> PathBuf {
    if let Some(existing_ext) = base_path.extension() {
        if existing_ext == expected_ext {
            base_path.to_path_buf()
        } else {
            let mut path = base_path.to_path_buf();
            let new_name = format!(
                "{}.{}",
                base_path.file_name().unwrap().to_string_lossy(),
                expected_ext
            );
            path.set_file_name(new_name);
            path
        }
    } else {
        let mut path = base_path.to_path_buf();
        path.set_extension(expected_ext);
        path
    }
}

pub fn create_output_paths(
    base_path: &Path,
    format: AudioFormat,
    has_atmos: bool,
) -> (PathBuf, PathBuf) {
    let audio_ext = match (format, has_atmos) {
        (AudioFormat::Caf, false) => "caf",
        (AudioFormat::Pcm, false) => "pcm",
        (AudioFormat::W64, false) => "wav",
        (_, true) => "atmos.audio",
    };

    let audio_path = create_path_with_extension(base_path, audio_ext);

    let metadata_path = if has_atmos {
        create_path_with_extension(base_path, "atmos.metadata")
    } else {
        PathBuf::new() // Empty path for non-atmos
    };

    (audio_path, metadata_path)
}

pub enum AudioWriter {
    Pcm(BufWriter<File>),
    Caf(CAFWriter<BufWriter<File>>),
    W64(WAVWriter<File>),
}

impl AudioWriter {
    pub fn create_pcm(path: PathBuf) -> Result<Self> {
        let pcm_writer = BufWriter::new(File::create(path)?);
        Ok(AudioWriter::Pcm(pcm_writer))
    }

    pub fn create_caf(path: PathBuf, sample_rate: u32, channel_count: u32) -> Result<Self> {
        let mut caf_writer = CAFWriter::new(BufWriter::new(File::create(path)?));
        caf_writer.configure_audio_format(sample_rate, channel_count, 24)?;
        caf_writer.write_header()?;
        Ok(AudioWriter::Caf(caf_writer))
    }

    pub fn create_w64(path: PathBuf, sample_rate: u32, channel_count: u32) -> Result<Self> {
        let mut w64_writer = WAVWriter::new(File::create(path)?);
        w64_writer.configure_audio_format(sample_rate, channel_count, 24)?;
        w64_writer.write_header()?;
        Ok(AudioWriter::W64(w64_writer))
    }

    pub fn write_pcm_samples(&mut self, samples: &[i32], channel_count: usize) -> Result<()> {
        match self {
            AudioWriter::Pcm(pcm_writer) => {
                for sample_idx in 0..(samples.len() / channel_count) {
                    for ch in 0..channel_count {
                        let sample = samples[sample_idx * channel_count + ch];
                        let bytes = sample.to_le_bytes();
                        pcm_writer.write_all(&bytes[..3])?;
                    }
                }
            }
            AudioWriter::Caf(caf_writer) => {
                caf_writer.write_pcm_24bit_as_packed(samples)?;
            }
            AudioWriter::W64(w64_writer) => {
                w64_writer.write_pcm_24bit_as_packed(samples)?;
            }
        }
        Ok(())
    }

    pub fn close_and_drop(self) -> Result<()> {
        match self {
            AudioWriter::Pcm(mut w) => {
                w.flush()?;
                drop(w);
            }
            AudioWriter::W64(mut w) => {
                w.finish()?;
                drop(w);
            }
            AudioWriter::Caf(mut w) => {
                w.finish()?;
                drop(w);
            }
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<()> {
        match self {
            AudioWriter::Caf(caf_writer) => {
                caf_writer.finish()?;
            }
            AudioWriter::Pcm(pcm_writer) => {
                pcm_writer.flush()?;
            }
            AudioWriter::W64(w64_writer) => {
                w64_writer.finish()?;
            }
        }
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        match self {
            AudioWriter::Pcm(pcm_writer) => {
                pcm_writer.flush()?;
            }
            AudioWriter::Caf(_) => {
                // CAF writer doesn't need explicit flush for our use case
            }
            AudioWriter::W64(_) => {
                // W64 writer handles flushing internally
            }
        }
        Ok(())
    }
}

/// Largest positive 24-bit sample. Note this is *not* the scale factor: see
/// [`float_to_i24`].
const I24_MAX: i32 = 8_388_607;
/// Most negative 24-bit sample. The range is asymmetric, and the decoders do
/// emit this value (`dca`'s `clip23` clamps to it), so it must survive.
const I24_MIN: i32 = -8_388_608;
/// 2^23 — the divisor the decoders use to produce their f32 output, so the
/// multiplier that inverts it.
const I24_SCALE: f32 = 8_388_608.0;

/// Convert a decoder's float sample back to the 24-bit integer it came from.
///
/// Used by the DTS and E-AC-3 handlers; the TrueHD path stays integer
/// end-to-end and never goes through here.
///
/// `dca` emits `int24 as f32 / 2^23`, so this must scale by 2^23 and round.
/// Scaling by 2^23 - 1 and truncating (as this used to) shaved one count off
/// *every* nonzero sample, which is inaudible but cost bit-exactness: DTS-HD MA
/// is lossless, so its output has to match a reference decoder sample for
/// sample. +1.0 lands one past the positive maximum, hence the clamp.
#[inline]
pub fn float_to_i24(sample: f32) -> i32 {
    // NaN must map to silence, not rely on `as`-cast saturation semantics:
    // a decoder bug upstream must never turn into full-scale output.
    if !sample.is_finite() {
        return 0;
    }
    ((sample.clamp(-1.0, 1.0) * I24_SCALE).round_ties_even() as i32).clamp(I24_MIN, I24_MAX)
}

pub fn create_caf_writer_from_existing_file(file: File) -> Result<CAFWriter<BufWriter<File>>> {
    let mut temp_file = file.try_clone()?;
    let file_info = damf::caf::parse_caf_file(&mut temp_file)?;
    temp_file.seek(std::io::SeekFrom::End(0))?;
    Ok(CAFWriter::from_parsed_info(
        BufWriter::new(file),
        file_info,
    )?)
}

#[cfg(test)]
mod tests {
    use super::{I24_MAX, I24_MIN, float_to_i24};

    /// The decoders divide by 2^23; this must be the exact inverse, or lossless
    /// output stops matching a reference decoder sample for sample.
    #[test]
    fn float_to_i24_round_trips_every_24_bit_value() {
        for n in [
            I24_MIN,
            I24_MIN + 1,
            -8_000_000,
            -4_194_304,
            -1_000_001,
            -3,
            -2,
            -1,
            0,
            1,
            2,
            3,
            1_000_001,
            4_194_304,
            8_000_000,
            I24_MAX - 1,
            I24_MAX,
        ] {
            assert_eq!(float_to_i24(n as f32 / 8_388_608.0), n, "round trip of {n}");
        }
    }

    /// Exhaustive over the low end and a strided sweep of the full range: the
    /// old `* (2^23 - 1) as i32` lost exactly one count here on every nonzero
    /// sample, in both directions.
    #[test]
    fn float_to_i24_round_trips_exhaustively() {
        for n in (I24_MIN..=I24_MAX).step_by(97) {
            assert_eq!(float_to_i24(n as f32 / 8_388_608.0), n, "round trip of {n}");
        }
        for n in -4096..=4096 {
            assert_eq!(float_to_i24(n as f32 / 8_388_608.0), n, "round trip of {n}");
        }
    }

    /// The output boundary is the last guard between a decoder bug and the
    /// user's speakers: non-finite maps to silence, everything else saturates.
    #[test]
    fn float_to_i24_guards_non_finite_and_out_of_range_samples() {
        assert_eq!(float_to_i24(f32::NAN), 0);
        assert_eq!(float_to_i24(f32::INFINITY), 0);
        assert_eq!(float_to_i24(f32::NEG_INFINITY), 0);
        assert_eq!(float_to_i24(1.0e9), I24_MAX);
        assert_eq!(float_to_i24(-1.0e9), I24_MIN);
        // +1.0 scales to 2^23, one past full scale; -1.0 is exactly I24_MIN.
        assert_eq!(float_to_i24(1.0), I24_MAX);
        assert_eq!(float_to_i24(-1.0), I24_MIN);
        assert_eq!(float_to_i24(0.0), 0);
        assert_eq!(float_to_i24(-0.0), 0);
    }
}
