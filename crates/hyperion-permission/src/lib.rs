use clap::ValueEnum;
use flecs_ecs::{
    core::{EntityViewGet, QueryBuilderImpl, SystemAPI, TermBuilderImpl, World, WorldGet},
    macros::{Component, observer},
    prelude::{Module, flecs},
};
use hyperion::{
    net::{Compose, ConnectionId},
    simulation::{Player, Uuid, command::get_command_packet},
    storage::LocalDb,
};
use num_derive::{FromPrimitive, ToPrimitive};

#[derive(Component)]
pub struct PermissionModule;

mod storage;

#[derive(
    Default,
    Component,
    FromPrimitive,
    ToPrimitive,
    Copy,
    Clone,
    Debug,
    PartialEq,
    ValueEnum,
    Eq
)]
#[repr(C)]
pub enum Group {
    Banned,
    #[default]
    Normal,
    Moderator,
    Admin,
}

// todo:

impl Module for PermissionModule {
    fn module(world: &World) {
        world.component::<Group>();
        world.component::<storage::PermissionStorage>();

        world.get::<&LocalDb>(|db| {
            let storage = storage::PermissionStorage::new(db).unwrap();
            world.set(storage);
        });
    }
}
