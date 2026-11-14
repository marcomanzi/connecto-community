//! The per-session hook server.
//!
//! Reads are open inside a session, so a token can be stolen. The check that
//! matters is that the peer descends from this session's root process.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crate::turn::LogBoundary;
use connecto_adapter::{HookCtx, ToolAdapter};
use connecto_core::{Reply, ToolEvent};
use connecto_hook::{ACK, MAX_FRAME, decode};
use connecto_pty::Wake;

use crate::local_socket::{self, Listener, Stream};

/// Events waiting for the session's consumer.
pub const EVENT_QUEUE_CAPACITY: usize = 256;

/// Reply bytes the queue holds before the oldest reply becomes `Dropped`. The
/// 64 KiB cap that bounded memory before replies were allowed to be whole is
/// replaced by this bound.
pub const QUEUED_REPLY_BYTES: usize = 8 * 1024 * 1024;

/// One client may hold the server this long, so a client that stalls can only
/// delay its own session's hooks.
const CLIENT_DEADLINE: Duration = Duration::from_secs(1);
const MAX_ANCESTRY_DEPTH: usize = 64;

fn reply_bytes(event: &ToolEvent) -> usize {
    match event {
        ToolEvent::TurnEnded {
            reply: Reply::Text(text),
            ..
        } => text.expose().len(),
        _ => 0,
    }
}

/// An event with the turn log boundary sampled when its hook was accepted, if
/// it started a session. The boundary is taken before the sender is released,
/// so records the agent writes after that point are never skipped.
pub struct Queued {
    pub event: ToolEvent,
    pub log: Option<LogBoundary>,
}

#[derive(Default)]
struct QueueState {
    items: VecDeque<Queued>,
    gaps: u64,
}

/// A bounded queue. When full it drops the oldest non-terminal event and counts
/// a gap. If every queued event is terminal, the oldest of those goes, which
/// needs 256 turns to end with nobody consuming.
#[derive(Default)]
pub struct EventQueue {
    state: Mutex<QueueState>,
    ready: Condvar,
}

impl EventQueue {
    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn push(&self, event: ToolEvent) {
        self.push_logged(event, None);
    }

    /// Queue an event that carries the log boundary of its hook.
    pub fn push_logged(&self, event: ToolEvent, log: Option<LogBoundary>) {
        let mut state = self.lock();
        let queued: usize = state
            .items
            .iter()
            .map(|item| reply_bytes(&item.event))
            .sum::<usize>()
            + reply_bytes(&event);
        if queued > QUEUED_REPLY_BYTES {
            // Over the bound, the oldest queued reply is given up, with its
            // lifecycle facts kept.
            let mut over = queued - QUEUED_REPLY_BYTES;
            for item in state.items.iter_mut() {
                if over == 0 {
                    break;
                }
                if let ToolEvent::TurnEnded { reply, .. } = &mut item.event
                    && let Reply::Text(text) = reply
                {
                    over = over.saturating_sub(text.expose().len());
                    *reply = Reply::Dropped;
                }
            }
        }
        if state.items.len() >= EVENT_QUEUE_CAPACITY {
            let victim = state
                .items
                .iter()
                .position(|item| !item.event.is_terminal())
                .unwrap_or(0);
            state.items.remove(victim);
            state.gaps += 1;
        }
        state.items.push_back(Queued { event, log });
        self.ready.notify_one();
    }

    /// The next event, waiting up to `timeout`.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<ToolEvent> {
        self.recv_queued_timeout(timeout).map(|queued| queued.event)
    }

    /// The next event with its log boundary, waiting up to `timeout`.
    pub fn recv_queued_timeout(&self, timeout: Duration) -> Option<Queued> {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock();
        loop {
            if let Some(queued) = state.items.pop_front() {
                return Some(queued);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            state = self
                .ready
                .wait_timeout(state, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Events lost to the bound so far.
    pub fn gaps(&self) -> u64 {
        self.lock().gaps
    }
}

#[derive(Default)]
struct Shared {
    events: EventQueue,
    rejected: AtomicU64,
    stop: AtomicBool,
}

/// Removes the socket's path, on every way out.
struct SocketFile(PathBuf);

impl Drop for SocketFile {
    fn drop(&mut self) {
        local_socket::remove(&self.0);
    }
}

/// A bound socket that nothing serves yet. It exists before the child, so an
/// early hook waits in the backlog rather than failing.
pub(crate) struct BoundSocket {
    listener: Listener,
    file: SocketFile,
}

impl BoundSocket {
    pub(crate) fn bind(path: &Path) -> io::Result<Self> {
        let listener = local_socket::bind(path)?;
        Ok(Self {
            listener,
            file: SocketFile(path.to_owned()),
        })
    }
}

/// What the server checks a frame against.
pub(crate) struct ServerConfig {
    pub(crate) token: String,
    pub(crate) root_pid: i32,
    pub(crate) adapter: &'static dyn ToolAdapter,
    pub(crate) ctx: HookCtx,
    pub(crate) wake: Wake,
}

/// A running hook server. Dropping it stops the server and removes the socket.
pub struct HookHandle {
    shared: Arc<Shared>,
    file: SocketFile,
}

impl HookHandle {
    /// Events from this session. They inform; nothing here can start an action.
    #[must_use]
    pub fn events(&self) -> &EventQueue {
        &self.shared.events
    }

    /// Frames refused so far.
    #[must_use]
    pub fn rejected(&self) -> u64 {
        self.shared.rejected.load(Ordering::Relaxed)
    }

    /// The socket, for tests that play a forger.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.file.0
    }
}

impl Drop for HookHandle {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        // Wakes the blocking accept, which then sees the flag.
        local_socket::wake(&self.file.0);
    }
}

/// Start serving, now that the session's root process is known.
pub(crate) fn serve(socket: BoundSocket, config: ServerConfig) -> io::Result<HookHandle> {
    let shared = Arc::new(Shared::default());
    let server_shared = Arc::clone(&shared);
    let BoundSocket { listener, file } = socket;
    thread::Builder::new()
        .name("hook-server".into())
        .spawn(move || accept_loop(&listener, &server_shared, &config))?;
    Ok(HookHandle { shared, file })
}

fn accept_loop(listener: &Listener, shared: &Shared, config: &ServerConfig) {
    for stream in listener.incoming() {
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        let Ok(mut stream) = stream else { continue };
        match take_frame(&mut stream, config) {
            Ok(payload) => {
                let parsed = config.adapter.parse_hook(&payload, &config.ctx);
                // The boundary is sampled before the ACK releases the sender:
                // from then on the agent may write this turn's records.
                let log = parsed
                    .as_ref()
                    .ok()
                    .and_then(|events| log_boundary(events, config));
                let _ = stream.write_all(&[ACK]);
                match parsed {
                    Ok(events) => {
                        for event in events {
                            let boundary = match event {
                                ToolEvent::Started { .. } => log.clone(),
                                _ => None,
                            };
                            shared.events.push_logged(event, boundary);
                        }
                        (config.wake)();
                    }
                    Err(_) => {
                        shared.rejected.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Err(_) => {
                shared.rejected.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Why a frame was refused. Only ever counted.
struct Refused;

impl From<io::Error> for Refused {
    fn from(_: io::Error) -> Self {
        Self
    }
}

/// Read and check one frame. The ACK is not sent here: the caller sends it
/// after sampling the turn log, so the sampled length is never past records
/// the agent wrote after the sender was released.
fn take_frame(stream: &mut Stream, config: &ServerConfig) -> Result<Vec<u8>, Refused> {
    let peer = local_socket::peer_pid(stream).map_err(|_| Refused)?;
    if !descends_from(peer, config.root_pid) {
        return Err(Refused);
    }
    let body = read_within(stream, CLIENT_DEADLINE)?;
    let frame = decode(&body).ok_or(Refused)?;
    if !same_token(frame.token, config.token.as_bytes()) {
        return Err(Refused);
    }
    Ok(frame.payload.to_vec())
}

/// The turn log a session start names, with its length now. `None` when the
/// events do not start a session or the reported path is not under the profile.
fn log_boundary(events: &[ToolEvent], config: &ServerConfig) -> Option<LogBoundary> {
    events.iter().find_map(|event| match event {
        ToolEvent::Started {
            turn_log: Some(reported),
            ..
        } => config
            .adapter
            .turn_log(Path::new(reported.expose()), &config.ctx)
            .map(LogBoundary::sample),
        _ => None,
    })
}

/// Read a frame under one overall deadline, so a client sending a byte at a
/// time cannot hold the server past it.
fn read_within(stream: &mut Stream, limit: Duration) -> Result<Vec<u8>, Refused> {
    let deadline = Instant::now() + limit;
    let mut received = Vec::new();
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        if let Some(body) = complete_body(&received)? {
            return Ok(body);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Refused);
        }
        stream.set_read_timeout(Some(remaining))?;
        match stream.read(&mut chunk) {
            Ok(0) => return Err(Refused),
            Ok(count) => received.extend_from_slice(chunk.get(..count).ok_or(Refused)?),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err(Refused),
        }
    }
}

/// The body, once all of it has arrived.
fn complete_body(received: &[u8]) -> Result<Option<Vec<u8>>, Refused> {
    let Some((header, rest)) = received.split_first_chunk::<4>() else {
        return Ok(None);
    };
    let len = usize::try_from(u32::from_be_bytes(*header)).map_err(|_| Refused)?;
    if len > MAX_FRAME {
        return Err(Refused);
    }
    Ok(rest.get(..len).map(<[u8]>::to_vec))
}

fn same_token(actual: &[u8], expected: &[u8]) -> bool {
    actual.len() == expected.len()
        && actual
            .iter()
            .zip(expected)
            .fold(0_u8, |diff, (a, b)| diff | (a ^ b))
            == 0
}

fn descends_from(pid: i32, root: i32) -> bool {
    crate::process_tree::descends_from(pid, root, MAX_ANCESTRY_DEPTH)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use connecto_core::EventText;

    use super::*;

    fn note() -> ToolEvent {
        ToolEvent::NeedsUser
    }

    fn end(text: &str) -> ToolEvent {
        end_with(Reply::Text(EventText::new(text.to_owned())))
    }

    fn end_with(reply: Reply) -> ToolEvent {
        ToolEvent::TurnEnded {
            input: Vec::new(),
            input_total: 0,
            reply,
            lifecycle_known: true,
            background: Vec::new(),
            thread: None,
            turn: None,
        }
    }

    /// Replies are bounded by bytes: the oldest becomes `Dropped` and keeps its
    /// lifecycle facts.
    #[test]
    fn queued_replies_are_bounded_by_bytes_and_the_oldest_is_dropped() {
        let queue = EventQueue::default();
        let big = "x".repeat(200 * 1024);
        for _ in 0..50 {
            queue.push(end_with(Reply::Text(EventText::reply(big.clone()))));
        }
        let mut dropped = 0;
        let mut kept = 0;
        while let Some(event) = queue.recv_timeout(Duration::ZERO) {
            match event {
                ToolEvent::TurnEnded {
                    reply: Reply::Dropped,
                    lifecycle_known: true,
                    ..
                } => dropped += 1,
                ToolEvent::TurnEnded {
                    reply: Reply::Text(_),
                    ..
                } => kept += 1,
                other => panic!("{other:?}"),
            }
        }
        assert!(dropped > 0 && kept > 0, "dropped {dropped}, kept {kept}");
        assert!(kept * 200 * 1024 <= QUEUED_REPLY_BYTES);
        assert_eq!(dropped + kept, 50, "no lifecycle fact was lost");
    }

    #[test]
    fn a_full_queue_drops_the_oldest_non_terminal_and_keeps_turn_ends() {
        let queue = EventQueue::default();
        queue.push(end("kept"));
        for _ in 0..EVENT_QUEUE_CAPACITY {
            queue.push(note());
        }
        assert_eq!(queue.gaps(), 1);
        assert_eq!(queue.recv_timeout(Duration::ZERO), Some(end("kept")));
    }

    #[test]
    fn a_queue_of_only_turn_ends_stays_bounded_and_counts_the_gap() {
        let queue = EventQueue::default();
        for index in 0..=EVENT_QUEUE_CAPACITY {
            queue.push(end(&index.to_string()));
        }
        assert_eq!(queue.gaps(), 1);
        assert_eq!(queue.recv_timeout(Duration::ZERO), Some(end("1")));
    }

    #[test]
    fn waiting_on_an_empty_queue_times_out() {
        let queue = EventQueue::default();
        assert_eq!(queue.recv_timeout(Duration::from_millis(1)), None);
    }

    #[test]
    fn tokens_must_match_exactly() {
        assert!(same_token(b"abc", b"abc"));
        assert!(!same_token(b"abd", b"abc"));
        assert!(!same_token(b"ab", b"abc"));
    }

    #[cfg(unix)]
    #[test]
    fn ancestry_follows_real_parents() {
        let me = i32::try_from(std::process::id()).unwrap();
        let parent = i32::try_from(std::os::unix::process::parent_id()).unwrap();
        assert!(descends_from(me, me));
        assert!(descends_from(me, parent));
        assert!(!descends_from(parent, me));
        assert!(!descends_from(1, me));
    }

    /// The hook server over a named pipe, with the real client transport: a
    /// caller in the root's tree is answered, one outside it is refused before
    /// its frame is read.
    #[cfg(windows)]
    #[test]
    fn a_named_pipe_hook_accepts_only_the_roots_tree() {
        use std::process::{Command, Stdio};

        fn server(root_pid: i32) -> HookHandle {
            let path = crate::platform::endpoint("hook", String::new()).unwrap();
            let socket = BoundSocket::bind(&path).unwrap();
            serve(
                socket,
                ServerConfig {
                    token: "secret".to_owned(),
                    root_pid,
                    adapter: &connecto_adapter::Plain,
                    ctx: HookCtx {
                        profile: std::env::temp_dir(),
                        hooks: false,
                    },
                    wake: Arc::new(|| {}),
                },
            )
            .unwrap()
        }

        /// Send one frame; true when the server acknowledged it.
        fn send(hook: &HookHandle, token: &[u8]) -> bool {
            let frame = connecto_hook::encode(token, b"{}").unwrap();
            let mut stream =
                connecto_hook::transport::connect(hook.socket(), Duration::from_secs(5)).unwrap();
            stream.write_all(&frame).unwrap();
            let mut ack = [0_u8; 1];
            stream.read_exact(&mut ack).is_ok() && ack[0] == ACK
        }

        /// The count is taken after the connection is closed.
        fn rejected_reaches(hook: &HookHandle, count: u64) -> bool {
            let deadline = Instant::now() + Duration::from_secs(5);
            while hook.rejected() < count && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            hook.rejected() == count
        }

        let me = i32::try_from(std::process::id()).unwrap();
        let inside = server(me);
        assert!(send(&inside, b"secret"));
        assert_eq!(inside.rejected(), 0);
        assert!(!send(&inside, b"wrong"));
        assert!(rejected_reaches(&inside, 1));

        // Rooted at a child of this process: this process is outside its tree.
        let mut child = Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-Command",
                "Start-Sleep -Seconds 30",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let outside = server(i32::try_from(child.id()).unwrap());
        assert!(!send(&outside, b"secret"));
        assert!(rejected_reaches(&outside, 1));
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn a_frame_is_complete_only_when_all_of_it_arrived() {
        assert!(matches!(complete_body(&[0, 0]), Ok(None)));
        assert!(matches!(complete_body(&[0, 0, 0, 2, 9]), Ok(None)));
        assert!(matches!(
            complete_body(&[0, 0, 0, 2, 9, 8]),
            Ok(Some(body)) if body == [9, 8]
        ));
        assert!(complete_body(&[0xff, 0xff, 0xff, 0xff]).is_err());
    }

    /// The start is accepted (ACK sent), then this turn's records are written,
    /// and only after that does the consumer take the queued start. The tail
    /// built from the queued boundary still reads those records.
    #[test]
    fn records_written_after_the_start_is_accepted_are_read_by_the_tail() {
        use connecto_adapter::{Claude, LogMark};
        use std::fs;
        use std::io::Read;

        let dir = std::env::temp_dir().join(format!("mg-hook-boundary-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let transcript = dir.join("session.jsonl");
        fs::write(
            &transcript,
            b"{\"type\":\"user\",\"message\":{\"content\":\"earlier\"}}\n",
        )
        .unwrap();
        // A socket path is limited to about a hundred bytes, so the socket sits
        // in a short folder of its own, as the launch tests place theirs.
        #[cfg(unix)]
        let sockets = PathBuf::from(format!("/tmp/mg-hook-{}", std::process::id()));
        #[cfg(unix)]
        let socket = {
            let _ = fs::remove_dir_all(&sockets);
            fs::create_dir_all(&sockets).unwrap();
            sockets.join("h.sock")
        };
        // Windows: a named pipe of its own.
        #[cfg(windows)]
        let socket = crate::platform::endpoint("hook", String::new()).unwrap();
        let bound = BoundSocket::bind(&socket).unwrap();
        let config = ServerConfig {
            token: "token".to_owned(),
            root_pid: i32::try_from(std::process::id()).unwrap(),
            adapter: &Claude,
            ctx: HookCtx {
                profile: dir.clone(),
                hooks: true,
            },
            wake: Arc::new(|| {}),
        };
        let handle = serve(bound, config).unwrap();

        // As a JSON string: a Windows path's `\` must be escaped.
        let payload = format!(
            r#"{{"hook_event_name":"SessionStart","session_id":"s1","transcript_path":{}}}"#,
            serde_json::Value::from(transcript.display().to_string())
        );
        let frame = connecto_hook::encode(b"token", payload.as_bytes()).unwrap();
        #[cfg(unix)]
        let mut client = {
            let client = std::os::unix::net::UnixStream::connect(&socket).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            client
        };
        #[cfg(windows)]
        let mut client =
            connecto_hook::transport::connect(&socket, Duration::from_secs(5)).unwrap();
        client.write_all(&frame).unwrap();
        let mut ack = [0_u8; 1];
        client.read_exact(&mut ack).unwrap();
        assert_eq!(ack[0], ACK);

        // The agent writes this turn's prompt after the sender was released,
        // before the consumer has taken the start off the queue.
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        file.write_all(b"{\"type\":\"user\",\"message\":{\"content\":\"this turn\"}}\n")
            .unwrap();
        drop(file);

        let queued = handle
            .events()
            .recv_queued_timeout(Duration::from_secs(5))
            .unwrap();
        assert!(matches!(queued.event, ToolEvent::Started { .. }));
        let Some(boundary) = queued.log else {
            panic!("a start under the profile names its log");
        };
        assert_eq!(boundary.path, transcript);
        let mut tail = crate::turn::LogTail::from_boundary(boundary);
        let (marks, gap) = tail.read(&Claude, crate::turn::POLL_BYTES);
        assert!(!gap);
        assert_eq!(
            marks,
            [LogMark::Prompt {
                text: "this turn".into()
            }]
        );
        drop(handle);
        #[cfg(unix)]
        let _ = fs::remove_dir_all(&sockets);
        let _ = fs::remove_dir_all(&dir);
    }
}
