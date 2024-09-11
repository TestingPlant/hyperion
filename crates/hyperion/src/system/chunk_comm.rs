use std::cmp::Ordering;

use derive_more::{Deref, DerefMut};
use flecs_ecs::{
    core::{flecs::pipeline, QueryBuilderImpl, SystemAPI, TermBuilderImpl, World, WorldProvider},
    macros::{system, Component},
};
use glam::I16Vec2;
use tracing::{info, instrument, trace_span};
use valence_protocol::packets::play;

use crate::{
    component::{
        blocks::{GetChunkBytes, MinecraftWorld},
        ChunkPosition, Play, Pose,
    },
    config::CONFIG,
    net::{Compose, NetworkStreamRef},
    tracing_ext::TracingExt,
    SystemRegistry,
};

#[derive(Component, Deref, DerefMut, Default)]
pub struct ChunkSendQueue {
    changes: Vec<I16Vec2>,
}

pub fn load_pending(world: &World) {
    system!(
        "load_pending",
        world,
        &mut MinecraftWorld($),
    )
    .each_iter(|iter, _, blocks| {
        let span = trace_span!("load_pending");
        let _enter = span.enter();
        blocks.load_pending(&iter.world());
    });
}

#[instrument(skip_all, level = "trace")]
pub fn generate_chunk_changes(world: &World, registry: &mut SystemRegistry) {
    let radius = CONFIG.view_distance as i16;
    let liberal_radius = radius + 2;

    let system_id = registry.register();

    world
        .system_named::<(
            &Compose,
            &mut ChunkPosition,
            &Pose,
            &NetworkStreamRef,
            &mut ChunkSendQueue,
        )>("generate_chunk_changes")
        .term_at(0u32)
        .singleton()
        .kind::<pipeline::OnUpdate>()
        .multi_threaded()
        .tracing_each_entity(
            trace_span!("generate_chunk_changes"),
            move |entity, (compose, last_sent, pose, &stream_id, chunk_changes)| {
                let world = entity.world();

                let last_sent_chunk = last_sent.0;

                let current_chunk = pose.chunk_pos();

                if last_sent_chunk == current_chunk {
                    return;
                }

                // center chunk
                let center_chunk = play::ChunkRenderDistanceCenterS2c {
                    chunk_x: i32::from(current_chunk.x).into(),
                    chunk_z: i32::from(current_chunk.y).into(),
                };

                compose
                    .unicast(&center_chunk, stream_id, system_id, &world)
                    .unwrap();

                last_sent.0 = current_chunk;

                let last_sent_x_range = (last_sent_chunk.x - radius)..(last_sent_chunk.x + radius);
                let last_sent_z_range = (last_sent_chunk.y - radius)..(last_sent_chunk.y + radius);

                let current_x_range = (current_chunk.x - radius)..(current_chunk.x + radius);
                let current_z_range = (current_chunk.y - radius)..(current_chunk.y + radius);

                let current_x_range_liberal =
                    (current_chunk.x - liberal_radius)..(current_chunk.x + liberal_radius);
                let current_z_range_liberal =
                    (current_chunk.y - liberal_radius)..(current_chunk.y + liberal_radius);

                chunk_changes.retain(|elem| {
                    current_x_range_liberal.contains(&elem.x)
                        && current_z_range_liberal.contains(&elem.y)
                });

                let added_chunks = current_x_range
                    .flat_map(move |x| current_z_range.clone().map(move |z| I16Vec2::new(x, z)))
                    .filter(|pos| {
                        !last_sent_x_range.contains(&pos.x) || !last_sent_z_range.contains(&pos.y)
                    });

                let mut num_chunks_added = 0;

                // drain all chunks not in current_{x,z} range

                for chunk in added_chunks {
                    chunk_changes.push(chunk);
                    num_chunks_added += 1;
                }

                if num_chunks_added > 0 {
                    // remove further than radius

                    // commented out because it can break things
                    // todo: re-add but have better check so we don't prune things and then never
                    // send them
                    // chunk_changes.retain(|elem| {
                    //     let elem = elem.distance_squared(current_chunk);
                    //     elem <= r2_very_liberal
                    // });

                    chunk_changes.sort_unstable_by(|a, b| {
                        let r1 = a.distance_squared(current_chunk);
                        let r2 = b.distance_squared(current_chunk);

                        // reverse because we want to get the closest chunks first and we are poping from the end
                        match r1.cmp(&r2).reverse() {
                            Ordering::Less => Ordering::Less,
                            Ordering::Greater => Ordering::Greater,

                            // so we can dedup properly (without same element could be scattered around)
                            Ordering::Equal => a.to_array().cmp(&b.to_array()),
                        }
                    });
                    chunk_changes.dedup();
                }
            },
        );
}

// fn abc(queue: &ChunkSendQueue){
//     queue.
// }

#[allow(clippy::cast_sign_loss)]
pub fn send_full_loaded_chunks(world: &World, registry: &mut SystemRegistry) {
    let system_id = registry.register();

    world
        .system_named::<(
            &MinecraftWorld,
            &Compose,
            &NetworkStreamRef,
            &mut ChunkSendQueue,
        )>("send_full_loaded_chunks")
        .term_at(0u32)
        .singleton()
        .term_at(1u32)
        .singleton()
        .with::<&Play>()
        .kind::<pipeline::OnUpdate>()
        .multi_threaded()
        .each_entity(
            // trace_span!("send_full_loaded_chunks"),
            move |entity, (chunks, compose, &stream_id, queue)| {
                const MAX_CHUNKS_PER_TICK: usize = 16;

                let world = entity.world();

                let last = None;

                let mut iter_count = 0;

                let mut idx = (queue.changes.len() as isize) - 1;

                while idx >= 0 {
                    let elem = queue.changes[idx as usize];

                    // de-duplicate. todo: there are cases where duplicate will not be removed properly
                    // since sort is unstable
                    if last == Some(elem) {
                        queue.changes.swap_remove(idx as usize);
                        idx -= 1;
                        continue;
                    }

                    if iter_count >= MAX_CHUNKS_PER_TICK {
                        break;
                    }

                    match chunks.get_cached_or_load(elem, &world) {
                        GetChunkBytes::Loaded(chunk) => {
                            compose
                                .io_buf()
                                .unicast_raw(chunk, stream_id, system_id, &world);

                            iter_count += 1;
                            queue.changes.swap_remove(idx as usize);
                        }
                        GetChunkBytes::Loading => {}
                    }

                    idx -= 1;
                }
            },
        );
}
