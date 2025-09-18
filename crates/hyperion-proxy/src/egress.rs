use std::{
    cell::SyncUnsafeCell,
    ops::Deref,
    sync::{
        Arc,
        atomic::{self, AtomicUsize},
    },
};

use bytes::Bytes;
use hyperion_proto::{ArchivedSetReceiveBroadcasts, ArchivedShutdown};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use tokio::sync::Notify;
use tracing::{error, instrument, warn};

use crate::{CloneSyncUnsafeCell, data::PlayerHandle, server_sender::ServerSender};

/// Maximum number of bytes that will be copied to the channel instead
/// of being sent as shared bytes
const MAX_SHARED_COPY: usize = 2048;

pub enum MaybeShared<T> {
    Shared(Arc<T>),
    Owned(T),
}

impl<T> From<Arc<T>> for MaybeShared<T> {
    fn from(value: Arc<T>) -> Self {
        Self::Shared(value)
    }
}

impl<T> From<T> for MaybeShared<T> {
    fn from(value: T) -> Self {
        Self::Owned(value)
    }
}

impl<T> Deref for MaybeShared<T> {
    type Target = T;

    fn deref(&self) -> &T {
        match self {
            Self::Shared(value) => value,
            Self::Owned(value) => value,
        }
    }
}

pub enum EgressInstruction {
    AddPlayer {
        stream: u64,
        writer: packet_channel::raw::Sender,
        notify: Arc<Notify>,
        total_bytes_pending_send: Arc<AtomicUsize>,
    },
    ShutdownPlayer {
        stream: u64,
    },
    RemovePlayer {
        stream: u64,
    },
    Unicast {
        stream: u64,
        data: Bytes,
    },
    Broadcast {
        streams: MaybeShared<FxHashSet<u64>>,
        exclude: u64,
        data: Bytes,
    },
    BroadcastGlobal {
        exclude: u64,
        data: Bytes,
    },
    Ping {
        sender: oneshot::Sender<()>,
    }
}

pub struct Egress {
    // TODO remove SyncUnsafeCell
    pub(crate) player_registry: Arc<FxHashMap<u64, CloneSyncUnsafeCell<PlayerHandle>>>,
    pub(crate) server_sender: ServerSender,
    pub(crate) egress_sender: kanal::Sender<EgressInstruction>,
    pub(crate) maybe_pending: bool,
}

impl Egress {
    #[must_use]
    pub fn new(server_sender: ServerSender) -> Self {
        let (egress_sender, egress_receiver) = kanal::unbounded();

        std::thread::Builder::new()
            .name("egress_writer".to_string())
            .spawn(move || {
                Self::writer(egress_receiver);
            })
            .unwrap();

        Self {
            player_registry: Arc::new(FxHashMap::default()),
            server_sender,
            egress_sender,
            maybe_pending: false,
        }
    }

    pub fn add_player(
        &mut self,
        stream: u64,
        writer: packet_channel::raw::Sender,
        notify: Arc<Notify>,
        total_bytes_pending_send: Arc<AtomicUsize>,
    ) {
        self.execute(EgressInstruction::AddPlayer {
            stream,
            writer,
            notify,
            total_bytes_pending_send: total_bytes_pending_send.clone(),
        });

        if Arc::make_mut(&mut self.player_registry)
            .insert(
                stream,
                CloneSyncUnsafeCell::new(PlayerHandle::new(total_bytes_pending_send)),
            )
            .is_some()
        {
            error!(
                "player connection message inserted another player with id {stream:?} but a \
                 player with the same id is already connected"
            );
        }
    }

    pub fn remove_player(&mut self, stream: u64) {
        self.execute(EgressInstruction::RemovePlayer { stream });

        if Arc::make_mut(&mut self.player_registry)
            .remove(&stream)
            .is_none()
        {
            error!("player connection message attempted to remove invalid stream id {stream:?}");
        }
    }

    pub fn execute(&mut self, instruction: EgressInstruction) {
        self.maybe_pending = true;
        self.egress_sender.send(instruction).unwrap();
    }

    pub fn block_until_flushed(&mut self) {
        if !self.maybe_pending {
            return;
        }

        let (sender, receiver) = oneshot::channel();
        self.execute(EgressInstruction::Ping { sender });
        let _ = receiver.recv();
        self.maybe_pending = false;
    }

    #[instrument(skip_all)]
    pub fn handle_set_receive_broadcasts(&mut self, pkt: &ArchivedSetReceiveBroadcasts) {
        let Ok(stream) = rkyv::deserialize::<u64, !>(&pkt.stream);

        let Some(player) = Arc::make_mut(&mut self.player_registry).get_mut(&stream) else {
            error!("Player not found for stream {stream:?}");
            return;
        };

        player.get_mut().enable_receive_broadcasts();
    }

    fn writer(receiver: kanal::Receiver<EgressInstruction>) {
        let mut writers: FxHashMap<
            u64,
            SyncUnsafeCell<
                Option<(
                    packet_channel::raw::Sender,
                    Arc<Notify>,
                    Arc<atomic::AtomicUsize>,
                )>,
            >,
        > = FxHashMap::default();
        while let Ok(instruction) = receiver.recv() {
            match instruction {
                EgressInstruction::AddPlayer {
                    stream,
                    writer,
                    notify,
                    total_bytes_pending_send,
                } => {
                    let already_exists = writers
                        .insert(
                            stream,
                            SyncUnsafeCell::new(Some((writer, notify, total_bytes_pending_send))),
                        )
                        .is_some();
                    if already_exists {
                        error!(
                            "EgressInstruction::AddPlayer used stream id of existing player \
                             {stream}"
                        );
                    }
                }
                EgressInstruction::ShutdownPlayer { stream } => {
                    let Some(player) = writers.get_mut(&stream) else {
                        error!(
                            "EgressInstruction::ShutdownPlayer used invalid stream id {stream:?}"
                        );
                        return;
                    };

                    *player.get_mut() = None;
                }
                EgressInstruction::RemovePlayer { stream } => {
                    let exists = writers.remove(&stream).is_some();
                    if exists {
                        error!("EgressInstruction::RemovePlayer used invalid stream id {stream}");
                    }
                }
                EgressInstruction::Unicast { stream, data } => {
                    let Some(player) = writers.get_mut(&stream) else {
                        warn!("EgressInstruction::Unicast used invalid stream id {stream:?}");
                        return;
                    };

                    let Some((sender, notify, total_bytes_pending_send)) = player.get_mut() else {
                        warn!(
                            "EgressInstruction::Unicast used stream id {stream:?} which has \
                             already been shut down"
                        );
                        return;
                    };

                    total_bytes_pending_send.fetch_add(data.len(), atomic::Ordering::Relaxed);
                    sender.send_maybe_shared(data, MAX_SHARED_COPY);
                    notify.notify_waiters();
                }
                EgressInstruction::Broadcast {
                    streams,
                    exclude,
                    data,
                } => {
                    (*streams).par_iter().for_each(|&stream| {
                        if stream == exclude {
                            return;
                        }

                        let Some(player) = writers.get(&stream) else {
                            warn!("EgressInstruction::Broadcast used invalid stream id {stream:?}");
                            return;
                        };

                        // SAFETY: Since streams is stored as a HashSet, there cannot be duplicate
                        // keys, so there cannot be any other code currently holding a reference to
                        // this player at the time.In addition, this will not clone the player
                        // while a mutable reference is held.
                        let player = unsafe { &mut *player.get() };

                        let Some((sender, notify, total_bytes_pending_send)) = player else {
                            warn!(
                                "EgressInstruction::Broadcast used stream id {stream:?} which has \
                                 already been shut down"
                            );
                            return;
                        };

                        total_bytes_pending_send.fetch_add(data.len(), atomic::Ordering::Relaxed);
                        sender.send_maybe_shared_ref(&data, MAX_SHARED_COPY);
                        notify.notify_waiters();
                    });
                }
                EgressInstruction::BroadcastGlobal { exclude, data } => {
                    writers.par_iter_mut().for_each(|(&stream, player)| {
                        if stream == exclude {
                            return;
                        }

                        let Some((sender, notify, total_bytes_pending_send)) = player.get_mut()
                        else {
                            return;
                        };

                        total_bytes_pending_send.fetch_add(data.len(), atomic::Ordering::Relaxed);
                        sender.send_maybe_shared_ref(&data, MAX_SHARED_COPY);
                        notify.notify_waiters();
                    });
                }
                EgressInstruction::Ping { sender } => {
                    let _result = sender.send(());
                }
            }
        }
    }
}
