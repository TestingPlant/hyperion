use byteorder::WriteBytesExt;
use flecs_ecs::prelude::*;
use hyperion_proto::{Flush, ServerToProxyMessage};
use rkyv::util::AlignedVec;
use tracing::{error, info_span};
use valence_protocol::{VarInt, packets::play};

use crate::{net::Compose, simulation::EgressComm};

pub mod metadata;
pub mod player_join;
mod stats;
mod sync_entity_state;

use player_join::PlayerJoinModule;
use stats::StatsModule;
use sync_entity_state::EntityStateSyncModule;

use crate::{
    net::ConnectionId,
};

#[derive(Component)]
pub struct EgressModule;

impl Module for EgressModule {
    fn module(world: &World) {
        let flush = {
            let flush = ServerToProxyMessage::Flush(Flush);

            let mut v: AlignedVec = AlignedVec::new();
            // length
            v.write_u64::<byteorder::BigEndian>(0).unwrap();

            rkyv::api::high::to_bytes_in::<_, rkyv::rancor::Error>(&flush, &mut v).unwrap();

            let len = u64::try_from(v.len() - size_of::<u64>()).unwrap();
            v[0..8].copy_from_slice(&len.to_be_bytes());

            let s = Box::leak(v.into_boxed_slice());
            bytes::Bytes::from_static(s)
        };

        let pipeline = world
            .entity()
            .add::<flecs::pipeline::Phase>()
            .depends_on::<flecs::pipeline::OnStore>();

        world.import::<StatsModule>();
        world.import::<PlayerJoinModule>();
        world.import::<EntityStateSyncModule>();

        system!(
            "clear_bump",
            world,
            &mut Compose($),
        )
        .kind_id(pipeline)
        .each(move |compose| {
            let span = info_span!("clear_bump");
            let _enter = span.enter();
            compose.clear_bump();
        });
    }
}
