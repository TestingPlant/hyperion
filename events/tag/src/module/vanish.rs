use flecs_ecs::{core::World, prelude::*};
use hyperion::{
    net::{Compose, ConnectionId},
    simulation::{Uuid, metadata::entity::EntityFlags},
};
use valence_protocol::packets::play::{self, player_list_s2c::PlayerListActions};
use valence_server::GameMode;

#[derive(Component)]
pub struct VanishModule;

#[derive(Default, Component, Debug)]
pub struct Vanished(pub bool);

impl Vanished {
    #[must_use]
    pub const fn new(is_vanished: bool) -> Self {
        Self(is_vanished)
    }

    #[must_use]
    pub const fn is_vanished(&self) -> bool {
        self.0
    }
}

impl Module for VanishModule {
    fn module(world: &World) {
        world.component::<Vanished>();
    }
}
