use std::{borrow::Cow, sync::Arc};

use anyhow::Context;
use colored::Colorize;
use flecs_ecs::prelude::*;
use hyperion_utils::EntityExt;
use serde_json::json;
use sha2::Digest;
use tracing::{error, info, info_span, trace, warn};
use valence_protocol::{
    Bounded, Packet, VarInt, packets,
    packets::{
        handshaking::handshake_c2s::HandshakeNextState, login, login::LoginCompressionS2c, play,
    },
};
use valence_text::IntoText;

use crate::{
    Prev, Shutdown,
    net::{
        Compose, ConnectionId, MINECRAFT_VERSION, PROTOCOL_VERSION, PacketDecoder,
        decoder::BorrowedPacketFrame, proxy::ReceiveState,
    },
    runtime::AsyncRuntime,
    simulation::{
        AiTargetable, Comms, ConfirmBlockSequences, EntitySize, IgnMap,
        ImmuneStatus, Name, PacketState, Pitch, Player, Position, StreamLookup, Uuid, Velocity, Xp,
        Yaw,
        animation::ActiveAnimation,
        metadata::{MetadataPrefabs, entity::Pose},
        skin::PlayerSkin,
    },
    util::{TracingExt, mojang::MojangClient},
};

#[derive(Component, Debug)]
pub struct PendingRemove {
    pub reason: String,
}

impl PendingRemove {
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

#[derive(Component)]
pub struct IngressModule;

impl Module for IngressModule {
    #[expect(clippy::too_many_lines)]
    fn module(world: &World) {
        system!(
            "shutdown",
            world,
            &Shutdown($),
        )
        .kind::<flecs::pipeline::OnLoad>()
        .each_iter(|it, _, shutdown| {
            let world = it.world();
            if shutdown.value.load(std::sync::atomic::Ordering::Relaxed) {
                info!("shutting down");
                world.quit();
            }
        });

        system!(
            "update_ign_map",
            world,
            &mut IgnMap($),
        )
        .kind::<flecs::pipeline::OnLoad>()
        .each_iter(|_, _, ign_map| {
            let span = info_span!("update_ign_map");
            let _enter = span.enter();
            ign_map.update();
        });

        system!(
            "generate_ingress_events",
            world,
            &mut StreamLookup($),
            &ReceiveState($),
        )
        .immediate(true)
        .kind::<flecs::pipeline::OnLoad>()
        .term_at(0)
        .each_iter(move |it, _, (lookup, receive)| {
            tracing_tracy::client::Client::running()
                .expect("Tracy client should be running")
                .frame_mark();

            let span = info_span!("generate_ingress_events");
            let _enter = span.enter();

            let world = it.world();

            let recv = &receive.0;

            for connect in recv.player_connect.lock().drain(..) {
                info!("player_connect");
                let view = world
                    .entity()
                    .set(ConnectionId::new(connect))
                    .set(ConfirmBlockSequences::default())
                    .set(PacketState::Handshake)
                    .set(ActiveAnimation::NONE)
                    .set(PacketDecoder::default())
                    .add::<Player>();

                lookup.insert(connect, view.id());
            }

            for disconnect in recv.player_disconnect.lock().drain(..) {
                // will initiate the removal of entity
                info!("queue pending remove");
                let Some(id) = lookup.get(&disconnect).copied() else {
                    error!("failed to get id for disconnect stream {disconnect:?}");
                    continue;
                };
                world
                    .entity_from_id(*id)
                    .set(PendingRemove::new("disconnected"));
            }
        });

        world
            .system_named::<(&ReceiveState, &ConnectionId, &mut PacketDecoder)>("ingress_to_ecs")
            .term_at(0u32)
            .singleton() // StreamLookup
            .immediate(true)
            .kind::<flecs::pipeline::PostLoad>()
            .each(move |(receive, connection_id, decoder)| {
                // 134µs with par_iter
                // 150-208µs with regular drain
                let span = info_span!("ingress_to_ecs");
                let _enter = span.enter();

                let Some(mut bytes) = receive.0.packets.get_mut(&connection_id.inner()) else {
                    return;
                };

                if bytes.is_empty() {
                    return;
                }

                decoder.shift_excess();
                decoder.queue_slice(bytes.as_ref());
                bytes.clear();
            });


        system!(
            "recv_data",
            world,
            &Compose($),
            &AsyncRuntime($),
            &Comms($),
            &MojangClient($),
            &mut PacketDecoder,
            &mut PacketState,
            &ConnectionId,
            ?&mut Pose,
            &EntitySize,
            ?&mut Position,
            &mut Yaw,
            &mut Pitch,
            &mut ConfirmBlockSequences,
            &mut ActiveAnimation,
            &IgnMap($),
        )
        .kind::<flecs::pipeline::OnUpdate>()
        .multi_threaded()
        .each_iter(
            move |it,
                  row,
                  (
                compose,
                tasks,
                comms,
                mojang,
                decoder,
                login_state,
                &io_ref,
                mut pose,
                size,
                mut position,
                yaw,
                pitch,
                confirm_block_sequences,
                animation,
                ign_map,
            )| {
                let system = it.system();
                let world = it.world();
                let entity = it.entity(row);

                if *login_state == PacketState::Handshake {
                    entity.set(Name::from(Arc::from("name")));
                    *login_state = PacketState::Login;
                }
            },
        );
    }
}
