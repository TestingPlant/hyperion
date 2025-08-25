use std::sync::{
    Arc,
    atomic::{self, AtomicBool, AtomicUsize},
};

use bytes::Bytes;
use tokio::sync::Notify;
use slotmap::{KeyData, new_key_type};

/// Maximum total size of pending clientbound packets in a player's communication channel.
/// If this limit is exceeded, the player will be disconnected to prevent
/// memory exhaustion from slow or unresponsive clients. Note that this limit only uses the sum of
/// the size of each packet; the total number of packets is not limited.
/// TODO implement with rolling average
const MAX_PLAYER_CLIENTBOUND_BYTES: usize = 1_024;

/// Maximum number of bytes that, when using `send_shared`, will be copied to the channel instead
/// of being sent as shared bytes
const MAX_SHARED_COPY: usize = 2048;

new_key_type! {
    pub struct PlayerId;
}

impl From<u64> for PlayerId {
    fn from(id: u64) -> Self {
        let raw = KeyData::from_ffi(id);
        Self::from(raw)
    }
}

pub struct PlayerHandle {
    writer: Option<packet_channel::raw::Sender>,

    notify: Arc<Notify>,

    // TODO no atomic
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
        writer: packet_channel::raw::Sender,
        notify: Arc<Notify>,
        total_bytes_pending_send: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            writer: Some(writer),
            notify,
            can_receive_broadcasts: AtomicBool::new(false),
            total_bytes_pending_send,
        }
    }

    pub fn shutdown(&mut self) {
        // Dropping the writer will close the channel
        self.writer = None;
    }

    pub fn enable_receive_broadcasts(&self) {
        self.can_receive_broadcasts
            .store(true, atomic::Ordering::Relaxed);
    }

    pub fn can_receive_broadcasts(&self) -> bool {
        self.can_receive_broadcasts.load(atomic::Ordering::Relaxed)
    }

    pub fn send(&mut self, data: &[u8]) {
        let Some(writer) = &mut self.writer else { return };
        self.total_bytes_pending_send
            .fetch_add(data.len(), atomic::Ordering::Relaxed);
        writer.send(data);
        self.notify.notify_waiters();
    }

    pub fn send_shared(&mut self, bytes: Bytes) {
        if bytes.len() <= MAX_SHARED_COPY {
            self.send(&bytes);
        } else {
            tracing::info!("sharing {:?}", bytes.len());
            let Some(writer) = &mut self.writer else { return };
            self.total_bytes_pending_send
                .fetch_add(bytes.len(), atomic::Ordering::Relaxed);
            writer.send_shared(bytes);
            self.notify.notify_waiters();
        }
    }

    pub fn total_bytes_pending_send(&self) -> usize {
        self.total_bytes_pending_send
            .load(atomic::Ordering::Relaxed)
    }
}
