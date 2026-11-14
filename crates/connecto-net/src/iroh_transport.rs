//! The iroh transport: QUIC, dialed by endpoint id, direct when hole punching
//! works and through the self-hosted relay when it does not.
//!
//! iroh is async. Everything async lives here, on one thread of its own that
//! runs a current-thread tokio runtime. The rest of the app sees the blocking
//! [`Connection`] and [`Stream`] traits and crossbeam channels (ADR 0002).
//!
//! No n0 default services are used: the endpoint is built from the `Minimal`
//! preset, so it contacts no n0 relay and no n0 DNS. The relay is the one in
//! [`IrohConfig`], with its access token when it has one.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded};
use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, PublicKey, RelayMap, RelayMode, RelayUrl, SecretKey};
use tokio::runtime::{Builder, Handle};
use tokio::sync::mpsc as tmpsc;

use crate::transport::{
    ChannelStream, Connection, EndpointId, NetError, PathKind, STREAM_QUEUE, Stream,
};

/// The application protocol name of a Connecto remote connection.
pub const ALPN: &[u8] = b"connecto/remote/1";

/// The protocol name of the temporary pairing endpoint. A different name from
/// [`ALPN`], so a pairing endpoint refuses every normal session at the TLS
/// handshake, and an ordinary endpoint refuses pairing.
pub const PAIR_ALPN: &[u8] = b"pair/1";

// `test-relay` switches off the relay's certificate check, so that a test can
// run a relay with a self-signed certificate. Nothing outside connecto-net's
// own tests can turn it on: it is reached only through the dev-dependency on
// this crate, and a shipped binary's feature graph never has it (checked by
// `cargo tree -e features -p connecto-app -p connecto-hook`, see ADR 0022).
// It is not refused in release builds, because the tests run in release too.
#[cfg(feature = "test-relay")]
static TRUST_ANY_RELAY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// From now on, every endpoint in this process accepts any relay certificate.
/// For tests that run a relay with a self-signed one; the feature is never on
/// in the app.
#[cfg(feature = "test-relay")]
pub fn trust_any_relay_certificate_for_tests() {
    TRUST_ANY_RELAY.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// The endpoint identity that goes with a secret key.
#[must_use]
pub fn endpoint_id_for_key(secret: &[u8; 32]) -> EndpointId {
    *SecretKey::from_bytes(secret).public().as_bytes()
}

/// Label for the value both ends derive from the TLS session.
const BINDING_LABEL: &[u8] = b"connecto-remote-binding-v1";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const CHUNK: usize = 64 * 1024;
/// How long a closing connection lets its streams drain.
const CLOSE_LINGER: Duration = Duration::from_millis(300);

/// How the endpoint is built.
#[derive(Clone)]
pub struct IrohConfig {
    /// This machine's endpoint secret.
    pub device_key: [u8; 32],
    /// The self-hosted relay, from settings. `None` runs with no relay at all:
    /// direct connections only.
    pub relay_url: Option<String>,
    /// The relay's shared access token, from the secret store.
    pub relay_token: Option<String>,
    /// A fixed local address to listen on; `None` lets the OS choose.
    pub bind: Option<SocketAddr>,
    /// Accept incoming connections. Off, the endpoint only dials, and a peer
    /// that dials it is refused during the TLS handshake (no ALPN).
    pub listen: bool,
    /// Use the relay only: no direct (UDP) paths at all. For checking a relay,
    /// since two endpoints on one machine would otherwise connect directly.
    pub relay_only: bool,
    /// The protocol this endpoint speaks: [`ALPN`], or [`PAIR_ALPN`].
    pub alpn: &'static [u8],
}

impl std::fmt::Debug for IrohConfig {
    /// The token and the key are never printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IrohConfig")
            .field("relay_url", &self.relay_url)
            .field("relay_token", &self.relay_token.as_ref().map(|_| "<set>"))
            .field("device_key", &"<hidden>")
            .field("bind", &self.bind)
            .field("listen", &self.listen)
            .field("relay_only", &self.relay_only)
            .field("alpn", &String::from_utf8_lossy(self.alpn))
            .finish()
    }
}

impl IrohConfig {
    /// A one-line description for a log, run through `connecto-redact` as well.
    #[must_use]
    pub fn describe(&self) -> String {
        let text = format!("{self:?}");
        let known: Vec<String> = self.relay_token.iter().cloned().collect();
        connecto_redact::redact(&text, &known)
            .map_or_else(|_| "<withheld>".to_owned(), |r| r.get().clone())
    }
}

/// How to reach a peer: its id, and any hints.
#[derive(Debug, Clone)]
pub struct PeerAddr {
    pub id: EndpointId,
    pub relay: Option<String>,
    pub direct: Vec<SocketAddr>,
}

fn net_err(err: impl std::fmt::Display) -> NetError {
    NetError::Transport(err.to_string())
}

fn public_key(id: &EndpointId) -> Result<PublicKey, NetError> {
    PublicKey::from_bytes(id).map_err(net_err)
}

/// Run `fut` on the network thread and wait for its result.
fn run<T: Send + 'static>(
    handle: &Handle,
    timeout: Duration,
    fut: impl std::future::Future<Output = T> + Send + 'static,
) -> Result<T, NetError> {
    let (tx, rx) = bounded(1);
    handle.spawn(async move {
        let _ = tx.send(fut.await);
    });
    rx.recv_timeout(timeout)
        .map_err(|_| NetError::Transport("timed out".to_owned()))
}

/// The relay the endpoint uses, with its access token, or `None` for direct
/// connections only. The map is built from the URL alone, so the relay's QUIC
/// address discovery (UDP 7842, iroh's default) stays on: only the host and
/// the default port are needed to reach it.
fn relay_map(config: &IrohConfig) -> Result<Option<RelayMap>, NetError> {
    let Some(url) = &config.relay_url else {
        return Ok(None);
    };
    let url: RelayUrl = url.parse().map_err(net_err)?;
    let mut map = RelayMap::from_iter([url]);
    if let Some(token) = &config.relay_token {
        map = map.with_auth_token(token.clone());
    }
    Ok(Some(map))
}

/// One endpoint on one network thread.
pub struct IrohNode {
    endpoint: Endpoint,
    handle: Handle,
    incoming: Receiver<Box<dyn Connection>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl IrohNode {
    /// Bind the endpoint and start accepting.
    ///
    /// # Errors
    /// When the runtime or the endpoint cannot start, or the relay URL is bad.
    pub fn start(config: &IrohConfig) -> Result<Self, NetError> {
        #[cfg(feature = "test-relay")]
        let tls = TRUST_ANY_RELAY
            .load(std::sync::atomic::Ordering::SeqCst)
            .then(iroh::tls::CaTlsConfig::insecure_skip_verify);
        #[cfg(not(feature = "test-relay"))]
        let tls = None;
        Self::start_with(config, tls)
    }

    /// As [`IrohNode::start`], with a TLS trust override. Only the tests that
    /// run a relay with a self-signed certificate pass one.
    fn start_with(
        config: &IrohConfig,
        tls: Option<iroh::tls::CaTlsConfig>,
    ) -> Result<Self, NetError> {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(net_err)?;
        let handle = runtime.handle().clone();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("connecto-net".into())
            .spawn(move || {
                runtime.block_on(async {
                    let _ = stop_rx.await;
                });
            })
            .map_err(net_err)?;

        let relay_mode = match relay_map(config)? {
            None => RelayMode::Disabled,
            Some(map) => RelayMode::Custom(map),
        };
        let secret = SecretKey::from_bytes(&config.device_key);
        let bind = config.bind;
        let listen = config.listen;
        let relay_only = config.relay_only;
        let alpn = config.alpn;
        let endpoint = run(&handle, CONNECT_TIMEOUT, async move {
            let mut builder = Endpoint::builder(presets::Minimal)
                .secret_key(secret)
                .alpns(if listen {
                    vec![alpn.to_vec()]
                } else {
                    Vec::new()
                })
                .relay_mode(relay_mode);
            if let Some(tls) = tls {
                builder = builder.ca_tls_config(tls);
            }
            if relay_only {
                builder = builder.clear_ip_transports();
            } else if let Some(addr) = bind {
                builder = builder
                    .clear_ip_transports()
                    .bind_addr(addr)
                    .map_err(net_err)?;
            }
            builder.bind().await.map_err(net_err)
        })??;

        let self_id = *endpoint.id().as_bytes();
        let (in_tx, incoming) = bounded(8);
        let accept_endpoint = endpoint.clone();
        let accept_handle = handle.clone();
        let local = self_id;
        handle.spawn(async move {
            while let Some(incoming) = accept_endpoint.accept().await {
                let in_tx = in_tx.clone();
                let handle = accept_handle.clone();
                tokio::spawn(async move {
                    // A peer that fails the TLS handshake is dropped here.
                    let Ok(conn) = incoming.await else { return };
                    let conn = IrohConnection::new(local, conn, handle);
                    let boxed: Box<dyn Connection> = Box::new(conn);
                    let _ = in_tx.try_send(boxed);
                });
            }
        });
        Ok(Self {
            endpoint,
            handle,
            incoming,
            stop: Some(stop_tx),
            thread: Some(thread),
        })
    }

    /// This endpoint's identity.
    #[must_use]
    pub fn id(&self) -> EndpointId {
        *self.endpoint.id().as_bytes()
    }

    /// The endpoint's current direct addresses, as it knows them.
    #[must_use]
    pub fn known_addrs(&self) -> Vec<SocketAddr> {
        self.endpoint.addr().ip_addrs().copied().collect()
    }

    /// Where this endpoint listens, for a peer that is told directly.
    #[must_use]
    pub fn direct_addrs(&self) -> Vec<SocketAddr> {
        self.endpoint.bound_sockets()
    }

    /// Dial a peer. The transport checks the peer is the id dialed.
    ///
    /// # Errors
    /// When no path to the peer works in time.
    pub fn connect(&self, peer: &PeerAddr) -> Result<Box<dyn Connection>, NetError> {
        self.connect_alpn(peer, ALPN)
    }

    /// As [`IrohNode::connect`], speaking `alpn` (the pairing protocol dials
    /// [`PAIR_ALPN`]).
    ///
    /// # Errors
    /// When no path to the peer works in time.
    pub fn connect_alpn(
        &self,
        peer: &PeerAddr,
        alpn: &'static [u8],
    ) -> Result<Box<dyn Connection>, NetError> {
        self.connect_alpn_within(peer, alpn, CONNECT_TIMEOUT)
    }

    /// As [`IrohNode::connect_alpn`], giving up after `limit`. A dial to an id
    /// nobody has registered takes the whole time to fail.
    ///
    /// # Errors
    /// When no path to the peer works in time.
    pub fn connect_alpn_within(
        &self,
        peer: &PeerAddr,
        alpn: &'static [u8],
        limit: Duration,
    ) -> Result<Box<dyn Connection>, NetError> {
        let mut addr = EndpointAddr::new(public_key(&peer.id)?);
        if let Some(relay) = &peer.relay {
            addr = addr.with_relay_url(relay.parse().map_err(net_err)?);
        }
        for direct in &peer.direct {
            addr = addr.with_ip_addr(*direct);
        }
        let endpoint = self.endpoint.clone();
        let handle = self.handle.clone();
        let local = self.id();
        let conn = run(&self.handle, limit, async move {
            tokio::time::timeout(limit, endpoint.connect(addr, alpn))
                .await
                .map_err(|_| NetError::Transport("timed out".to_owned()))?
                .map_err(net_err)
        })??;
        Ok(Box::new(IrohConnection::new(local, conn, handle)))
    }

    /// The next peer that connected, `Ok(None)` after `timeout`.
    ///
    /// # Errors
    /// `Closed` once the node is shut down.
    pub fn accept(&self, timeout: Duration) -> Result<Option<Box<dyn Connection>>, NetError> {
        match self.incoming.recv_timeout(timeout) {
            Ok(conn) => Ok(Some(conn)),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => Ok(None),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => Err(NetError::Closed),
        }
    }

    /// Wait until the endpoint is reachable (it reached its relay, or has a
    /// direct address), up to `timeout`.
    #[must_use]
    pub fn wait_online(&self, timeout: Duration) -> bool {
        let endpoint = self.endpoint.clone();
        run(&self.handle, timeout + Duration::from_secs(2), async move {
            tokio::time::timeout(timeout, endpoint.online())
                .await
                .is_ok()
        })
        .unwrap_or(false)
    }

    /// Whether the relay accepted this endpoint, and why not if it did not.
    /// For a status line; `None` when no relay is configured.
    #[must_use]
    pub fn relay_problem(&self) -> Option<String> {
        use iroh::Watcher;
        let status = self.endpoint.home_relay_status().get();
        status
            .iter()
            .find_map(|s| s.last_error().map(ToString::to_string))
    }
}

impl Drop for IrohNode {
    fn drop(&mut self) {
        let endpoint = self.endpoint.clone();
        let _ = run(&self.handle, Duration::from_secs(5), async move {
            endpoint.close().await;
        });
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// One QUIC connection.
pub struct IrohConnection {
    local: EndpointId,
    conn: iroh::endpoint::Connection,
    handle: Handle,
    streams: Receiver<ChannelStream>,
}

impl IrohConnection {
    fn new(local: EndpointId, conn: iroh::endpoint::Connection, handle: Handle) -> Self {
        let (tx, streams) = bounded(16);
        let accepting = conn.clone();
        let stream_handle = handle.clone();
        handle.spawn(async move {
            while let Ok((send, recv)) = accepting.accept_bi().await {
                let stream = pump(&stream_handle, send, recv);
                if tx.send(stream).is_err() {
                    break;
                }
            }
        });
        Self {
            local,
            conn,
            handle,
            streams,
        }
    }
}

/// Join a QUIC stream to a [`ChannelStream`] with two tasks.
fn pump(
    handle: &Handle,
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
) -> ChannelStream {
    let (out_tx, mut out_rx) = tmpsc::channel::<Vec<u8>>(STREAM_QUEUE);
    let (in_tx, in_rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = bounded(STREAM_QUEUE);
    handle.spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if send.write_all(&frame).await.is_err() {
                return;
            }
        }
        let _ = send.finish();
    });
    handle.spawn(async move {
        let mut buf = vec![0_u8; CHUNK];
        while let Ok(Some(n)) = recv.read(&mut buf).await {
            let mut chunk = buf.get(..n).unwrap_or_default().to_vec();
            // A full queue stops reading, so QUIC flow control slows the peer.
            loop {
                match in_tx.try_send(chunk) {
                    Ok(()) => break,
                    Err(crossbeam_channel::TrySendError::Full(back)) => {
                        chunk = back;
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                    Err(crossbeam_channel::TrySendError::Disconnected(_)) => return,
                }
            }
        }
    });
    ChannelStream::new(
        Box::new(move |frame| out_tx.blocking_send(frame).is_ok()),
        in_rx,
    )
}

impl Connection for IrohConnection {
    fn local_id(&self) -> EndpointId {
        self.local
    }

    fn remote_id(&self) -> EndpointId {
        *self.conn.remote_id().as_bytes()
    }

    fn channel_binding(&self) -> Result<[u8; 32], NetError> {
        let mut out = [0_u8; 32];
        self.conn
            .export_keying_material(&mut out, BINDING_LABEL, &[])
            .map_err(|_| NetError::Transport("cannot derive the channel binding".to_owned()))?;
        Ok(out)
    }

    fn open_stream(&self) -> Result<Box<dyn Stream>, NetError> {
        let conn = self.conn.clone();
        let handle = self.handle.clone();
        let (send, recv) = run(&self.handle, CONNECT_TIMEOUT, async move {
            conn.open_bi().await.map_err(net_err)
        })??;
        Ok(Box::new(pump(&handle, send, recv)))
    }

    fn accept_stream(&self, timeout: Duration) -> Result<Option<Box<dyn Stream>>, NetError> {
        match self.streams.recv_timeout(timeout) {
            Ok(stream) => Ok(Some(Box::new(stream))),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => Ok(None),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => Err(NetError::Closed),
        }
    }

    fn close(&self) {
        // A close frame can overtake stream data still in flight, and a peer
        // that is refused would see a dead connection instead of the reason.
        // The streams get a moment to drain first.
        let conn = self.conn.clone();
        self.handle.spawn(async move {
            tokio::time::sleep(CLOSE_LINGER).await;
            conn.close(0_u32.into(), b"bye");
        });
    }

    fn path(&self) -> PathKind {
        let paths = self.conn.paths();
        paths
            .iter()
            .find(|p| p.is_selected())
            .map_or(PathKind::Unknown, |p| {
                if p.is_relay() {
                    PathKind::Relayed
                } else {
                    PathKind::Direct
                }
            })
    }
}

/// An `Arc`-able handle type for callers that share a connection.
pub type SharedConnection = Arc<dyn Connection>;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use iroh::test_utils::run_relay_server_with_access;
    use iroh_relay::server::{Access, AccessControl, ClientRequest};

    use super::*;

    const TOKEN: &str = "relay-secret-token";

    /// A relay that lets in only clients that send the shared token.
    #[derive(Debug, Default)]
    struct NeedsToken {
        admitted: AtomicUsize,
        refused: AtomicUsize,
    }

    #[derive(Debug)]
    struct Shared(Arc<NeedsToken>);

    impl AccessControl for Shared {
        async fn on_connect(&self, request: &ClientRequest) -> Access {
            if request.auth_token().as_deref() == Some(TOKEN) {
                self.0.admitted.fetch_add(1, Ordering::SeqCst);
                Access::Allow
            } else {
                self.0.refused.fetch_add(1, Ordering::SeqCst);
                Access::Deny { reason: None }
            }
        }
    }

    fn config(seed: u8, relay: &str, token: Option<&str>) -> IrohConfig {
        IrohConfig {
            device_key: [seed; 32],
            relay_url: Some(relay.to_owned()),
            relay_token: token.map(str::to_owned),
            bind: Some("127.0.0.1:0".parse().unwrap()),
            listen: true,
            relay_only: false,
            alpn: ALPN,
        }
    }

    #[test]
    fn the_config_never_prints_its_secrets() {
        let cfg = config(1, "https://relay.example", Some(TOKEN));
        assert!(!format!("{cfg:?}").contains(TOKEN));
        assert!(!cfg.describe().contains(TOKEN));
        assert!(cfg.describe().contains("relay.example"));
    }

    #[test]
    fn the_relay_keeps_quic_address_discovery_on_its_default_port_and_gets_the_token() {
        let cfg = config(1, "https://relay.connecto.studio", Some(TOKEN));
        let map = relay_map(&cfg).unwrap().unwrap();
        let relays: Vec<_> = map.relays::<Vec<_>>();
        assert_eq!(relays.len(), 1);
        assert_eq!(relays[0].url.host_str(), Some("relay.connecto.studio"));
        // UDP 7842, derived from the host: nothing turns discovery off.
        assert_eq!(relays[0].quic, Some(iroh_relay::RelayQuicConfig::new(7842)));
        assert_eq!(relays[0].auth_token.as_deref(), Some(TOKEN));
        assert!(
            relay_map(&config(1, "https://x.example", None))
                .unwrap()
                .unwrap()
                .relays::<Vec<_>>()[0]
                .auth_token
                .is_none()
        );
    }

    #[test]
    fn a_bad_relay_url_is_an_error_and_no_relay_means_direct_only() {
        assert!(IrohNode::start(&config(1, "not a url", None)).is_err());
        let mut direct = config(2, "", None);
        direct.relay_url = None;
        assert!(IrohNode::start(&direct).is_ok());
    }

    /// The relay admits a client that sends the token and refuses one that
    /// does not; with it, two endpoints that know only each other's id and the
    /// relay reach each other.
    #[test]
    fn the_relay_token_is_sent_and_a_relay_only_peer_can_be_reached() {
        let access = Arc::new(NeedsToken::default());
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (_map, url, _server) = runtime
            .block_on(run_relay_server_with_access(
                false,
                Arc::new(Shared(Arc::clone(&access))),
            ))
            .unwrap();
        let url = url.to_string();
        let skip = || Some(iroh::tls::CaTlsConfig::insecure_skip_verify());

        // Without the token the relay turns the endpoint away.
        let tokenless = IrohNode::start_with(&config(5, &url, None), skip()).unwrap();
        assert!(
            !tokenless.wait_online(Duration::from_secs(12)),
            "a tokenless endpoint got in"
        );
        assert!(
            access.refused.load(Ordering::SeqCst) > 0,
            "the relay never saw it"
        );
        assert_eq!(access.admitted.load(Ordering::SeqCst), 0);
        assert!(
            tokenless.relay_problem().is_some(),
            "the refusal is not visible"
        );
        drop(tokenless);

        let host = IrohNode::start_with(&config(6, &url, Some(TOKEN)), skip()).unwrap();
        let client = IrohNode::start_with(&config(7, &url, Some(TOKEN)), skip()).unwrap();
        assert!(host.wait_online(Duration::from_secs(10)));
        assert!(client.wait_online(Duration::from_secs(10)));
        assert!(access.admitted.load(Ordering::SeqCst) >= 2);
        // The client is given the relay and the id, no direct address.
        let accepted = std::thread::scope(|scope| {
            let waiter = scope.spawn(|| host.accept(Duration::from_secs(20)));
            let conn = client
                .connect(&PeerAddr {
                    id: host.id(),
                    relay: Some(url.clone()),
                    direct: vec![],
                })
                .unwrap();
            let server_side = waiter.join().unwrap().unwrap().unwrap();
            (conn, server_side)
        });
        // Both ends derive the same binding from the same TLS session.
        assert_eq!(
            accepted.0.channel_binding().unwrap(),
            accepted.1.channel_binding().unwrap()
        );
        assert_eq!(accepted.0.remote_id(), host.id());
        assert_eq!(accepted.1.remote_id(), client.id());
        assert_eq!(accepted.1.local_id(), host.id());
    }
}
