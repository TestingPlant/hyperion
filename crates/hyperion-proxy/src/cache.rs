use std::{collections::HashSet, time::SystemTime};

use bvh::{Aabb, Bvh, Data, Point};
use bytes::Bytes;
use glam::I16Vec2;
use rayon::prelude::*;
use hyperion_proto::{ArchivedUpdateChannelPositions, ArchivedServerToProxyMessage};
use roaring::RoaringBitmap;
use rustc_hash::FxHashMap;
use tracing::{debug, error, warn};

use crate::egress::Egress;

/// Maximum chunk distance between a packet's center and a player for a local broadcast to be sent to
/// that player
const LOCAL_RADIUS: i16 = 16;

/// Maximum chunk distance between a packet's center and a player for an unsubscribed player to subscribe to a channel
const SUBSCRIBE_RADIUS: i16 = 16;

/// Minimum chunk distance between a packet's center and a player for a subscribed player to be unsubscribed from a channel.
const UNSUBSCRIBE_RADIUS: i16 = SUBSCRIBE_RADIUS + 2;

/// If a player's total bytes pending send is greater then this value, it will not receive more
/// subscribe packets. This is useful for clients with slow network connections.
const MAX_PENDING_BYTES_FOR_SUBSCRIBE: usize = 16_384;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Player {
    stream: u64,
    chunk_position: I16Vec2,
}

impl Point for Player {
    fn point(&self) -> I16Vec2 {
        self.chunk_position
    }
}

impl Data for Player {
    type Unit = u64;

    fn data<'a: 'c, 'b: 'c, 'c>(&'a self, (): Self::Context<'b>) -> &'c [Self::Unit] {
        std::slice::from_ref(&self.stream)
    }
}

pub struct Channel {
    /// List of connection ids that are pending a subscription to this channel
    pending_connections: HashSet<u64>,

    /// List of connection ids that are currently subscribed to this channel
    subscribed_connections: HashSet<u64>,

    cached_subscribe_packets: Option<Bytes>,
    unsubscribe_packets: Bytes,

    chunk_position: I16Vec2,
}

#[derive(Default)]
pub struct ChannelManager {
    channels: FxHashMap<u32, Channel>,
}

/// Buffers egress operations for optimized processing.
pub struct BufferedEgress {
    /// Manages channels
    channel_manager: ChannelManager,
    /// Reference to the underlying egress handler.
    pub(crate) egress: Egress,
    player_bvh: Bvh<Vec<u64>>,
    pub(crate) ticks_elapsed_since_yield: u64,
}

impl BufferedEgress {
    /// Creates a new `BufferedEgress` instance.
    #[must_use]
    pub fn new(egress: Egress) -> Self {
        Self {
            channel_manager: ChannelManager::default(),
            egress,
            player_bvh: Bvh::default(),
            ticks_elapsed_since_yield: 0,
        }
    }

    pub fn update_channel_positions(&mut self, packet: &ArchivedUpdateChannelPositions<'_>) {
        if self.ticks_elapsed_since_yield != 0 {
            return;
        }

        let start = std::time::Instant::now();

        // Update channel positions
        for update in packet.updates.get() {
            let channel_id = update.channel_id.into();
            let Some(channel) = self.channel_manager.channels.get_mut(&channel_id) else {
                error!(
                    "server sent UpdateChannelPositions with an invalid channel id \
                             {channel_id}"
                );
                continue;
            };

            let Ok(position) = rkyv::deserialize::<_, !>(&update.position);
            let position = I16Vec2::from(position);
            channel.chunk_position = position;
        }
        // TODO rename Player struct to smth more generic
        let mut channels = self.channel_manager.channels
            .iter()
            .map(|(&channel_id, channel)| {
                Player {
                    stream: channel_id as u64,
                    chunk_position: channel.chunk_position,
                }
            })
        .collect::<Vec<_>>();

        let channel_bvh = Bvh::build(&mut channels, ());
        let mut requested_subscriptions = Vec::new();
        let mut num_sub = 0;
        let mut num_unsub = 0;

        for (&stream, player) in &mut self.egress.player_registry {
            let player = player.get_mut();
            let mut channels_below_unsubscribe_radius = RoaringBitmap::new();
            // let mut channels_below_subscribe_radius = RoaringBitmap::new();
//            for slice in channel_bvh.get_in(Aabb::new(
//                    player.position - I16Vec2::splat(UNSUBSCRIBE_RADIUS),
//                    player.position + I16Vec2::splat(UNSUBSCRIBE_RADIUS),
//            )) {
//                let (_, channel_ids) = channel_bvh.inner();
//
//                let start = slice.start as usize;
//                let end = slice.end as usize;
//
//                let channel_ids = &channel_ids[start..end];
//
//                channels_below_unsubscribe_radius.extend(channel_ids.iter().map(|id| *id as u32));
//                // channels_below_subscribe_radius.extend(channel_ids.iter().map(|id| *id as u32));
//
////                for &channel_id in channel_ids {
////                    // TODO store u32s directly
////                    let channel_id = channel_id as u32;
////
////                    let Some(channel) = self.channel_manager.channels.get_mut(&channel_id) else {
////                        error!("bvh contains invalid channel");
////                        continue;
////                    };
////
////                    // TODO shoild this be a separate bvh query?
////                    let within_subscribe_radius = (channel.chunk_position - player.position).abs().max_element() <= SUBSCRIBE_RADIUS;
////
////                    if within_subscribe_radius {
////                        channels_below_subscribe_radius.insert(channel_id);
////                    }
////                }
//            }

            let (_, channel_ids) = channel_bvh.inner();

            channels_below_unsubscribe_radius.extend(channel_bvh.get_in(Aabb::new(
                    player.position - I16Vec2::splat(UNSUBSCRIBE_RADIUS),
                    player.position + I16Vec2::splat(UNSUBSCRIBE_RADIUS),
            )).into_iter().flat_map(|slice| {
                let start = slice.start as usize;
                let end = slice.end as usize;

                let channel_ids = &channel_ids[start..end];

                channel_ids.iter().map(|id| *id as u32)
            }));
            let channels_below_subscribe_radius = &channels_below_unsubscribe_radius;

            for channel_id in channels_below_subscribe_radius - &player.subscribed_channels {
                // This channel is below the subscribe radius but is not currently subscribed. Subscribe to the channel
                let Some(channel) = self.channel_manager.channels.get_mut(&channel_id) else {
                    error!("bvh contains invalid channel");
                    continue;
                };
                if channel.pending_connections.is_empty() {
                    // Request subscribe packets from the server
                    requested_subscriptions.push((
                            channel_id,
                            channel.cached_subscribe_packets.is_some(),
                    ));
                }
                let new = channel.pending_connections.insert(stream);
                if new {
                    num_sub += 1;
                }
            }

            for channel_to_unsubscribe in &player.subscribed_channels - channels_below_unsubscribe_radius {
                let Some(channel) = self.channel_manager.channels.get_mut(&channel_to_unsubscribe) else {
                    error!("could not find channel to unsubscribe");
                    continue;
                };

                num_unsub += 1;

                // TODO removing one at a time if probabbly inefficient
                player.subscribed_channels.remove(channel_to_unsubscribe);
                let was_subscribed = channel.subscribed_connections.remove(&stream);
                assert!(was_subscribed);
                player.send_shared(channel.unsubscribe_packets.clone());
            }
        }

        if !requested_subscriptions.is_empty() {
            let request = rkyv::to_bytes::<rkyv::rancor::Error>(
                &hyperion_proto::ProxyToServerMessage::RequestSubscribeChannelPackets(
                    hyperion_proto::RequestSubscribeChannelPackets {
                        channels: &requested_subscriptions,
                    },
                ),
            )
                .unwrap();
            let server_sender = self.egress.server_sender.clone();
            tokio::spawn(async move {
                if let Err(e) = server_sender.send(request).await {
                    error!("failed to send request subscribe channel packets: {e}");
                }
            });
        }
        tracing::info!("updated channel positions in {:?} - {num_sub} new subscriptions and {num_unsub} unsubscribed", start.elapsed());
    }

//    pub fn update_channel_positions(&mut self, packet: &ArchivedUpdateChannelPositions<'_>) {
//        let start = std::time::Instant::now();
//        let mut requested_subscriptions = Vec::new();
//
//        // Update channel positions
//        for update in packet.updates.get() {
//            let channel_id = update.channel_id.into();
//            let Some(channel) = self.channel_manager.channels.get_mut(&channel_id) else {
//                error!(
//                    "server sent UpdateChannelPositions with an invalid channel id \
//                             {channel_id}"
//                );
//                continue;
//            };
//
//            let Ok(position) = rkyv::deserialize::<_, !>(&update.position);
//            let position = I16Vec2::from(position);
//            channel.chunk_position = position;
//        }
//
//        // Determine which players to unsubscribe and subscribe to each channel. This is
//        // done in a separate loop to also calculate this info for channels who didn't update
//        // their position
//        for (&channel_id, channel) in &mut self.channel_manager.channels {
//            // The separation between the checks for which players should remain subscribed and the checks for which unsubscribed players should subscribe with separate radii for each check exists so that if a player repeatedly crosses the same chunk boundary, the player won't subscribe and unsubscribe from the same channels repeatedly. Avoiding this issue helps reduce network and CPU usage.
//
//            let mut should_remain_subscribed = HashSet::new();
//
//            // Determine which subscribed players should remain subscribed to this channel. All players below the unsubscribe radius can remain subscribed to this
//            // channel.
//            for slice in self.player_bvh.get_in(Aabb::new(
//                    channel.chunk_position - I16Vec2::splat(UNSUBSCRIBE_RADIUS),
//                    channel.chunk_position + I16Vec2::splat(UNSUBSCRIBE_RADIUS),
//            )) {
//                let (_, streams) = self.player_bvh.inner();
//
//                let start = slice.start as usize;
//                let end = slice.end as usize;
//
//                let streams = &streams[start..end];
//                for &stream in streams {
//                    // This steam is below the unsubscribe radius...
//                    if channel.subscribed_connections.contains(&stream) {
//                        // ... and it is already subscribed, so it can remain subscribed to this channel
//                        should_remain_subscribed.insert(stream);
//                    }
//                }
//            }
//
//            // Determine which unsubscribed players should subscribe to this channel. All players below the subscribe radius can subscribe to this
//            // channel.
//            for slice in self.player_bvh.get_in(Aabb::new(
//                    channel.chunk_position - I16Vec2::splat(SUBSCRIBE_RADIUS),
//                    channel.chu.len()nk_position + I16Vec2::splat(SUBSCRIBE_RADIUS),
//            )) {
//                let (_, streams) = self.player_bvh.inner();
//
//                let start = slice.start as usize;
//                let end = slice.end as usize;
//
//                let streams = &streams[start..end];
//                for &stream in streams {
//                    let Some(player) = self.egress.player_registry.get(&stream) else {
//                        error!("bvh contains invalid stream id {stream}");
//                        continue;
//                    };
//
//                    if !player.can_receive_broadcasts() {
//                        continue;
//                    }
//
//                    // This stream should be subscribed to this channel...
//                    if !channel.subscribed_connections.contains(&stream) {
//                        // ... but it is not currently subscribed
//
//                        if channel.pending_connections.is_empty() {
//                            // Request subscribe packets from the server
//                            requested_subscriptions.push((
//                                    channel_id,
//                                    channel.cached_subscribe_packets.is_some(),
//                            ));
//                        }
//                        channel.pending_connections.insert(stream);
//                    }
//                }
//            }
//
//            if channel.subscribed_connections.len() != should_remain_subscribed.len() {
//                channel.subscribed_connections.retain(|stream| {
//                    let should_remain = should_remain_subscribed.contains(stream);
//
//                    if !should_remain {
//                        // This stream is currently subscribed. It will be sent unsubscribe
//                        // packets and removed from the subscribed connections set
//                        self.egress
//                            .unicast(*stream, channel.unsubscribe_packets.clone());
//                        debug!("unsubscribing player {stream} from channel {channel_id}");
//                    }
//
//                    should_remain
//                });
//            }
//        }
//
//        if !requested_subscriptions.is_empty() {
//            let request = rkyv::to_bytes::<rkyv::rancor::Error>(
//                &hyperion_proto::ProxyToServerMessage::RequestSubscribeChannelPackets(
//                    hyperion_proto::RequestSubscribeChannelPackets {
//                        channels: &requested_subscriptions,
//                    },
//                ),
//            )
//                .unwrap();
//            let server_sender = self.egress.server_sender.clone();
//            tokio::spawn(async move {
//                if let Err(e) = server_sender.send(request).await {
//                    error!("failed to send request subscribe channel packets: {e}");
//                }
//            });
//        }
//        tracing::info!("updated channel positions in {:?}", start.elapsed());
//    }

    /// Handles incoming server-to-proxy messages.
    // #[instrument(skip_all)]
    #[expect(clippy::excessive_nesting)]
    pub fn handle_packet(&mut self, message: &ArchivedServerToProxyMessage<'_>) {
        match message {
            ArchivedServerToProxyMessage::UpdatePlayerPositions(packet) => {
                if self.ticks_elapsed_since_yield != 0 {
                    return;
                }

                let mut players = packet
                    .updates
                    .iter()
                    .map(|update| {
                        let Ok(stream) = rkyv::deserialize::<u64, !>(&update.stream);
                        let Ok(position) = rkyv::deserialize::<_, !>(&update.position);
                        let position = I16Vec2::from(position);

                        Player {
                            stream,
                            chunk_position: position,
                        }
                    })
                    .collect::<Vec<_>>();

                self.player_bvh = Bvh::build(&mut players, ());

                for update in packet.updates.iter() {
                    let Ok(stream) = rkyv::deserialize::<u64, !>(&update.stream);
                    let Ok(position) = rkyv::deserialize::<_, !>(&update.position);
                    let position = I16Vec2::from(position);

                    let Some(player) = self.egress.player_registry.get_mut(&stream) else {
                        error!("unknown player {stream:?}");
                        continue;
                    };

                    player.get_mut().position = position;
                }
            }
            ArchivedServerToProxyMessage::AddChannel(packet) => {
                let Ok(position) = rkyv::deserialize::<_, !>(&packet.position);
                let unsubscribe_packets = match rkyv::deserialize::<_, rkyv::rancor::Error>(
                    &packet.unsubscribe_packets,
                ) {
                    Ok(data) => data,
                    Err(e) => {
                        error!(
                            "failed to deserialize raw unsubscribe packets from AddChannel: {e}"
                        );
                        return;
                    }
                };
                debug!("adding channel {}", u32::from(packet.channel_id));

                let previous_channel =
                    self.channel_manager
                        .channels
                        .insert(packet.channel_id.into(), Channel {
                            pending_connections: HashSet::new(),
                            subscribed_connections: HashSet::new(),
                            cached_subscribe_packets: None,
                            unsubscribe_packets: Bytes::from(unsubscribe_packets),
                            chunk_position: position.into(),
                        });

                if previous_channel.is_some() {
                    error!(
                        "server sent AddChannel for a channel with the same id as an existing \
                         channel"
                    );
                }
            }
            ArchivedServerToProxyMessage::UpdateChannelPositions(packet) => {
                self.update_channel_positions(packet);
            }
            ArchivedServerToProxyMessage::RemoveChannel(packet) => {
                debug!("removing channel {}", u32::from(packet.channel_id));
                let Some(channel) = self
                    .channel_manager
                    .channels
                    .remove(&packet.channel_id.into())
                else {
                    error!("server sent RemoveChannel for a channel that does not exist");
                    return;
                };

                // Unsubscribe all players
                for &stream in &channel.subscribed_connections {
                    self.egress
                        .unicast(stream, channel.unsubscribe_packets.clone());
                }
            }
            ArchivedServerToProxyMessage::SubscribeChannelPackets(packet) => {
                let exclude = u64::from(packet.exclude);
                let channel_id = packet.channel_id.into();
                let Some(channel) = self.channel_manager.channels.get_mut(&channel_id) else {
                    error!("server sent SubscribeChannelPackets for a channel that does not exist");
                    return;
                };

                if channel.pending_connections.is_empty() {
                    warn!(
                        "server sent SubscribeChannelPackets but there are no pending subscribers"
                    );
                    return;
                }

                // TODO no box

                let data = if let Some(data) =
                    rkyv::deserialize::<Option<Box<[u8]>>, rkyv::rancor::Error>(&packet.data)
                        .unwrap()
                {
                    let bytes = Bytes::from(data);
                    channel.cached_subscribe_packets = packet.cache.then(|| bytes.clone());
                    bytes
                } else {
                    let Some(bytes) = channel.cached_subscribe_packets.clone() else {
                        error!(
                            "server sent SubscribeChannelPackets without subscribe packets, but \
                             the subscribe packets are not cached on the proxy"
                        );
                        return;
                    };
                    if !packet.cache {
                        channel.cached_subscribe_packets = None;
                    }
                    bytes
                };

                for &stream in &channel.pending_connections {
                    let Some(player) = self.egress.player_registry.get_mut(&stream) else {
                        error!("player not found");
                        continue;
                    };

                    let player = player.get_mut();

                    // TODO use max pending bytes
                    player.send_shared(data.clone());
                    player.subscribed_channels.insert(channel_id);
                    channel.subscribed_connections.insert(stream);

                    if stream == exclude {
                        continue;
                    }

                    // TODO exclde check messes this up
                    debug!("subscribing player {stream} to channel {channel_id}");
//                    let sent = self.egress.maybe_unicast(
//                        stream,
//                        data.clone(),
//                        MAX_PENDING_BYTES_FOR_SUBSCRIBE,
//                    );
//
//                    if sent {
//                    }

                    // Note that even if the subscribe packets were not sent and a stream is not
                    // subscribed, the stream is still removed from pending connections. This
                    // allows the game server to send new subscribe packets later because the proxy
                    // will not request subscribe packets if there's already a stream in the
                    // pending connections list.
                }

                channel.pending_connections.clear();
            }
            ArchivedServerToProxyMessage::BroadcastGlobal(packet) => {
                let data =
                    Bytes::from(rkyv::deserialize::<_, rkyv::rancor::Error>(&packet.data).unwrap());
                let Ok(exclude) = rkyv::deserialize::<u64, !>(&packet.exclude);

                self.egress.player_registry.par_iter_mut().for_each(|(&stream, player)| {
                    let player = player.get_mut();
                    if !player.can_receive_broadcasts() || stream == exclude {
                        return;
                    }

                    player.send_shared(data.clone());
                });
            }
            ArchivedServerToProxyMessage::BroadcastLocal(packet) => {
                let Ok(center_x) = rkyv::deserialize::<i16, !>(&packet.center.x);
                let Ok(center_z) = rkyv::deserialize::<i16, !>(&packet.center.z);
                let Ok(player_id_to_exclude) = rkyv::deserialize::<u64, !>(&packet.exclude);
                let data =
                    Bytes::from(rkyv::deserialize::<_, rkyv::rancor::Error>(&packet.data).unwrap());

                let position = I16Vec2::new(center_x, center_z);
                let min = position - I16Vec2::splat(LOCAL_RADIUS);
                let max = position + I16Vec2::splat(LOCAL_RADIUS);

                let aabb = Aabb::new(min, max);

                let slices = self.player_bvh.get_in(aabb);
                let (_, streams) = self.player_bvh.inner();

//                for slice in slices {
//                    let start = slice.start as usize;
//                    let end = slice.end as usize;
//
//                    let streams = &streams[start..end];
//                    for &stream in streams {
//                        if stream == player_id_to_exclude {
//                            continue;
//                        }
//
//                        self.egress.unicast(stream, data.clone());
//                    }
//                }
//                unsafe {
//                    self.egress.broadcast(slices.into_par_iter().flat_map(|slice| {
//                        let start = slice.start as usize;
//                        let end = slice.end as usize;
//
//                        streams[start..end].par_iter()
//                    }).copied().filter(|stream| *stream != player_id_to_exclude),
//                    data
//                    );
//                }
                unsafe {
                    self.egress.broadcast(slices.into_iter().flat_map(|slice| {
                        let start = slice.start as usize;
                        let end = slice.end as usize;

                        streams[start..end].iter()
                    }).copied().filter(|stream| *stream != player_id_to_exclude).par_bridge(),
                    data
                    );
                }
            }
            ArchivedServerToProxyMessage::BroadcastChannel(packet) => {
                let exclude = u64::from(packet.exclude);
                let data =
                    Bytes::from(rkyv::deserialize::<_, rkyv::rancor::Error>(&packet.data).unwrap());

                let Some(channel) = self.channel_manager.channels.get(&packet.channel_id.into())
                else {
                    error!("server sent BroadcastChannel for a channel that does not exist");
                    return;
                };

                // SAFETY: subscribed_connections is a HashSet which guarantees unique stream ids
                unsafe { self.egress.broadcast(channel.subscribed_connections.par_iter().copied().filter(|stream| *stream != exclude), data) };
//                for &stream in &channel.subscribed_connections {
//                    if stream == exclude {
//                        continue;
//                    }
//
//                    self.egress.unicast(stream, data.clone());
//                }
            }
            ArchivedServerToProxyMessage::Unicast(unicast) => {
                let data = rkyv::deserialize::<_, rkyv::rancor::Error>(&unicast.data).unwrap();
                self.egress
                    .unicast(unicast.stream.into(), Bytes::from(data));
            }
            ArchivedServerToProxyMessage::SetReceiveBroadcasts(pkt) => {
                self.egress.handle_set_receive_broadcasts(pkt);
            }
            ArchivedServerToProxyMessage::Shutdown(pkt) => {
                self.egress.handle_shutdown(pkt);
            }
            ArchivedServerToProxyMessage::CurrentTime(pkt) => {
                let Ok(time_since_epoch) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)
                else {
                    error!("system time must be after unix epoch");
                    return;
                };

                let current_seconds_since_epoch = time_since_epoch.as_secs();
                if pkt.seconds_since_epoch > current_seconds_since_epoch {
                    error!(
                        "game server's system clock is {} seconds ahead",
                        pkt.seconds_since_epoch - current_seconds_since_epoch
                    );
                    return;
                }

                let delay = current_seconds_since_epoch - pkt.seconds_since_epoch;
                if delay > 5 || self.ticks_elapsed_since_yield > 5 {
                    warn!(
                        "proxy is running too slow - it is {delay} seconds behind on processing \
                         server to proxy messages and {} game ticks have elapsed since the server to proxy task has yielded execution",
                        self.ticks_elapsed_since_yield,
                    );
                }
                self.ticks_elapsed_since_yield += 1;
            }
        }
    }
}
