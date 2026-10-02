// SPDX-License-Identifier: Apache-2.0
//
// Sample buffers kept from one frame to the next, so that a stream whose
// layout does not change decodes without allocating per frame.

/// Channel buffers no longer in use. A decoder hands every buffer it drops
/// back here and draws the next frame's from here, so once the first frames
/// have sized them the pool turns over the same allocations.
#[derive(Default)]
pub(crate) struct BufferPool(Vec<Vec<i32>>);

impl BufferPool {
    /// A buffer of `len` zeros, on a returned allocation when there is one.
    pub(crate) fn zeroed(&mut self, len: usize) -> Vec<i32> {
        let mut buffer = self.0.pop().unwrap_or_default();
        buffer.clear();
        buffer.resize(len, 0);
        buffer
    }

    /// Keep `buffer`'s allocation for a later [`Self::zeroed`].
    pub(crate) fn give(&mut self, buffer: Vec<i32>) {
        if buffer.capacity() != 0 {
            self.0.push(buffer);
        }
    }

    /// [`Self::give`] every buffer of `buffers`, leaving it empty (its own
    /// capacity kept).
    pub(crate) fn give_all(&mut self, buffers: &mut Vec<Vec<i32>>) {
        for buffer in buffers.drain(..) {
            self.give(buffer);
        }
    }

    /// [`Self::give`] every buffer of a speaker-indexed set, leaving each
    /// slot `None`.
    pub(crate) fn give_slots(&mut self, slots: &mut [Option<Vec<i32>>]) {
        for slot in slots {
            if let Some(buffer) = slot.take() {
                self.give(buffer);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::BufferPool;

    #[test]
    fn returned_buffers_come_back_zeroed() {
        let mut pool = BufferPool::default();
        let mut buffer = pool.zeroed(4);
        buffer.copy_from_slice(&[1, 2, 3, 4]);
        let pointer = buffer.as_ptr();
        pool.give(buffer);
        let again = pool.zeroed(3);
        assert_eq!(again, [0, 0, 0]);
        assert_eq!(again.as_ptr(), pointer, "the allocation is reused");
        pool.give(Vec::new());
        assert!(pool.0.is_empty(), "an empty vector is not worth keeping");
    }
}
