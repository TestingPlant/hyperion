use flecs_ecs::{
    core::{EntityViewGet, QueryBuilderImpl, SystemAPI, TableIter, TermBuilderImpl, World, flecs},
    macros::{Component, system},
    prelude::Module,
};
use hyperion::{
    net::ConnectionId,
    simulation::{Name, Player, Position, event},
    storage::EventQueue,
    system_registry::SystemId,
    valence_protocol::{packets::play, text::IntoText},
};
use tracing::info_span;

const CHAT_COOLDOWN_SECONDS: i64 = 15; // 15 seconds
const CHAT_COOLDOWN_TICKS: i64 = CHAT_COOLDOWN_SECONDS * 20; // Convert seconds to ticks

#[derive(Default, Component)]
#[meta]
pub struct ChatCooldown {
    pub expires: i64,
}

#[derive(Component)]
pub struct ChatModule;

impl Module for ChatModule {
    fn module(world: &World) {
        let system_id = SystemId(8);

        world.component::<ChatCooldown>().meta();

        world
            .component::<Player>()
            .add_trait::<(flecs::With, ChatCooldown)>();

        system!("handle_chat_messages", world, &mut EventQueue<event::ChatMessage<'static>>($), &hyperion::net::Compose($))
            .multi_threaded()
            .each_iter(move |it: TableIter<'_, false>, _: usize, (event_queue, compose): (&mut EventQueue<event::ChatMessage<'static>>, &hyperion::net::Compose)| {
                let world = it.world();
                let span = info_span!("handle_chat_messages");
                let _enter = span.enter();

                let current_tick = compose.global().tick;

                event_queue.drain();
            });
    }
}
