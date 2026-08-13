//! Page-aligned heap buffer.
//!
//! `O_DIRECT` bypasses the page cache, and in exchange the kernel requires the
//! user buffer, the file offset and the length to all be aligned to the logical
//! block size. A plain `Vec<u8>` gives no such guarantee, so we allocate by
//! hand.

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::slice;

/// Alignment that satisfies every logical sector size in practice (512 through
/// 4096), and matches the page size on all supported targets.
pub const ALIGN: usize = 4096;

pub struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
}

impl AlignedBuf {
    /// `len` is rounded up to the next multiple of [`ALIGN`].
    pub fn new(len: usize) -> Self {
        let len = len.next_multiple_of(ALIGN).max(ALIGN);
        let layout = Layout::from_size_align(len, ALIGN).expect("valid layout");
        // SAFETY: layout has non-zero size, so alloc_zeroed is well-defined.
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Self { ptr, len }
    }

    /// Fill with a non-trivial pattern so compressing or deduplicating storage
    /// layers cannot fake a fast write pass.
    pub fn fill_incompressible(&mut self, seed: u64) {
        // `max(1)` rather than `| 1`: OR-ing would collapse each even seed onto
        // its odd neighbour, so two different seeds could yield identical data.
        let mut state = seed.max(1);
        let buf = self.as_mut_slice();
        for chunk in buf.chunks_mut(8) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let bytes = state.to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: ptr is valid for len bytes and initialised by alloc_zeroed.
        unsafe { slice::from_raw_parts(self.ptr, self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as above, and &mut self guarantees exclusive access.
        unsafe { slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.len, ALIGN).expect("valid layout");
        // SAFETY: ptr came from alloc_zeroed with exactly this layout.
        unsafe { dealloc(self.ptr, layout) }
    }
}

// SAFETY: the buffer owns its allocation exclusively and holds no thread-local
// state, so moving it to the benchmark worker thread is sound.
unsafe impl Send for AlignedBuf {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_is_aligned_and_rounded_up() {
        let b = AlignedBuf::new(100);
        assert_eq!(b.as_slice().len(), ALIGN);
        assert_eq!(b.as_slice().as_ptr() as usize % ALIGN, 0);

        let b = AlignedBuf::new(4 << 20);
        assert_eq!(b.as_slice().len(), 4 << 20);
        assert_eq!(b.as_slice().as_ptr() as usize % ALIGN, 0);
    }

    #[test]
    fn pattern_is_not_a_run_of_zeroes() {
        let mut b = AlignedBuf::new(ALIGN);
        b.fill_incompressible(42);
        assert!(b.as_slice().iter().any(|&x| x != 0));
        let mut c = AlignedBuf::new(ALIGN);
        c.fill_incompressible(43);
        assert_ne!(b.as_slice(), c.as_slice());
    }
}
