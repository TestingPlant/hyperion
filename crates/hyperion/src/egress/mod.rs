use bevy::prelude::*;
use tracing::error;
use valence_protocol::{VarInt, packets::play::PlayerActionResponseS2c};

use crate::{
    Blocks,
    net::{
        Compose, ConnectionId,
        intermediate::{
            IntermediateServerToProxyMessage, UpdatePlayerPosition, UpdatePlayerPositions,
        },
    },
    simulation::{ChannelCache, ChunkLookup, Position},
};
pub mod channel;
pub mod metadata;
pub mod player_join;
mod stats;
mod sync_entity_state;

use channel::ChannelPlugin;
use player_join::PlayerJoinPlugin;
use stats::StatsPlugin;
use sync_entity_state::EntityStateSyncPlugin;

fn send_chunk_positions(
    compose: Res<'_, Compose>,
    query: Query<'_, '_, (&ConnectionId, &Position)>,
) {
    let updates = query
        .iter()
        .map(|(&io, pos)| UpdatePlayerPosition {
            stream: io,
            position: hyperion_proto::ChunkPosition::from(pos.to_chunk()),
        })
        .collect::<Vec<_>>();

    let message = IntermediateServerToProxyMessage::UpdatePlayerPositions(UpdatePlayerPositions {
        updates: &updates,
    });

    compose.io_buf().add_proxy_message(&message);
}

// TODO move to sync chunks
fn broadcast_chunk_deltas(
    compose: Res<'_, Compose>,
    mut blocks: ResMut<'_, Blocks>,
    chunk_lookup: Res<'_, ChunkLookup>,
    mut channel_cache_query: Query<'_, '_, &mut ChannelCache>,
    connection_query: Query<'_, '_, &ConnectionId>,
) {
    blocks.for_each_to_update_mut(|chunk| {
        let Some(&chunk_channel) = chunk_lookup.get(&chunk.position.as_i16vec2()) else {
            error!(
                "failed to send chunk delta packet: chunk position {:?} is not in chunk lookup",
                chunk.position
            );
            return;
        };

        let mut channel_cache = match channel_cache_query.get_mut(chunk_channel) {
            Ok(data) => data,
            Err(e) => {
                error!("failed to invalidate chunk channel cache: cache query failed: {e}");
                return;
            }
        };

        // Invalidate the cached subscribe packets
        channel_cache.clear();

        for packet in chunk.delta_drain_packets() {
            if let Err(e) = compose
                .broadcast_channel(packet, chunk_channel.into())
                .send()
            {
                error!("failed to send chunk delta packet: {e}");
                return;
            }
        }
    });
    blocks.clear_should_update();

    for to_confirm in blocks.to_confirm.drain(..) {
        let connection_id = match connection_query.get(to_confirm.entity) {
            Ok(connection_id) => *connection_id,
            Err(e) => {
                error!("failed to send player action response: connection query failed: {e}");
                continue;
            }
        };

        let pkt = PlayerActionResponseS2c {
            sequence: VarInt(to_confirm.sequence),
        };

        if let Err(e) = compose.unicast(&pkt, connection_id) {
            error!("failed to send player action response: {e}");
        }
    }
}

#[derive(Component)]
pub struct EgressPlugin;

impl Plugin for EgressPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PostUpdate, (send_chunk_positions, broadcast_chunk_deltas));
        app.add_plugins((
            PlayerJoinPlugin,
            StatsPlugin,
            EntityStateSyncPlugin,
            ChannelPlugin,
        ));
    }
}
