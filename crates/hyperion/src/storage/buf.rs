//! Buffer implementations. todo: We might be able to scrap this for [`bytes::Buf`] in the future.
use std::mem::MaybeUninit;

/// # Safety
/// - `get_contiguous` must return a slice of exactly `len` bytes long.
/// - `advance` must advance the buffer by exactly `len` bytes.
pub unsafe trait Buf {
    /// What type we get when we advance. For example, if we have a [`bytes::BytesMut`], we get a [`bytes::BytesMut`].
    type Output;

    /// Get a contiguous slice of memory of length `len`.
    ///
    /// The returned slice must be exactly `len` bytes long.
    fn get_contiguous(&mut self, len: usize) -> &mut [u8];

    /// Advance the buffer by exactly `len` bytes.
    /// # Safety:
    /// `len` must be less than or equal to the length of the slice from the last [`Buf::get_contiguous`]
    /// call
    unsafe fn advance(&mut self, len: usize) -> Self::Output;
}

unsafe impl Buf for bytes::BytesMut {
    type Output = Self;

    fn get_contiguous(&mut self, len: usize) -> &mut [u8] {
        let original_len = self.len();
        self.resize(std::cmp::max(self.len() + len, self.capacity()), 0);
        unsafe { self.set_len(original_len); }
        let cap = &mut self.spare_capacity_mut()[0..len];
        let cap = unsafe { MaybeUninit::slice_assume_init_mut(cap) };
        debug_assert!(cap.len() == len);
        cap
    }
    unsafe fn advance(&mut self, len: usize) -> Self::Output {
        debug_assert!(self.len() + len <= self.capacity());
        unsafe { self.set_len(self.len() + len) };
        self.split_to(len)
    }
}
