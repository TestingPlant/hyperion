use std::fmt::Debug;

use flecs_ecs::prelude::*;
use glam::{IVec3, Vec3};
use hyperion_utils::EntityExt;
use itertools::Either;
use valence_protocol::{
    ByteAngle, RawBytes, VarInt,
    packets::play::{self},
};

use crate::{
    Prev,
    net::{Compose, ConnectionId, DataBundle},
    simulation::{
        Flight, MovementTracking, Owner, PendingTeleportation, Pitch, Position, Velocity, Xp, Yaw,
        animation::ActiveAnimation,
        blocks::Blocks,
        entity_kind::EntityKind,
        event::{self, HitGroundEvent},
        metadata::{MetadataChanges, get_and_clear_metadata},
    },
    spatial::get_first_collision,
    storage::Events,
};

#[derive(Component)]
pub struct EntityStateSyncModule;

fn track_previous<T: ComponentId + Copy + Debug + PartialEq>(world: &World) {
    let post_store = world
        .entity_named("post_store")
        .add::<flecs::pipeline::Phase>()
        .depends_on::<flecs::pipeline::OnStore>();

    // we include names so that if we call this multiple times, we don't get multiple observers/systems
    let component_name = std::any::type_name::<T>();

    // get the last stuff after ::
    let component_name = component_name.split("::").last().unwrap();
    let component_name = component_name.to_lowercase();

    let observer_name = format!("init_prev_{component_name}");
    let system_name = format!("track_prev_{component_name}");

    world
        .observer_named::<flecs::OnSet, &T>(&observer_name)
        .without::<(Prev, T)>() // we have not set Prev yet
        .each_entity(|entity, value| {
            entity.set_pair::<Prev, T>(*value);
        });

    world
        .system_named::<(&mut (Prev, T), &T)>(system_name.as_str())
        .kind_id(post_store)
        .each(|(prev, value)| {
            *prev = *value;
        });
}

impl Module for EntityStateSyncModule {
    fn module(world: &World) {
        track_previous::<Position>(world);
        track_previous::<Yaw>(world);
        track_previous::<Pitch>(world);
    }
}
