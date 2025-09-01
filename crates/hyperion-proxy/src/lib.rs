#![feature(maybe_uninit_slice)]
#![feature(allocator_api)]
#![feature(let_chains)]
#![feature(never_type)]
#![feature(stmt_expr_attributes)]
#![feature(gen_blocks)]
#![allow(
    clippy::redundant_pub_crate,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::missing_errors_doc,
    clippy::module_name_repetitions,
    clippy::missing_panics_doc,
    clippy::module_inception,
    clippy::future_not_send
)]

use std::{
    fmt::Debug,
    path::Path,
    sync::{Arc, atomic::AtomicUsize},
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use anyhow::Context as _;
use colored::Colorize;
use hyperion_proto::ArchivedServerToProxyMessage;
use rustls::{RootCertStore, client::ClientConfig};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName, pem::PemObject};
use pin_project_lite::pin_project;
use tokio::{
    io::{AsyncRead, AsyncReadExt, BufReader},
    net::{TcpStream, ToSocketAddrs},
    sync::Notify,
};
use tokio_rustls::TlsConnector;
use tokio_util::net::Listener;
use tracing::{Instrument, debug, error, info, info_span, instrument, trace, warn};

use crate::{
    cache::BufferedEgress, data::PlayerHandle, egress::Egress, player::initiate_player_connection,
    server_sender::launch_server_writer,
};

/// 4 KiB
const DEFAULT_BUFFER_SIZE: usize = 4 * 1024;

/// 8 KiB
const DEFAULT_FRAGMENT_SIZE: usize = 8 * 1024;

pub mod cache;
pub mod data;
pub mod egress;
pub mod player;
pub mod server_sender;
pub mod util;

#[tracing::instrument(level = "trace", skip_all)]
async fn connect(addr: impl ToSocketAddrs + Debug + Clone) -> TcpStream {
    loop {
        if let Ok(stream) = TcpStream::connect(addr.clone()).await {
            return stream;
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[derive(Debug, PartialEq)]
enum ShutdownType {
    Reconnect,
    Full,
}

enum PlayerConnectionMessage {
    AddPlayer {
        stream: u64,
        handle: PlayerHandle
    },
    RemovePlayer {
        stream: u64,
    }
}

#[tracing::instrument(level = "trace", skip_all)]
pub async fn run_proxy(
    mut listener: impl HyperionListener,
    server_addr: impl ToSocketAddrs + Debug + Clone,
    mut server_name: String,
    root_ca_cert_path: &Path,
    proxy_cert_path: &Path,
    proxy_private_key_path: &Path,
) -> anyhow::Result<()> {
    // Remove port
    let Some(port_index) = server_name.rfind(':') else {
        anyhow::bail!("server name is missing port");
    };
    server_name.truncate(port_index);

    let server_name = ServerName::try_from(server_name).context("failed to parse server name")?;

    let root_ca_cert = CertificateDer::from_pem_file(root_ca_cert_path)
        .context("failed to load root certificate authority certificate")?;
    let proxy_cert = CertificateDer::from_pem_file(proxy_cert_path)
        .context("failed to load proxy certificate")?;

    let root_cert_store = Arc::new(RootCertStore {
        roots: vec![
            webpki::anchor_from_trusted_cert(&root_ca_cert)
                .context("failed to create trust anchor")?
                .to_owned(),
        ],
    });

    let cert_chain = vec![proxy_cert, root_ca_cert];
    let key_der = PrivateKeyDer::from_pem_file(proxy_private_key_path)
        .context("failed to load proxy private key")?;

    let config = Arc::new(
        ClientConfig::builder()
            .with_root_certificates(root_cert_store)
            .with_client_auth_cert(cert_chain, key_der)
            .context("failed to create tls client config")?,
    );

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(None);

    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("failed to register SIGTERM handler")?;

    #[cfg(unix)]
    let mut sigquit = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::quit())
        .context("failed to register SIGQUIT handler")?;

    #[cfg(unix)]
    tokio::spawn({
        let shutdown_tx = shutdown_tx.clone();
        async move {
            tokio::select! {
                _ = sigterm.recv() => {
                    warn!("SIGTERM received, shutting down");
                    shutdown_tx.send(Some(ShutdownType::Full)).unwrap();
                }
                _ = sigquit.recv() => {
                    warn!("SIGQUIT received, shutting down");
                    shutdown_tx.send(Some(ShutdownType::Full)).unwrap();
                }
            }
        }
    });

    loop {
        let mut shutdown_rx2 = shutdown_rx.clone();

        if *shutdown_rx2.borrow() == Some(ShutdownType::Full) {
            break Ok(());
        }

        tokio::select! {
            _ = shutdown_rx2.wait_for(|value| *value == Some(ShutdownType::Full)) => {
                warn!("Received shutdown signal, exiting proxy loop");
                break Ok(());
            }
            () = async {

                // clear shutdown channel
                shutdown_tx.send(None).unwrap();

                let binding_help = "~ Make sure the event server is running".dimmed();
                info!("⏳ Binding to server... {binding_help}");

                let server_socket = connect(server_addr.clone()).await;
                server_socket.set_nodelay(true).unwrap();

                if let Err(e) = connect_to_server_and_run_proxy(&mut listener, server_socket, server_name.clone(), config.clone(), shutdown_rx.clone(), shutdown_tx.clone()).await {
                    error!("Error connecting to server: {e:?}");
                }


            } => {}
        }
    }
}

#[tracing::instrument(level = "trace", skip_all)]
async fn connect_to_server_and_run_proxy(
    listener: &mut impl HyperionListener,
    server_socket: TcpStream,
    server_name: ServerName<'static>,
    config: Arc<ClientConfig>,
    shutdown_rx: tokio::sync::watch::Receiver<Option<ShutdownType>>,
    shutdown_tx: tokio::sync::watch::Sender<Option<ShutdownType>>,
) -> anyhow::Result<()> {
    info!("🔗 Connected to server, accepting connections");

    let connector = TlsConnector::from(config);
    let server_stream = connector
        .connect(server_name, server_socket)
        .await
        .context("failed to connect to game server")?;

    let (server_read, server_write) = tokio::io::split(server_stream);
    let server_sender = launch_server_writer(server_write);

    let egress = Egress::new(server_sender.clone());

    let egress = BufferedEgress::new(egress);

    let mut handler = IngressHandler::new(BufReader::new(server_read), egress);

    let (connection_tx, mut connection_rx) = tokio::sync::mpsc::unbounded_channel();

    tokio::spawn({
        let mut shutdown_rx = shutdown_rx.clone();
        async move {
                loop {
                    tokio::select! {
                    _ = shutdown_rx.wait_for(Option::is_some) => return,
                    // TODO handle_next is not cancel safe
                    result = handler.handle_next() => {
                        match result {
                            Ok(()) => {},
                            Err(e) => {
                                error!(
                                    "Error reading next packet: {e:?}. Are you connected to a valid \
                                     hyperion server? If you are connected to a vanilla server, \
                                     hyperion-proxy will not work."
                                );
                                break;
                            }
                        }
                    },
                    message = connection_rx.recv() => {
                        match message {
                            Some(message) => handler.handle_player_connection_message(message),
                            None => {
                                error!("Error reading from player connection message channel: channel closed");
                                break;
                            }
                        }
                    },
                }
                }

                debug!("Sending shutdown to all players");

                shutdown_tx.send(Some(ShutdownType::Reconnect)).unwrap();
            }
                .instrument(info_span!("server_reader_loop"))
    });

    // 0 is reserved for "None" value
    let mut player_id_on = 1;

    loop {
        let mut shutdown_rx = shutdown_rx.clone();
        let socket = tokio::select! {
            _ = shutdown_rx.wait_for(Option::is_some) => {
                return Ok(())
            }
            Ok((socket, addr)) = listener.accept() => {
                info!("New client connection from {addr:?}");
                socket
            }
        };

        let total_bytes_pending_send = Arc::new(AtomicUsize::new(0));

        let notify = Arc::new(Notify::new());
        let tx = packet_channel::raw::Sender::new(DEFAULT_FRAGMENT_SIZE);
        let rx = tx.receiver();

        // todo: some SlotMap like thing
        debug!("got player with id {player_id_on:?}");

        if let Err(e) = connection_tx.send(PlayerConnectionMessage::AddPlayer { stream: player_id_on, handle: PlayerHandle::new(tx, notify.clone(), total_bytes_pending_send.clone()) }) {
            anyhow::bail!("failed to send player connection message: {e:?}");
        }

        initiate_player_connection(
            socket,
            shutdown_rx.clone(),
            player_id_on,
            rx,
            notify,
            total_bytes_pending_send,
            server_sender.clone(),
            connection_tx.clone(),
        );

        player_id_on += 1;
    }
}

struct IngressHandler<R> {
    server_read: BufReader<R>,
    buffer: Vec<u8>,
    egress: BufferedEgress,
}

impl<R> Debug for IngressHandler<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerReader").finish()
    }
}

impl<R> IngressHandler<R>
where
    R: AsyncRead + Unpin,
{
    pub fn new(server_read: BufReader<R>, egress: BufferedEgress) -> Self {
        Self {
            server_read,
            egress,
            buffer: Vec::with_capacity(DEFAULT_BUFFER_SIZE),
        }
    }

    pub async fn handle_next(&mut self) -> anyhow::Result<()> {
        let (result, yielded) = FutureTracker::new(self.handle_next_inner()).await;
        if yielded {
            self.egress.ticks_elapsed_since_yield = 0;
        }
        result
    }

    async fn handle_next_inner(&mut self) -> anyhow::Result<()> {
        let len = self.read_len().await?;
        let len = usize::try_from(len).context("Failed to convert len to usize")?;

        if len > 1_000_000 {
            dbg!(len);
        }
        assert!(len <= 1_000_000);

        trace!("Received packet of length {len}");

        self.handle_next_server_packet(len).await
    }

    #[instrument(level = "trace")]
    async fn read_len(&mut self) -> anyhow::Result<u64> {
        self.server_read
            .read_u64()
            .await
            .context("Failed to read int")
    }

    #[instrument(level = "trace")]
    async fn handle_next_server_packet(&mut self, len: usize) -> anyhow::Result<()> {
        // [A]
        if self.buffer.len() < len {
            self.buffer.resize(len, 0);
        }

        #[expect(
            clippy::indexing_slicing,
            reason = "we already verified in [A] that length of buffer is at least {len}"
        )]
        let slice = &mut self.buffer[..len];
        self.server_read.read_exact(slice).await?;

        let result = unsafe { rkyv::access_unchecked::<ArchivedServerToProxyMessage<'_>>(slice) };

        self.egress.handle_packet(result);

        Ok(())
    }

    fn handle_player_connection_message(&mut self, message: PlayerConnectionMessage) {
        match message {
            PlayerConnectionMessage::AddPlayer { stream, handle } => {
                if self.egress.egress.player_registry.insert(stream, handle).is_some() {
                    error!("player connection message inserted another player with id {stream:?} but a player with the same id is already connected");
                }
            },
            PlayerConnectionMessage::RemovePlayer { stream } => {
                if !self.egress.egress.player_registry.remove(&stream).is_some() {
                    error!("player connection message attempted to remove {stream:?} which does not exist");
                }
            },
        }
    }
}

pin_project! {
    /// Tracks whether a future ever returns `Poll::Pending`
    struct FutureTracker<F> {
        #[pin]
        future: F,
        yielded: bool
    }
}

impl<F> FutureTracker<F> {
    fn new(future: F) -> Self {
        Self {
            future,
            yielded: false,
        }
    }
}

impl<F> Future for FutureTracker<F> where F: Future {
    type Output = (F::Output, bool);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match this.future.poll(cx) {
            Poll::Ready(value) => Poll::Ready((value, *this.yielded)),
            Poll::Pending => {
                *this.yielded = true;
                Poll::Pending
            }
        }
    }
}

trait HyperionListener: Listener<Io: Send, Addr: Debug> + 'static {}

impl<L: Listener<Io: Send, Addr: Debug> + 'static> HyperionListener for L {}
