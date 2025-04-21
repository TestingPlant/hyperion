use flecs_ecs::prelude::*;
use tracing::{error, info_span};

use crate::{
    net::Compose,
};

#[derive(Component)]
pub struct StatsModule;

impl Module for StatsModule {
    fn module(world: &World) {
    }
}
