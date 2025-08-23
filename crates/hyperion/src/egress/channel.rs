use bevy::{ecs::world::OnDespawn, prelude::*};
use hyperion_proto::UpdateChannelPosition;
use hyperion_utils::EntityExt;
use tracing::error;
use valence_bytes::{Bytes, CowBytes};
use valence_protocol::{ByteAngle, ChunkPos, RawBytes, VarInt, packets::play};

use crate::{
    egress::metadata::show_all,
    net::{
        Channel, ChannelId, Compose, ConnectionId,
        intermediate::{IntermediateServerToProxyMessage, UpdateChannelPositions},
    },
    simulation::{
        ChannelCache, ChunkPosition, Pitch, Position, RequestSubscribeChannelPackets, Uuid,
        Velocity, Yaw,
        blocks::{Blocks, GetChunk},
        entity_kind::EntityKind,
        metadata::{MetadataChanges, get_and_clear_metadata},
    },
};

fn add_channel(
    trigger: Trigger<'_, OnAdd, Channel>,
    entity_query: Query<'_, '_, (), With<EntityKind>>,
    chunk_query: Query<'_, '_, &ChunkPosition>,
    compose: Res<'_, Compose>,
    mut commands: Commands<'_, '_>,
) {
    let channel = trigger.target();
    let packet_buf = match (entity_query.get(channel), chunk_query.get(channel)) {
        (Ok(()), Err(_)) => {
            let packet = play::EntitiesDestroyS2c {
                entity_ids: vec![VarInt(channel.minecraft_id())].into(),
            };

            compose.io_buf().encode_packet(&packet, &compose).unwrap()
        }
        (Err(_), Ok(chunk_position)) => {
            let pos = ChunkPos::new(i32::from(chunk_position.x), i32::from(chunk_position.y));
            let packet = play::UnloadChunkS2c { pos };

            compose.io_buf().encode_packet(&packet, &compose).unwrap()
        }
        (Ok(()), Ok(_)) => {
            error!(
                "failed to add channel: channel cannot have both EntityKind and ChunkPosition \
                 component"
            );
            return;
        }
        (Err(_), Err(_)) => {
            error!(
                "failed to add channel: channel should have one of the following components: \
                 EntityKind, ChunkPosition"
            );
            return;
        }
    };

    compose.io_buf().add_channel(channel.into(), &packet_buf);

    commands.entity(channel).insert(ChannelCache::default());
}

fn remove_channel(trigger: Trigger<'_, OnDespawn, Channel>, compose: Res<'_, Compose>) {
    compose
        .io_buf()
        .remove_channel(ChannelId::new(trigger.target().id()));
}

fn update_channel_positions(
    compose: Res<'_, Compose>,
    query: Query<'_, '_, (Entity, Option<&Position>, Option<&ChunkPosition>), With<Channel>>,
) {
    let updates = query
        .iter()
        .filter_map(|(entity, position, chunk_position)| {
            Some(UpdateChannelPosition {
                channel_id: entity.id(),
                position: match (position, chunk_position) {
                    (Some(position), None) => Some(position.to_chunk().into()),
                    (None, Some(chunk_position)) => Some((**chunk_position).into()),
                    (Some(_), Some(_)) => {
                        error!(
                            "failed to update channel positions: channel cannot have both \
                             Position and ChunkPosition component"
                        );
                        None
                    }
                    (None, None) => {
                        error!(
                            "failed to update channel positions: channel should have one of the \
                             following components: Position, ChunkPosition"
                        );
                        None
                    }
                }?,
            })
        })
        .collect::<Vec<_>>();

    compose
        .io_buf()
        .add_proxy_message(&IntermediateServerToProxyMessage::UpdateChannelPositions(
            UpdateChannelPositions { updates: &updates },
        ));
}

fn send_subscribe_channel_packets(
    mut events: EventReader<'_, '_, RequestSubscribeChannelPackets>,
    blocks: Res<'_, Blocks>,
    compose: Res<'_, Compose>,
    entity_query: Query<'_, '_, (&Uuid, &Position, &Pitch, &Yaw, &Velocity, &EntityKind)>,
    chunk_query: Query<'_, '_, &ChunkPosition>,
    connection_query: Query<'_, '_, &ConnectionId>,
    mut set: ParamSet<'_, '_, (Query<'_, '_, &mut ChannelCache>, &World)>,
) {
    for event in events.read() {
        let mut cache_query = set.p0();
        let mut cache = match cache_query.get_mut(event.channel) {
            Ok(cache) => cache,
            Err(e) => {
                error!("failed to send subscribe channel packets: ChannelCache query failed: {e}");
                continue;
            }
        };

        let exclude = connection_query.get(event.channel).ok().copied();

        if cache.contains(&event.requester) {
            // The cached version of the subscribe packets on the proxy is still valid, so let the
            // proxy use its cache

            compose.io_buf().send_subscribe_channel_packets(
                event.channel.into(),
                event.requester,
                true,
                None,
                exclude,
            );
            continue;
        }

        let (should_cache, data) = match (
            entity_query.get(event.channel),
            chunk_query.get(event.channel),
        ) {
            (Ok((uuid, position, &pitch, &yaw, velocity, &entity_kind)), Err(_)) => {
                // Entity subscribe packets are not cached
                cache.remove(&event.requester);
                (
                    false,
                    entity_subscribe_channel_packets(
                        &compose,
                        event.channel,
                        uuid,
                        position,
                        pitch,
                        yaw,
                        velocity,
                        entity_kind,
                        set.p1(),
                    ),
                )
            }
            (Err(_), Ok(&chunk_position)) => {
                // Chunk subscribe packets are cached
                cache.insert(event.requester);
                (
                    true,
                    chunk_subscribe_channel_packets(&blocks, chunk_position),
                )
            }
            (Ok(_), Ok(_)) => {
                error!(
                    "failed to send subscribe channel packets: channel should not match both the \
                     entity and chunk queries"
                );
                continue;
            }
            (Err(_), Err(_)) => {
                error!(
                    "failed to send subscribe channel packets: channel should match the either \
                     the entity or chunk query"
                );
                continue;
            }
        };

        compose.io_buf().send_subscribe_channel_packets(
            event.channel.into(),
            event.requester,
            should_cache,
            Some(&data),
            exclude,
        );
    }
}

fn entity_subscribe_channel_packets(
    compose: &Compose,
    entity: Entity,
    uuid: &Uuid,
    position: &Position,
    pitch: Pitch,
    yaw: Yaw,
    velocity: &Velocity,
    entity_kind: EntityKind,
    world: &World,
) -> Bytes {
    let mut packet_buf;
    let minecraft_id = entity.minecraft_id();

    match entity_kind {
        EntityKind::Player => {
            let spawn_packet = play::PlayerSpawnS2c {
                entity_id: VarInt(minecraft_id),
                player_uuid: **uuid,
                position: position.as_dvec3(),
                yaw: ByteAngle::from_degrees(*yaw),
                pitch: ByteAngle::from_degrees(*pitch),
            };
            packet_buf = compose
                .io_buf()
                .encode_packet(&spawn_packet, compose)
                .unwrap();

            let show_all = show_all(minecraft_id);
            packet_buf
                .extend_from_slice(&compose.io_buf().encode_packet(&show_all, compose).unwrap());
        }
        _ => {
            let velocity = velocity.to_packet_units();

            let spawn_packet = play::EntitySpawnS2c {
                entity_id: VarInt(minecraft_id),
                object_uuid: uuid.0,
                kind: VarInt(entity_kind as i32),
                position: position.as_dvec3(),
                pitch: ByteAngle::from_degrees(*pitch),
                yaw: ByteAngle::from_degrees(*yaw),
                head_yaw: ByteAngle::from_degrees(0.0), // todo:
                data: VarInt::default(),                // todo:
                velocity,
            };
            packet_buf = compose
                .io_buf()
                .encode_packet(&spawn_packet, compose)
                .unwrap();

            let velocity_packet = play::EntityVelocityUpdateS2c {
                entity_id: VarInt(minecraft_id),
                velocity,
            };
            packet_buf.extend_from_slice(
                &compose
                    .io_buf()
                    .encode_packet(&velocity_packet, compose)
                    .unwrap(),
            );
        }
    }

    let mut metadata = MetadataChanges::default();
    metadata.encode_non_default_components(world.entity(entity));

    if let Some(view) = get_and_clear_metadata(&mut metadata) {
        let pkt = play::EntityTrackerUpdateS2c {
            entity_id: VarInt(minecraft_id),
            tracked_values: RawBytes(CowBytes::Borrowed(&view)),
        };
        packet_buf.extend_from_slice(&compose.io_buf().encode_packet(&pkt, compose).unwrap());
    }
    packet_buf.freeze()
}

fn chunk_subscribe_channel_packets(blocks: &Blocks, chunk_position: ChunkPosition) -> Bytes {
    match blocks.get_cached_or_load(*chunk_position) {
        GetChunk::Loaded(chunk) => {
            // TODO update base packet bytes when chunk changes
            chunk.base_packet_bytes.clone()
        }
        GetChunk::Loading => {
            todo!()
        }
    }
}

pub struct ChannelPlugin;

impl Plugin for ChannelPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(add_channel);
        app.add_observer(remove_channel);
        app.add_systems(
            FixedUpdate,
            (update_channel_positions, send_subscribe_channel_packets),
        );
    }
}
