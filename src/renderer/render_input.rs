// SPDX-License-Identifier: Apache-2.0

use super::types::{BedChannel, ObjectAnchor, Vec3};

#[derive(Debug, Clone, PartialEq)]
/// One labeled bed channel carried by a [`RenderInputFrame`].
pub struct RenderInputChannel {
    pub channel: BedChannel,
    pub samples: Vec<f32>,
}

#[derive(Debug, Clone, PartialEq)]
/// One block update inside a renderer metadata element.
pub struct RenderMetadataBlockUpdate {
    pub offset: i64,
    pub ramp_duration: i64,
}

#[derive(Debug, Clone, PartialEq)]
/// One fully-resolved object state carried by renderer metadata.
pub struct RenderMetadataObject {
    pub gain: Option<f32>,
    pub anchor: ObjectAnchor,
    pub position_valid: bool,
    pub differential_position: bool,
    pub position: Option<Vec3>,
    pub distance: Option<f32>,
    pub size: Option<f32>,
    pub screen_factor: f32,
    pub depth_factor: f32,
}

#[derive(Debug, Clone, PartialEq)]
/// One metadata element consumed by [`crate::renderer::Renderer714`].
pub struct RenderMetadataElement {
    pub block_updates: Vec<RenderMetadataBlockUpdate>,
    pub object_blocks: Vec<Vec<RenderMetadataObject>>,
}

#[derive(Debug, Clone, PartialEq)]
/// Codec-neutral object metadata consumed by [`crate::renderer::Renderer714`].
pub struct RenderMetadata {
    pub object_count: usize,
    pub bed_or_isf_objects: usize,
    pub bed_channels: Vec<BedChannel>,
    pub elements: Vec<RenderMetadataElement>,
}

#[derive(Debug, Clone, PartialEq)]
/// One metadata payload update carried by a [`RenderInputFrame`].
pub struct RenderMetadataUpdate {
    pub sample_offset: u16,
    pub metadata: RenderMetadata,
}

#[derive(Debug, Clone, PartialEq)]
/// Codec-agnostic render contract shared between decoders and [`crate::renderer::Renderer714`].
///
/// Decoders fill this structure with labeled bed PCM, dynamic object PCM, and any metadata
/// updates that become active during the same frame. The renderer only depends on this IR and
/// does not need to know which codec produced it.
pub struct RenderInputFrame {
    pub sample_rate: u32,
    pub bed_channels: Vec<RenderInputChannel>,
    pub object_channels: Vec<Vec<f32>>,
    pub metadata_updates: Vec<RenderMetadataUpdate>,
}

impl RenderInputFrame {
    /// Number of samples carried by each input channel.
    pub fn samples_per_channel(&self) -> usize {
        self.bed_channels
            .first()
            .map(|channel| channel.samples.len())
            .or_else(|| self.object_channels.first().map(Vec::len))
            .unwrap_or(0)
    }

    /// Number of bed channels in this frame.
    pub fn bed_channel_count(&self) -> usize {
        self.bed_channels.len()
    }

    /// Number of dynamic object channels in this frame.
    pub fn object_count(&self) -> usize {
        self.object_channels.len()
    }

    /// Number of metadata payload updates carried by this frame.
    pub fn metadata_update_count(&self) -> usize {
        self.metadata_updates.len()
    }
}

impl RenderMetadata {
    pub(crate) fn dynamic_object_count(&self) -> usize {
        self.object_count.saturating_sub(self.bed_or_isf_objects)
    }
}
