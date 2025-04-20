use std::{borrow::Cow, collections::BTreeSet, ops::Index};

use anyhow::Context;
use flecs_ecs::prelude::*;
use glam::DVec3;
use hyperion_utils::EntityExt;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use tracing::{info, instrument};
use valence_protocol::{
    ByteAngle, GameMode, Ident, PacketEncoder, RawBytes, VarInt, Velocity,
    game_mode::OptGameMode,
    ident,
    packets::play::{
        self, GameJoinS2c,
        player_position_look_s2c::PlayerPositionLookFlags,
        team_s2c::{CollisionRule, Mode, NameTagVisibility, TeamColor, TeamFlags},
    },
};
use valence_registry::{BiomeRegistry, RegistryCodec};
use valence_server::entity::EntityKind;
use valence_text::IntoText;

use crate::simulation::{MovementTracking, PacketState, Pitch};

mod list;
pub use list::*;

use crate::{
    config::Config,
    egress::metadata::show_all,
    ingress::PendingRemove,
    net::{Compose, ConnectionId, DataBundle},
    simulation::{
        Comms, Name, Position, Uuid, Yaw,
        command::{Command, ROOT_COMMAND, get_command_packet},
        metadata::{MetadataChanges, entity::EntityFlags},
        skin::PlayerSkin,
        util::registry_codec_raw,
    },
    util::{SendableQuery, SendableRef},
};


fn send_sync_tags(encoder: &mut PacketEncoder) -> anyhow::Result<()> {
    let bytes = include_bytes!("data/tags.json");

    let groups = serde_json::from_slice(bytes)?;

    let pkt = play::SynchronizeTagsS2c { groups };

    encoder.append_packet(&pkt)?;

    Ok(())
}

#[tracing::instrument(skip_all)]
pub fn spawn_entity_packet(
    id: Entity,
    kind: EntityKind,
    uuid: Uuid,
    yaw: &Yaw,
    pitch: &Pitch,
    position: &Position,
) -> play::EntitySpawnS2c {
    info!("spawning entity");

    let entity_id = VarInt(id.minecraft_id());

    play::EntitySpawnS2c {
        entity_id,
        object_uuid: *uuid,
        kind: VarInt(kind.get()),
        position: position.as_dvec3(),
        yaw: ByteAngle::from_degrees(**yaw),
        pitch: ByteAngle::from_degrees(**pitch),
        head_yaw: ByteAngle::from_degrees(**yaw), // todo: unsure if this is correct
        data: VarInt::default(),
        velocity: Velocity([0; 3]),
    }
}

#[derive(Component)]
pub struct PlayerJoinModule;

impl Module for PlayerJoinModule {
    fn module(world: &World) {
        let root_command = world.entity().set(Command::ROOT);

        #[expect(
            clippy::unwrap_used,
            reason = "this is only called once on startup. We mostly care about crashing during \
                      server execution"
        )]
        ROOT_COMMAND.set(root_command.id()).unwrap();
    }
}
