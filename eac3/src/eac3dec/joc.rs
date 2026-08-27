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
            let gain_step = 0.2f32 - quantization_table as f32 * 0.1f32;
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
            // The public documentation for this sparse coding path is ambiguous and
            // reconstructing it naively produces obvious artifacts, so fall back to a zero
            // matrix until the coding path is specified well enough to decode safely.
            for data_point in 0..data_points {
                if channel_indices[data_point].len() != bands || vectors[data_point].len() != bands
                {
                    return Err(ParseError::InvalidHeader("joc_sparse_bands"));
                }
                for channel in &mut mix_matrix[data_point] {
                    for band in &mut channel[..bands] {
                        *band = 0.0;
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

    #[test]
    fn sparse_joc_falls_back_to_silence() {
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

        // The zeroed parameter bands are expanded across every subband on the
        // way out, so whatever the matrices held before is gone entirely.
        assert!(
            mix_matrix[0]
                .iter()
                .all(|channel| channel.iter().all(|value| *value == 0.0))
        );
        assert!(
            mix_matrix[1]
                .iter()
                .all(|channel| channel.iter().all(|value| *value == 0.0))
        );
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
                // against point 0: point 0 decodes to 0.0, 0.2, 0.4 and point 1
                // to 0.4, 0.6, 0.8.
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
        assert!((half[BAND_1] - 0.2).abs() < 1e-6);
        assert!((half[13] - 0.2).abs() < 1e-6);
        assert!((half[BAND_2] - 0.4).abs() < 1e-6);
        assert!((half[63] - 0.4).abs() < 1e-6);

        // Midway through the second half: half of the way from point 0 to
        // point 1, band by band.
        let between = &output[2][0];
        assert!((between[0] - 0.2).abs() < 1e-6);
        assert!((between[BAND_1] - 0.4).abs() < 1e-6);
        assert!((between[BAND_2] - 0.6).abs() < 1e-6);

        // And the frame ends on point 1 itself, expanded the same way.
        let end = &output[3][0];
        assert!((end[0] - 0.4).abs() < 1e-6);
        assert!((end[BAND_1] - 0.6).abs() < 1e-6);
        assert!((end[13] - 0.6).abs() < 1e-6);
        assert!((end[BAND_2] - 0.8).abs() < 1e-6);
        assert!((end[63] - 0.8).abs() < 1e-6);

        // The matrix carried into the next frame is the last point, expanded -
        // not the raw parameter array, which would zero the top end again on
        // the very next frame's first half.
        assert!((prev_matrix[0][BAND_2] - 0.8).abs() < 1e-6);
        assert!((prev_matrix[0][63] - 0.8).abs() < 1e-6);
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
                // step once, so the three bands decode to 0.0, 0.2 and 0.4.
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
        assert!((switched[3] - 0.2).abs() < 1e-6);
        assert!((switched[13] - 0.2).abs() < 1e-6);
        assert!((switched[14] - 0.4).abs() < 1e-6);
        assert!((switched[63] - 0.4).abs() < 1e-6);
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
        assert!(output[3][0].iter().all(|value| (*value - 0.2).abs() < 1e-6));
        assert!(
            prev_matrix[0]
                .iter()
                .all(|value| (*value - 0.2).abs() < 1e-6)
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
