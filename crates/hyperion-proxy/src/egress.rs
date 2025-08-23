use bytes::Bytes;
use hyperion_proto::{ArchivedSetReceiveBroadcasts, ArchivedShutdown};
use rustc_hash::FxHashMap;
use tracing::{error, instrument, warn};

use crate::{data::PlayerHandle, server_sender::ServerSender};

pub struct Egress {
    // todo: can we do some type of EntityId and SlotMap
    pub(crate) player_registry: FxHashMap<u64, PlayerHandle>,
    pub(crate) server_sender: ServerSender,
}

impl Egress {
    #[must_use]
    pub fn new(
        server_sender: ServerSender,
    ) -> Self {
        Self {
            player_registry: FxHashMap::default(),
            server_sender,
        }
    }

    #[instrument(skip_all)]
    pub fn unicast(&self, stream: u64, data: Bytes) {
        let Some(player) = self.player_registry.get(&stream) else {
            // expected to still happen infrequently
            warn!("Player not found for id {stream:?}");
            return;
        };

        // todo: handle error; kick player if cannot send (buffer full)
        if let Err(e) = player.send(data) {
            warn!("Failed to send data to player: {:?}", e);
            player.shutdown();
        }
    }

    /// Unicasts a packet to a stream. If the stream's total bytes pending send is greater than
    /// `max_total_bytes`, `data` will not be sent. Returns whether the packet was sent.
    #[instrument(skip_all)]
    pub fn maybe_unicast(&self, stream: u64, data: Bytes, max_total_bytes: usize) -> bool {
        let Some(player) = self.player_registry.get(&stream) else {
            // expected to still happen infrequently
            warn!("Player not found for id {stream:?}");
            return false;
        };

        if player.total_bytes_pending_send() > max_total_bytes {
            return false;
        }

        // todo: handle error; kick player if cannot send (buffer full)
        if let Err(e) = player.send(data) {
            warn!("Failed to send data to player: {:?}", e);
            player.shutdown();
        }

        true
    }

    #[instrument(skip_all)]
    pub fn handle_set_receive_broadcasts(&self, pkt: &ArchivedSetReceiveBroadcasts) {
        let Ok(stream) = rkyv::deserialize::<u64, !>(&pkt.stream);

        let Some(player) = self.player_registry.get(&stream) else {
            error!("Player not found for stream {stream:?}");
            return;
        };

        player.enable_receive_broadcasts();
    }

    #[instrument(skip_all)]
    pub fn handle_shutdown(&self, pkt: &ArchivedShutdown) {
        let Ok(stream) = rkyv::deserialize::<u64, !>(&pkt.stream);

        if let Some(result) = self.player_registry.get(&stream) {
            result.shutdown();
        } else {
            error!("Player not found for stream {stream:?}");
        }
    }
}
