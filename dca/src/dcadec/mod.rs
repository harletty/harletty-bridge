// SPDX-License-Identifier: Apache-2.0
//
// DCA core decoder internals: generated tables, Huffman VLCs, and the subband
// DSP decode (ported incrementally from ffmpeg's dca_core.c / dcadsp.c).

pub(crate) mod core;
pub(crate) mod huffman;
pub(crate) mod synth;
pub(crate) mod tables;
