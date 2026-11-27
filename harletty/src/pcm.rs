//! Integer PCM from a WAV container: what a front end pipes in when it has
//! decoded a lossless track bit-exactly (`ffmpeg -i film.flac -c:a pcm_s24le
//! -f wav -`). Auro-3D's height layer lives in the low bits of such PCM, so
//! the samples must arrive as they were decoded.
//!
//! Four containers carry the same `fmt ` and `data` chunks: RIFF/WAVE, its
//! 64-bit forms RF64 and BW64 (sizes in a `ds64` chunk), and Sony Wave64
//! (GUID chunk names and 64-bit sizes). A writer that cannot seek back,
//! ffmpeg into a pipe, leaves the sizes of the header and of the `data`
//! chunk as placeholders (0 or all ones): such a `data` chunk runs to the
//! end of the input.

use std::io::{self, Read};

use anyhow::{Context, Result, anyhow, bail};

/// `WAVE_FORMAT_PCM`.
const FORMAT_PCM: u16 = 1;
/// `WAVE_FORMAT_IEEE_FLOAT`.
const FORMAT_FLOAT: u16 = 3;
/// `WAVE_FORMAT_EXTENSIBLE`: the format is the first two bytes of the
/// sub-format GUID.
const FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// The `fmt ` chunk of an extensible format is 40 bytes; some writers add
/// to it. Anything past this is not a format chunk.
const MAX_FMT_SIZE: u64 = 4096;

/// The Wave64 GUIDs, in file byte order. Every chunk but the first shares the
/// last twelve bytes of the `wave`, `fmt ` and `data` GUIDs.
const W64_RIFF: [u8; 16] = [
    0x72, 0x69, 0x66, 0x66, 0x2E, 0x91, 0xCF, 0x11, 0xA5, 0xD6, 0x28, 0xDB, 0x04, 0xC1, 0x00, 0x00,
];
const W64_WAVE: [u8; 16] = [
    0x77, 0x61, 0x76, 0x65, 0xF3, 0xAC, 0xD3, 0x11, 0x8C, 0xD1, 0x00, 0xC0, 0x4F, 0x8E, 0xDB, 0x8A,
];
const W64_FMT: [u8; 16] = [
    0x66, 0x6D, 0x74, 0x20, 0xF3, 0xAC, 0xD3, 0x11, 0x8C, 0xD1, 0x00, 0xC0, 0x4F, 0x8E, 0xDB, 0x8A,
];
const W64_DATA: [u8; 16] = [
    0x64, 0x61, 0x74, 0x61, 0xF3, 0xAC, 0xD3, 0x11, 0x8C, 0xD1, 0x00, 0xC0, 0x4F, 0x8E, 0xDB, 0x8A,
];
/// A Wave64 chunk size counts the 24 bytes of its own header.
const W64_CHUNK_HEADER: u64 = 24;

/// Speaker positions of a WAVE_FORMAT_EXTENSIBLE channel mask (bit numbers).
pub mod position {
    pub const FRONT_LEFT: u32 = 0;
    pub const FRONT_RIGHT: u32 = 1;
    pub const FRONT_CENTER: u32 = 2;
    pub const LOW_FREQUENCY: u32 = 3;
    pub const BACK_LEFT: u32 = 4;
    pub const BACK_RIGHT: u32 = 5;
    pub const BACK_CENTER: u32 = 8;
    pub const SIDE_LEFT: u32 = 9;
    pub const SIDE_RIGHT: u32 = 10;
}

/// The mask a writer would state for a plain file of this many channels: the
/// WAV default order. Only the layouts a carrier can be: 5.1 and 7.1.
fn default_mask(channels: u16) -> Option<u32> {
    match channels {
        6 => Some(0x3F),
        8 => Some(0x63F),
        _ => None,
    }
}

/// What the `fmt ` chunk says about the samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcmFormat {
    pub channels: u16,
    pub sample_rate: u32,
    /// Bits of the container of one sample: 8, 16, 24 or 32.
    pub bits: u16,
    /// The speakers the file's channels are, from the extensible format's
    /// mask. `None` for a plain format, or a mask of another channel count.
    pub channel_mask: Option<u32>,
}

impl PcmFormat {
    /// Bytes of one sample frame, all channels.
    pub fn frame_bytes(&self) -> usize {
        usize::from(self.channels) * usize::from(self.bits / 8)
    }

    /// The speaker position (a bit number of the channel mask) of each
    /// channel of the file, in file order: the mask's, or the default order
    /// of the channel count when the file states none. `None` when neither
    /// says, and the channels cannot be told apart.
    pub fn positions(&self) -> Option<Vec<u32>> {
        let mask = self.channel_mask.or_else(|| default_mask(self.channels))?;
        Some((0..32).filter(|bit| mask & (1 << bit) != 0).collect())
    }
}

/// A WAV input, read up to the first sample of its `data` chunk.
pub struct PcmReader<R: Read> {
    reader: R,
    format: PcmFormat,
    /// Bytes of `data` left to read; `None` when the header leaves it open
    /// (read to the end of the input).
    remaining: Option<u64>,
}

/// The container of a header, which says how the rest of it is laid out.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Container {
    Riff,
    /// RF64 or BW64.
    Rf64,
    Wave64,
}

impl Container {
    /// Whether `head` opens a WAV container, as `--codec auto` asks.
    fn of(head: &[u8]) -> Option<Self> {
        match head.get(..4)? {
            b"RIFF" if head.get(8..12) == Some(b"WAVE") => Some(Self::Riff),
            b"RF64" | b"BW64" if head.get(8..12) == Some(b"WAVE") => Some(Self::Rf64),
            _ if head.get(..16) == Some(&W64_RIFF) && head.get(24..40) == Some(&W64_WAVE) => {
                Some(Self::Wave64)
            }
            _ => None,
        }
    }
}

/// Whether `head`, the first bytes of an input, opens a RIFF/WAVE, RF64,
/// BW64 or Wave64 file.
pub fn is_wav(head: &[u8]) -> bool {
    Container::of(head).is_some()
}

impl<R: Read> PcmReader<R> {
    /// Read the headers of the WAV container `reader` starts, up to the
    /// first sample.
    pub fn new(mut reader: R) -> Result<Self> {
        let mut magic = [0u8; 40];
        // 12 bytes open a RIFF container, 40 a Wave64 one.
        read_fill(&mut reader, &mut magic[..12])?;
        let container = if magic[..4] == W64_RIFF[..4] {
            read_fill(&mut reader, &mut magic[12..])?;
            Container::of(&magic)
        } else {
            Container::of(&magic[..12])
        }
        .ok_or_else(|| anyhow!("not a RIFF/WAVE, RF64, BW64 or Wave64 file"))?;

        let mut format = None;
        // The 64-bit size of `data` an RF64 file keeps in `ds64`.
        let mut ds64_data = None;
        loop {
            let (id, size) = read_chunk_header(&mut reader, container)?;
            match id {
                ChunkId::Fmt => {
                    if size > MAX_FMT_SIZE {
                        bail!("the fmt chunk is {size} bytes long");
                    }
                    let mut body = vec![0u8; size as usize];
                    read_fill(&mut reader, &mut body)?;
                    format = Some(parse_fmt(&body)?);
                    skip_padding(&mut reader, container, size)?;
                }
                ChunkId::Ds64 => {
                    if size > MAX_FMT_SIZE {
                        bail!("the ds64 chunk is {size} bytes long");
                    }
                    let mut body = vec![0u8; size as usize];
                    read_fill(&mut reader, &mut body)?;
                    ds64_data = body
                        .get(8..16)
                        .map(|b| u64::from_le_bytes(b.try_into().expect("8 bytes")));
                    skip_padding(&mut reader, container, size)?;
                }
                ChunkId::Data => {
                    let format = format.ok_or_else(|| anyhow!("the data chunk precedes fmt"))?;
                    let remaining = data_length(container, size, ds64_data);
                    return Ok(Self {
                        reader,
                        format,
                        remaining,
                    });
                }
                ChunkId::Other => {
                    skip(&mut reader, size)?;
                    skip_padding(&mut reader, container, size)?;
                }
            }
        }
    }

    pub fn format(&self) -> PcmFormat {
        self.format
    }

    /// Read up to `max_frames` whole sample frames into `out` (replacing its
    /// contents), and return how many. 0 is the end of the data.
    pub fn read_frames(&mut self, max_frames: usize, out: &mut Vec<u8>) -> Result<usize> {
        let frame_bytes = self.format.frame_bytes();
        let mut want = max_frames * frame_bytes;
        if let Some(remaining) = self.remaining {
            want = want.min(usize::try_from(remaining).unwrap_or(usize::MAX));
        }
        out.clear();
        out.resize(want, 0);
        let mut filled = 0;
        while filled < want {
            let n = match self.reader.read(&mut out[filled..]) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e).context("reading the PCM data"),
            };
            filled += n;
        }
        if let Some(remaining) = &mut self.remaining {
            *remaining -= filled as u64;
        }
        // A frame cut short by the end of the input is not one.
        let frames = filled / frame_bytes;
        out.truncate(frames * frame_bytes);
        Ok(frames)
    }
}

#[derive(Clone, Copy)]
enum ChunkId {
    Fmt,
    Ds64,
    Data,
    Other,
}

/// The next chunk's kind and the size of its body (without its header or
/// padding). `u64::MAX` is a size the header left open.
fn read_chunk_header<R: Read>(reader: &mut R, container: Container) -> Result<(ChunkId, u64)> {
    if container == Container::Wave64 {
        let mut header = [0u8; 24];
        read_fill(reader, &mut header)?;
        let id = if header[..16] == W64_FMT {
            ChunkId::Fmt
        } else if header[..16] == W64_DATA {
            ChunkId::Data
        } else {
            ChunkId::Other
        };
        let size = u64::from_le_bytes(header[16..].try_into().expect("8 bytes"));
        // Open, or too short to hold its own header.
        let size = match size.checked_sub(W64_CHUNK_HEADER) {
            Some(size) if size < u64::MAX - W64_CHUNK_HEADER => size,
            _ => u64::MAX,
        };
        return Ok((id, size));
    }
    let mut header = [0u8; 8];
    read_fill(reader, &mut header)?;
    let id = match &header[..4] {
        b"fmt " => ChunkId::Fmt,
        b"ds64" if container == Container::Rf64 => ChunkId::Ds64,
        b"data" => ChunkId::Data,
        _ => ChunkId::Other,
    };
    let size = u32::from_le_bytes(header[4..].try_into().expect("4 bytes"));
    let size = if size == u32::MAX {
        u64::MAX
    } else {
        u64::from(size)
    };
    Ok((id, size))
}

/// How many bytes the `data` chunk holds, `None` when it runs to the end of
/// the input.
fn data_length(container: Container, size: u64, ds64_data: Option<u64>) -> Option<u64> {
    let size = match (container, size) {
        // RF64 keeps the size of a chunk too big for 32 bits in `ds64`.
        (Container::Rf64, u64::MAX) => ds64_data.unwrap_or(u64::MAX),
        _ => size,
    };
    // A writer that cannot go back leaves 0 (or all ones) where the size is.
    match size {
        0 | u64::MAX => None,
        size => Some(size),
    }
}

/// `fmt ` chunk body: integer PCM only.
fn parse_fmt(body: &[u8]) -> Result<PcmFormat> {
    if body.len() < 16 {
        bail!("the fmt chunk is {} bytes long, 16 at least", body.len());
    }
    let u16_at = |at: usize| u16::from_le_bytes([body[at], body[at + 1]]);
    let mut tag = u16_at(0);
    let channels = u16_at(2);
    let sample_rate = u32::from_le_bytes(body[4..8].try_into().expect("4 bytes"));
    let bits = u16_at(14);
    let mut channel_mask = None;
    if tag == FORMAT_EXTENSIBLE {
        // cbSize, valid bits, channel mask, sub-format GUID.
        if body.len() < 40 {
            bail!(
                "an extensible fmt chunk is {} bytes long, 40 at least",
                body.len()
            );
        }
        let mask = u32::from_le_bytes(body[20..24].try_into().expect("4 bytes"));
        channel_mask = (mask != 0).then_some(mask);
        tag = u16_at(24);
    }
    match tag {
        FORMAT_PCM => {}
        FORMAT_FLOAT => bail!("floating-point PCM is not supported, integer PCM only"),
        tag => bail!("WAVE format {tag:#06x} is not supported, integer PCM only"),
    }
    if channels == 0 {
        bail!("the PCM has no channels");
    }
    if !matches!(bits, 8 | 16 | 24 | 32) {
        bail!("{bits}-bit PCM is not supported (8, 16, 24 and 32 bits are)");
    }
    // A mask for another number of channels says nothing of these.
    if channel_mask.is_some_and(|mask| mask.count_ones() != u32::from(channels)) {
        log::warn!(
            "the channel mask {:#x} does not name {channels} channels; ignored",
            channel_mask.unwrap_or(0)
        );
        channel_mask = None;
    }
    Ok(PcmFormat {
        channels,
        sample_rate,
        bits,
        channel_mask,
    })
}

/// Fill `buf`, or fail: the header is cut short.
fn read_fill<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<()> {
    reader.read_exact(buf).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            anyhow!("the WAV header is cut short")
        } else {
            anyhow!(e).context("reading the WAV header")
        }
    })
}

fn skip<R: Read>(reader: &mut R, size: u64) -> Result<()> {
    if size == u64::MAX {
        bail!("a chunk before the data has no size");
    }
    let skipped = io::copy(&mut reader.take(size), &mut io::sink())?;
    if skipped < size {
        bail!("the WAV header is cut short");
    }
    Ok(())
}

/// The byte a RIFF chunk of odd size is padded with, or the padding up to a
/// multiple of eight a Wave64 one has.
fn skip_padding<R: Read>(reader: &mut R, container: Container, size: u64) -> Result<()> {
    let padding = match container {
        Container::Wave64 => (8 - (size + W64_CHUNK_HEADER) % 8) % 8,
        _ => size % 2,
    };
    skip(reader, padding)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `fmt ` chunk body: plain or extensible.
    fn fmt(channels: u16, bits: u16, mask: Option<u32>) -> Vec<u8> {
        let mut body = Vec::new();
        let tag = if mask.is_some() {
            FORMAT_EXTENSIBLE
        } else {
            FORMAT_PCM
        };
        body.extend(tag.to_le_bytes());
        body.extend(channels.to_le_bytes());
        body.extend(48_000u32.to_le_bytes());
        let align = channels * (bits / 8);
        body.extend((48_000 * u32::from(align)).to_le_bytes());
        body.extend(align.to_le_bytes());
        body.extend(bits.to_le_bytes());
        if let Some(mask) = mask {
            body.extend(22u16.to_le_bytes());
            body.extend(bits.to_le_bytes());
            body.extend(mask.to_le_bytes());
            // KSDATAFORMAT_SUBTYPE_PCM
            body.extend([
                1, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xAA, 0, 0x38, 0x9B, 0x71,
            ]);
        }
        body
    }

    fn riff_chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut chunk = id.to_vec();
        chunk.extend((body.len() as u32).to_le_bytes());
        chunk.extend(body);
        if body.len() % 2 == 1 {
            chunk.push(0);
        }
        chunk
    }

    fn w64_chunk(id: &[u8; 16], body: &[u8]) -> Vec<u8> {
        let mut chunk = id.to_vec();
        chunk.extend((body.len() as u64 + W64_CHUNK_HEADER).to_le_bytes());
        chunk.extend(body);
        chunk.resize(chunk.len().next_multiple_of(8), 0);
        chunk
    }

    /// Six channels of 24-bit frames counting up from 1.
    fn samples(frames: usize) -> Vec<u8> {
        (0..frames * 6 * 3).map(|i| (i % 251) as u8).collect()
    }

    fn riff(fmt: &[u8], data_size: u32, data: &[u8]) -> Vec<u8> {
        let mut file = b"RIFF".to_vec();
        file.extend(0u32.to_le_bytes());
        file.extend(b"WAVE");
        file.extend(riff_chunk(b"fmt ", fmt));
        file.extend(b"data");
        file.extend(data_size.to_le_bytes());
        file.extend(data);
        file
    }

    fn read_all(file: &[u8]) -> (PcmFormat, Vec<u8>, usize) {
        let mut reader = PcmReader::new(file).unwrap();
        let mut all = Vec::new();
        let mut chunk = Vec::new();
        let mut frames = 0;
        // Odd sizes, so a frame never lines up with a read.
        loop {
            let n = reader.read_frames(7, &mut chunk).unwrap();
            if n == 0 {
                break;
            }
            frames += n;
            all.extend(&chunk);
        }
        (reader.format(), all, frames)
    }

    #[test]
    fn a_sized_riff_file_reads_its_data_and_stops() {
        let data = samples(20);
        let mut file = riff(&fmt(6, 24, None), data.len() as u32, &data);
        // Whatever follows the data chunk is not audio.
        file.extend(riff_chunk(b"LIST", b"INFOabc"));
        let (format, all, frames) = read_all(&file);
        assert_eq!(
            format,
            PcmFormat {
                channels: 6,
                sample_rate: 48_000,
                bits: 24,
                channel_mask: None
            }
        );
        assert_eq!((frames, all), (20, data));
    }

    #[test]
    fn placeholder_sizes_read_to_the_end() {
        for placeholder in [0, u32::MAX] {
            let data = samples(20);
            let mut file = riff(&fmt(6, 24, None), placeholder, &data);
            // The riff size is a placeholder as well (it is 0 above).
            file[4..8].copy_from_slice(&placeholder.to_le_bytes());
            let (_, all, frames) = read_all(&file);
            assert_eq!((frames, all), (20, data), "placeholder {placeholder:#x}");
        }
    }

    #[test]
    fn a_frame_cut_short_is_dropped() {
        let mut data = samples(5);
        data.extend([1, 2, 3, 4]);
        let (_, all, frames) = read_all(&riff(&fmt(6, 24, None), 0, &data));
        assert_eq!(frames, 5);
        assert_eq!(all.len(), 5 * 18);
    }

    #[test]
    fn chunks_before_data_are_skipped_with_their_padding() {
        let data = samples(3);
        let mut file = b"RIFF".to_vec();
        file.extend(0u32.to_le_bytes());
        file.extend(b"WAVE");
        file.extend(riff_chunk(b"JUNK", b"odd"));
        file.extend(riff_chunk(b"fmt ", &fmt(6, 24, None)));
        file.extend(riff_chunk(b"LIST", b"INFO"));
        file.extend(b"data");
        file.extend((data.len() as u32).to_le_bytes());
        file.extend(&data);
        let (_, all, frames) = read_all(&file);
        assert_eq!((frames, all), (3, data));
    }

    #[test]
    fn an_extensible_format_names_its_speakers() {
        let data = samples(2);
        let file = riff(&fmt(6, 24, Some(0x60F)), 0, &data);
        let (format, _, _) = read_all(&file);
        assert_eq!(format.channel_mask, Some(0x60F));
        // FL FR FC LFE SL SR
        assert_eq!(format.positions(), Some(vec![0, 1, 2, 3, 9, 10]));
    }

    #[test]
    fn without_a_mask_the_channel_count_gives_the_default_order() {
        let positions = |channels, bits| {
            let file = riff(&fmt(channels, bits, None), 0, &[]);
            PcmReader::new(&file[..]).unwrap().format().positions()
        };
        // FL FR FC LFE BL BR
        assert_eq!(positions(6, 24), Some(vec![0, 1, 2, 3, 4, 5]));
        // FL FR FC LFE BL BR SL SR
        assert_eq!(positions(8, 24), Some(vec![0, 1, 2, 3, 4, 5, 9, 10]));
        assert_eq!(positions(2, 16), None);
    }

    #[test]
    fn a_mask_with_the_wrong_channel_count_is_ignored() {
        // A 7.1 mask on six channels: the default order of six stands.
        let file = riff(&fmt(6, 24, Some(0x63F)), 0, &[]);
        let reader = PcmReader::new(&file[..]).unwrap();
        assert_eq!(reader.format().channel_mask, None);
        assert_eq!(reader.format().positions(), Some(vec![0, 1, 2, 3, 4, 5]));
        // An unspecified mask (0) is no mask.
        let file = riff(&fmt(8, 24, Some(0)), 0, &[]);
        assert_eq!(
            PcmReader::new(&file[..]).unwrap().format().channel_mask,
            None
        );
    }

    #[test]
    fn rf64_takes_the_data_size_from_ds64() {
        let data = samples(10);
        let mut ds64 = Vec::new();
        ds64.extend(0u64.to_le_bytes()); // riff size
        ds64.extend((data.len() as u64).to_le_bytes());
        ds64.extend(10u64.to_le_bytes()); // sample count
        ds64.extend(0u32.to_le_bytes()); // table length
        for magic in [&b"RF64"[..], &b"BW64"[..]] {
            let mut file = magic.to_vec();
            file.extend(u32::MAX.to_le_bytes());
            file.extend(b"WAVE");
            file.extend(riff_chunk(b"ds64", &ds64));
            file.extend(riff_chunk(b"fmt ", &fmt(6, 24, Some(0x3F))));
            file.extend(b"data");
            file.extend(u32::MAX.to_le_bytes());
            file.extend(&data);
            file.extend(riff_chunk(b"LIST", b"INFOabc"));
            assert!(is_wav(&file));
            let (_, all, frames) = read_all(&file);
            assert_eq!((frames, all), (10, data.clone()));
        }
    }

    #[test]
    fn rf64_without_a_known_size_reads_to_the_end() {
        let data = samples(10);
        let mut ds64 = vec![0u8; 8];
        ds64.extend(u64::MAX.to_le_bytes());
        ds64.extend([0u8; 12]);
        let mut file = b"RF64".to_vec();
        file.extend(u32::MAX.to_le_bytes());
        file.extend(b"WAVE");
        file.extend(riff_chunk(b"ds64", &ds64));
        file.extend(riff_chunk(b"fmt ", &fmt(6, 24, None)));
        file.extend(b"data");
        file.extend(u32::MAX.to_le_bytes());
        file.extend(&data);
        let (_, all, frames) = read_all(&file);
        assert_eq!((frames, all), (10, data));
    }

    #[test]
    fn wave64_reads_its_guid_chunks() {
        let data = samples(10);
        for data_size in [data.len() as u64 + W64_CHUNK_HEADER, 0, u64::MAX] {
            let mut file = W64_RIFF.to_vec();
            file.extend(0u64.to_le_bytes());
            file.extend(W64_WAVE);
            // A body of 40 bytes needs no padding; 3 does.
            file.extend(w64_chunk(&W64_FMT, &fmt(6, 24, Some(0x60F))));
            file.extend(&W64_DATA);
            file.extend(data_size.to_le_bytes());
            file.extend(&data);
            assert!(is_wav(&file));
            let (format, all, frames) = read_all(&file);
            assert_eq!(format.channel_mask, Some(0x60F));
            assert_eq!((frames, all), (10, data.clone()), "data size {data_size}");
        }
    }

    #[test]
    fn wave64_skips_a_padded_chunk() {
        let data = samples(4);
        let mut file = W64_RIFF.to_vec();
        file.extend(0u64.to_le_bytes());
        file.extend(W64_WAVE);
        file.extend(w64_chunk(&[0x6A; 16], b"odd"));
        file.extend(w64_chunk(&W64_FMT, &fmt(6, 24, None)));
        file.extend(&W64_DATA);
        file.extend((data.len() as u64 + W64_CHUNK_HEADER).to_le_bytes());
        file.extend(&data);
        let (_, all, frames) = read_all(&file);
        assert_eq!((frames, all), (4, data));
    }

    #[test]
    fn only_a_wav_header_is_one() {
        assert!(is_wav(b"RIFF\0\0\0\0WAVEfmt "));
        assert!(is_wav(b"RF64\xFF\xFF\xFF\xFFWAVE"));
        assert!(!is_wav(b"RIFF\0\0\0\0AVI LIST"));
        assert!(!is_wav(b"RIFF"));
        assert!(!is_wav(&[0xF8, 0x72, 0x6F, 0xBA, 0, 0, 0, 0, 0, 0, 0, 0]));
        assert!(!is_wav(&[]));
    }

    #[test]
    fn what_is_not_integer_pcm_is_refused() {
        let mut float = fmt(6, 32, None);
        float[..2].copy_from_slice(&FORMAT_FLOAT.to_le_bytes());
        let error = |file: &[u8]| PcmReader::new(file).err().unwrap().to_string();
        assert!(error(&riff(&float, 0, &[])).contains("floating-point"));
        let mut ac3 = fmt(2, 16, None);
        ac3[..2].copy_from_slice(&0x2000u16.to_le_bytes());
        assert!(error(&riff(&ac3, 0, &[])).contains("0x2000"));
        assert!(error(&riff(&fmt(2, 12, None), 0, &[])).contains("12-bit"));
        // Extensible, with a float sub-format.
        let mut ext = fmt(2, 32, Some(3));
        ext[24..26].copy_from_slice(&FORMAT_FLOAT.to_le_bytes());
        assert!(error(&riff(&ext, 0, &[])).contains("floating-point"));
    }

    #[test]
    fn a_damaged_header_is_an_error_not_a_panic() {
        let file = riff(&fmt(6, 24, Some(0x3F)), 0, &samples(1));
        for len in 0..file.len() - 18 {
            assert!(PcmReader::new(&file[..len]).is_err(), "cut at {len}");
        }
        // Data before the format.
        let mut file = b"RIFF".to_vec();
        file.extend(0u32.to_le_bytes());
        file.extend(b"WAVEdata");
        file.extend(0u32.to_le_bytes());
        assert!(PcmReader::new(&file[..]).is_err());
    }
}
