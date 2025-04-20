use flecs_ecs::core::Entity;
use valence_protocol::Hand;

pub struct CommandCompletionRequest<'a> {
    pub query: &'a str,
    pub id: i32,
}

pub struct InteractEvent {
    pub hand: Hand,
    pub sequence: i32,
}

pub struct PlayerJoinServer {
    pub username: String,
    pub entity: Entity,
}
