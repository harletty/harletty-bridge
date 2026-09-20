use anyhow::Result;
use damf::caf::CAFWriter;
use damf::wav::WAVWriter;
use std::fs::File;
use std::io::{BufWriter, Seek, Write};
use std::path::{Path, PathBuf};
use truehd::structs::channel::ChannelLabel;

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
    /// One mono RIFF WAV per channel, `<prefix>_<n>.wav`, n from 0 in the
    /// order the interleaved file would have had.
    Mono(MonoWavSet),
}

/// A set of mono 24-bit RIFF WAV files written in lockstep, one per channel.
/// Plain RIFF rather than Wave64: a mono channel of a feature film is under
/// a gigabyte, and the readers these files are for expect RIFF.
pub struct MonoWavSet {
    files: Vec<BufWriter<File>>,
    /// Bytes of PCM written to each file so far.
    data_bytes: u64,
    /// Scratch, one per channel, reused across calls: no allocation per frame.
    scratch: Vec<Vec<u8>>,
}

impl MonoWavSet {
    const HEADER: u64 = 44;

    /// Create `<prefix>_<n>.wav` for n in 0..channel_count, headers written
    /// with placeholder sizes that `finish` fills in.
    pub fn create(prefix: &Path, sample_rate: u32, channel_count: usize) -> Result<Self> {
        let mut files = Vec::with_capacity(channel_count);
        for n in 0..channel_count {
            let path = mono_path(prefix, n);
            let mut file = BufWriter::new(File::create(&path)?);
            write_riff_header(&mut file, sample_rate, 0)?;
            files.push(file);
        }
        Ok(Self {
            files,
            data_bytes: 0,
            scratch: vec![Vec::new(); channel_count],
        })
    }

    pub fn channel_count(&self) -> usize {
        self.files.len()
    }

    /// De-interleave `samples` (24-bit values in i32) into the files.
    pub fn write_interleaved(&mut self, samples: &[i32], channel_count: usize) -> Result<()> {
        if channel_count != self.files.len() {
            anyhow::bail!(
                "mono export: {} channels written to a set of {}",
                channel_count,
                self.files.len()
            );
        }
        let frames = samples.len() / channel_count;
        for (channel, scratch) in self.scratch.iter_mut().enumerate() {
            scratch.clear();
            scratch.reserve(frames * 3);
            for frame in 0..frames {
                let bytes = samples[frame * channel_count + channel].to_le_bytes();
                scratch.extend_from_slice(&bytes[..3]);
            }
            self.files[channel].write_all(scratch)?;
        }
        self.data_bytes += (frames * 3) as u64;
        Ok(())
    }

    /// Patch the RIFF and data sizes and flush every file.
    pub fn finish(&mut self) -> Result<()> {
        for file in &mut self.files {
            file.flush()?;
            let end = file.stream_position()?;
            let data = end.saturating_sub(Self::HEADER).min(u64::from(u32::MAX));
            file.seek(std::io::SeekFrom::Start(4))?;
            file.write_all(&((data + Self::HEADER - 8) as u32).to_le_bytes())?;
            file.seek(std::io::SeekFrom::Start(40))?;
            file.write_all(&(data as u32).to_le_bytes())?;
            file.flush()?;
            file.seek(std::io::SeekFrom::Start(end))?;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        for file in &mut self.files {
            file.flush()?;
        }
        Ok(())
    }
}

/// `<prefix>_<n>.wav`.
pub fn mono_path(prefix: &Path, n: usize) -> PathBuf {
    let mut name = prefix
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    name.push(format!("_{n}.wav"));
    prefix.with_file_name(name)
}

/// A 44-byte canonical RIFF header for mono 24-bit PCM.
fn write_riff_header(w: &mut impl Write, sample_rate: u32, data_bytes: u32) -> Result<()> {
    w.write_all(b"RIFF")?;
    w.write_all(&(36 + data_bytes).to_le_bytes())?;
    w.write_all(b"WAVEfmt ")?;
    w.write_all(&16u32.to_le_bytes())?;
    w.write_all(&1u16.to_le_bytes())?; // PCM
    w.write_all(&1u16.to_le_bytes())?; // mono
    w.write_all(&sample_rate.to_le_bytes())?;
    w.write_all(&(sample_rate * 3).to_le_bytes())?;
    w.write_all(&3u16.to_le_bytes())?; // block align
    w.write_all(&24u16.to_le_bytes())?;
    w.write_all(b"data")?;
    w.write_all(&data_bytes.to_le_bytes())?;
    Ok(())
}

impl AudioWriter {
    pub fn create_pcm(path: PathBuf) -> Result<Self> {
        let pcm_writer = BufWriter::new(File::create(path)?);
        Ok(AudioWriter::Pcm(pcm_writer))
    }

    /// `labels` are the decoder's own channel labels, from which the file's channel
    /// layout is named. Pass an empty slice where the decoder reports none, or where
    /// they do not describe the channels being written.
    pub fn create_caf(
        path: PathBuf,
        sample_rate: u32,
        channel_count: u32,
        labels: &[ChannelLabel],
    ) -> Result<Self> {
        let mut caf_writer = CAFWriter::new(BufWriter::new(File::create(path)?));
        caf_writer.configure_audio_format(sample_rate, channel_count, 24, labels)?;
        caf_writer.write_header()?;
        Ok(AudioWriter::Caf(caf_writer))
    }

    pub fn create_mono(prefix: &Path, sample_rate: u32, channel_count: usize) -> Result<Self> {
        Ok(AudioWriter::Mono(MonoWavSet::create(
            prefix,
            sample_rate,
            channel_count,
        )?))
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
            AudioWriter::Mono(set) => {
                set.write_interleaved(samples, channel_count)?;
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
            AudioWriter::Mono(mut set) => {
                set.finish()?;
                drop(set);
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
            AudioWriter::Mono(set) => {
                set.finish()?;
            }
        }
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        match self {
            AudioWriter::Mono(set) => {
                set.flush()?;
            }
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
    use super::{I24_MAX, I24_MIN, MonoWavSet, float_to_i24, mono_path};

    /// The decoders divide by 2^23; this must be the exact inverse, or lossless
    /// output stops matching a reference decoder sample for sample.
    #[test]
    fn mono_set_writes_one_riff_wav_per_channel_in_lockstep() {
        let dir = std::env::temp_dir().join(format!("harletty-mono-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let prefix = dir.join("ch");
        let mut set = MonoWavSet::create(&prefix, 48_000, 3).unwrap();
        // Two frames of three channels, then one more frame.
        set.write_interleaved(&[1, 2, 3, 4, 5, 6], 3).unwrap();
        set.write_interleaved(&[-1, -2, -3], 3).unwrap();
        assert!(
            set.write_interleaved(&[0, 0], 2).is_err(),
            "channel count is fixed"
        );
        set.finish().unwrap();
        drop(set);

        for (n, expected) in [(0usize, [1i32, 4, -1]), (1, [2, 5, -2]), (2, [3, 6, -3])] {
            let bytes = std::fs::read(mono_path(&prefix, n)).unwrap();
            assert_eq!(
                bytes.len(),
                44 + 9,
                "channel {n}: header + 3 samples of 3 bytes"
            );
            assert_eq!(&bytes[..4], b"RIFF");
            assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 36 + 9);
            assert_eq!(&bytes[8..16], b"WAVEfmt ");
            assert_eq!(
                u16::from_le_bytes(bytes[22..24].try_into().unwrap()),
                1,
                "mono"
            );
            assert_eq!(
                u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
                48_000
            );
            assert_eq!(u16::from_le_bytes(bytes[34..36].try_into().unwrap()), 24);
            assert_eq!(&bytes[36..40], b"data");
            assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 9);
            let samples: Vec<i32> = bytes[44..]
                .chunks(3)
                .map(|c| (i32::from_le_bytes([c[0], c[1], c[2], 0]) << 8) >> 8)
                .collect();
            assert_eq!(samples, expected, "channel {n}");
        }
        assert_eq!(
            mono_path(std::path::Path::new("/tmp/out/vo"), 12),
            std::path::PathBuf::from("/tmp/out/vo_12.wav")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

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
