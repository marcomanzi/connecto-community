//! What the protocol needs from a network: connections that carry independent
//! framed streams, and a value both ends derive from the same session.
//!
//! The traits are blocking, because the rest of the app is threads and bounded
//! channels (ADR 0002). The iroh implementation hides its async runtime behind
//! the same shape. [`ChannelStream`] is the shared stream core: it turns byte
//! chunks, in any chunking, into messages, so the loopback transport exercises
//! the same framing the network does.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError, bounded};

use crate::wire::{FrameDecoder, Message, WireError};

/// A 32-byte endpoint identity (an Ed25519 public key).
pub type EndpointId = [u8; 32];

/// Why a stream or connection operation failed.
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("the connection is closed")]
    Closed,
    /// The peer's queue is full: it is not reading yet. Not the end of the
    /// connection; the sender decides whether to wait.
    #[error("the peer is not reading yet")]
    Full,
    #[error("bad frame from the peer: {0}")]
    Wire(#[from] WireError),
    #[error("transport: {0}")]
    Transport(String),
}

/// One ordered, framed message pipe. Both halves may be used from different
/// threads at once.
pub trait Stream: Send + Sync {
    /// Send at once; it never waits for the peer to read.
    ///
    /// # Errors
    /// `Full` when the peer's queue is full (it is not reading yet), which a
    /// sender may retry; `Closed` when the peer or the connection is gone.
    fn send(&self, message: &Message) -> Result<(), NetError>;

    /// The next message, `Ok(None)` after `timeout` with nothing.
    ///
    /// # Errors
    /// `Closed` at the end of the stream; `Wire` for a bad frame, after which
    /// the stream is dead.
    fn recv(&self, timeout: Duration) -> Result<Option<Message>, NetError>;
}

/// An established connection to one peer.
pub trait Connection: Send + Sync {
    fn local_id(&self) -> EndpointId;
    /// The peer's identity, authenticated by the transport.
    fn remote_id(&self) -> EndpointId;
    /// 32 bytes both ends derive from this session's secrets and nothing else.
    ///
    /// # Errors
    /// When the transport cannot derive it.
    fn channel_binding(&self) -> Result<[u8; 32], NetError>;
    /// Open a new stream. The peer sees it in `accept_stream`.
    ///
    /// # Errors
    /// `Closed` when the connection is gone.
    fn open_stream(&self) -> Result<Box<dyn Stream>, NetError>;
    /// Open a new stream, waiting at most `timeout` for it: a request with a
    /// deadline does not wait out the transport's own. By default as
    /// [`Self::open_stream`], for a transport that never waits.
    ///
    /// # Errors
    /// `Closed` when the connection is gone; `Transport` when it took too long.
    fn open_stream_within(&self, timeout: Duration) -> Result<Box<dyn Stream>, NetError> {
        let _ = timeout;
        self.open_stream()
    }
    /// The next stream the peer opened, `Ok(None)` after `timeout`.
    ///
    /// # Errors
    /// `Closed` when the connection is gone.
    fn accept_stream(&self, timeout: Duration) -> Result<Option<Box<dyn Stream>>, NetError>;
    /// Close at once; blocked calls on either end return `Closed`.
    fn close(&self);
    /// How the data travels, for a status line.
    fn path(&self) -> PathKind {
        PathKind::Unknown
    }
}

/// Whether traffic goes straight to the peer or through the relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    Unknown,
    Direct,
    Relayed,
}

/// Messages a stream may queue before `send` has to wait for the reader.
pub const STREAM_QUEUE: usize = 256;

/// A stream over two byte-chunk channels. Whatever moves the chunks (a
/// loopback peer, or tasks pumping a QUIC stream) sits on the far ends.
pub struct ChannelStream {
    tx: ChunkSink,
    rx: Receiver<Vec<u8>>,
    decoder: Mutex<FrameDecoder>,
    /// When set, the stream is dead: the link under it was cut.
    kill: Option<Arc<AtomicBool>>,
}

/// Hands one encoded frame to whatever carries it; false when that is gone.
/// Queues one frame without waiting: `Full` when the peer is not reading yet,
/// `Closed` when the peer is gone.
pub type ChunkSink = Box<dyn Fn(Vec<u8>) -> Result<(), NetError> + Send + Sync>;

impl ChannelStream {
    #[must_use]
    pub fn new(tx: ChunkSink, rx: Receiver<Vec<u8>>) -> Self {
        Self {
            tx,
            rx,
            decoder: Mutex::new(FrameDecoder::default()),
            kill: None,
        }
    }

    /// The same stream, dead once `kill` is set.
    #[must_use]
    pub fn with_kill(mut self, kill: Arc<AtomicBool>) -> Self {
        self.kill = Some(kill);
        self
    }

    fn killed(&self) -> bool {
        self.kill
            .as_ref()
            .is_some_and(|k| k.load(Ordering::Acquire))
    }

    /// A stream whose outgoing frames go to a crossbeam channel.
    #[must_use]
    pub fn over_channels(tx: Sender<Vec<u8>>, rx: Receiver<Vec<u8>>) -> Self {
        // Never waits: a peer that does not read fills its queue, and the send
        // then fails, which ends the stream rather than holding up the host.
        Self::new(
            Box::new(move |frame| match tx.try_send(frame) {
                Ok(()) => Ok(()),
                Err(TrySendError::Full(_)) => Err(NetError::Full),
                Err(TrySendError::Disconnected(_)) => Err(NetError::Closed),
            }),
            rx,
        )
    }

    /// Two streams wired to each other.
    #[must_use]
    pub fn pair() -> (Self, Self) {
        let (a_tx, b_rx) = bounded(STREAM_QUEUE);
        let (b_tx, a_rx) = bounded(STREAM_QUEUE);
        (
            Self::over_channels(a_tx, a_rx),
            Self::over_channels(b_tx, b_rx),
        )
    }
}

impl Stream for ChannelStream {
    fn send(&self, message: &Message) -> Result<(), NetError> {
        if self.killed() {
            return Err(NetError::Closed);
        }
        let frame = message.encode()?;
        (self.tx)(frame)
    }

    fn recv(&self, timeout: Duration) -> Result<Option<Message>, NetError> {
        let mut decoder = self
            .decoder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(message) = decoder.next_message()? {
                return Ok(Some(message));
            }
            // What was sent before the cut is still read, as on a real link
            // that closes gracefully.
            if let Ok(chunk) = self.rx.try_recv() {
                decoder.push(&chunk);
                continue;
            }
            if self.killed() {
                return Err(NetError::Closed);
            }
            // A cut link is noticed within one slice.
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let slice = if self.kill.is_some() {
                left.min(Duration::from_millis(10))
            } else {
                left
            };
            match self.rx.recv_timeout(slice) {
                Ok(chunk) => decoder.push(&chunk),
                Err(RecvTimeoutError::Timeout) => {
                    if std::time::Instant::now() >= deadline {
                        return Ok(None);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return Err(NetError::Closed),
            }
        }
    }
}

/// An in-memory transport for tests: two connected ends.
pub mod loopback {
    use super::{Arc, AtomicBool, Ordering};
    use super::{
        ChannelStream, Connection, Duration, EndpointId, NetError, Receiver, RecvTimeoutError,
        Sender, Stream, TrySendError, bounded,
    };

    struct Shared {
        closed: Arc<AtomicBool>,
        binding: [u8; 32],
    }

    /// One end of a loopback connection.
    pub struct LoopbackConnection {
        local: EndpointId,
        remote: EndpointId,
        shared: Arc<Shared>,
        open: Sender<ChannelStream>,
        accept: Receiver<ChannelStream>,
    }

    /// Two connected ends. `binding` is what both report as the channel
    /// binding, so a test can make two sessions differ.
    #[must_use]
    pub fn pair(
        a_id: EndpointId,
        b_id: EndpointId,
        binding: [u8; 32],
    ) -> (LoopbackConnection, LoopbackConnection) {
        let shared = Arc::new(Shared {
            closed: Arc::new(AtomicBool::new(false)),
            binding,
        });
        let (a_open, b_accept) = bounded(64);
        let (b_open, a_accept) = bounded(64);
        (
            LoopbackConnection {
                local: a_id,
                remote: b_id,
                shared: Arc::clone(&shared),
                open: a_open,
                accept: a_accept,
            },
            LoopbackConnection {
                local: b_id,
                remote: a_id,
                shared,
                open: b_open,
                accept: b_accept,
            },
        )
    }

    impl Connection for LoopbackConnection {
        fn local_id(&self) -> EndpointId {
            self.local
        }
        fn remote_id(&self) -> EndpointId {
            self.remote
        }
        fn channel_binding(&self) -> Result<[u8; 32], NetError> {
            Ok(self.shared.binding)
        }
        fn open_stream(&self) -> Result<Box<dyn Stream>, NetError> {
            if self.shared.closed.load(Ordering::Acquire) {
                return Err(NetError::Closed);
            }
            let (mine, theirs) = ChannelStream::pair();
            let (mine, theirs) = (
                mine.with_kill(Arc::clone(&self.shared.closed)),
                theirs.with_kill(Arc::clone(&self.shared.closed)),
            );
            // Never waits: a peer that has not accepted its pending streams is not
            // read, and the opener is told so.
            match self.open.try_send(theirs) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => return Err(NetError::Full),
                Err(TrySendError::Disconnected(_)) => return Err(NetError::Closed),
            }
            // The peer may have closed between the check and the send, after
            // it drained its queue: this stream would then never be served.
            if self.shared.closed.load(Ordering::Acquire) {
                return Err(NetError::Closed);
            }
            Ok(Box::new(mine))
        }
        fn accept_stream(&self, timeout: Duration) -> Result<Option<Box<dyn Stream>>, NetError> {
            if self.shared.closed.load(Ordering::Acquire) {
                return Err(NetError::Closed);
            }
            match self.accept.recv_timeout(timeout) {
                Ok(stream) => Ok(Some(Box::new(stream))),
                Err(RecvTimeoutError::Timeout) => Ok(None),
                Err(RecvTimeoutError::Disconnected) => Err(NetError::Closed),
            }
        }
        fn close(&self) {
            self.shared.closed.store(true, Ordering::Release);
            // Streams the peer opened and nobody accepted are dropped, so the
            // peer's reads end instead of waiting.
            while self.accept.try_recv().is_ok() {}
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_pair_carries_messages_both_ways() {
        let (a, b) = ChannelStream::pair();
        a.send(&Message::Hello { version: 1 }).unwrap();
        b.send(&Message::AuthOk).unwrap();
        assert_eq!(
            b.recv(Duration::from_secs(1)).unwrap(),
            Some(Message::Hello { version: 1 })
        );
        assert_eq!(
            a.recv(Duration::from_secs(1)).unwrap(),
            Some(Message::AuthOk)
        );
        assert_eq!(a.recv(Duration::from_millis(5)).unwrap(), None);
    }

    #[test]
    fn a_dropped_peer_ends_the_stream() {
        let (a, b) = ChannelStream::pair();
        drop(b);
        assert!(matches!(
            a.recv(Duration::from_millis(5)),
            Err(NetError::Closed)
        ));
        assert!(matches!(a.send(&Message::Bye), Err(NetError::Closed)));
    }

    #[test]
    fn a_bad_frame_kills_the_stream() {
        let (tx, rx) = bounded(4);
        let (dead_tx, _dead_rx) = bounded(4);
        let stream = ChannelStream::over_channels(dead_tx, rx);
        tx.send(vec![0xff, 0xff, 0xff, 0xff]).unwrap();
        assert!(matches!(
            stream.recv(Duration::from_millis(50)),
            Err(NetError::Wire(_))
        ));
        assert!(stream.recv(Duration::from_millis(5)).is_err());
    }

    #[test]
    fn loopback_streams_arrive_at_the_peer_and_close_stops_both_ends() {
        let (a, b) = loopback::pair([1; 32], [2; 32], [9; 32]);
        assert_eq!(a.remote_id(), [2; 32]);
        assert_eq!(b.remote_id(), [1; 32]);
        assert_eq!(a.channel_binding().unwrap(), b.channel_binding().unwrap());
        let s = a.open_stream().unwrap();
        let peer = b.accept_stream(Duration::from_secs(1)).unwrap().unwrap();
        s.send(&Message::Bye).unwrap();
        assert_eq!(
            peer.recv(Duration::from_secs(1)).unwrap(),
            Some(Message::Bye)
        );
        assert!(b.accept_stream(Duration::from_millis(5)).unwrap().is_none());
        a.close();
        assert!(matches!(
            b.accept_stream(Duration::from_millis(5)),
            Err(NetError::Closed)
        ));
        assert!(a.open_stream().is_err());
    }
}
