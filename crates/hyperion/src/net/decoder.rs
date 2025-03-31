use std::{
    cell::Cell,
    ops::{Index, RangeFull},
    sync::atomic::{Ordering, AtomicUsize, AtomicI32}
};

use anyhow::{Context, bail, ensure};
use bytes::Buf;
use flecs_ecs::macros::Component;
use valence_protocol::{
    CompressionThreshold, Decode, MAX_PACKET_SIZE, Packet, VarInt, var_int::VarIntDecodeError,
};

#[derive(Default)]
struct RefBytesMut {
    cursor: AtomicUsize,
    inner: Vec<u8>,
}

impl RefBytesMut {
    // TODO: any intended usage of this function seems sketchy
    pub fn advance(&self, amount: usize) {
    }

    pub fn split_to(&self, len: usize) -> &[u8] {
        &[]
    }
}

impl Index<RangeFull> for RefBytesMut {
    type Output = [u8];

    fn index(&self, _: RangeFull) -> &Self::Output {
        let on = self.cursor.load(Ordering::Relaxed);
        #[expect(
            clippy::indexing_slicing,
            reason = "this is probably fine? todo: verify"
        )]
        &self.inner[on..]
    }
}

/// A buffer for saving bytes that are not yet decoded.
#[derive(Component)]
pub struct PacketDecoder {
    buf: RefBytesMut,
    threshold: AtomicI32,
}

#[derive(Copy, Clone)]
pub struct BorrowedPacketFrame<'a> {
    /// The ID of the decoded packet.
    pub id: i32,
    /// The contents of the packet after the leading [`VarInt`] ID.
    pub body: &'a [u8],
}

impl std::fmt::Debug for BorrowedPacketFrame<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BorrowedPacketFrame")
            .field("id", &format!("0x{:x}", self.id))
            .field("body", &bytes::Bytes::copy_from_slice(self.body))
            .finish()
    }
}

impl<'a> BorrowedPacketFrame<'a> {
    /// Attempts to decode this packet as type `P`. An error is returned if the
    /// packet ID does not match, the body of the packet failed to decode, or
    /// some input was missed.
    pub fn decode<P>(&self) -> anyhow::Result<P>
    where
        P: Packet + Decode<'a>,
    {
        ensure!(
            P::ID == self.id,
            "packet ID mismatch while decoding '{}': expected {}, got {}",
            P::NAME,
            P::ID,
            self.id
        );

        let mut r = self.body;

        let pkt = P::decode(&mut r)?;

        ensure!(
            r.is_empty(),
            "missed {} bytes while decoding '{}'",
            r.len(),
            P::NAME
        );

        Ok(pkt)
    }
}

impl PacketDecoder {
    /// Tries to get the next packet from the buffer.
    /// If a new packet is found, the buffer will be truncated by the length of the packet.
    pub fn try_next_packet<'b>(
        &'b self,
        bump: &'b bumpalo::Bump,
    ) -> anyhow::Result<Option<BorrowedPacketFrame<'b>>> {
        bail!("");
    }

    pub fn shift_excess(&mut self) {
    }

    /// Get the compression threshold.
    #[must_use]
    pub fn compression(&self) -> CompressionThreshold {
        CompressionThreshold(self.threshold.load(Ordering::Relaxed))
    }

    /// Sets the compression threshold.
    pub fn set_compression(&self, threshold: CompressionThreshold) {
        self.threshold.store(threshold.0, Ordering::Relaxed);
    }

    /// Queues a slice of bytes into the buffer.
    pub fn queue_slice(&mut self, bytes: &[u8]) {
    }
}

impl Default for PacketDecoder {
    fn default() -> Self {
        Self {
            buf: RefBytesMut::default(),
            threshold: AtomicI32::new(CompressionThreshold::default().0)
        }
    }
}
