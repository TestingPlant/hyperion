use flecs_ecs::core::Entity;
use hyperion_utils::Lifetime;
use valence_protocol::Hand;

pub struct CommandCompletionRequest<'a> {
    pub query: &'a str,
    pub id: i32,
}

unsafe impl Lifetime for CommandCompletionRequest<'_> {
    type WithLifetime<'a> = CommandCompletionRequest<'a>;
}

pub struct InteractEvent {
    pub hand: Hand,
    pub sequence: i32,
}

unsafe impl Lifetime for InteractEvent {
    type WithLifetime<'a> = Self;
}

pub struct PlayerJoinServer {
    pub username: String,
    pub entity: Entity,
}
