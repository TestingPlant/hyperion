use std::{sync::Arc, time::SystemTime};

use bvh::{Aabb, Bvh, Data, Point};
use bytes::Bytes;
use enum_map::{Enum, EnumMap};
use glam::I16Vec2;
use hyperion_proto::{ArchivedServerToProxyMessage, ArchivedUpdateChannelPositions};
use rayon::prelude::*;
use roaring::RoaringBitmap;
use rustc_hash::{FxHashMap, FxHashSet};
use tracing::{debug, error, warn};

use crate::{
    CloneSyncUnsafeCell,
    data::PlayerHandle,
    egress::{Egress, EgressInstruction},
};

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

#[derive(Default)]
struct ChannelSubscriptionDelta {
    /// List of connection ids that are pending a subscription to this channel that were not
    /// previously pending a connection
    new_pending_connections: Vec<u64>,

    /// List of connection ids that should be unsubscribed from this channel
    unsubscribed_connections: FxHashSet<u64>,
}

#[derive(Default)]
struct ChannelsDelta {
    /// Channels where subscribe packets have been sent. This stores a tuple containing the channel
    /// id and the list of connection ids which have been sent.
    subscribed_channels: Vec<(u32, Arc<FxHashSet<u64>>)>,

    /// Channels which have been removed
    removed_channels: RoaringBitmap,
}

// TODO merge this with CHannel
#[derive(Clone)]
struct ChannelSubscribers {
    /// List of connection ids that are pending a subscription to this channel
    pending_connections: Arc<FxHashSet<u64>>,

    /// List of connection ids that are currently subscribed to this channel
    subscribed_connections: Arc<FxHashSet<u64>>,

    chunk_position: I16Vec2,
}

pub struct Channel {
    subscribers: ChannelSubscribers,
    cached_subscribe_packets: Option<Bytes>,
    unsubscribe_packets: Bytes,
}

#[derive(Default)]
pub struct ChannelManager {
    channels: FxHashMap<u32, Channel>,
}

impl ChannelManager {
    fn to_subscribers(&self) -> FxHashMap<u32, ChannelSubscribers> {
        self.channels
            .iter()
            .map(|(id, channel)| (*id, channel.subscribers.clone()))
            .collect()
    }
}

#[derive(Debug, Enum, Copy, Clone, PartialEq, Eq)]
pub enum ServerToProxyMessageType {
    UpdatePlayerPositions,
    AddChannel,
    UpdateChannelPositions,
    RemoveChannel,
    SubscribeChannelsPackets,
    BroadcastGlobal,
    BroadcastLocal,
    BroadcastChannel,
    Unicast,
    SetReceiveBroadcasts,
    Shutdown,
    CurrentTime,
}

/// Buffers egress operations for optimized processing.
pub struct BufferedEgress {
    /// Manages channels
    channel_manager: ChannelManager,
    /// Reference to the underlying egress handler.
    pub(crate) egress: Egress,
    player_bvh: Bvh<Vec<u64>>,
    pub(crate) ticks_elapsed_since_yield: u64,
    total_time: std::time::Duration,
    total_time_in_write: std::time::Duration,
    total_time_in_sub: std::time::Duration,
    total_time_in_clone_subs: std::time::Duration,
    total_time_in_update_subs: std::time::Duration,
    block_time: std::time::Duration,
    timings: EnumMap<ServerToProxyMessageType, std::time::Duration>,
    behind: bool,

    subscription_sender: kanal::Sender<(
        Arc<FxHashMap<u64, CloneSyncUnsafeCell<PlayerHandle>>>,
        ChannelsDelta,
        FxHashMap<u32, ChannelSubscribers>,
        Vec<Player>,
    )>,
    delta_receiver: tokio::sync::mpsc::Receiver<FxHashMap<u32, ChannelSubscriptionDelta>>,
    pending_subscription_deltas: bool,
    pending_subscription_packets: bool,

    channels_delta: ChannelsDelta,
}

impl BufferedEgress {
    /// Creates a new `BufferedEgress` instance.
    #[must_use]
    pub fn new(egress: Egress) -> Self {
        let (subscription_sender, subscription_receiver) = kanal::bounded(1);
        let (delta_sender, delta_receiver) = tokio::sync::mpsc::channel(1);

        std::thread::Builder::new()
            .name("channel_subscriptions".to_string())
            .spawn(move || {
                Self::subscription_worker(subscription_receiver, delta_sender);
            })
            .unwrap();

        Self {
            channel_manager: ChannelManager::default(),
            egress,
            player_bvh: Bvh::default(),
            ticks_elapsed_since_yield: 0,
            total_time: std::time::Duration::from_secs(0),
            total_time_in_write: std::time::Duration::from_secs(0),
            total_time_in_sub: std::time::Duration::from_secs(0),
            total_time_in_clone_subs: std::time::Duration::from_secs(0),
            total_time_in_update_subs: std::time::Duration::from_secs(0),
            block_time: std::time::Duration::from_secs(0),
            timings: EnumMap::default(),
            behind: false,

            subscription_sender,
            delta_receiver,
            pending_subscription_deltas: false,
            pending_subscription_packets: false,
            channels_delta: ChannelsDelta::default(),
        }
    }

    /// Polls for updates from background threads. This is cancel safe.
    pub fn poll(&mut self) {
        if !self.pending_subscription_deltas {
            return;
        }

        // TODO use await
        if let Ok(deltas) = self.delta_receiver.try_recv() {
            let start = std::time::Instant::now();
            self.pending_subscription_deltas = false;
            let mut requested_subscriptions = Vec::new();
            let player_registry = Arc::make_mut(&mut self.egress.player_registry);
            for (channel_id, delta) in deltas {
                let Some(channel) = self.channel_manager.channels.get_mut(&channel_id) else {
                    error!(
                        "server sent UpdateChannelPositions with an invalid channel id \
                         {channel_id}"
                    );
                    continue;
                };

                if !delta.new_pending_connections.is_empty() {
                    let pending_connections =
                        Arc::make_mut(&mut channel.subscribers.pending_connections);

                    if pending_connections.is_empty() {
                        // Request subscribe packets from the server
                        requested_subscriptions
                            .push((channel_id, channel.cached_subscribe_packets.is_some()));
                    }

                    pending_connections.extend(delta.new_pending_connections.into_iter());
                }

                if !delta.unsubscribed_connections.is_empty() {
                    let subscribed_connections =
                        Arc::make_mut(&mut channel.subscribers.subscribed_connections);
                    for stream in &delta.unsubscribed_connections {
                        // eprintln!("removing stream {stream} from channel {channel_id}");
                        assert!(subscribed_connections.remove(stream));
                    }

                    self.egress
                        .num_pending
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                    self.egress
                        .egress_sender
                        .send(EgressInstruction::Broadcast {
                            streams: delta.unsubscribed_connections.into(),
                            exclude: 0,
                            data: channel.unsubscribe_packets.clone(),
                        })
                        .unwrap();
                }
            }

            if !requested_subscriptions.is_empty() {
                self.pending_subscription_packets = true;

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
            warn!("spent {:?} syncing deltas", start.elapsed());
        } else {
            // error!("channel subscription delta channel closed");
        }
    }

    pub fn update_channel_positions(&mut self, packet: &ArchivedUpdateChannelPositions<'_>) {
        let start = std::time::Instant::now();

        // Update channel positions
        for update in packet.updates.get() {
            let channel_id = update.channel_id.into();
            let Some(channel) = self.channel_manager.channels.get_mut(&channel_id) else {
                error!(
                    "server sent UpdateChannelPositions with an invalid channel id {channel_id}"
                );
                continue;
            };

            let Ok(position) = rkyv::deserialize::<_, !>(&update.position);
            let position = I16Vec2::from(position);
            channel.subscribers.chunk_position = position;
        }

        if self.pending_subscription_deltas || self.pending_subscription_packets || self.behind {
            return;
        }

        self.pending_subscription_deltas = true;

        // TODO rename Player struct to smth more generic
        let mut channels = self
            .channel_manager
            .channels
            .iter()
            .map(|(&channel_id, channel)| Player {
                stream: channel_id as u64,
                chunk_position: channel.subscribers.chunk_position,
            })
            .collect::<Vec<_>>();

        self.subscription_sender
            .send((
                self.egress.player_registry.clone(),
                std::mem::take(&mut self.channels_delta),
                self.channel_manager.to_subscribers(),
                channels,
            ))
            .unwrap();

        // let channel_bvh = Bvh::build(&mut channels, ());
        // let mut requested_subscriptions = Vec::new();
        // let mut num_sub = 0;
        // let mut num_unsub = 0;
        //
        // let mut total_psub = std::time::Duration::from_secs(0);
        // let mut total_sub = std::time::Duration::from_secs(0);
        // let mut total_unsub = std::time::Duration::from_secs(0);
        // let mut total_request_subscribes = std::time::Duration::from_secs(0);
        //
        // for (&stream, player) in Arc::make_mut(&mut self.egress.player_registry) {
        // let player = player.get_mut();
        // let mut channels_below_unsubscribe_radius = RoaringBitmap::new();
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
        // /                for &channel_id in channel_ids {
        // /                    // TODO store u32s directly
        // /                    let channel_id = channel_id as u32;
        // /
        // /                    let Some(channel) = self.channel_manager.channels.get_mut(&channel_id) else {
        // /                        error!("bvh contains invalid channel");
        // /                        continue;
        // /                    };
        // /
        // /                    // TODO shoild this be a separate bvh query?
        // /                    let within_subscribe_radius = (channel.chunk_position - player.position).abs().max_element() <= SUBSCRIBE_RADIUS;
        // /
        // /                    if within_subscribe_radius {
        // /                        channels_below_subscribe_radius.insert(channel_id);
        // /                    }
        // /                }
        //            }
        //
        // let (_, channel_ids) = channel_bvh.inner();
        //
        // channels_below_unsubscribe_radius.extend(
        // channel_bvh
        // .get_in(Aabb::new(
        // player.position - I16Vec2::splat(UNSUBSCRIBE_RADIUS),
        // player.position + I16Vec2::splat(UNSUBSCRIBE_RADIUS),
        // ))
        // .into_iter()
        // .flat_map(|slice| {
        // let start = slice.start as usize;
        // let end = slice.end as usize;
        //
        // let channel_ids = &channel_ids[start..end];
        //
        // channel_ids.iter().map(|id| *id as u32)
        // }),
        // );
        // let channels_below_subscribe_radius = &channels_below_unsubscribe_radius;
        //
        // for channel_id in channels_below_subscribe_radius - &player.subscribed_channels {
        // let start3 = std::time::Instant::now();
        // This channel is below the subscribe radius but is not currently subscribed. Subscribe to the channel
        // let Some(channel) = self.channel_manager.channels.get_mut(&channel_id) else {
        // error!("bvh contains invalid channel");
        // continue;
        // };
        // if channel.subscribers.pending_connections.is_empty() {
        // Request subscribe packets from the server
        // requested_subscriptions
        // .push((channel_id, channel.cached_subscribe_packets.is_some()));
        // }
        // total_psub += start3.elapsed();
        // let new = Arc::make_mut(&mut channel.subscribers.pending_connections).insert(stream);
        // total_sub += start3.elapsed();
        // if new {
        // num_sub += 1;
        // }
        // }
        //
        // for channel_to_unsubscribe in
        // &player.subscribed_channels - channels_below_unsubscribe_radius
        // {
        // let start3 = std::time::Instant::now();
        // let Some(channel) = self
        // .channel_manager
        // .channels
        // .get_mut(&channel_to_unsubscribe)
        // else {
        // error!("could not find channel to unsubscribe");
        // continue;
        // };
        //
        // num_unsub += 1;
        //
        // TODO removing one at a time if probabbly inefficient
        // player.subscribed_channels.remove(channel_to_unsubscribe);
        // let was_subscribed =
        // Arc::make_mut(&mut channel.subscribers.subscribed_connections).remove(&stream);
        // assert!(was_subscribed);
        // self.egress
        // .egress_sender
        // .send(EgressInstruction::Unicast {
        // stream,
        // data: channel.unsubscribe_packets.clone(),
        // })
        // .unwrap();
        // total_unsub += start3.elapsed();
        // }
        // }
        //
        // if !requested_subscriptions.is_empty() {
        // let start3 = std::time::Instant::now();
        // let request = rkyv::to_bytes::<rkyv::rancor::Error>(
        // &hyperion_proto::ProxyToServerMessage::RequestSubscribeChannelPackets(
        // hyperion_proto::RequestSubscribeChannelPackets {
        // channels: &requested_subscriptions,
        // },
        // ),
        // )
        // .unwrap();
        // let server_sender = self.egress.server_sender.clone();
        // tokio::spawn(async move {
        // if let Err(e) = server_sender.send(request).await {
        // error!("failed to send request subscribe channel packets: {e}");
        // }
        // });
        // total_request_subscribes += start3.elapsed();
        // }
        // tracing::warn!(
        // "updated channel positions in {:?} ({total_psub:?} psub {total_sub:?} sub \
        // {total_unsub:?} unsub {total_request_subscribes:?} request subscribes) - {num_sub} new subscriptions and \
        // {num_unsub} unsubscribed",
        // start.elapsed()
        // );
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
    //                    if channel.subscribers.subscribed_connections.contains(&stream) {
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
    //                    if !channel.subscribers.subscribed_connections.contains(&stream) {
    //                        // ... but it is not currently subscribed
    //
    //                        if channel.subscribers.pending_connections.is_empty() {
    //                            // Request subscribe packets from the server
    //                            requested_subscriptions.push((
    //                                    channel_id,
    //                                    channel.cached_subscribe_packets.is_some(),
    //                            ));
    //                        }
    //                        channel.subscribers.pending_connections.insert(stream);
    //                    }
    //                }
    //            }
    //
    //            if channel.subscribers.subscribed_connections.len() != should_remain_subscribed.len() {
    //                channel.subscribers.subscribed_connections.retain(|stream| {
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
        let start = std::time::Instant::now();

        let ty = match &message {
            ArchivedServerToProxyMessage::UpdatePlayerPositions(_) => {
                ServerToProxyMessageType::UpdatePlayerPositions
            }
            ArchivedServerToProxyMessage::AddChannel(_) => ServerToProxyMessageType::AddChannel,
            ArchivedServerToProxyMessage::UpdateChannelPositions(_) => {
                ServerToProxyMessageType::UpdateChannelPositions
            }
            ArchivedServerToProxyMessage::RemoveChannel(_) => {
                ServerToProxyMessageType::RemoveChannel
            }
            ArchivedServerToProxyMessage::SubscribeChannelsPackets(_) => {
                ServerToProxyMessageType::SubscribeChannelsPackets
            }
            ArchivedServerToProxyMessage::BroadcastGlobal(_) => {
                ServerToProxyMessageType::BroadcastGlobal
            }
            ArchivedServerToProxyMessage::BroadcastLocal(_) => {
                ServerToProxyMessageType::BroadcastLocal
            }
            ArchivedServerToProxyMessage::BroadcastChannel(_) => {
                ServerToProxyMessageType::BroadcastChannel
            }
            ArchivedServerToProxyMessage::Unicast(_) => ServerToProxyMessageType::Unicast,
            ArchivedServerToProxyMessage::SetReceiveBroadcasts(_) => {
                ServerToProxyMessageType::SetReceiveBroadcasts
            }
            ArchivedServerToProxyMessage::Shutdown(_) => ServerToProxyMessageType::Shutdown,
            ArchivedServerToProxyMessage::CurrentTime(_) => ServerToProxyMessageType::CurrentTime,
        };

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

                    let Some(player) =
                        Arc::make_mut(&mut self.egress.player_registry).get_mut(&stream)
                    else {
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
                            subscribers: ChannelSubscribers {
                                pending_connections: Arc::default(),
                                subscribed_connections: Arc::default(),
                                chunk_position: position.into(),
                            },
                            cached_subscribe_packets: None,
                            unsubscribe_packets: Bytes::from(unsubscribe_packets),
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
                self.egress.execute(EgressInstruction::Broadcast {
                    streams: channel.subscribers.subscribed_connections.into(),
                    exclude: 0,
                    data: channel.unsubscribe_packets,
                });

                self.channels_delta
                    .removed_channels
                    .insert(packet.channel_id.into());
            }
            ArchivedServerToProxyMessage::SubscribeChannelsPackets(packet) => {
                if !self.pending_subscription_packets {
                    error!("server unexpectedly sent subscribe channel packets");
                }
                self.pending_subscription_packets = false;

                let start5 = std::time::Instant::now();
                let aaa = self.egress.block_until_flushed();
                self.block_time += start5.elapsed();
                eprintln!("took {:?} to flush {aaa} packets", start.elapsed());
                for update in packet.channels.iter() {
                    let exclude = u64::from(update.exclude);
                    let channel_id = update.channel_id.into();
                    let Some(channel) = self.channel_manager.channels.get_mut(&channel_id) else {
                        error!(
                            "server sent SubscribeChannelPackets for a channel that does not exist"
                        );
                        return;
                    };

                    let pending_connections =
                        std::mem::take(&mut channel.subscribers.pending_connections);

                    if pending_connections.is_empty() {
                        warn!(
                            "server sent SubscribeChannelPackets but there are no pending \
                             subscribers"
                        );
                        return;
                    }

                    // TODO no box

                    let data = if let Some(data) =
                        rkyv::deserialize::<Option<Bytes>, rkyv::rancor::Error>(&update.data)
                            .unwrap()
                    {
                        channel.cached_subscribe_packets = update.cache.then(|| data.clone());
                        data
                    } else {
                        let Some(bytes) = channel.cached_subscribe_packets.clone() else {
                            error!(
                                "server sent SubscribeChannelPackets without subscribe updates, \
                                 but the subscribe updates are not cached on the proxy"
                            );
                            return;
                        };
                        if !update.cache {
                            channel.cached_subscribe_packets = None;
                        }
                        bytes
                    };

                    // TODO use max pending bytes
                    // for stream in pending_connections.iter() {
                    // eprintln!("adding stream {stream} to channel {channel_id}");
                    // }

                    // TODO the clone here is clearly somewhat expensive
                    let start4 = std::time::Instant::now();
                    let foo = Arc::get_mut(&mut channel.subscribers.subscribed_connections)
                        .unwrap_or_else(|| {
                            // Ideally this should never be reached because cloning this HashSet has been
                            // shown to be expensive. This is why the egress instruction queue is
                            // flushed beforehand so we can ensure that there are no other strong
                            // references.
                            //                        error!(
                            //                            "could not get subscribed connections mutably due to {} strong references, this is unexpected and will result in additional memory copies and allocations",
                            //                            Arc::strong_count(&channel.subscribers.subscribed_connections)
                            //                        );
                            todo!()
                            // Arc::make_mut(&mut channel.subscribers.subscribed_connections)
                        });

                    self.total_time_in_clone_subs += start4.elapsed();
                    let start3 = std::time::Instant::now();
                    foo.extend(pending_connections.iter().copied());
                    self.total_time_in_update_subs += start3.elapsed();
                    self.channels_delta
                        .subscribed_channels
                        .push((channel_id, pending_connections.clone()));
                    let start2 = std::time::Instant::now();
                    self.egress.execute(EgressInstruction::Broadcast {
                        streams: pending_connections.into(),
                        exclude,
                        data,
                    });

                    // self.egress.maybe_pending = false;

                    self.total_time_in_write += start2.elapsed();
                }
            }
            ArchivedServerToProxyMessage::BroadcastGlobal(packet) => {
                let data =
                    Bytes::from(rkyv::deserialize::<_, rkyv::rancor::Error>(&packet.data).unwrap());
                let Ok(exclude) = rkyv::deserialize::<u64, !>(&packet.exclude);

                let start2 = std::time::Instant::now();
                // TODO handle can_receive_broadcasts
                self.egress
                    .execute(EgressInstruction::BroadcastGlobal { exclude, data });
                self.total_time_in_write += start2.elapsed();
            }
            ArchivedServerToProxyMessage::BroadcastLocal(packet) => {
                let Ok(center_x) = rkyv::deserialize::<i16, !>(&packet.center.x);
                let Ok(center_z) = rkyv::deserialize::<i16, !>(&packet.center.z);
                let Ok(player_id_to_exclude) = rkyv::deserialize::<u64, !>(&packet.exclude);
                let data =
                    Bytes::from(rkyv::deserialize::<_, rkyv::rancor::Error>(&packet.data).unwrap());

                let start3 = std::time::Instant::now();
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

                let targets = slices
                    .into_iter()
                    .flat_map(|slice| {
                        let start = slice.start as usize;
                        let end = slice.end as usize;

                        streams[start..end].iter()
                    })
                    .copied()
                    .collect::<FxHashSet<u64>>();

                self.total_time_in_sub += start3.elapsed();

                let start2 = std::time::Instant::now();
                self.egress.execute(EgressInstruction::Broadcast {
                    streams: targets.into(),
                    exclude: player_id_to_exclude,
                    data,
                });

                self.total_time_in_write += start2.elapsed();
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

                let start2 = std::time::Instant::now();
                self.egress.execute(EgressInstruction::Broadcast {
                    streams: channel.subscribers.subscribed_connections.clone().into(),
                    exclude,
                    data,
                });
                self.total_time_in_write += start2.elapsed();
                //                for &stream in &channel.subscribers.subscribed_connections {
                //                    if stream == exclude {
                //                        continue;
                //                    }
                //
                //                    self.egress.unicast(stream, data.clone());
                //                }
            }
            ArchivedServerToProxyMessage::Unicast(unicast) => {
                let data = rkyv::deserialize::<_, rkyv::rancor::Error>(&unicast.data).unwrap();
                let start2 = std::time::Instant::now();
                self.egress.execute(EgressInstruction::Unicast {
                    stream: unicast.stream.into(),
                    data: Bytes::from(data),
                });
                self.total_time_in_write += start2.elapsed();
            }
            ArchivedServerToProxyMessage::SetReceiveBroadcasts(pkt) => {
                self.egress.handle_set_receive_broadcasts(pkt);
            }
            ArchivedServerToProxyMessage::Shutdown(pkt) => {
                let Ok(stream) = rkyv::deserialize::<u64, !>(&pkt.stream);
                self.egress
                    .execute(EgressInstruction::ShutdownPlayer { stream });
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
                self.behind = delay >= 2;
                if delay > 5 || self.ticks_elapsed_since_yield > 5 {
                    warn!(
                        "proxy is running too slow - it is {delay} seconds behind on processing \
                         server to proxy messages and {} game ticks have elapsed since the server \
                         to proxy task has yielded execution. (write {:?} / sub {:?} / clone {:?} \
                         / update {:?} / block time {:?} / total {:?}), {:#?}",
                        self.ticks_elapsed_since_yield,
                        self.total_time_in_write,
                        self.total_time_in_sub,
                        self.total_time_in_clone_subs,
                        self.total_time_in_update_subs,
                        self.block_time,
                        self.total_time,
                        &self.timings,
                    );
                }
                self.ticks_elapsed_since_yield += 1;
            }
        }
        self.timings[ty] += start.elapsed();
        self.total_time += start.elapsed();
    }

    fn subscription_worker(
        mut subscription_receiver: kanal::Receiver<(
            Arc<FxHashMap<u64, CloneSyncUnsafeCell<PlayerHandle>>>,
            ChannelsDelta,
            FxHashMap<u32, ChannelSubscribers>,
            Vec<Player>,
        )>,
        delta_sender: tokio::sync::mpsc::Sender<FxHashMap<u32, ChannelSubscriptionDelta>>,
    ) {
        #[derive(Default)]
        struct PlayerSubscriptions {
            pending_subscribe: RoaringBitmap,
            subscribed: RoaringBitmap,
        }

        let mut player_subscriptions_map: FxHashMap<u64, PlayerSubscriptions> =
            FxHashMap::default();

        while let Ok((player_registry, channels_delta, channels, mut channel_bvh_data)) =
            subscription_receiver.recv()
        {
            let start = std::time::Instant::now();

            let mut deltas: FxHashMap<u32, ChannelSubscriptionDelta> = FxHashMap::default();

            for (channel_id, subscribed_players) in channels_delta.subscribed_channels {
                // If the channel has been removed since the players were subscribed, do nothing
                if channels_delta.removed_channels.contains(channel_id) {
                    continue;
                }

                for stream in subscribed_players.iter() {
                    let Some(player_subscriptions) = player_subscriptions_map.get_mut(stream)
                    else {
                        error!("could not find player subscriptions for {stream}");
                        continue;
                    };

                    // eprintln!("update bitmap adding stream {stream} to channel {channel_id}");
                    assert!(player_subscriptions.subscribed.insert(channel_id));
                    assert!(player_subscriptions.pending_subscribe.remove(channel_id));
                }
            }

            if !channels_delta.removed_channels.is_empty() {
                for player_subscriptions in player_subscriptions_map.values_mut() {
                    player_subscriptions.pending_subscribe -= &channels_delta.removed_channels;
                    player_subscriptions.subscribed -= &channels_delta.removed_channels;
                }
            }

            // TODO calculate channel bvh data here
            let channel_bvh = Bvh::build(&mut channel_bvh_data, ());
            let mut num_sub = 0;
            let mut num_unsub = 0;

            let mut total_psub = std::time::Duration::from_secs(0);
            let mut total_sub = std::time::Duration::from_secs(0);
            let mut total_unsub = std::time::Duration::from_secs(0);
            let mut total_request_subscribes = std::time::Duration::from_secs(0);

            for (&stream, player) in player_registry.iter() {
                // SAFETY: we only get a mutable reference with a unique Arc reference so there are
                // no mut references righ tnow
                let player = unsafe { &*player.get() };

                let mut player_subscriptions = player_subscriptions_map.entry(stream).or_default();

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

                channels_below_unsubscribe_radius.extend(
                    channel_bvh
                        .get_in(Aabb::new(
                            player.position - I16Vec2::splat(UNSUBSCRIBE_RADIUS),
                            player.position + I16Vec2::splat(UNSUBSCRIBE_RADIUS),
                        ))
                        .into_iter()
                        .flat_map(|slice| {
                            let start = slice.start as usize;
                            let end = slice.end as usize;

                            let channel_ids = &channel_ids[start..end];

                            channel_ids.iter().map(|id| *id as u32)
                        }),
                );
                let channels_below_subscribe_radius = &channels_below_unsubscribe_radius;

                let channels_to_subscribe = channels_below_subscribe_radius
                    - &player_subscriptions.subscribed
                    - &player_subscriptions.pending_subscribe;
                for channel_id in &channels_to_subscribe {
                    let start3 = std::time::Instant::now();
                    // This channel is below the subscribe radius but is not currently subscribed. Subscribe to the channel
                    deltas
                        .entry(channel_id)
                        .or_default()
                        .new_pending_connections
                        .push(stream);
                }
                player_subscriptions.pending_subscribe |= channels_to_subscribe;

                let channels_to_unsubscribe =
                    &player_subscriptions.subscribed - channels_below_unsubscribe_radius;
                for channel_to_unsubscribe in &channels_to_unsubscribe {
                    let start3 = std::time::Instant::now();

                    num_unsub += 1;

                    deltas
                        .entry(channel_to_unsubscribe)
                        .or_default()
                        .unsubscribed_connections
                        .insert(stream);

                    // eprintln!("requesting removing stream {stream} to channel {channel_to_unsubscribe}");

                    // assert!(player_subscribed_channels.remove(channel_to_unsubscribe));
                    total_unsub += start3.elapsed();
                }
                player_subscriptions.subscribed -= channels_to_unsubscribe;
            }

            tracing::warn!(
                "updated channel positions in {:?} ({total_psub:?} psub {total_sub:?} sub \
                 {total_unsub:?} unsub {total_request_subscribes:?} request subscribes) - {num_sub} new subscriptions and \
                 {num_unsub} unsubscribed",
                start.elapsed()
            );
            // TODO drop player reigstry arc to avoid clone from make_mut
            // TODO dont use tokio channels maybe
            delta_sender.blocking_send(deltas).unwrap();
        }
    }
}
