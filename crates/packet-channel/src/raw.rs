use std::{
    cell::SyncUnsafeCell,
    mem::MaybeUninit,
    ops::{Deref, Range},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use arc_swap::ArcSwapOption;
use bytes::Bytes;
use more_asserts::debug_assert_le;

enum FragmentData {
    OwnedPartial {
        /// Bytes `0..read_cursor` in the `data` field have been initialized, can be read, will never
        /// be modified again, and contain entire packets.
        read_cursor: AtomicUsize,

        // TODO: Consider using unsized types to avoid this extra Box allocation
        /// Stores packets. There is no padding or delimiter between packets. The same packet may be split
        /// between different fragments.
        data: Box<[SyncUnsafeCell<MaybeUninit<u8>>]>,
    },
    Shared(Bytes),

    /// Notification that this is the last fragment and the channel is closed
    Closed,
}

/// Reference counted fragment. Fragments are a fixed-size block of data which may contain zero or
/// more packets, including partial packets.
struct Fragment {
    /// Points to the next fragment, if available. Once this is set to `Some`, this fragment's `read_cursor` will never be
    /// updated and `next` will never be modified.
    next: ArcSwapOption<Fragment>,

    /// Fragment id. The first fragment will have an id of 0, and the next fragment will have an id
    /// that is one greater than the previous fragment's id.
    id: usize,

    data: FragmentData,
}

impl Fragment {
    fn new_owned_partial(size: usize, id: usize) -> Self {
        let data: Box<[MaybeUninit<u8>]> = Box::new_uninit_slice(size);
        // SAFETY: MaybeUninit<u8> and SyncUnsafeCell<MaybeUninit<u8>> have the same layout
        let data: Box<[SyncUnsafeCell<MaybeUninit<u8>>]> =
            unsafe { Box::from_raw(Box::into_raw(data) as *mut _) };
        Self {
            next: ArcSwapOption::from(None),
            id,
            data: FragmentData::OwnedPartial {
                read_cursor: AtomicUsize::new(0),
                data,
            },
        }
    }

    fn new_shared(data: Bytes, id: usize) -> Self {
        Self {
            next: ArcSwapOption::from(None),
            id,
            data: FragmentData::Shared(data),
        }
    }

    fn new_closed(id: usize) -> Self {
        Self {
            next: ArcSwapOption::from(None),
            id,
            data: FragmentData::Closed,
        }
    }

    /// # Safety
    /// `range` must be valid for reading
    unsafe fn read_unchecked(&self, range: Range<usize>) -> &[u8] {
        match &self.data {
            FragmentData::OwnedPartial { read_cursor, data } => {
                // Avoid unused warning if debug assertions is off
                let _ = read_cursor;
                #[cfg(debug_assertions)]
                if !range.is_empty() {
                    let read_cursor = read_cursor.load(Ordering::Relaxed);
                    debug_assert_le!(
                        range.end,
                        read_cursor,
                        "attempted to read bytes {range:?} which is outside of readable fragment \
                         range (0..{read_cursor})"
                    );
                }

                // SAFETY: Caller ensures that this should be in bounds
                let data: &[SyncUnsafeCell<MaybeUninit<u8>>] = unsafe { data.get_unchecked(range) };

                // SAFETY: SyncUnsafeCell<MaybeUninit<u8>> has the same layout as u8. The caller ensures
                // that these bytes are ready for reading, meaning that they have been properly
                // initialized. Once fragment bytes are initialized, they are never written to again, so
                // creating a shared reference to them is okay.
                let data: &[u8] =
                    unsafe { std::slice::from_raw_parts(data.as_ptr().cast(), data.len()) };

                data
            }
            FragmentData::Shared(bytes) => {
                // SAFETY: Caller ensures that this should be in bounds
                unsafe { bytes.get_unchecked(range) }
            }
            FragmentData::Closed => {
                if range.is_empty() {
                    &[]
                } else {
                    // SAFETY: Caller ensures that the range is valid to read, so the range must be
                    // empty for a close notification fragment, so this must be unreachable
                    unsafe { std::hint::unreachable_unchecked() }
                }
            }
        }
    }
}

pub struct Sender {
    current: Arc<Fragment>,
    default_fragment_size: usize,
}

impl Sender {
    /// Unbounded spmc channel.
    #[must_use]
    pub fn new(default_fragment_size: usize) -> Self {
        let fragment = Arc::new(Fragment::new_owned_partial(default_fragment_size, 0));
        Sender {
            current: fragment,
            default_fragment_size,
        }
    }

    /// Sends data.
    pub fn send(&mut self, mut data_to_send: &[u8]) {
        while !data_to_send.is_empty() {
            match &self.current.data {
                FragmentData::OwnedPartial { read_cursor, data } => {
                    // Get the current read cursor. Relaxed ordering is okay bcause only the sender
                    // is updating the read cursor, and there is only one sender.
                    let current_read_cursor = read_cursor.load(Ordering::Relaxed);
                    let remaining_capacity = data.len() - current_read_cursor;
                    let amount_to_copy = std::cmp::min(data_to_send.len(), remaining_capacity);

                    // Copying the bytes one at a time is required because using SyncUnsafeCell::{raw,}get is the
                    // only legal way to get the underlying *mut MaybeUninit<u8>. However, the compiler seems
                    // to be able to successfully optimize this into a memcpy in release mode.
                    for i in 0..amount_to_copy {
                        // SAFETY: i must be in bounds because amount_to_copy is limited by the source buffer's length
                        let source_byte = unsafe { data_to_send.get_unchecked(i) };

                        // SAFETY: i + current_read_cursor must be in bounds because amount_to_copy
                        // is limited by the destination buffer's capacity
                        let dest_byte = unsafe { data.get_unchecked(i + current_read_cursor) };

                        // SAFETY: The reader cannot be reading these bytes because the read cursor points to
                        // an index before these bytes.
                        unsafe { (*dest_byte.get()).write(*source_byte) };
                    }

                    // Update the read cursor
                    read_cursor.store(current_read_cursor + amount_to_copy, Ordering::Release);

                    // Remove the bytes that have been copied from data_to_send
                    // TODO use unchecked
                    data_to_send = &data_to_send[amount_to_copy..];

                    // If there's nothing else to send, break now so the next fragment isn't
                    // allocated
                    if data_to_send.is_empty() {
                        return;
                    }
                }
                FragmentData::Shared { .. } => {
                    // Do nothing and let the code below allocate a new fragment
                }
                FragmentData::Closed => {
                    unreachable!()
                }
            }

            let next_fragment = Arc::new(Fragment::new_owned_partial(
                self.default_fragment_size,
                self.current.id + 1,
            ));

            self.current.next.store(Some(next_fragment.clone()));
            self.current = next_fragment;
        }
    }

    /// Sends shared data.
    pub fn send_shared(&mut self, data_to_send: Bytes) {
        let next_fragment = Arc::new(Fragment::new_shared(data_to_send, self.current.id + 1));

        self.current.next.store(Some(next_fragment.clone()));
        self.current = next_fragment;
    }

    /// Sends shared data if its size is above the threshold and copies the data if its size is less than or equal to the threshold.
    pub fn send_maybe_shared(&mut self, data_to_send: Bytes, threshold: usize) {
        if data_to_send.len() > threshold {
            self.send_shared(data_to_send);
        } else {
            self.send(&data_to_send);
        }
    }

    /// Sends shared data if its size is above the threshold and copies the data if its size is less than or equal to the threshold. This avoids modifying the reference count of `data_to_send` if it is below the threshold.
    pub fn send_maybe_shared_ref(&mut self, data_to_send: &Bytes, threshold: usize) {
        if data_to_send.len() > threshold {
            self.send_shared(data_to_send.clone());
        } else {
            self.send(data_to_send);
        }
    }

    /// Create a receiver. The receiver will only receive data sent after the receiver was created.
    pub fn receiver(&self) -> Receiver {
        let read_cursor = match &self.current.data {
            FragmentData::OwnedPartial { read_cursor, .. } => read_cursor.load(Ordering::Relaxed),
            FragmentData::Shared(_) => 0,
            FragmentData::Closed => {
                unreachable!()
            }
        };

        Receiver {
            current: self.current.clone(),
            read_cursor,
            pending_next: false,
        }
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        let next_fragment = Arc::new(Fragment::new_closed(self.current.id + 1));
        self.current.next.store(Some(next_fragment));
    }
}

pub enum RecvError {
    Closed,
}

pub struct Receiver {
    current: Arc<Fragment>,
    read_cursor: usize,
    pending_next: bool,
}

impl Receiver {
    /// Receives data
    pub fn try_recv(&mut self) -> Result<Option<InitFragment>, RecvError> {
        loop {
            if self.pending_next {
                let Some(next) = self.current.next.load_full() else {
                    return Ok(None);
                };
                self.current = next;
                self.read_cursor = 0;
                self.pending_next = false;
            }

            match &self.current.data {
                FragmentData::OwnedPartial { read_cursor, .. } => {
                    let fragment_read_cursor = read_cursor.load(Ordering::Acquire);

                    if self.read_cursor == fragment_read_cursor {
                        if let Some(next_fragment) = self.current.next.load_full() {
                            // The next fragment is available. This needs to check if the read cursor for
                            // the current fragment has been updated to check for the following situation:
                            // - Reader notices that the reader's read cursor is the same as the fragment's
                            //   read cursor, indicating that there are no new packets available in the
                            //   current fragment
                            // - Writer (in another thread) writes a packet to the current fragment and
                            //   updates the read cursor
                            // - Writer allocates a new fragment to store another packet
                            // - Reader notices that the next fragment is available and reaches this code
                            // In this situation, checking the read cursor again will allow the reader to
                            // read the remaining packets in the current fragment by allowing the loop to
                            // continue to its next iteration
                            let new_fragment_read_cursor = read_cursor.load(Ordering::Acquire);
                            if fragment_read_cursor == new_fragment_read_cursor {
                                // There are no more packets on the current fragment.
                                // Start reading from the next fragment
                                self.current = next_fragment;
                                self.read_cursor = 0;
                            }
                        } else {
                            // There are no packets at the moment
                            return Ok(None);
                        }
                    } else {
                        // There are bytes to read

                        // SAFETY: The packet data must be valid to read because the it is below
                        // the read cursor
                        let packet_data = unsafe {
                            InitFragment::new_unchecked(
                                self.current.clone(),
                                self.read_cursor..fragment_read_cursor,
                            )
                        };
                        self.read_cursor = fragment_read_cursor;
                        return Ok(Some(packet_data));
                    }
                }
                FragmentData::Shared(bytes) => {
                    // SAFETY: Range provided is in bounds
                    let init_fragment = unsafe {
                        InitFragment::new_unchecked(
                            self.current.clone(),
                            self.read_cursor..bytes.len(),
                        )
                    };
                    self.pending_next = true;
                    return Ok(Some(init_fragment));
                }
                FragmentData::Closed => {
                    return Err(RecvError::Closed);
                }
            }
        }
    }
}

/// Fragment which is guaranteed to be initialized. This fragmnet may contain zero or more packets.
/// Packets are not delimited.
#[derive(Clone)]
pub struct InitFragment {
    fragment: Arc<Fragment>,
    range: Range<usize>,
}

impl InitFragment {
    /// # Safety
    /// For an owned partial fragment, `range` must be within `0..read_cursor`. For shared
    /// data, the range must be in bounds.
    const unsafe fn new_unchecked(fragment: Arc<Fragment>, range: Range<usize>) -> Self {
        Self { fragment, range }
    }

    #[expect(clippy::missing_const_for_fn, reason = "false positive")]
    #[must_use]
    pub fn fragment_id(&self) -> usize {
        self.fragment.id
    }

    /// Removes the first `n` bytes from this packet
    pub const fn remove_front(&mut self, n: usize) {
        if let Some(range_len) = self.range.end.checked_sub(self.range.start)
            && range_len > n
        {
            // The length of the range is greater than `n`, so adding `n` to the range start is
            // okay and will not cause an integer overflow
            self.range.start += n;
        } else {
            // `n` is greater than or equal to the length of the range, so the resulting range is
            // empty. This code cannot use `start += n` because that could cause an integer
            // overflow
            self.range.start = self.range.end;
        }
    }
}

impl Deref for InitFragment {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: Ensured by safety requirements of InitFragment::new_unchecked
        unsafe { self.fragment.read_unchecked(self.range.clone()) }
    }
}

impl AsRef<[u8]> for InitFragment {
    fn as_ref(&self) -> &[u8] {
        self
    }
}
