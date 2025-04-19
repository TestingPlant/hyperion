use hyperion::{
    flecs_ecs::{
        self,
        core::{EntityViewGet, World, WorldGet},
        macros::Component,
        prelude::Module,
    },
    net::{ConnectionId, DataBundle},
    protocol::{
        game_mode::OptGameMode,
        packets::play::{self, PlayerAbilitiesS2c},
        BlockPos, ByteAngle, GlobalPos, VarInt,
    },
    server::{abilities::PlayerAbilitiesFlags, ident, GameMode},
    simulation::{
        event::{ClientStatusCommand, ClientStatusEvent},
        handlers::PacketSwitchQuery,
        metadata::{entity::Pose, living_entity::Health},
        packet::HandlerRegistry,
        Flight, FlyingSpeed, Pitch, Position, Uuid, Xp, Yaw,
    },
};
use hyperion_utils::{EntityExt, LifetimeHandle};

#[derive(Component)]
pub struct RespawnModule;

impl Module for RespawnModule {
    fn module(world: &World) {
    }
}
