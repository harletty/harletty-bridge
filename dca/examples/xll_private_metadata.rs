// SPDX-License-Identifier: Apache-2.0
// Offline, deliberately narrow private-metadata investigation. Nothing here
// runs in the decoder or determines a playback presentation.
//
// cargo run -p dca --release --example xll_private_metadata --
//     [--max-mb 64] input.dts [input.dts ...]

use std::collections::BTreeMap;
use std::io::{BufReader, Read};

use dca::{HdDecoder, HdError, exss_substream_size, parse_header};

const OUTER_SUFFIX: &[u8] = &[3, 0x34, 0x38, 0x8c, 0x4f, 0];
const MAX_PREFIX: usize = 96;

#[derive(Debug, PartialEq, Eq)]
enum Error {
    Truncated,
    Unsupported(&'static str),
    Invalid(&'static str),
}

struct Bits<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn read(&mut self, width: usize) -> Result<u32, Error> {
        let end = self.pos.checked_add(width).ok_or(Error::Truncated)?;
        if width > 32 || end > self.bytes.len().saturating_mul(8) {
            return Err(Error::Truncated);
        }
        let mut value = 0;
        for bit in self.pos..end {
            value = (value << 1) | u32::from((self.bytes[bit / 8] >> (7 - bit % 8)) & 1);
        }
        self.pos = end;
        Ok(value)
    }

    fn expect(&mut self, width: usize, expected: u32, field: &'static str) -> Result<(), Error> {
        if self.read(width)? != expected {
            return Err(Error::Unsupported(field));
        }
        Ok(())
    }
}

fn crc16(bytes: &[u8]) -> u16 {
    let mut crc = 0xffffu16;
    for &byte in bytes {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = (crc << 1) ^ if crc & 0x8000 != 0 { 0x1021 } else { 0 };
        }
    }
    crc
}

// This investigation currently knows only the two layouts independently
// calibrated in the fixed-height corpus. Other masks remain unsupported.
fn channels(mask: u32) -> Result<&'static [&'static str], Error> {
    match mask {
        0x084b => Ok(&["C", "L", "R", "LFE", "Lb", "Rb", "Lss", "Rss"]),
        0x8020 => Ok(&["TFL", "TFR", "TBL", "TBR"]),
        _ => Err(Error::Unsupported("speaker mask")),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Route {
    source: &'static str,
    target: &'static str,
    gain_code: u8,
    // Only calibration points verified against the existing Q15 downmix
    // table are exposed. Unknown codes must not silently get a default gain.
    calibrated_q15: Option<u16>,
}

#[derive(Debug, PartialEq, Eq)]
struct Matrix {
    reference_mask: u32,
    output_mask: u32,
    mix_code: u8,
    routes: Vec<Route>,
    encoded_bytes: usize,
}

#[derive(Debug, PartialEq, Eq)]
struct LayoutHeader {
    kind: u8,
    reference_mask: u32,
    output_mask: u32,
    end_bit: usize,
}

// Read only the observed header forms. Uninterpreted flags are constrained,
// not silently accepted as though their semantics were understood.
fn layout_header(bytes: &[u8], inherited_mask: u32) -> Result<LayoutHeader, Error> {
    let mut b = Bits { bytes, pos: 0 };
    let kind = b.read(8)? as u8;
    if !matches!(kind, 2 | 3) {
        return Err(Error::Unsupported("element type"));
    }
    b.expect(8, 0, "element header byte")?;
    b.expect(1, 0, "header flag")?;
    b.expect(4, 1, "header field A")?;
    b.expect(4, u32::from(kind == 3), "header field B")?;
    let reference_mask = if b.read(1)? != 0 {
        b.expect(1, 0, "reference flag")?;
        b.expect(1, 1, "explicit reference mask")?;
        b.expect(2, 0, "reference mode")?;
        b.expect(3, 0, "reference field")?;
        let width = 4 * (b.read(3)? as usize + 1);
        b.read(width)?
    } else {
        b.expect(1, 0, "implicit reference mask")?;
        b.expect(4, 0, "implicit reference field")?;
        inherited_mask
    };
    b.expect(1, 1, "level field present")?;
    b.expect(6, 61, "level code")?;
    b.expect(1, 0, "additional level")?;
    let width = 4 * (b.read(3)? as usize + 1);
    let output_mask = b.read(width)?;
    if output_mask & reference_mask != reference_mask {
        return Err(Error::Unsupported("non-superset layout"));
    }
    Ok(LayoutHeader {
        kind,
        reference_mask,
        output_mask,
        end_bit: b.pos,
    })
}

fn parse_matrix(bytes: &[u8], inherited_mask: u32) -> Result<Matrix, Error> {
    let header = layout_header(bytes, inherited_mask)?;
    let targets = channels(header.reference_mask)?;
    let sources = channels(header.output_mask & !header.reference_mask)?;
    let mut b = Bits {
        bytes,
        pos: header.end_bit,
    };
    b.expect(5, 2, "matrix header field")?;
    b.expect(6, 0, "matrix header value")?;
    b.expect(1, 1, "matrix mode")?;
    b.expect(1, 0, "optional matrix parameters")?;
    // The alternate type-3 declaration diverges here. Do not jump forward to
    // coefficient-looking bits or reuse the standard matrix on these streams.
    b.expect(1, 1, "inline matrix flag")?;
    let mix_code = b.read(6)? as u8;
    b.expect(1, 0, "additional scales")?;
    let mut routes = Vec::new();
    for &source in sources {
        let mask = b.read(targets.len())?;
        if mask == 0 {
            return Err(Error::Unsupported("empty matrix row"));
        }
        for (index, &target) in targets.iter().enumerate() {
            if mask & (1 << index) == 0 {
                continue;
            }
            let gain_code = b.read(6)? as u8;
            if gain_code > 61 {
                return Err(Error::Unsupported("gain escape"));
            }
            routes.push(Route {
                source,
                target,
                gain_code,
                calibrated_q15: match gain_code {
                    55 => Some(23_170),
                    61 => Some(32_768),
                    _ => None,
                },
            });
        }
    }
    let padding = (8 - b.pos % 8) % 8;
    if b.read(padding)? != 0 {
        return Err(Error::Invalid("matrix padding"));
    }
    let encoded_bytes = (b.pos / 8).checked_add(2).ok_or(Error::Truncated)?;
    let protected = bytes.get(..encoded_bytes).ok_or(Error::Truncated)?;
    if crc16(protected) != 0 {
        return Err(Error::Invalid("matrix CRC"));
    }
    Ok(Matrix {
        reference_mask: header.reference_mask,
        output_mask: header.output_mask,
        mix_code,
        routes,
        encoded_bytes,
    })
}

fn alternate_prefix(payload: &[u8]) -> Result<&[u8], Error> {
    let mut result = None;
    for end in 4..=payload.len().min(MAX_PREFIX) {
        if payload.get(end..end + OUTER_SUFFIX.len()) == Some(OUTER_SUFFIX)
            && crc16(&payload[..end]) == 0
        {
            if result.is_some() {
                return Err(Error::Invalid("ambiguous prefix"));
            }
            result = Some(&payload[..end]);
        }
    }
    result.ok_or(Error::Invalid("alternate prefix CRC/boundary"))
}

#[derive(Debug, PartialEq, Eq)]
struct Declaration241 {
    index: u8,
    mode: u8,
}

#[derive(Debug, PartialEq, Eq)]
struct Header241 {
    reference_mask: u32,
    declarations: Vec<Declaration241>,
    end_bit: usize,
}

#[derive(Debug, PartialEq, Eq)]
struct Position241 {
    index: u8,
    azimuth_code: u8,
    elevation_code: u8,
    distance_code: u8,
    // Azimuth/elevation in half-degrees, distance in units of 1/64.
    spherical_units: (i16, i16, u8),
    reference_rows: [Vec<(usize, u8)>; 2],
}

fn spherical_units(azimuth: u8, elevation: u8, distance: u8) -> (i16, i16, u8) {
    let azimuth = match azimuth {
        47 => -220,
        193 => 220,
        value => (3 * i16::from(value) - 360).min(357),
    };
    let elevation = (3 * (i16::from(elevation) - 60)).min(180);
    let distance = if distance == 0 { 0 } else { distance + 1 };
    (azimuth, elevation, distance)
}

#[derive(Debug, PartialEq, Eq)]
struct Dynamic241 {
    positions: Vec<Position241>,
    // Raw auxiliary declarations; no inferred labels or PCM routing.
    auxiliary: Vec<(u8, u32, u8)>,
    end_bit: usize,
}

fn sparse_codes(b: &mut Bits<'_>, columns: usize) -> Result<Vec<(usize, u8)>, Error> {
    let mask = b.read(columns)?;
    let mut row = Vec::new();
    for index in 0..columns {
        if mask & (1 << index) != 0 {
            let code = b.read(6)? as u8;
            if code > 61 {
                return Err(Error::Unsupported("type-241 gain escape"));
            }
            row.push((index, code));
        }
    }
    Ok(row)
}

// Consume the supported mode-1 variable form all the way to the type-3
// boundary. Raw codes only: waveform identity and presentation are separate.
fn type241_dynamic(bytes: &[u8]) -> Result<Dynamic241, Error> {
    let header = type241_header(bytes)?;
    let mut b = Bits {
        bytes,
        pos: header.end_bit,
    };
    let mut positions = Vec::new();
    for declaration in &header.declarations {
        if declaration.mode != 1 {
            return Err(Error::Unsupported("type-241 dynamic mode"));
        }
        b.expect(3, 0, "type-241 record options")?;
        b.expect(7, 0x20, "type-241 position options")?;
        b.expect(1, 1, "type-241 gain present")?;
        b.expect(1, 1, "type-241 position flag")?;
        b.expect(2, 0, "type-241 position mode")?;
        b.expect(6, 61, "type-241 position gain")?;
        let distance_code = b.read(6)? as u8;
        let azimuth_code = b.read(8)? as u8;
        let elevation_code = b.read(7)? as u8;
        b.expect(1, 0, "type-241 extent")?;
        let present = [b.read(1)? != 0, b.read(1)? != 0];
        let mut reference_rows = [Vec::new(), Vec::new()];
        for (row, present) in reference_rows.iter_mut().zip(present) {
            if present {
                *row = sparse_codes(&mut b, 8)?;
            }
        }
        positions.push(Position241 {
            index: declaration.index,
            azimuth_code,
            elevation_code,
            distance_code,
            spherical_units: spherical_units(azimuth_code, elevation_code, distance_code),
            reference_rows,
        });
    }
    let mut auxiliary = Vec::new();
    if b.read(1)? != 0 {
        for declaration in &header.declarations {
            if b.read(1)? == 0 {
                continue;
            }
            let count = b.read(2)? + 1;
            let width = b.read(5)? as usize;
            for _ in 0..count {
                let mask = b.read(width)?;
                // Only the observed single-channel auxiliary form is known.
                if mask != 0x80 {
                    return Err(Error::Unsupported("type-241 auxiliary layout"));
                }
                b.expect(1, 1, "type-241 auxiliary flag")?;
                b.expect(1, 1, "type-241 auxiliary route")?;
                let code = b.read(6)? as u8;
                if code > 61 {
                    return Err(Error::Unsupported("type-241 auxiliary gain"));
                }
                auxiliary.push((declaration.index, mask, code));
            }
        }
    }
    let end_bit = b.pos;
    let padding = (8 - b.pos % 8) % 8;
    if b.read(padding)? != 0 || b.pos / 8 != bytes.len() {
        return Err(Error::Invalid("type-241 end/padding"));
    }
    Ok(Dynamic241 {
        positions,
        auxiliary,
        end_bit,
    })
}

// Narrow static-declaration reader. The caller supplies only the portion
// preceding type 3 in a CRC-validated envelope. These indices are metadata
// indices, not yet verified PCM channel identities. Dynamic data remains unread.
fn type241_header(bytes: &[u8]) -> Result<Header241, Error> {
    let mut b = Bits { bytes, pos: 0 };
    for (width, value) in [
        (8, 241),
        (8, 0x40),
        (4, 0),
        (1, 0),
        (4, 1),
        (1, 1),
        (1, 0),
        (1, 1),
    ] {
        b.expect(width, value, "type-241 header form")?;
    }
    let count = b.read(4)? as usize + 1;
    for (width, value) in [(1, 0), (1, 0), (1, 1), (1, 1), (2, 0), (3, 0)] {
        b.expect(width, value, "type-241 reference form")?;
    }
    let width = 4 * (b.read(3)? as usize + 1);
    let reference_mask = b.read(width)?;
    if reference_mask != 0x84b {
        return Err(Error::Unsupported("type-241 reference layout"));
    }
    // No optional levels, additional layouts or timing parameters in the
    // supported form. Precision zero gives two-bit and three-bit indices.
    b.expect(8, 0, "type-241 optional fields/precision")?;
    let mut declarations = Vec::with_capacity(count);
    for _ in 0..count {
        b.expect(1, 1, "inactive type-241 declaration")?;
        b.expect(2, 3, "type-241 declaration field")?;
        let index = b.read(3)? as u8;
        let mode = b.read(2)? as u8;
        if mode > 1 {
            return Err(Error::Unsupported("type-241 declaration mode"));
        }
        // One component, component code zero, no optional parameter,
        // parameter mode zero, one subcomponent.
        b.expect(10, 0, "type-241 component form")?;
        declarations.push(Declaration241 { index, mode });
    }
    Ok(Header241 {
        reference_mask,
        declarations,
        end_bit: b.pos,
    })
}

// Experimental type-3 reading: four sparse rows over the complete 12-channel
// layout. Control semantics and waveform associations are NOT established.
// Return indices/codes only, never playback routes or calibrated gains.
fn type3_rows_candidate(bytes: &[u8]) -> Result<Vec<Vec<(usize, u8)>>, Error> {
    let header = layout_header(bytes, 0x84b)?;
    if header.kind != 3 || header.output_mask != 0x886b {
        return Err(Error::Unsupported("type-3 candidate layout"));
    }
    let mut b = Bits {
        bytes,
        pos: header.end_bit,
    };
    b.expect(5, 2, "candidate field")?;
    b.expect(6, 0, "candidate value")?;
    b.expect(1, 1, "candidate flag")?;
    b.expect(1, 0, "candidate option")?;
    b.expect(12, 0x3fa, "unresolved type-3 control")?;
    let mut rows = Vec::new();
    for _ in 0..4 {
        let mask = b.read(12)?;
        let mut row = Vec::new();
        for index in 0..12 {
            if mask & (1 << index) != 0 {
                let code = b.read(6)? as u8;
                if code > 61 {
                    return Err(Error::Unsupported("candidate gain escape"));
                }
                row.push((index, code));
            }
        }
        if row.is_empty() {
            return Err(Error::Unsupported("empty candidate row"));
        }
        rows.push(row);
    }
    let padding = (8 - b.pos % 8) % 8;
    if b.read(padding)? != 0 || b.pos / 8 + 2 != bytes.len() {
        return Err(Error::Invalid("candidate end/padding"));
    }
    Ok(rows)
}

fn describe(payload: &[u8]) -> Result<String, Error> {
    match payload.get(..4) {
        Some([2, 0, 8, 0x50]) => {
            let matrix = parse_matrix(payload, 0x84b)?;
            Ok(format!("standard matrix={matrix:?}"))
        }
        Some([0xf1, 0x40, 0, profile @ (0xd0 | 0xd1 | 0xd3)]) => {
            let prefix = alternate_prefix(payload)?;
            let mut candidates = Vec::new();
            // Bounded offline discovery only, never a production navigation
            // rule. A CRC for the outer envelope does not establish element
            // boundaries or the identity of its audio waveforms.
            for start in 4..prefix.len().saturating_sub(2) {
                if prefix.get(start..start + 2) != Some(&[3, 0]) {
                    continue;
                }
                if let Ok(header) = layout_header(&prefix[start..], 0x84b) {
                    if header.output_mask == 0x886b {
                        let status = parse_matrix(&prefix[start..], 0x84b);
                        let rows = type3_rows_candidate(&prefix[start..]);
                        let declaration = type241_header(&prefix[..start]);
                        let dynamic = type241_dynamic(&prefix[..start]);
                        candidates.push(format!("byte={start} header={header:?} matrix={status:?} unverified_rows12={rows:?} static241={declaration:?} dynamic241={dynamic:?}"));
                    }
                }
            }
            Ok(format!(
                "profile={profile:02x} prefix_crc_bytes={} layout_candidates={candidates:?}",
                prefix.len()
            ))
        }
        _ => Err(Error::Unsupported("profile")),
    }
}

fn probe(path: &str, limit: usize, input_index: usize) -> Result<(), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open: {e}"))?;
    let mut reader = BufReader::new(file);
    let mut decoder = HdDecoder::new();
    let mut offset = 0usize;
    let mut frames = 0usize;
    let mut pending = 0usize;
    let mut nav_matches = 0usize;
    let mut type69_marker_matches = 0usize;
    let mut extension_errors = 0usize;
    let mut invalid_metadata = 0usize;
    let mut observations = BTreeMap::<String, usize>::new();
    let mut core = Vec::new();
    let mut exss = Vec::new();
    let mut incomplete_tail = false;
    while offset < limit {
        let mut header = [0u8; 18];
        // Distinguish clean EOF from a partial header; no silent self-skip.
        let n = reader.read(&mut header[..1]).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        reader
            .read_exact(&mut header[1..])
            .map_err(|e| format!("core header: {e}"))?;
        let info = parse_header(&header).map_err(|e| format!("core header: {e:?}"))?;
        if info.frame_size < header.len() {
            return Err("short core size".into());
        }
        core.resize(info.frame_size, 0);
        core[..header.len()].copy_from_slice(&header);
        reader
            .read_exact(&mut core[header.len()..])
            .map_err(|e| format!("core data: {e}"))?;
        exss.resize(16, 0);
        reader
            .read_exact(&mut exss)
            .map_err(|e| format!("EXSS header: {e}"))?;
        if exss.get(..4) != Some(&[0x64, 0x58, 0x20, 0x25]) {
            return Err("EXSS sync".into());
        }
        let mut bits = Bits {
            bytes: &exss,
            pos: 42,
        };
        let wide = bits.read(1).map_err(|e| format!("{e:?}"))? as usize;
        bits.read(8 + wide * 4).map_err(|e| format!("{e:?}"))?;
        let size = bits.read(16 + wide * 4).map_err(|e| format!("{e:?}"))? as usize + 1;
        if size < 16 {
            return Err("short EXSS size".into());
        }
        let end = offset
            .checked_add(core.len())
            .and_then(|n| n.checked_add(size))
            .ok_or("frame overflow")?;
        if end > limit {
            break;
        }
        exss.resize(size, 0);
        if let Err(e) = reader.read_exact(&mut exss[16..]) {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                incomplete_tail = true;
                break;
            }
            return Err(format!("EXSS data: {e}"));
        }
        if exss_substream_size(&exss) != Some(size) {
            return Err("EXSS parser rejected frame".into());
        }
        match decoder.decode(&core, &exss) {
            Ok(frame) => {
                frames += 1;
                if frame.x_descriptor_navigation_used
                    && frame.x_descriptor_offset == Some(frame.x_payload_offset)
                    && frame.x_descriptor_size == Some(frame.x_payload.len())
                {
                    nav_matches += 1;
                    // A marker observation within the existing, independently
                    // checked navigation word; not a general association parser.
                    let mut tail = Bits {
                        bytes: &frame.exss_descriptor_tail,
                        pos: 37,
                    };
                    if tail.read(8) == Ok(69) {
                        type69_marker_matches += 1;
                    }
                }
                if frame.x_decode_error.is_some() {
                    extension_errors += 1;
                }
                let observation = if frame.x_payload.is_empty() {
                    "no extension".to_string()
                } else {
                    match describe(&frame.x_payload) {
                        Ok(value) => value,
                        Err(error) => {
                            if matches!(error, Error::Invalid(_) | Error::Truncated) {
                                invalid_metadata += 1;
                            }
                            format!("unresolved={error:?}")
                        }
                    }
                };
                *observations.entry(observation).or_default() += 1;
            }
            Err(HdError::Pending) => pending += 1,
            Err(e) => return Err(format!("decode at byte {offset}: {e:?}")),
        }
        offset = end;
    }
    if frames == 0 {
        return Err("no decoded frames; corpus was not exercised".into());
    }
    println!(
        "input={input_index} frames={frames} pending={pending} bytes={offset} descriptor_matches={nav_matches} type69_marker_matches={type69_marker_matches} extension_errors={extension_errors} invalid_metadata={invalid_metadata} incomplete_tail={incomplete_tail}"
    );
    for (observation, count) in observations {
        println!("  frames={count} {observation}");
    }
    if incomplete_tail {
        return Err("truncated final frame".into());
    }
    if extension_errors != 0 {
        return Err("extension decode failures".into());
    }
    if invalid_metadata != 0 {
        return Err("invalid private metadata".into());
    }
    Ok(())
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mut limit = 64usize * 1024 * 1024;
    let mut count = 0;
    while let Some(arg) = args.next() {
        if arg == "--max-mb" {
            limit = args
                .next()
                .ok_or("missing --max-mb")?
                .parse::<usize>()
                .map_err(|_| "invalid --max-mb")?
                .checked_mul(1024 * 1024)
                .filter(|n| *n > 0)
                .ok_or("invalid byte limit")?;
        } else {
            count += 1;
            probe(&arg, limit, count)?;
        }
    }
    if count == 0 {
        return Err("usage: xll_private_metadata [--max-mb 64] input.dts ...".into());
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("private metadata probe: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declaration_fixture(indices: &[u8], mode: u32) -> Vec<u8> {
        let mut fields = vec![
            (241, 8),
            (0x40, 8),
            (0, 4),
            (0, 1),
            (1, 4),
            (1, 1),
            (0, 1),
            (1, 1),
            (indices.len() as u32 - 1, 4),
            (0, 1),
            (0, 1),
            (1, 1),
            (1, 1),
            (0, 2),
            (0, 3),
            (2, 3),
            (0x84b, 12),
            (0, 8),
        ];
        for &index in indices {
            fields.extend([(1, 1), (3, 2), (u32::from(index), 3), (mode, 2), (0, 10)]);
        }
        let mut bits = Vec::new();
        for (value, width) in fields {
            for bit in (0..width).rev() {
                bits.push(((value >> bit) & 1) as u8);
            }
        }
        let mut bytes = vec![0; bits.len().div_ceil(8)];
        for (i, bit) in bits.into_iter().enumerate() {
            bytes[i / 8] |= bit << (7 - i % 8);
        }
        bytes
    }

    fn dynamic_fixture(azimuth: u32, target: u32, gain: u32, auxiliary: bool) -> Vec<u8> {
        let header = declaration_fixture(&[0], 1);
        let mut bits: Vec<u8> = (0..82)
            .map(|i| (header[i / 8] >> (7 - i % 8)) & 1)
            .collect();
        let mut fields = vec![
            (0, 3),
            (0x20, 7),
            (1, 1),
            (1, 1),
            (0, 2),
            (61, 6),
            (63, 6),
            (azimuth, 8),
            (90, 7),
            (0, 1),
            (1, 1),
            (0, 1),
            (target, 8),
            (gain, 6),
            (u32::from(auxiliary), 1),
        ];
        if auxiliary {
            fields.extend([(1, 1), (0, 2), (8, 5), (0x80, 8), (1, 1), (1, 1), (61, 6)]);
        }
        for (value, width) in fields {
            for bit in (0..width).rev() {
                bits.push(((value >> bit) & 1) as u8);
            }
        }
        let mut bytes = vec![0; bits.len().div_ceil(8)];
        for (i, bit) in bits.into_iter().enumerate() {
            bytes[i / 8] |= bit << (7 - i % 8);
        }
        bytes
    }

    #[test]
    fn variable_positions_routes_and_auxiliary_are_read_from_fields() {
        for auxiliary in [false, true] {
            let bytes = dynamic_fixture(47, 2, 55, auxiliary);
            let result = type241_dynamic(&bytes).unwrap();
            assert_eq!(result.positions[0].azimuth_code, 47);
            assert_eq!(result.positions[0].elevation_code, 90);
            assert_eq!(result.positions[0].distance_code, 63);
            assert_eq!(result.positions[0].spherical_units, (-220, 90, 64));
            assert_eq!(result.positions[0].reference_rows, [vec![(1, 55)], vec![]]);
            assert_eq!(
                result.auxiliary,
                if auxiliary {
                    vec![(0, 0x80, 61)]
                } else {
                    vec![]
                }
            );
            let changed = type241_dynamic(&dynamic_fixture(193, 16, 54, auxiliary)).unwrap();
            assert_eq!(changed.positions[0].azimuth_code, 193);
            assert_eq!(changed.positions[0].reference_rows[0], vec![(4, 54)]);
            for end in 0..bytes.len() {
                assert!(type241_dynamic(&bytes[..end]).is_err());
            }
            let mut extended = bytes;
            extended.push(0);
            assert!(type241_dynamic(&extended).is_err());
        }
        assert!(type241_dynamic(&dynamic_fixture(47, 2, 63, false)).is_err());
    }

    #[test]
    fn spherical_calibration_handles_special_angles_and_limits() {
        assert_eq!(spherical_units(193, 60, 0), (220, 0, 0));
        assert_eq!(spherical_units(120, 77, 63), (0, 51, 64));
        assert_eq!(spherical_units(23, 79, 63), (-291, 57, 64));
        assert_eq!(spherical_units(255, 127, 1), (357, 180, 2));
    }

    #[test]
    fn declaration_count_and_indices_are_read_not_inferred_from_profile() {
        for indices in [&[0][..], &[1, 0], &[3, 1, 0, 2], &[4, 2, 1, 0, 3]] {
            for mode in 0..=1 {
                let bytes = declaration_fixture(indices, mode);
                let header = type241_header(&bytes).unwrap();
                assert_eq!(header.end_bit, 64 + indices.len() * 18);
                assert_eq!(header.reference_mask, 0x84b);
                assert_eq!(
                    header
                        .declarations
                        .iter()
                        .map(|d| d.index)
                        .collect::<Vec<_>>(),
                    indices
                );
                assert!(header.declarations.iter().all(|d| d.mode == mode as u8));
                for end in 0..bytes.len() {
                    assert!(type241_header(&bytes[..end]).is_err());
                }
            }
        }
    }

    #[test]
    fn declaration_unsupported_forms_are_not_silently_skipped() {
        assert!(type241_header(&declaration_fixture(&[0], 2)).is_err());
        let mut bytes = declaration_fixture(&[0], 0);
        bytes[7] = 1;
        assert!(type241_header(&bytes).is_err());
        bytes[7] = 0;
        bytes[8] &= 0x7f;
        assert!(type241_header(&bytes).is_err());
    }

    // Construct metadata from fields, not an identifying corpus excerpt.
    fn fixture(gain: u32, first_target: u32) -> Vec<u8> {
        let mut bits = Vec::new();
        let mut put = |value: u32, width: usize| {
            for bit in (0..width).rev() {
                bits.push(((value >> bit) & 1) as u8);
            }
        };
        for (value, width) in [
            (2, 8),
            (0, 8),
            (0, 1),
            (1, 4),
            (0, 4),
            (1, 1),
            (0, 1),
            (1, 1),
            (0, 2),
            (0, 3),
            (2, 3),
            (0x84b, 12),
            (1, 1),
            (61, 6),
            (0, 1),
            (3, 3),
            (0x886b, 16),
            (2, 5),
            (0, 6),
            (1, 1),
            (0, 1),
            (1, 1),
            (61, 6),
            (0, 1),
            (first_target, 8),
            (gain, 6),
            (4, 8),
            (55, 6),
            (16, 8),
            (55, 6),
            (32, 8),
            (55, 6),
        ] {
            put(value, width);
        }
        let mut bytes = vec![0u8; bits.len().div_ceil(8)];
        for (i, bit) in bits.into_iter().enumerate() {
            bytes[i / 8] |= bit << (7 - i % 8);
        }
        bytes.extend_from_slice(&crc16(&bytes).to_be_bytes());
        bytes
    }

    #[test]
    fn standard_matrix_reads_routes_and_gain_codes() {
        let matrix = parse_matrix(&fixture(55, 2), 0).expect("synthetic matrix");
        assert_eq!(matrix.reference_mask, 0x84b);
        assert_eq!(matrix.output_mask, 0x886b);
        assert_eq!(matrix.encoded_bytes, 21);
        assert_eq!(
            matrix
                .routes
                .iter()
                .map(|r| (r.source, r.target, r.gain_code))
                .collect::<Vec<_>>(),
            vec![
                ("TFL", "L", 55),
                ("TFR", "R", 55),
                ("TBL", "Lb", 55),
                ("TBR", "Rb", 55)
            ]
        );
        let changed = parse_matrix(&fixture(54, 1), 0).expect("changed matrix");
        assert_eq!(changed.routes[0].target, "C");
        assert_eq!(changed.routes[0].gain_code, 54);
        assert_eq!(matrix.routes[0].calibrated_q15, Some(23170));
        assert_eq!(changed.routes[0].calibrated_q15, None);
    }

    #[test]
    fn every_single_bit_corruption_and_truncation_is_rejected() {
        let bytes = fixture(55, 2);
        for end in 0..bytes.len() {
            assert!(parse_matrix(&bytes[..end], 0).is_err());
        }
        for bit in 0..bytes.len() * 8 {
            let mut damaged = bytes.clone();
            damaged[bit / 8] ^= 1 << (bit % 8);
            assert!(parse_matrix(&damaged, 0).is_err(), "bit {bit}");
        }
    }

    #[test]
    fn gain_escape_is_not_silently_calibrated() {
        assert_eq!(
            parse_matrix(&fixture(63, 2), 0),
            Err(Error::Unsupported("gain escape"))
        );
    }

    #[test]
    fn alternate_candidate_remains_separate_from_validated_matrix() {
        let fields = [
            (3, 8),
            (0, 8),
            (0, 1),
            (1, 4),
            (1, 4),
            (0, 1),
            (0, 1),
            (0, 4),
            (1, 1),
            (61, 6),
            (0, 1),
            (3, 3),
            (0x886b, 16),
            (2, 5),
            (0, 6),
            (1, 1),
            (0, 1),
            (0x3fa, 12),
            (0x12, 12),
            (55, 6),
            (61, 6),
            (0x24, 12),
            (55, 6),
            (61, 6),
            (0x440, 12),
            (55, 6),
            (61, 6),
            (0x880, 12),
            (55, 6),
            (61, 6),
            (0, 5),
        ];
        let mut bits = Vec::new();
        for (value, width) in fields {
            for bit in (0..width).rev() {
                bits.push(((value >> bit) & 1) as u8);
            }
        }
        let mut prefix = vec![0xf1, 0x40, 0, 0xd1];
        for byte in bits.chunks_exact(8) {
            prefix.push(byte.iter().fold(0, |a, b| (a << 1) | b));
        }
        prefix.extend_from_slice(&crc16(&prefix).to_be_bytes());
        let body = &prefix[4..];
        assert_eq!(
            parse_matrix(body, 0x84b),
            Err(Error::Unsupported("inline matrix flag"))
        );
        assert_eq!(
            type3_rows_candidate(body).unwrap(),
            vec![
                vec![(1, 55), (4, 61)],
                vec![(2, 55), (5, 61)],
                vec![(6, 55), (10, 61)],
                vec![(7, 55), (11, 61)]
            ]
        );
        let prefix_len = prefix.len();
        prefix.extend_from_slice(OUTER_SUFFIX);
        assert_eq!(alternate_prefix(&prefix).unwrap().len(), prefix_len);
        for bit in 0..prefix_len * 8 {
            let mut damaged = prefix.clone();
            damaged[bit / 8] ^= 1 << (bit % 8);
            assert!(alternate_prefix(&damaged).is_err());
        }
    }

    #[test]
    fn unrelated_or_unprotected_alternate_data_is_rejected() {
        assert!(alternate_prefix(&[0; 128]).is_err());
        let mut bytes = vec![0xf1, 0x40, 0, 0xd3, 0, 0];
        bytes.extend_from_slice(OUTER_SUFFIX);
        assert!(alternate_prefix(&bytes).is_err());
        assert!(describe(&[0xf1, 0x40, 0, 0xd4]).is_err());
    }
}
