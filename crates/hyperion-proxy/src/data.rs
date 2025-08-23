use std::sync::{
    Arc,
    atomic::{self, AtomicBool, AtomicUsize},
};

use anyhow::bail;
use bytes::Bytes;
use slotmap::{KeyData, new_key_type};

/// Maximum total size of pending clientbound packets in a player's communication channel.
/// If this limit is exceeded, the player will be disconnected to prevent
/// memory exhaustion from slow or unresponsive clients. Note that this limit only uses the sum of
/// the size of each packet; the total number of packets is not limited.
/// TODO implement with rolling average
const MAX_PLAYER_CLIENTBOUND_BYTES: usize = 1_024;

new_key_type! {
    pub struct PlayerId;
}

impl From<u64> for PlayerId {
    fn from(id: u64) -> Self {
        let raw = KeyData::from_ffi(id);
        Self::from(raw)
    }
}

#[derive(Debug)]
pub struct PlayerHandle {
    writer: kanal::AsyncSender<Bytes>,

    /// Whether the player is allowed to send broadcasts.
    ///
    /// This exists because the player is not automatically in the play state,
    /// and if they are not in the play state and they receive broadcasts,
    /// they will get packets that it deems are invalid because the broadcasts are using the play
    /// state and play IDs.
    can_receive_broadcasts: AtomicBool,

    /// Total amount of bytes that are waiting to be sent
    total_bytes_pending_send: Arc<AtomicUsize>,
}

impl PlayerHandle {
    #[must_use]
    pub const fn new(
        writer: kanal::AsyncSender<Bytes>,
        total_bytes_pending_send: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            writer,
            can_receive_broadcasts: AtomicBool::new(false),
            total_bytes_pending_send,
        }
    }

    pub fn shutdown(&self) {
        // Ignore error for if the channel is already closed
        let _ = self.writer.close();
    }

    pub fn enable_receive_broadcasts(&self) {
        self.can_receive_broadcasts
            .store(true, atomic::Ordering::Relaxed);
    }

    pub fn can_receive_broadcasts(&self) -> bool {
        self.can_receive_broadcasts.load(atomic::Ordering::Relaxed)
    }

    pub fn send(&self, bytes: Bytes) -> anyhow::Result<()> {
        self.total_bytes_pending_send
            .fetch_add(bytes.len(), atomic::Ordering::Relaxed);
        match self.writer.try_send(bytes) {
            Ok(true) => Ok(()),

            Ok(false) => {
                let is_full = self.writer.is_full();
                self.shutdown();
                bail!("failed to send packet to player, channel is full: {is_full}");
            }
            Err(e) => {
                self.shutdown();
                bail!("failed to send packet to player: {e}");
            }
        }
    }

    pub fn total_bytes_pending_send(&self) -> usize {
        self.total_bytes_pending_send
            .load(atomic::Ordering::Relaxed)
    }
}
