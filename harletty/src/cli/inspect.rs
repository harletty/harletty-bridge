//! What a stream's encoder did, block by block.
//!
//! `harletty info` says what a stream *is*. This says what its encoder
//! *chose* — which channels are matrixed against which, with what
//! coefficients, how often the filters and codebooks are restated, how wide
//! the residuals are — in the units the bitstream itself uses. It exists to
//! hold one stream next to another: a shipped stream next to one from an
//! encoder under development, and see where their choices part.
//!
//! Three views, each behind its own flag of `harletty info`:
//!
//! - `--matrices` prints every matrix as it is declared or changed, restart
//!   headers included, with coefficients scaled to real numbers.
//! - `--params` prints each block's channel parameters: which channels say
//!   anything, and what.
//! - `--filters` prints every prediction filter as it is restated, its taps
//!   scaled to real numbers, so a stream's choice of predictors can be read.
//! - `--stats` totals the same things over the whole stream, per substream.
//!
//! Sync word C sends coefficients without repeating a matrix's
//! configuration, so the configuration a substream holds is carried here
//! from block to block, as a decoder carries it. Its interpolation step is
//! printed as the decoder applies it: a coefficient moves linearly from its
//! value to value plus step across the access unit, and keeps the step at
//! the end.

use anyhow::Result;
use log::Level;
use truehd::process::extract::Extractor;
use truehd::process::parse::Parser;
use truehd::structs::access_unit::AccessUnit;
use truehd::structs::block::Block;
use truehd::structs::matrix::Matrixing;
use truehd::structs::restart_header::RestartSyncWord;

use super::command::{Cli, InfoArgs};
use crate::input::InputReader;

const SUBSTREAMS: usize = 4;
const CHANNELS: usize = 16;
const MAX_ORDER: usize = 8;

/// One primitive matrix's configuration, held between blocks.
#[derive(Clone, Copy, Default)]
struct MatrixConfig {
    channel: u8,
    frac_bits: u8,
    /// Sync word C's whole-matrix shift, as a power of two.
    shift: i8,
    bypass_bits: u8,
    dither: u8,
    delta_bits: u8,
    delta_precision: u8,
}

/// What a decoder holds for one substream between blocks.
#[derive(Clone, Copy)]
struct SubstreamState {
    sync: RestartSyncWord,
    min_chan: usize,
    max_chan: usize,
    max_matrix_chan: usize,
    matrices: usize,
    config: [MatrixConfig; CHANNELS],
    fir: [usize; CHANNELS],
    iir: [usize; CHANNELS],
    book: [usize; CHANNELS],
    lsbs: [u32; CHANNELS],
    shift: [i8; CHANNELS],
}

impl Default for SubstreamState {
    fn default() -> Self {
        Self {
            sync: RestartSyncWord::None,
            min_chan: 0,
            max_chan: 0,
            max_matrix_chan: 0,
            matrices: 0,
            config: [MatrixConfig::default(); CHANNELS],
            fir: [0; CHANNELS],
            iir: [0; CHANNELS],
            book: [0; CHANNELS],
            lsbs: [24; CHANNELS],
            shift: [0; CHANNELS],
        }
    }
}

impl SubstreamState {
    /// What a restart header leaves a decoder holding: no filters, no
    /// codebook, twenty-four raw bits, no shift, no matrices.
    fn restart(&mut self, header: &truehd::structs::restart_header::RestartHeader) {
        self.sync = header.restart_sync_word;
        self.min_chan = header.min_chan as usize;
        self.max_chan = header.max_chan as usize;
        self.max_matrix_chan = header.max_matrix_chan as usize;
        self.matrices = 0;
        self.fir = [0; CHANNELS];
        self.iir = [0; CHANNELS];
        self.book = [0; CHANNELS];
        self.lsbs = [24; CHANNELS];
        self.shift = [0; CHANNELS];
    }
}

/// Totals over the stream, per substream.
#[derive(Clone, Copy, Default)]
struct Stats {
    bytes: u64,
    units: u64,
    restarts: u64,
    blocks: u64,
    channel_blocks: u64,
    restated: u64,
    fir_restated: u64,
    iir_restated: u64,
    fir: [u64; MAX_ORDER + 1],
    iir: [u64; MAX_ORDER + 1],
    /// Channel-blocks at each (first, second) order pair.
    pairs: [[u64; MAX_ORDER + 1]; MAX_ORDER + 1],
    book: [u64; 4],
    lsbs: u64,
    shifts: [u64; 8],
    units_matrixed: u64,
    matrix_updates: u64,
    blocks_interpolated: u64,
}

struct Inspector<'a> {
    args: &'a InfoArgs,
    units: usize,
    substreams: usize,
    state: [SubstreamState; SUBSTREAMS],
    stats: [Stats; SUBSTREAMS],
}

pub fn run(args: &InfoArgs, cli: &Cli) -> Result<()> {
    let mut reader = InputReader::new(&args.input)?;
    let mut extractor = Extractor::default();
    let mut parser = Parser::default();
    parser.set_fail_level(if cli.strict {
        Level::Warn
    } else {
        Level::Error
    });

    let limit = args.units.unwrap_or(usize::MAX);
    let mut inspector = Inspector {
        args,
        units: 0,
        substreams: 0,
        state: [SubstreamState::default(); SUBSTREAMS],
        stats: [Stats::default(); SUBSTREAMS],
    };

    reader.process_chunks(64 * 1024, |chunk| {
        extractor.push_bytes(chunk);
        for frame in extractor.by_ref() {
            let Ok(frame) = frame else { continue };
            match parser.parse(&frame) {
                Ok(access_unit) => inspector.unit(&access_unit),
                Err(e) => {
                    if cli.strict {
                        return Err(e);
                    }
                    log::warn!("Parse error at access unit {}: {e}", inspector.units);
                    inspector.units += 1;
                }
            }
            if inspector.units >= limit {
                return Ok(false);
            }
        }
        Ok(true)
    })?;

    if args.stats {
        inspector.print_stats();
    }
    Ok(())
}

impl Inspector<'_> {
    fn unit(&mut self, unit: &AccessUnit) {
        if let Some(major_sync) = &unit.major_sync_info {
            self.substreams = major_sync.substreams.min(SUBSTREAMS);
        }
        let mut end = 0u64;
        for substream in 0..self.substreams {
            let words = u64::from(unit.substream_directory[substream].substream_end_ptr);
            self.stats[substream].bytes += words.saturating_sub(end) * 2;
            end = words;
            self.stats[substream].units += 1;

            let blocks = &unit.substream_segment[substream].block;
            for (index, block) in blocks.iter().enumerate() {
                self.block(substream, index, block);
            }
            if self.state[substream].matrices > 0 {
                self.stats[substream].units_matrixed += 1;
            }
        }
        self.units += 1;
    }

    fn block(&mut self, substream: usize, index: usize, block: &Block) {
        let unit = self.units;
        let state = &mut self.state[substream];
        let stats = &mut self.stats[substream];
        stats.blocks += 1;

        if let Some(header) = &block.restart_header {
            state.restart(header);
            stats.restarts += 1;
            if self.args.matrices {
                let assignment = &header.ch_assign[..=state.max_matrix_chan];
                println!(
                    "AU {unit} ss{substream} restart {:?}: channels {}..{}, matrix channels 0..{}, \
                     assignment {:?}, dither shift {}, seed {:#08x}, max shift {}, max lsbs {}",
                    header.restart_sync_word,
                    state.min_chan,
                    state.max_chan,
                    state.max_matrix_chan,
                    assignment,
                    header.dither_shift,
                    header.dither_seed,
                    header.max_shift,
                    header.max_lsbs,
                );
            }
        }

        let Some(header) = &block.block_header else {
            return;
        };

        if let Some(matrixing) = &header.matrixing {
            Self::matrices(self.args, unit, substream, index, state, stats, matrixing);
        }

        for channel in 0..CHANNELS {
            if let Some(shift) = header.output_shift[channel] {
                state.shift[channel] = shift;
            }
        }

        let mut line = String::new();
        for channel in state.min_chan..=state.max_chan.min(CHANNELS - 1) {
            stats.channel_blocks += 1;
            match &header.channel_params[channel] {
                Some(params) => {
                    stats.restated += 1;
                    let mut said = String::new();
                    if let Some(a) = &params.coeffs_a {
                        state.fir[channel] = a.order as usize;
                        stats.fir_restated += 1;
                        said.push_str(&format!(" A{}q{}", a.order, a.coeff_q));
                        if self.args.filters {
                            println!(
                                "AU {unit} ss{substream} blk{index} ch{channel} A {}",
                                taps(a)
                            );
                        }
                    }
                    if let Some(b) = &params.coeffs_b {
                        state.iir[channel] = b.order as usize;
                        stats.iir_restated += 1;
                        said.push_str(&format!(" B{}q{}", b.order, b.coeff_q));
                        if self.args.filters {
                            println!(
                                "AU {unit} ss{substream} blk{index} ch{channel} B {}",
                                taps(b)
                            );
                        }
                    }
                    if let Some(offset) = params.huff_offset {
                        said.push_str(&format!(" off{offset}"));
                    }
                    state.book[channel] = params.huff_type;
                    state.lsbs[channel] = params.huff_lsbs;
                    said.push_str(&format!(" H{}/{}", params.huff_type, params.huff_lsbs));
                    if let Some(shift) = header.output_shift[channel] {
                        said.push_str(&format!(" s{shift}"));
                    }
                    line.push_str(&format!("  ch{channel}:{said}"));
                }
                None => line.push_str(&format!("  ch{channel}: =")),
            }
            stats.fir[state.fir[channel].min(MAX_ORDER)] += 1;
            stats.iir[state.iir[channel].min(MAX_ORDER)] += 1;
            stats.pairs[state.fir[channel].min(MAX_ORDER)][state.iir[channel].min(MAX_ORDER)] += 1;
            stats.book[state.book[channel].min(3)] += 1;
            stats.lsbs += u64::from(state.lsbs[channel]);
            stats.shifts[state.shift[channel].clamp(0, 7) as usize] += 1;
        }

        if self.args.params {
            let size = header
                .block_size
                .map(|s| format!(" size {s}"))
                .unwrap_or_default();
            println!("AU {unit} ss{substream} blk{index}{size}{line}");
        }
    }

    /// One block's matrix statement, folded into the substream's state and
    /// printed if asked. Only a change is printed: sync words A and B restate
    /// a matrix whole whenever they say anything, and sync word C says which
    /// of the configuration, the coefficients and the step it is sending.
    fn matrices(
        args: &InfoArgs,
        unit: usize,
        substream: usize,
        index: usize,
        state: &mut SubstreamState,
        stats: &mut Stats,
        matrixing: &Matrixing,
    ) {
        let sync_c = state.sync == RestartSyncWord::C;
        // Sync word A reads two coefficients past the last matrix channel:
        // the decoder's own noise channels.
        let sources = state.max_matrix_chan
            + if state.sync == RestartSyncWord::A {
                2
            } else {
                0
            };

        let new_config = !sync_c || matrixing.new_matrix_config;
        let new_coefficients = !sync_c || matrixing.new_matrix;
        if new_config {
            state.matrices = matrixing.primitive_matrices;
            for (i, matrix) in matrixing.matrices[..state.matrices].iter().enumerate() {
                let config = &mut state.config[i];
                config.channel = matrix.matrix_ch;
                config.frac_bits = matrix.frac_bits;
                config.shift = if sync_c { matrix.cf_shift_code } else { 0 };
                config.bypass_bits = if sync_c {
                    matrix.lsb_bypass_bit_count
                } else {
                    u8::from(matrix.lsb_bypass_used)
                };
                config.dither = matrix.dither_scale;
            }
        }
        if new_coefficients {
            stats.matrix_updates += 1;
            if args.matrices {
                println!(
                    "AU {unit} ss{substream} blk{index}: {} matrices{}",
                    state.matrices,
                    if new_config {
                        ", new configuration"
                    } else {
                        ""
                    }
                );
                for (i, matrix) in matrixing.matrices[..state.matrices].iter().enumerate() {
                    let config = state.config[i];
                    let terms: Vec<String> = (0..=sources)
                        .filter(|&c| matrix.m_coeff[c] != 0)
                        .map(|c| {
                            format!(
                                "ch{c}:{:+.5}",
                                coefficient(matrix.m_coeff[c], config.frac_bits, config.shift)
                            )
                        })
                        .collect();
                    println!(
                        "  #{i} -> ch{}  frac {} shift {} bypass {} dither {}  [{}]",
                        config.channel,
                        config.frac_bits,
                        config.shift,
                        config.bypass_bits,
                        config.dither,
                        terms.join(" ")
                    );
                }
            }
        }

        if sync_c && matrixing.interpolation_used {
            stats.blocks_interpolated += 1;
            if matrixing.new_delta {
                if matrixing.new_delta_config {
                    for (i, matrix) in matrixing.matrices[..state.matrices].iter().enumerate() {
                        state.config[i].delta_bits = matrix.delta_bits;
                        state.config[i].delta_precision = matrix.delta_precision;
                    }
                }
                if args.matrices {
                    println!(
                        "AU {unit} ss{substream} blk{index}: interpolation step, reached over the unit"
                    );
                    for (i, matrix) in matrixing.matrices[..state.matrices].iter().enumerate() {
                        let config = state.config[i];
                        let terms: Vec<String> = (0..=state.max_matrix_chan)
                            .filter(|&c| matrix.delta_cf[c] != 0)
                            .map(|c| {
                                format!(
                                    "ch{c}:{:+.3e}",
                                    coefficient(
                                        matrix.delta_cf[c],
                                        config.frac_bits + config.delta_precision,
                                        config.shift
                                    )
                                )
                            })
                            .collect();
                        println!(
                            "  #{i} -> ch{}  delta bits {} precision {}  [{}]",
                            config.channel,
                            config.delta_bits,
                            config.delta_precision,
                            terms.join(" ")
                        );
                    }
                }
            }
        }
    }

    fn print_stats(&self) {
        println!();
        println!("Block statistics over {} access units", self.units);
        let total_bytes: u64 = self.stats[..self.substreams].iter().map(|s| s.bytes).sum();
        for (index, stats) in self.stats[..self.substreams].iter().enumerate() {
            if stats.units == 0 {
                continue;
            }
            let state = &self.state[index];
            println!(
                "  substream {index}: channels {}..{}, {} bytes ({:.1} % of the substreams)",
                state.min_chan,
                state.max_chan,
                stats.bytes,
                percent(stats.bytes, total_bytes)
            );
            println!(
                "    restarts              {} (one every {:.1} units), {} blocks",
                stats.restarts,
                stats.units as f64 / stats.restarts.max(1) as f64,
                stats.blocks
            );
            println!(
                "    channel parameters    said in {:.1} % of channel-blocks; first filter restated in \
                 {:.1} %, second in {:.1} %",
                percent(stats.restated, stats.channel_blocks),
                percent(stats.fir_restated, stats.channel_blocks),
                percent(stats.iir_restated, stats.channel_blocks)
            );
            println!(
                "    first filter orders   {}",
                histogram(&stats.fir, stats.channel_blocks)
            );
            println!(
                "    second filter orders  {}",
                histogram(&stats.iir, stats.channel_blocks)
            );
            println!(
                "    order pairs           {}",
                pairs(&stats.pairs, stats.channel_blocks)
            );
            println!(
                "    codebooks             {}",
                histogram(&stats.book, stats.channel_blocks).replace("0:", "raw:")
            );
            println!(
                "    raw residual width    {:.2} bits a sample (huff_lsbs)",
                stats.lsbs as f64 / stats.channel_blocks.max(1) as f64
            );
            println!(
                "    output shift          {}",
                histogram(&stats.shifts, stats.channel_blocks)
            );
            println!(
                "    matrices              in force in {:.1} % of units, {} statements, interpolation \
                 in {:.1} % of blocks",
                percent(stats.units_matrixed, stats.units),
                stats.matrix_updates,
                percent(stats.blocks_interpolated, stats.blocks)
            );
        }
    }
}

/// A filter's taps as the decoder applies them. Each coefficient is read at
/// `coeff_bits` and shifted up by `coeff_shift` — the parser has already done
/// that to the values it holds — and the whole sum is shifted down by
/// `coeff_q`, so a tap is `coeff · 2^−coeff_q`. The order, the precision and
/// the shift are printed with them, since they are what the description
/// costs.
fn taps(filter: &truehd::structs::filter::FilterCoeffs) -> String {
    let values: Vec<String> = filter.coeff[..filter.order as usize]
        .iter()
        .map(|&c| {
            format!(
                "{:+.5}",
                f64::from(c) * 2f64.powi(-i32::from(filter.coeff_q))
            )
        })
        .collect();
    format!(
        "order {} q {} bits {} shift {}{}  [{}]",
        filter.order,
        filter.coeff_q,
        filter.coeff_bits,
        filter.coeff_shift,
        if filter.new_states { " with state" } else { "" },
        values.join(" ")
    )
}

/// A coefficient as the decoder applies it: `frac_bits` fractional bits,
/// then sync word C's whole-matrix shift.
fn coefficient(raw: i32, frac_bits: u8, shift: i8) -> f64 {
    f64::from(raw) * 2f64.powi(i32::from(shift) - i32::from(frac_bits))
}

fn percent(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        100.0 * part as f64 / whole as f64
    }
}

/// The (first, second) order pairs that account for most channel-blocks,
/// largest first, until 95 % is explained or eight are listed.
fn pairs(bins: &[[u64; MAX_ORDER + 1]; MAX_ORDER + 1], total: u64) -> String {
    let mut all: Vec<(u64, usize, usize)> = Vec::new();
    for (a, row) in bins.iter().enumerate() {
        for (b, &count) in row.iter().enumerate() {
            if count > 0 {
                all.push((count, a, b));
            }
        }
    }
    all.sort_unstable_by(|x, y| y.cmp(x));
    let mut shown = 0u64;
    let mut out = Vec::new();
    for (count, a, b) in all.into_iter().take(8) {
        out.push(format!("{a}+{b}:{:.0}%", percent(count, total)));
        shown += count;
        if shown * 20 >= total * 19 {
            break;
        }
    }
    out.join("  ")
}

/// `value:share%` for every non-empty bin.
fn histogram(bins: &[u64], total: u64) -> String {
    bins.iter()
        .enumerate()
        .filter(|(_, count)| **count > 0)
        .map(|(value, count)| format!("{value}:{:.0}%", percent(*count, total)))
        .collect::<Vec<_>>()
        .join("  ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_coefficient_is_scaled_by_its_fractional_bits_and_the_shift() {
        assert_eq!(coefficient(1 << 14, 14, 0), 1.0);
        assert_eq!(coefficient(-(1 << 13), 14, 0), -0.5);
        assert_eq!(coefficient(1 << 14, 14, 2), 4.0);
        assert_eq!(coefficient(3, 2, -1), 0.375);
    }

    #[test]
    fn order_pairs_are_listed_largest_first() {
        let mut bins = [[0u64; MAX_ORDER + 1]; MAX_ORDER + 1];
        bins[6][2] = 60;
        bins[7][1] = 30;
        bins[8][0] = 10;
        assert_eq!(pairs(&bins, 100), "6+2:60%  7+1:30%  8+0:10%");
    }

    #[test]
    fn a_histogram_names_only_the_bins_that_are_used() {
        assert_eq!(histogram(&[3, 0, 1], 4), "0:75%  2:25%");
        assert_eq!(histogram(&[0, 0], 0), "");
    }
}
