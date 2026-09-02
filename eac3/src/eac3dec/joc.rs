// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::needless_range_loop)]

use std::sync::OnceLock;

use super::metadata::{JocObject, JocObjectData, JocPayload};
use super::pcm::CorePcmFrame;
use super::qmf::{QMF_SUBBANDS, QmfSubbands, QuadratureMirrorFilterBank};
use super::syncframe::ParseError;
use crate::BedChannel;

const JOC_INPUT_ORDER: [BedChannel; 7] = [
    BedChannel::FrontLeft,
    BedChannel::FrontRight,
    BedChannel::Center,
    BedChannel::SurroundLeft,
    BedChannel::SurroundRight,
    BedChannel::RearLeft,
    BedChannel::RearRight,
];

const JOC_PARAMETER_BAND_BOUNDARIES: [&[u8]; 8] = [
    &[0],
    &[0, 3, 14],
    &[0, 1, 3, 9, 23],
    &[0, 1, 2, 4, 8, 14, 23],
    &[0, 1, 2, 3, 5, 7, 9, 14, 23],
    &[0, 1, 2, 3, 4, 6, 8, 11, 14, 18, 23, 35],
    &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 11, 14, 18, 23, 35],
    &[
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 14, 16, 18, 20, 23, 26, 30, 35, 41, 48,
    ],
];

pub type JocSubbandMatrix = Vec<[f32; 64]>;
pub type JocTimeslotMatrices = Vec<JocSubbandMatrix>;
pub type JocObjectMatrices = Vec<JocTimeslotMatrices>;

type SubbandMatrix = JocSubbandMatrix;
type TimeslotMatrices = JocTimeslotMatrices;

#[derive(Debug, Default)]
pub(crate) struct JocObjectDecoderState {
    prev_matrix: Vec<SubbandMatrix>,
    mix_matrix: Vec<[SubbandMatrix; 2]>,
    timeslot_offsets: Vec<[u8; 2]>,
    forward_qmf: Vec<QuadratureMirrorFilterBank>,
    inverse_qmf: Vec<QuadratureMirrorFilterBank>,
    inverse_history: Vec<bool>,
    analysis: Vec<QmfSubbands>,
    last_frame_matrices: JocObjectMatrices,
}

impl JocObjectDecoderState {
    /// Whether the reconstruction is holding no cross-frame history.
    ///
    /// The filter banks and the previous frame's matrices only mean anything
    /// while the core keeps the shape they were filled from. Nothing in the
    /// decode path asks - a reconfiguration resets unconditionally - so this
    /// exists for the tests that check the reset actually happened.
    #[cfg(test)]
    pub fn is_cold(&self) -> bool {
        self.prev_matrix.is_empty()
            && self.forward_qmf.is_empty()
            && self.inverse_qmf.is_empty()
            && self.inverse_history.is_empty()
    }

    pub fn reset(&mut self) {
        self.prev_matrix.clear();
        self.mix_matrix.clear();
        self.timeslot_offsets.clear();
        self.forward_qmf.clear();
        self.inverse_qmf.clear();
        self.inverse_history.clear();
        self.analysis.clear();
        self.last_frame_matrices.clear();
    }

    pub fn decode_frame(
        &mut self,
        core: &CorePcmFrame,
        joc: &JocPayload,
    ) -> Result<Vec<Vec<f32>>, ParseError> {
        let mut objects = Vec::new();
        self.decode_frame_into(core, joc, &mut objects)?;
        Ok(objects)
    }

    pub fn decode_frame_into(
        &mut self,
        core: &CorePcmFrame,
        joc: &JocPayload,
        objects: &mut Vec<Vec<f32>>,
    ) -> Result<(), ParseError> {
        if joc.channel_count > JOC_INPUT_ORDER.len() {
            return Err(ParseError::UnsupportedFeature("joc-channel-count"));
        }
        let samples = core.samples_per_channel();
        if samples == 0 || !samples.is_multiple_of(QMF_SUBBANDS) {
            return Err(ParseError::InvalidHeader("joc-frame-samples"));
        }

        let input_indices = map_input_channel_indices(core, joc.channel_count)?;
        let timeslots = samples / QMF_SUBBANDS;
        self.reconfigure(joc.channel_count, joc.object_count);
        self.build_frame_matrices(joc, timeslots)?;
        prepare_object_output_buffers(objects, joc.object_count, samples);
        self.analysis.resize(joc.channel_count, QmfSubbands::zero());
        let analysis = &mut self.analysis[..joc.channel_count];

        for timeslot in 0..timeslots {
            let sample_offset = timeslot * QMF_SUBBANDS;
            for (joc_channel, core_channel) in input_indices.iter().enumerate() {
                analysis[joc_channel] = self.forward_qmf[joc_channel].process_forward(
                    &core.fullband_channels[*core_channel]
                        [sample_offset..sample_offset + QMF_SUBBANDS],
                );
            }

            for object_index in 0..joc.object_count {
                let active = joc.objects[object_index].active;
                let matrix = &self.last_frame_matrices[object_index][timeslot];
                let output =
                    &mut objects[object_index][sample_offset..sample_offset + QMF_SUBBANDS];
                if !active && !self.inverse_history[object_index] {
                    output.fill(0.0);
                    continue;
                }

                let mut mixed = QmfSubbands::zero();
                for (channel_index, gains) in matrix.iter().enumerate() {
                    for subband in 0..QMF_SUBBANDS {
                        mixed.real[subband] +=
                            analysis[channel_index].real[subband] * gains[subband];
                        mixed.imaginary[subband] +=
                            analysis[channel_index].imaginary[subband] * gains[subband];
                    }
                }

                self.inverse_qmf[object_index].process_inverse(&mixed, output);
                if joc.gain != 1.0 {
                    for sample in output.iter_mut() {
                        *sample *= joc.gain;
                    }
                }
                if active {
                    self.inverse_history[object_index] = true;
                }
            }
        }

        Ok(())
    }

    pub fn last_frame_matrices(&self) -> &JocObjectMatrices {
        &self.last_frame_matrices
    }

    fn reconfigure(&mut self, channel_count: usize, object_count: usize) {
        self.forward_qmf
            .resize_with(channel_count, QuadratureMirrorFilterBank::new);
        if self.forward_qmf.len() > channel_count {
            self.forward_qmf.truncate(channel_count);
        }

        self.inverse_qmf
            .resize_with(object_count, QuadratureMirrorFilterBank::new);
        if self.inverse_qmf.len() > object_count {
            self.inverse_qmf.truncate(object_count);
        }

        self.inverse_history.resize(object_count, false);
        if self.inverse_history.len() > object_count {
            self.inverse_history.truncate(object_count);
        }
        self.analysis.resize(channel_count, QmfSubbands::zero());
        if self.analysis.len() > channel_count {
            self.analysis.truncate(channel_count);
        }

        self.prev_matrix
            .resize_with(object_count, || vec![[0.0; QMF_SUBBANDS]; channel_count]);
        if self.prev_matrix.len() > object_count {
            self.prev_matrix.truncate(object_count);
        }
        for object in &mut self.prev_matrix {
            object.resize(channel_count, [0.0; QMF_SUBBANDS]);
            if object.len() > channel_count {
                object.truncate(channel_count);
            }
        }

        self.mix_matrix.resize_with(object_count, || {
            [
                vec![[0.0; QMF_SUBBANDS]; channel_count],
                vec![[0.0; QMF_SUBBANDS]; channel_count],
            ]
        });
        if self.mix_matrix.len() > object_count {
            self.mix_matrix.truncate(object_count);
        }
        for object in &mut self.mix_matrix {
            for slot in object {
                slot.resize(channel_count, [0.0; QMF_SUBBANDS]);
                if slot.len() > channel_count {
                    slot.truncate(channel_count);
                }
            }
        }

        self.timeslot_offsets.resize(object_count, [0; 2]);
        if self.timeslot_offsets.len() > object_count {
            self.timeslot_offsets.truncate(object_count);
        }
    }

    fn build_frame_matrices(
        &mut self,
        joc: &JocPayload,
        timeslots: usize,
    ) -> Result<(), ParseError> {
        ensure_object_matrix_storage(
            &mut self.last_frame_matrices,
            joc.object_count,
            timeslots,
            joc.channel_count,
        );
        for object_index in 0..joc.object_count {
            let object = joc
                .objects
                .get(object_index)
                .ok_or(ParseError::InvalidHeader("joc-object"))?;
            let prev_matrix = &mut self.prev_matrix[object_index];
            let mix_matrix = &mut self.mix_matrix[object_index];
            let timeslot_offsets = &mut self.timeslot_offsets[object_index];
            decode_parameter_points(mix_matrix, timeslot_offsets, object, joc.channel_count)?;
            build_object_timeslots(
                prev_matrix,
                mix_matrix,
                *timeslot_offsets,
                object,
                timeslots,
                &mut self.last_frame_matrices[object_index],
            )?;
        }
        Ok(())
    }
}

fn prepare_object_output_buffers(output: &mut Vec<Vec<f32>>, object_count: usize, samples: usize) {
    output.resize_with(object_count, Vec::new);
    if output.len() > object_count {
        output.truncate(object_count);
    }
    for channel in output.iter_mut() {
        if channel.len() != samples {
            channel.resize(samples, 0.0);
        }
    }
}

fn ensure_object_matrix_storage(
    storage: &mut JocObjectMatrices,
    object_count: usize,
    timeslots: usize,
    channel_count: usize,
) {
    storage.resize_with(object_count, Vec::new);
    if storage.len() > object_count {
        storage.truncate(object_count);
    }
    for object in storage.iter_mut() {
        object.resize_with(timeslots, Vec::new);
        if object.len() > timeslots {
            object.truncate(timeslots);
        }
        for timeslot in object.iter_mut() {
            timeslot.resize(channel_count, [0.0; QMF_SUBBANDS]);
            if timeslot.len() > channel_count {
                timeslot.truncate(channel_count);
            }
        }
    }
}

fn build_object_timeslots(
    prev_matrix: &mut SubbandMatrix,
    mix_matrix: &[SubbandMatrix; 2],
    timeslot_offsets: [u8; 2],
    object: &JocObject,
    timeslots: usize,
    output: &mut TimeslotMatrices,
) -> Result<(), ParseError> {
    if !object.active {
        zero_timeslot_matrices(output);
        return Ok(());
    }

    if object.data_points == 1 {
        if object.steep_slope {
            // Two regions, and only two: the previous matrix up to the
            // transmitted timeslot offset, this frame's from there on. With a
            // single data point there is no second matrix to switch to.
            let split = (timeslot_offsets[0] as usize).min(timeslots);
            for (timeslot, matrix) in output.iter_mut().enumerate() {
                if timeslot < split {
                    copy_matrix(matrix, prev_matrix.as_slice());
                } else {
                    copy_matrix(matrix, mix_matrix[0].as_slice());
                }
            }
        } else {
            for (timeslot, matrix) in output.iter_mut().enumerate() {
                let lerp = (timeslot + 1) as f32 / timeslots as f32;
                lerp_matrix(matrix, prev_matrix, &mix_matrix[0], lerp);
            }
        }
    } else if object.steep_slope {
        // Three regions, one boundary per transmitted offset.
        for (timeslot, matrix) in output.iter_mut().enumerate() {
            if timeslot < timeslot_offsets[0] as usize {
                copy_matrix(matrix, prev_matrix.as_slice());
            } else if timeslot < timeslot_offsets[1] as usize {
                copy_matrix(matrix, mix_matrix[0].as_slice());
            } else {
                copy_matrix(matrix, mix_matrix[1].as_slice());
            }
        }
    } else {
        let first_half = (timeslots >> 1).max(1);
        for (timeslot, matrix) in output.iter_mut().enumerate() {
            if timeslot < first_half {
                let lerp = (timeslot + 1) as f32 / first_half as f32;
                lerp_matrix(matrix, prev_matrix, &mix_matrix[0], lerp);
            } else {
                let second_len = (timeslots - first_half).max(1);
                let lerp = (timeslot + 1 - first_half) as f32 / second_len as f32;
                lerp_matrix(matrix, &mix_matrix[0], &mix_matrix[1], lerp);
            }
        }
    }

    copy_matrix(prev_matrix, mix_matrix[object.data_points - 1].as_slice());
    Ok(())
}

fn zero_timeslot_matrices(output: &mut TimeslotMatrices) {
    for timeslot in output.iter_mut() {
        for channel in timeslot.iter_mut() {
            *channel = [0.0; QMF_SUBBANDS];
        }
    }
}

fn map_input_channel_indices(
    core: &CorePcmFrame,
    channel_count: usize,
) -> Result<Vec<usize>, ParseError> {
    let mut indices = Vec::with_capacity(channel_count);
    for channel in &JOC_INPUT_ORDER[..channel_count] {
        let index = resolve_joc_input_channel_index(core, *channel)
            .ok_or(ParseError::UnsupportedFeature("joc-input-layout"))?;
        indices.push(index);
    }
    Ok(indices)
}

fn resolve_joc_input_channel_index(core: &CorePcmFrame, channel: BedChannel) -> Option<usize> {
    core.fullband_channel_order
        .iter()
        .position(|candidate| *candidate == channel)
        .or_else(|| match channel {
            BedChannel::RearLeft => core
                .fullband_channel_order
                .iter()
                .position(|candidate| *candidate == BedChannel::SurroundLeft),
            BedChannel::RearRight => core
                .fullband_channel_order
                .iter()
                .position(|candidate| *candidate == BedChannel::SurroundRight),
            _ => None,
        })
}

/// The dequantization step of clause 6.6.4 Pseudocode 5, which scales a
/// quantized coefficient by `820 / (4096 * (1 + joc_num_quant_idx))`.
///
/// That is 0,2001953125 on the coarse quantizer and 0,10009765625 on the fine
/// one - close enough to 0,2 and 0,1 to read like them, but 0,098 % short if
/// they are used instead, which leaves every non-zero coefficient in both the
/// dense and the sparse path slightly wrong. Both values are dyadic, so the
/// division is exact in binary floating point rather than merely close.
///
/// The successor codec keeps the same step: A-JOC's wet quantizer in ETSI
/// TS 103 190-2 tables 46 and 47 is 2,001953125/10 and 2,00195313/20, the same
/// two numbers.
fn dequantization_step(quantization_table: usize) -> f32 {
    820.0 / (4096.0 * (1 + quantization_table) as f32)
}

fn decode_parameter_points(
    mix_matrix: &mut [SubbandMatrix; 2],
    timeslot_offsets: &mut [u8; 2],
    object: &JocObject,
    channel_count: usize,
) -> Result<(), ParseError> {
    if !object.active {
        return Ok(());
    }

    let quantization_table = object
        .quantization_table
        .ok_or(ParseError::InvalidHeader("joc_num_quant_idx"))?
        as usize;
    let bands = object.bands;
    let data_points = object.data_points;
    let data = object
        .data
        .as_ref()
        .ok_or(ParseError::InvalidHeader("joc-object-data"))?;

    if object.steep_slope {
        for (slot, offset) in object.timeslot_offsets.iter().copied().enumerate() {
            timeslot_offsets[slot] = offset;
        }
    }

    match data {
        JocObjectData::Dense { matrices } => {
            if matrices.len() != data_points {
                return Err(ParseError::InvalidHeader("joc_dense_points"));
            }
            let gain_step = dequantization_step(quantization_table);
            let center = (quantization_table as f32 * 48.0 + 48.0) * gain_step;
            let max = center * 2.0;
            for (data_point, source) in matrices.iter().enumerate() {
                if source.len() != channel_count {
                    return Err(ParseError::InvalidHeader("joc_dense_channels"));
                }
                for (channel_index, source_channel) in source.iter().enumerate() {
                    if source_channel.len() != bands {
                        return Err(ParseError::InvalidHeader("joc_dense_bands"));
                    }
                    let channel = &mut mix_matrix[data_point][channel_index];
                    let mut current = 0.0f32;
                    for (band_index, value) in source_channel.iter().enumerate() {
                        current = if band_index == 0 {
                            (center + *value as f32 * gain_step) % max
                        } else {
                            (current + *value as f32 * gain_step) % max
                        };
                        channel[band_index] = current - center;
                    }
                }
            }
        }
        JocObjectData::Sparse {
            channel_indices,
            vectors,
        } => {
            if channel_indices.len() != data_points || vectors.len() != data_points {
                return Err(ParseError::InvalidHeader("joc_sparse_points"));
            }
            // Clause 6.6.2 Pseudocode 2. Sparse mode sources each parameter band
            // from exactly one input channel, so instead of a coefficient per
            // channel it transmits the channel to use and a single coefficient.
            // Both arrive differentially, as steps around the previous band.
            //
            // Three things about that pseudocode do not survive contact with
            // real streams, and between them they are why this path used to be
            // stubbed out. Each was settled on eleven E-AC-3 JOC streams
            // carrying 1 940 sparse frames between them - 29 100 sparse
            // objects, all on the coarse quantizer - by comparing every one
            // against the nearest dense frame on each side, which cannot have
            // moved much in 32 ms. The figures below are that correlation. For
            // scale, one dense frame predicts the next at 0,714 on the same
            // measure, so the 0,667 this code reaches is most of what the
            // measure allows, while Pseudocode 2 read exactly as printed
            // scores 0,047 - no agreement at all.
            //
            // Pseudocode 2 uses one `offset` - 50, or 100 for the fine
            // quantizer - for two different jobs, and only one of them is right.
            //
            // As the value the chain starts from it is correct as published.
            // Raising it improves agreement smoothly up to 0,667 at 50 and then
            // collapses to -0,312 at 51, because 50 is the largest start the
            // coded chains survive: the same 111 911 bands wrap past the top of
            // the quantizer at every start from 45 to 50, and 2 287 more do at
            // 51. Pseudocode 3 beside it seeds the dense chain from a different
            // constant - 48 or 96, the quantizer's zero - so the sparse 50 is a
            // deliberate coded offset and not a drafting slip.
            //
            // As the value every *other* channel takes it is wrong, because
            // clause 6.6.4 dequantizes about joc_num_quant/2 - 48 or 96 - so an
            // unnamed channel would come out at a gain of 0,4, at either
            // quantizer, rather than silent, which is the opposite of sparse.
            // Filling those channels with the quantizer's zero instead scores
            // 0,667 against 0,648. The successor tool agrees: A-JOC, in ETSI
            // TS 103 190-2, fills the channels its sparse mask does not select
            // with (nquant-1)/2, which its dequantization table maps to exactly
            // 0, and clause 6.3.6.2.6 says an untransmitted matrix element
            // "shall be set to a default value of 0,0" because that input "is
            // not mixed into the reconstruction". So the chain starts at 50/100
            // and unnamed channels take the quantizer's zero.
            //
            // Its channel step is written as the sum of two *transmitted*
            // indices, `joc_channel_idx[pb-1] + joc_channel_idx[pb]`. Read that
            // way an unbroken run of zeroes walks the object off its channel
            // after one band. Read as a running total - what the accumulator's
            // own name, joc_channel_idx_mod, points at, and what makes the 1-bit
            // Huffman codeword for a step of zero mean "same channel as the last
            // band" - agreement nearly triples (0,233 to 0,667). So the step
            // applies to the previous band's *resolved* channel.
            //
            // Its coefficient step is written as `joc_mix_mtx_q[ch][pb-1]` for
            // the named channel, which restarts the chain from whatever that
            // channel last held - the fill value - every time the object moves
            // to a different channel. The chain belongs to the object rather
            // than to the channel: carrying it across those moves agrees better
            // (0,667 against 0,557) and lands fewer bands on the quantizer's
            // rails.
            let gain_step = dequantization_step(quantization_table);
            // joc_num_quant, and the quantized value that clause 6.6.4
            // dequantizes to a gain of zero.
            let quantization_steps = (quantization_table as i32 + 1) * 96;
            let center = quantization_steps / 2;
            // The chain's own starting point, the `offset` Pseudocode 2
            // publishes: two coarse steps above that zero, four fine ones, and
            // a gain of 0,4 either way.
            let chain_offset = (quantization_table as i32 + 1) * 50;

            for data_point in 0..data_points {
                if channel_indices[data_point].len() != bands || vectors[data_point].len() != bands
                {
                    return Err(ParseError::InvalidHeader("joc_sparse_bands"));
                }
                let mut carried = chain_offset;
                let mut source_channel = 0usize;
                for band_index in 0..bands {
                    let step = channel_indices[data_point][band_index] as usize;
                    source_channel = if band_index == 0 {
                        step
                    } else {
                        (source_channel + step) % channel_count
                    };
                    if source_channel >= channel_count {
                        return Err(ParseError::InvalidHeader("joc_channel_idx"));
                    }

                    let step = vectors[data_point][band_index] as i32;
                    carried = (carried + step).rem_euclid(quantization_steps);
                    for (channel_index, channel) in mix_matrix[data_point][..channel_count]
                        .iter_mut()
                        .enumerate()
                    {
                        // Only the named channel carries the object; the rest of
                        // the column is silent, not offset.
                        let quantized = if channel_index == source_channel {
                            carried
                        } else {
                            center
                        };
                        channel[band_index] = (quantized - center) as f32 * gain_step;
                    }
                }
            }
        }
    }

    // The matrices above hold one value per parameter band; the reconstruction
    // needs one per QMF subband (ETSI TS 103 420 table 54). Expand them once
    // here, so the per-timeslot loops downstream stay straight copies and lerps
    // instead of re-gathering through the mapping on every slot. Walking the
    // subbands top-down makes the expansion safe in place: a band never starts
    // above its own index, so `mapping[subband] <= subband` and the source slot
    // is always still unexpanded when it is read.
    let bands_index = object
        .bands_index
        .ok_or(ParseError::InvalidHeader("joc_num_bands_idx"))? as usize;
    let mapping = expanded_parameter_band_mapping(bands_index)?;
    for matrix in mix_matrix.iter_mut().take(data_points) {
        for channel in matrix.iter_mut() {
            for subband in (0..QMF_SUBBANDS).rev() {
                channel[subband] = channel[mapping[subband] as usize];
            }
        }
    }
    Ok(())
}

fn expanded_parameter_band_mapping(
    bands_index: usize,
) -> Result<&'static [u8; QMF_SUBBANDS], ParseError> {
    static CACHE: OnceLock<[[u8; QMF_SUBBANDS]; 8]> = OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        let mut expanded = [[0u8; QMF_SUBBANDS]; 8];
        for (index, boundaries) in JOC_PARAMETER_BAND_BOUNDARIES.iter().enumerate() {
            for subband in 0..QMF_SUBBANDS {
                let parameter_band = match boundaries.binary_search(&(subband as u8)) {
                    Ok(found) => found,
                    Err(insert) => insert.saturating_sub(1),
                };
                expanded[index][subband] = parameter_band as u8;
            }
        }
        expanded
    });
    cache
        .get(bands_index)
        .ok_or(ParseError::InvalidHeader("joc_num_bands_idx"))
}

/// Both operands are indexed by subband: [`decode_parameter_points`] expands
/// every matrix it decodes before returning it, and `prev_matrix` only ever
/// holds copies of those.
fn copy_matrix(target: &mut SubbandMatrix, source: &[[f32; QMF_SUBBANDS]]) {
    for (dst, src) in target.iter_mut().zip(source.iter()) {
        *dst = *src;
    }
}

fn lerp_matrix(
    target: &mut SubbandMatrix,
    from: &[[f32; QMF_SUBBANDS]],
    to: &[[f32; QMF_SUBBANDS]],
    lerp: f32,
) {
    for ((dst, src_from), src_to) in target.iter_mut().zip(from.iter()).zip(to.iter()) {
        for subband in 0..QMF_SUBBANDS {
            dst[subband] = src_from[subband] + (src_to[subband] - src_from[subband]) * lerp;
        }
    }
}

#[cfg(test)]
// The expected coefficients below are written out to their last dyadic digit -
// 1.201171875 is exactly six steps of 820/4096, and 1.2011719 is only the
// shortest thing that round-trips. Writing what the step actually is keeps the
// expectations derivable by hand and independent of the constant the decoder
// uses, which is the whole point of asserting them.
#[allow(clippy::excessive_precision)]
mod tests {
    use super::{
        build_object_timeslots, decode_parameter_points, expanded_parameter_band_mapping,
        map_input_channel_indices,
    };
    use crate::{BedChannel, CorePcmFrame, JocObject, JocObjectData};

    #[test]
    fn parameter_band_mapping_expands_last_band() {
        let mapping = expanded_parameter_band_mapping(7).expect("mapping");
        assert_eq!(mapping[0], 0);
        assert_eq!(mapping[48], 22);
        assert_eq!(mapping[63], 22);
    }

    #[test]
    fn acmod7_core_layout_reorders_to_joc_input() {
        let frame = CorePcmFrame {
            sample_rate: 48_000,
            fullband_channel_order: vec![
                BedChannel::FrontLeft,
                BedChannel::Center,
                BedChannel::FrontRight,
                BedChannel::SurroundLeft,
                BedChannel::SurroundRight,
            ],
            fullband_channels: vec![vec![0.0; 64]; 5],
            lfe_channel: Some(vec![0.0; 64]),
        };

        let indices = map_input_channel_indices(&frame, 5).expect("indices");
        assert_eq!(indices, vec![0, 2, 1, 3, 4]);
    }

    #[test]
    fn five_channel_surround_core_can_feed_rear_joc_inputs() {
        let frame = CorePcmFrame {
            sample_rate: 48_000,
            fullband_channel_order: vec![
                BedChannel::FrontLeft,
                BedChannel::FrontRight,
                BedChannel::Center,
                BedChannel::SurroundLeft,
                BedChannel::SurroundRight,
            ],
            fullband_channels: vec![vec![0.0; 64]; 5],
            lfe_channel: Some(vec![0.0; 64]),
        };

        let indices = map_input_channel_indices(&frame, 7).expect("indices");
        assert_eq!(indices, vec![0, 1, 2, 3, 4, 3, 4]);
    }

    /// Clause 6.6.2 Pseudocode 2 as this decoder reads it, worked through by
    /// hand.
    ///
    /// Point 0 names channel 0 and then steps by 1 and 0, so its bands land on
    /// channels 0, 1 and 1. The coefficient chain starts at the transmitted
    /// offset of 50 and never restarts: 50+4, then 54+5 even though that band
    /// moved to another channel, then 59+6, each dequantized about the
    /// quantizer's zero of 48. Band 1 is the load-bearing one - restarting the
    /// chain for the newly named channel would make it 1,4 instead of 2,2.
    /// Point 1 names channels 1, 1 and 2 and starts its own chain.
    #[test]
    fn sparse_joc_carries_its_chain_across_a_channel_change() {
        let object = JocObject {
            active: true,
            bands_index: Some(1),
            bands: 3,
            sparse_coded: true,
            quantization_table: Some(0),
            steep_slope: false,
            data_points: 2,
            timeslot_offsets: Vec::new(),
            data: Some(JocObjectData::Sparse {
                channel_indices: vec![vec![0, 1, 0], vec![1, 0, 1]],
                vectors: vec![vec![4, 5, 6], vec![7, 8, 9]],
            }),
        };

        let mut mix_matrix = [vec![[1.0; 64]; 5], vec![[2.0; 64]; 5]];
        let mut timeslot_offsets = [0; 2];
        decode_parameter_points(&mut mix_matrix, &mut timeslot_offsets, &object, 5)
            .expect("points");

        // Table 54, three-band column: subband 0 is band 0, subbands 3 to 13
        // are band 1, and everything from 14 up is band 2 - so each band's
        // value, once decoded, is expected across the whole row it covers, not
        // just at the raw parameter-band slot it was computed into.
        const BAND_1: usize = 3;
        const BAND_2: usize = 14;

        // Dequantized at clause 6.6.4's exact 820/4096 per step, so 6, 11 and
        // 17 steps above the quantizer's zero on point 0, and 9, 17 and 26 on
        // point 1.
        let expected_point_0 = [
            [1.201171875, 0.0, 0.0],
            [0.0, 2.2021484375, 3.4033203125],
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0],
        ];
        let expected_point_1 = [
            [0.0, 0.0, 0.0],
            [1.8017578125, 3.4033203125, 0.0],
            [0.0, 0.0, 5.205078125],
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0],
        ];
        for (channel, expected) in mix_matrix[0].iter().zip(expected_point_0) {
            assert!(
                (channel[0] - expected[0]).abs() < 1e-6,
                "{channel:?} {expected:?}"
            );
            for subband in [BAND_1, 13] {
                assert!(
                    (channel[subband] - expected[1]).abs() < 1e-6,
                    "{channel:?} {expected:?}"
                );
            }
            for subband in [BAND_2, 63] {
                assert!(
                    (channel[subband] - expected[2]).abs() < 1e-6,
                    "{channel:?} {expected:?}"
                );
            }
        }
        for (channel, expected) in mix_matrix[1].iter().zip(expected_point_1) {
            assert!(
                (channel[0] - expected[0]).abs() < 1e-6,
                "{channel:?} {expected:?}"
            );
            for subband in [BAND_1, 13] {
                assert!(
                    (channel[subband] - expected[1]).abs() < 1e-6,
                    "{channel:?} {expected:?}"
                );
            }
            for subband in [BAND_2, 63] {
                assert!(
                    (channel[subband] - expected[2]).abs() < 1e-6,
                    "{channel:?} {expected:?}"
                );
            }
        }
    }

    /// The dense chain wraps in floating point, against a `max` that is itself
    /// a multiple of the step, so the step has to be exact or the wrap lands on
    /// the wrong side.
    ///
    /// 820/4096 is 205/1024, a dyadic rational, so `48 * step` and every
    /// `value * step` are exact in f32 and the modulo agrees with the integer
    /// arithmetic clause 6.6.2 actually specifies. 0,2 is not representable in
    /// binary, and the accumulated rounding pushes chains across the boundary:
    /// a band that should sit near the top of the quantizer comes back near the
    /// bottom, a gain error of the whole 19,2 range rather than the 0,098 % the
    /// wrong constant suggests.
    ///
    /// This drives [`decode_parameter_points`] itself rather than repeating the
    /// arithmetic beside it, so it holds the decoder to the integer-space
    /// result and not merely to whatever expression the dense branch currently
    /// happens to contain. Every start value on both quantizers is swept, and
    /// the two deltas after it carry the chain past the top of the range, so
    /// the first band exercises the `offset + value` branch and the two after
    /// it the running one. It is also the only place the fine quantizer's dense
    /// path is decoded end to end.
    #[test]
    fn dense_decode_wraps_exactly_like_the_integer_quantizer() {
        // Table 54's three-band column: band 0 covers subband 0, band 1 starts
        // at 3 and band 2 at 14, so each band's decoded value is read off the
        // subband its row begins at.
        const BAND_SUBBAND: [usize; 3] = [0, 3, 14];

        for quantization_table in 0..2u8 {
            let steps = (quantization_table as i32 + 1) * 96;
            let center = steps / 2;
            let step = super::dequantization_step(quantization_table as usize);

            for first in 0..steps {
                // Two deltas that walk the chain over the top of the quantizer
                // from wherever `first` left it.
                let deltas = [first as u16, (steps - 7) as u16, 11];
                let object = JocObject {
                    active: true,
                    bands_index: Some(1),
                    bands: 3,
                    sparse_coded: false,
                    quantization_table: Some(quantization_table),
                    steep_slope: false,
                    data_points: 1,
                    timeslot_offsets: Vec::new(),
                    data: Some(JocObjectData::Dense {
                        matrices: vec![vec![deltas.to_vec()]],
                    }),
                };

                let mut mix_matrix = [vec![[0.0; 64]; 1], vec![[0.0; 64]; 1]];
                let mut timeslot_offsets = [0; 2];
                decode_parameter_points(&mut mix_matrix, &mut timeslot_offsets, &object, 1)
                    .expect("points");

                let mut quantized = center;
                for (band, delta) in deltas.iter().enumerate() {
                    quantized = (quantized + *delta as i32) % steps;
                    let expected = (quantized - center) as f32 * step;
                    let decoded = mix_matrix[0][0][BAND_SUBBAND[band]];
                    assert_eq!(
                        decoded, expected,
                        "quant {quantization_table}, first {first}, band {band}: \
                         {decoded} != {expected}"
                    );
                }
            }
        }
    }

    /// A step that walks past the last channel wraps, and a coefficient step
    /// that walks past the quantizer's top wraps too - clause 6.6.2 takes both
    /// modulo, so a chain near the top of the range reappears at the bottom
    /// rather than saturating.
    #[test]
    fn sparse_joc_wraps_channel_and_coefficient_steps() {
        let object = JocObject {
            active: true,
            bands_index: Some(1),
            bands: 3,
            sparse_coded: true,
            quantization_table: Some(0),
            steep_slope: false,
            data_points: 1,
            timeslot_offsets: Vec::new(),
            // Channel 4 stepped by 3 wraps round to channel 2. The chain wraps
            // on every band too: 50+90 to 44, 44+90 to 38, 38+90 to 32.
            data: Some(JocObjectData::Sparse {
                channel_indices: vec![vec![4, 3, 0]],
                vectors: vec![vec![90, 90, 90]],
            }),
        };

        // Seeded with a value no band should leave behind, so "silent" below
        // means the decode wrote a zero rather than that nothing was written.
        let mut mix_matrix = [vec![[7.0; 64]; 5], vec![[7.0; 64]; 5]];
        let mut timeslot_offsets = [0; 2];
        decode_parameter_points(&mut mix_matrix, &mut timeslot_offsets, &object, 5)
            .expect("points");

        // Table 54, three-band column: subband 0 is band 0, subbands 3 to 13
        // are band 1, and everything from 14 up is band 2 - each band's value
        // is expected across the whole row it covers, not just the raw
        // parameter-band slot it was computed into.
        const BAND_1: usize = 3;
        const BAND_2: usize = 14;

        // Band 0 on channel 4: (50 + 90) % 96 = 44, so (44 - 48) * 820/4096.
        assert!((mix_matrix[0][4][0] - -0.80078125).abs() < 1e-6);
        assert!((mix_matrix[0][4][2] - -0.80078125).abs() < 1e-6);
        // Band 1 moves to channel 2 and the chain carries: (44 + 90) % 96 = 38.
        assert!((mix_matrix[0][2][BAND_1] - -2.001953125).abs() < 1e-6);
        assert!((mix_matrix[0][2][13] - -2.001953125).abs() < 1e-6);
        // Band 2 stays on channel 2: (38 + 90) % 96 = 32.
        assert!((mix_matrix[0][2][BAND_2] - -3.203125).abs() < 1e-6);
        assert!((mix_matrix[0][2][63] - -3.203125).abs() < 1e-6);
        // Everything the bands did not name is silent, not offset - channel 0
        // across the whole row, and channel 4 outside band 0's row.
        assert_eq!(mix_matrix[0][0][0], 0.0);
        assert_eq!(mix_matrix[0][0][63], 0.0);
        assert_eq!(mix_matrix[0][4][BAND_1], 0.0);
        assert_eq!(mix_matrix[0][4][BAND_2], 0.0);
    }

    /// The fine quantizer doubles every constant in the sparse path: 192 steps
    /// instead of 96, a zero at 96 instead of 48, a chain starting at 100
    /// instead of 50, and half the gain per step - 820/8192 rather than
    /// 820/4096. Only the coarse quantizer
    /// appears in the streams to hand, so this pins the scaling that the coarse
    /// tests cannot reach.
    #[test]
    fn fine_quantizer_scales_the_sparse_constants() {
        let object = JocObject {
            active: true,
            bands_index: Some(0),
            bands: 1,
            sparse_coded: true,
            quantization_table: Some(1),
            steep_slope: false,
            data_points: 1,
            timeslot_offsets: Vec::new(),
            data: Some(JocObjectData::Sparse {
                channel_indices: vec![vec![2]],
                vectors: vec![vec![5]],
            }),
        };

        let mut mix_matrix = [vec![[7.0; 64]; 5], vec![[7.0; 64]; 5]];
        let mut timeslot_offsets = [0; 2];
        decode_parameter_points(&mut mix_matrix, &mut timeslot_offsets, &object, 5)
            .expect("points");

        // (100 + 5) % 192 = 105, dequantized about 96 at 820/8192 per step.
        assert!((mix_matrix[0][2][0] - 0.90087890625).abs() < 1e-6);
        // And the channels band 0 did not name are silent here too.
        assert_eq!(mix_matrix[0][0][0], 0.0);
        assert_eq!(mix_matrix[0][4][0], 0.0);
    }

    /// The smooth branches are the ones nearly every block takes, and both
    /// halves of a two-point frame have to expand parameter bands as they
    /// interpolate: the first half from the previous frame's subband matrix
    /// towards point 0, the second between the two points. Interpolating
    /// band-indexed values as though they were subband-indexed is what emptied
    /// everything above the last band and put the wrong bands underneath it.
    #[test]
    fn smooth_two_point_frames_expand_parameter_bands_in_both_halves() {
        let object = JocObject {
            active: true,
            bands_index: Some(1), // three parameter bands
            bands: 3,
            sparse_coded: false,
            quantization_table: Some(0),
            steep_slope: false,
            data_points: 2,
            timeslot_offsets: Vec::new(),
            data: Some(JocObjectData::Dense {
                // Differential within each point, and point 1 is differential
                // against point 0: point 0 decodes to 0, 1 and 2 steps above
                // the quantizer's zero, point 1 to 2, 3 and 4 - a step being
                // clause 6.6.4's 820/4096, or 0,2001953125.
                matrices: vec![vec![vec![0, 1, 1]], vec![vec![2, 1, 1]]],
            }),
        };

        let mut mix_matrix = [vec![[0.0; 64]], vec![[0.0; 64]]];
        let mut timeslot_offsets = [0; 2];
        decode_parameter_points(&mut mix_matrix, &mut timeslot_offsets, &object, 1)
            .expect("points");
        let mut prev_matrix = vec![[0.0; 64]];
        let mut output = vec![vec![[0.0; 64]]; 4];
        build_object_timeslots(
            &mut prev_matrix,
            &mix_matrix,
            timeslot_offsets,
            &object,
            4,
            &mut output,
        )
        .expect("timeslots");

        // Table 54, three-band column: subband 0 is band 0, subbands 3 to 13 are
        // band 1, and everything from 14 up is band 2.
        const BAND_1: usize = 3;
        const BAND_2: usize = 14;

        // First half, four slots so two: from an all-zero previous matrix
        // towards point 0, reaching it on the last slot of the half.
        let half = &output[1][0];
        assert!((half[0] - 0.0).abs() < 1e-6);
        assert!((half[BAND_1] - 0.2001953125).abs() < 1e-6);
        assert!((half[13] - 0.2001953125).abs() < 1e-6);
        assert!((half[BAND_2] - 0.400390625).abs() < 1e-6);
        assert!((half[63] - 0.400390625).abs() < 1e-6);

        // Midway through the second half: half of the way from point 0 to
        // point 1, band by band.
        let between = &output[2][0];
        assert!((between[0] - 0.2001953125).abs() < 1e-6);
        assert!((between[BAND_1] - 0.400390625).abs() < 1e-6);
        assert!((between[BAND_2] - 0.6005859375).abs() < 1e-6);

        // And the frame ends on point 1 itself, expanded the same way.
        let end = &output[3][0];
        assert!((end[0] - 0.400390625).abs() < 1e-6);
        assert!((end[BAND_1] - 0.6005859375).abs() < 1e-6);
        assert!((end[13] - 0.6005859375).abs() < 1e-6);
        assert!((end[BAND_2] - 0.80078125).abs() < 1e-6);
        assert!((end[63] - 0.80078125).abs() < 1e-6);

        // The matrix carried into the next frame is the last point, expanded -
        // not the raw parameter array, which would zero the top end again on
        // the very next frame's first half.
        assert!((prev_matrix[0][BAND_2] - 0.80078125).abs() < 1e-6);
        assert!((prev_matrix[0][63] - 0.80078125).abs() < 1e-6);
    }

    /// The parameters arrive one per parameter band; the reconstruction needs
    /// one per QMF subband. A steep frame switches matrices rather than
    /// interpolating, and it still has to expand them on the way.
    #[test]
    fn steep_frames_expand_parameter_bands_across_the_subbands() {
        let object = JocObject {
            active: true,
            bands_index: Some(1), // three parameter bands
            bands: 3,
            sparse_coded: false,
            quantization_table: Some(0),
            steep_slope: true,
            data_points: 1,
            timeslot_offsets: vec![1],
            data: Some(JocObjectData::Dense {
                // Differential: band 0 stays at the centre, bands 1 and 2 each
                // step once, so the three bands decode to 0, 1 and 2 steps of
                // clause 6.6.4's 820/4096.
                matrices: vec![vec![vec![0, 1, 1]]],
            }),
        };

        let mut mix_matrix = [vec![[0.0; 64]], vec![[0.0; 64]]];
        let mut timeslot_offsets = [0; 2];
        decode_parameter_points(&mut mix_matrix, &mut timeslot_offsets, &object, 1)
            .expect("points");
        let mut prev_matrix = vec![[0.0; 64]];
        let mut output = vec![vec![[0.0; 64]]; 4];
        build_object_timeslots(
            &mut prev_matrix,
            &mix_matrix,
            timeslot_offsets,
            &object,
            4,
            &mut output,
        )
        .expect("timeslots");

        // Table 54, three-band column: subband 0 is band 0, subbands 3 to 13
        // are band 1, and everything from 14 up is band 2. Before this was
        // expanded, every subband past the third read whatever was left in the
        // parameter array - zero - and the object lost its whole top end.
        let switched = &output[3][0];
        assert!((switched[0] - 0.0).abs() < 1e-6);
        assert!((switched[3] - 0.2001953125).abs() < 1e-6);
        assert!((switched[13] - 0.2001953125).abs() < 1e-6);
        assert!((switched[14] - 0.400390625).abs() < 1e-6);
        assert!((switched[63] - 0.400390625).abs() < 1e-6);
    }

    #[test]
    fn steep_multi_point_switches_at_both_transmitted_offsets() {
        let object = JocObject {
            active: true,
            bands_index: Some(0),
            bands: 1,
            sparse_coded: false,
            quantization_table: Some(0),
            steep_slope: true,
            data_points: 2,
            // joc_offset_ts values, already carrying the +1 the parser applies.
            timeslot_offsets: vec![1, 3],
            data: Some(JocObjectData::Dense {
                // Point 0 decodes to 0.0, point 1 to one quantization step.
                matrices: vec![vec![vec![0]], vec![vec![1]]],
            }),
        };

        let mut prev_matrix = vec![[1.0; 64]];
        let mut mix_matrix = [vec![[0.0; 64]], vec![[0.0; 64]]];
        let mut timeslot_offsets = [0; 2];
        decode_parameter_points(&mut mix_matrix, &mut timeslot_offsets, &object, 1)
            .expect("points");
        let mut output = vec![vec![[0.0; 64]]; 4];
        build_object_timeslots(
            &mut prev_matrix,
            &mix_matrix,
            timeslot_offsets,
            &object,
            4,
            &mut output,
        )
        .expect("timeslots");

        // Clause 6.6.5 Pseudocode 6: previous matrix below the first offset,
        // point 0 between the offsets, point 1 from the second offset on.
        assert!(output[0][0].iter().all(|value| *value == 1.0));
        assert!(output[1][0].iter().all(|value| *value == 0.0));
        assert!(output[2][0].iter().all(|value| *value == 0.0));
        assert!(
            output[3][0]
                .iter()
                .all(|value| (*value - 0.2001953125).abs() < 1e-6)
        );
        assert!(
            prev_matrix[0]
                .iter()
                .all(|value| (*value - 0.2001953125).abs() < 1e-6)
        );
    }

    #[test]
    fn steep_single_point_never_reaches_for_a_second_slot() {
        let previous = JocObject {
            active: true,
            bands_index: Some(0),
            bands: 1,
            sparse_coded: false,
            quantization_table: Some(0),
            steep_slope: true,
            data_points: 2,
            timeslot_offsets: vec![2, 3],
            data: Some(JocObjectData::Dense {
                matrices: vec![vec![vec![0]], vec![vec![1]]],
            }),
        };
        let current = JocObject {
            active: true,
            bands_index: Some(0),
            bands: 1,
            sparse_coded: false,
            quantization_table: Some(0),
            steep_slope: true,
            data_points: 1,
            timeslot_offsets: vec![1],
            data: Some(JocObjectData::Dense {
                matrices: vec![vec![vec![0]]],
            }),
        };

        let mut mix_matrix = [vec![[0.0; 64]], vec![[0.0; 64]]];
        let mut timeslot_offsets = [0; 2];
        decode_parameter_points(&mut mix_matrix, &mut timeslot_offsets, &previous, 1)
            .expect("previous");
        decode_parameter_points(&mut mix_matrix, &mut timeslot_offsets, &current, 1)
            .expect("current");

        let mut prev_matrix = vec![[1.0; 64]];
        let mut output = vec![vec![[0.0; 64]]; 4];
        build_object_timeslots(
            &mut prev_matrix,
            &mix_matrix,
            timeslot_offsets,
            &current,
            4,
            &mut output,
        )
        .expect("timeslots");

        // The previous frame left a second data point behind in the shared
        // slot. Clause 6.6.5 gives a single-point steep frame two regions, so
        // that leftover must not appear anywhere in this frame's output.
        assert!(output[0][0].iter().all(|value| *value == 1.0));
        assert!(output[1][0].iter().all(|value| *value == 0.0));
        assert!(output[2][0].iter().all(|value| *value == 0.0));
        assert!(output[3][0].iter().all(|value| *value == 0.0));
        assert!(prev_matrix[0].iter().all(|value| *value == 0.0));
    }
}
