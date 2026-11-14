//! Windows side of [`crate::local_socket`]: named pipes.
//!
//! The server is `interprocess`'s named pipe listener (its tokio flavour), run
//! on one small runtime shared by every listener in the process; the callers'
//! threads stay blocking, as on Unix. What it guarantees, and how:
//!
//! - The caller's pid comes from `GetNamedPipeClientProcessId`
//!   (`client_process_id`). It must name a process that runs and started no
//!   later than the connection was accepted ([`peer_pid`]), so a pid that a
//!   later process took over is refused; the descendant check is the caller's.
//! - Only local callers: `PIPE_REJECT_REMOTE_CLIENTS` (`accept_remote` off).
//! - The first instance is made with `FILE_FLAG_FIRST_PIPE_INSTANCE`: when the
//!   name exists already (another process made it first), [`bind`] fails; it
//!   never joins someone else's pipe.
//! - The pipe's access list lets only this user and `SYSTEM` open it, not the
//!   default one that lets every user open it for reading.
//! - Reads and writes take the time-outs set on the stream, and a time-out
//!   cancels the operation, so a stalled caller holds only its own connection.
//!   A reply still in flight when the stream is dropped is delivered by the
//!   runtime, and then the pipe closes (end of stream for the caller).
//! - [`wake`] connects to the pipe, which ends a blocking accept.

use std::cell::Cell;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use interprocess::os::windows::named_pipe::pipe_mode::Bytes;
use interprocess::os::windows::named_pipe::tokio::{PipeListener, PipeStream};
use interprocess::os::windows::named_pipe::{PipeListenerOptions, PipeMode};
use interprocess::os::windows::security_descriptor::SecurityDescriptor;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::runtime::Runtime;
use widestring::U16CString;

/// How long [`wake`] tries to reach the pipe.
const WAKE_LIMIT: Duration = Duration::from_secs(2);
/// The pause after a failed accept.
const ACCEPT_RETRY: Duration = Duration::from_millis(50);

/// The runtime every pipe runs on. It lives as long as the process, so a
/// reply still being written when its stream is dropped is finished.
fn runtime() -> io::Result<&'static Runtime> {
    static RUNTIME: OnceLock<Result<Runtime, String>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("connecto-pipes")
                .enable_io()
                .enable_time()
                .build()
                .map_err(|err| err.to_string())
        })
        .as_ref()
        .map_err(|err| io::Error::other(format!("the pipe runtime did not start: {err}")))
}

/// This process's user, as a SID string (`S-1-5-21-...`).
fn own_user_sid() -> io::Result<&'static str> {
    static SID: OnceLock<Option<String>> = OnceLock::new();
    SID.get_or_init(|| {
        use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
        let pid = Pid::from_u32(std::process::id());
        let mut system = System::new().ok()?;
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_user(UpdateKind::Always),
        );
        let sid = system.process(pid)?.user_id()?.to_string();
        is_sid(&sid).then_some(sid)
    })
    .as_deref()
    .ok_or_else(|| io::Error::other("this process's user could not be read"))
}

/// A SID in its string form: `S-1-` and then digits and dashes only.
fn is_sid(text: &str) -> bool {
    text.strip_prefix("S-1-").is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'-')
    })
}

/// The pipe's security descriptor: a protected access list that gives full
/// access to `user` and to `SYSTEM`, and nothing to anyone else.
fn owner_only(user: &str) -> io::Result<SecurityDescriptor> {
    let sddl = format!("D:P(A;;GA;;;{user})(A;;GA;;;SY)");
    let wide = U16CString::from_str(&sddl)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad security descriptor"))?;
    SecurityDescriptor::deserialize(&wide)
}

/// A bound pipe.
pub(crate) struct Listener {
    pipe: PipeListener<Bytes, Bytes>,
    runtime: &'static Runtime,
}

impl Listener {
    /// Callers, one at a time, without end; each waits for the next caller.
    pub(crate) fn incoming(&self) -> impl Iterator<Item = io::Result<Stream>> + '_ {
        std::iter::repeat_with(move || self.accept())
    }

    fn accept(&self) -> io::Result<Stream> {
        let pipe = match self.runtime.block_on(self.pipe.accept()) {
            Ok(pipe) => pipe,
            Err(err) => {
                // A failure that repeats (no new pipe instance can be made)
                // must not spin the accept loop.
                std::thread::sleep(ACCEPT_RETRY);
                return Err(err);
            }
        };
        let accepted = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        Ok(Stream {
            pipe,
            runtime: self.runtime,
            accepted,
            read_timeout: Cell::new(None),
            write_timeout: Cell::new(None),
        })
    }
}

/// One caller's connection.
pub(crate) struct Stream {
    pipe: PipeStream<Bytes, Bytes>,
    runtime: &'static Runtime,
    /// When it was accepted, in whole seconds since the Unix epoch.
    accepted: u64,
    read_timeout: Cell<Option<Duration>>,
    write_timeout: Cell<Option<Duration>>,
}

impl Stream {
    pub(crate) fn set_read_timeout(&self, limit: Option<Duration>) -> io::Result<()> {
        self.read_timeout.set(limit);
        Ok(())
    }

    pub(crate) fn set_write_timeout(&self, limit: Option<Duration>) -> io::Result<()> {
        self.write_timeout.set(limit);
        Ok(())
    }

    /// Run `op` on the runtime, for at most `limit`. A time-out drops (and so
    /// cancels) the operation.
    fn within<T>(
        &self,
        limit: Option<Duration>,
        op: impl Future<Output = io::Result<T>>,
    ) -> io::Result<T> {
        self.runtime.block_on(async move {
            match limit {
                None => op.await,
                Some(limit) => tokio::time::timeout(limit, op).await.unwrap_or_else(|_| {
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "the named pipe timed out",
                    ))
                }),
            }
        })
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut pipe = &self.pipe;
        self.within(self.read_timeout.get(), pipe.read(buf))
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut pipe = &self.pipe;
        self.within(self.write_timeout.get(), pipe.write(buf))
    }

    /// Nothing to wait for: what was written is on its way (see the Drop).
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // The runtime finishes a write still in flight and closes the pipe
        // after it, so `interprocess`'s flushing thread (which would wait for
        // the caller to read, however long that takes) is not needed.
        self.pipe.assume_flushed();
    }
}

/// Bind the pipe `path` (`\\.\pipe\<name>`). Fails when the name is taken.
pub(crate) fn bind(path: &Path) -> io::Result<Listener> {
    let runtime = runtime()?;
    let name = U16CString::from_os_str(path.as_os_str())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a pipe name has no NUL"))?;
    let options = PipeListenerOptions::new()
        .path(name)
        .mode(PipeMode::Bytes)
        .accept_remote(false)
        .security_descriptor(Some(owner_only(own_user_sid()?)?));
    // The pipe is registered with the runtime's reactor, so it is made inside it.
    let _inside = runtime.enter();
    let pipe = options.create_tokio_duplex::<Bytes>()?;
    Ok(Listener { pipe, runtime })
}

/// The pid of the process on the other end, if that process still runs and
/// started no later than the connection was accepted.
pub(crate) fn peer_pid(stream: &Stream) -> io::Result<i32> {
    let pid = i32::try_from(stream.pipe.client_process_id()?)
        .map_err(|_| io::Error::other("the caller's pid is out of range"))?;
    if crate::process_tree::started_by(pid, stream.accepted) {
        Ok(pid)
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the caller is gone, or its pid now names a later process",
        ))
    }
}

/// Wake a blocking accept on `path`, which then sees its stop flag.
pub(crate) fn wake(path: &Path) {
    let _ = connecto_hook::transport::connect(path, WAKE_LIMIT);
}

/// Nothing to remove: a pipe goes with its server.
pub(crate) const fn remove(_path: &Path) {}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Instant;

    use connecto_hook::transport;

    use super::*;

    fn name(what: &str) -> std::path::PathBuf {
        let random = crate::platform::random_bytes::<8>().unwrap();
        let hex: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        std::path::PathBuf::from(format!(r"\\.\pipe\connecto-test-{what}-{hex}"))
    }

    /// Serve `count` callers on a thread: each gets its pid checked, its
    /// request read (up to 5 bytes) and echoed back with the pid, then closed.
    fn echo_server(
        listener: Listener,
        count: usize,
        read_limit: Duration,
    ) -> thread::JoinHandle<Vec<io::Result<i32>>> {
        thread::spawn(move || {
            let mut seen = Vec::new();
            for stream in listener.incoming().take(count) {
                let mut stream = stream.unwrap();
                let pid = peer_pid(&stream);
                stream.set_read_timeout(Some(read_limit)).unwrap();
                let mut request = [0_u8; 5];
                match stream.read_exact(&mut request) {
                    Ok(()) => {
                        stream.set_write_timeout(Some(read_limit)).unwrap();
                        stream.write_all(&request).unwrap();
                        seen.push(pid);
                    }
                    Err(err) => seen.push(Err(err)),
                }
            }
            seen
        })
    }

    #[test]
    fn a_caller_is_answered_and_known_by_its_pid() {
        let path = name("echo");
        let server = echo_server(bind(&path).unwrap(), 1, Duration::from_secs(5));
        let mut client = transport::connect(&path, Duration::from_secs(5)).unwrap();
        client.write_all(b"hello").unwrap();
        let mut reply = Vec::new();
        // The server closes after replying: the reply ends at end of stream.
        client.read_to_end(&mut reply).unwrap();
        assert_eq!(reply, b"hello");
        let seen = server.join().unwrap();
        assert_eq!(seen.len(), 1);
        let me = i32::try_from(std::process::id()).unwrap();
        assert_eq!(*seen[0].as_ref().unwrap(), me);
    }

    #[test]
    fn a_large_reply_arrives_whole_after_the_stream_is_dropped() {
        let path = name("large");
        let listener = bind(&path).unwrap();
        let body: Vec<u8> = (0..600_000_u32).map(|n| (n % 251) as u8).collect();
        let sent = body.clone();
        let server = thread::spawn(move || {
            let mut stream = listener.incoming().next().unwrap().unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            stream.write_all(&sent).unwrap();
            drop(stream);
        });
        let mut client = transport::connect(&path, Duration::from_secs(10)).unwrap();
        server.join().unwrap();
        let mut got = Vec::new();
        client.read_to_end(&mut got).unwrap();
        assert_eq!(got.len(), body.len());
        assert!(got == body);
    }

    #[test]
    fn a_name_that_is_taken_cannot_be_bound_again() {
        let path = name("taken");
        let first = bind(&path).unwrap();
        let second = bind(&path);
        assert!(second.is_err(), "a second listener bound a taken name");
        drop(first);
    }

    #[test]
    fn a_stalled_caller_does_not_hold_up_the_next() {
        let path = name("stall");
        let server = echo_server(bind(&path).unwrap(), 2, Duration::from_millis(300));
        // Connects, sends two bytes of five, and then nothing.
        let mut stalled = transport::connect(&path, Duration::from_secs(5)).unwrap();
        stalled.write_all(b"he").unwrap();
        let started = Instant::now();
        let mut client = transport::connect(&path, Duration::from_secs(5)).unwrap();
        client.write_all(b"world").unwrap();
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).unwrap();
        assert_eq!(reply, b"world");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        let seen = server.join().unwrap();
        assert_eq!(
            seen[0].as_ref().err().map(io::Error::kind),
            Some(io::ErrorKind::TimedOut)
        );
        assert!(seen[1].is_ok());
        drop(stalled);
    }

    #[test]
    fn garbage_and_an_early_hang_up_do_not_stop_the_listener() {
        let path = name("garbage");
        let server = echo_server(bind(&path).unwrap(), 3, Duration::from_millis(500));
        // Hangs up at once, before anything is read.
        drop(transport::connect(&path, Duration::from_secs(5)).unwrap());
        // Sends three bytes and hangs up.
        let mut short = transport::connect(&path, Duration::from_secs(5)).unwrap();
        short.write_all(b"xyz").unwrap();
        drop(short);
        let mut client = transport::connect(&path, Duration::from_secs(5)).unwrap();
        client.write_all(b"after").unwrap();
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).unwrap();
        assert_eq!(reply, b"after");
        let seen = server.join().unwrap();
        assert!(
            seen[0].is_err() && seen[1].is_err() && seen[2].is_ok(),
            "{seen:?}"
        );
    }

    #[test]
    fn waking_ends_a_blocking_accept() {
        let path = name("wake");
        let listener = bind(&path).unwrap();
        let (accepted, done) = mpsc::channel();
        let server = thread::spawn(move || {
            let first = listener.incoming().next();
            accepted.send(first.is_some()).unwrap();
            drop(listener);
        });
        thread::sleep(Duration::from_millis(100));
        wake(&path);
        assert!(done.recv_timeout(Duration::from_secs(5)).unwrap());
        server.join().unwrap();
        // The pipe went with its listener.
        assert!(transport::connect(&path, Duration::from_millis(200)).is_err());
    }

    #[test]
    fn a_process_that_started_after_the_accept_is_not_the_caller() {
        let path = name("late");
        let listener = bind(&path).unwrap();
        let server = thread::spawn(move || listener.incoming().next().unwrap().unwrap());
        let _client = transport::connect(&path, Duration::from_secs(5)).unwrap();
        let mut stream = server.join().unwrap();
        assert!(peer_pid(&stream).is_ok());
        // As if the connection had been accepted before this process started:
        // its pid would then name a later process than the caller.
        stream.accepted = 1;
        assert_eq!(
            peer_pid(&stream).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    /// The pipe's access list, as another program reads it: this user and
    /// SYSTEM only (the default would let Everyone and Anonymous read).
    #[test]
    fn only_this_user_and_system_may_open_the_pipe() {
        let path = name("acl");
        let listener = bind(&path).unwrap();
        let server = thread::spawn(move || {
            let _ = listener.incoming().next();
        });
        let leaf = path.file_name().unwrap().to_str().unwrap().to_owned();
        let script = format!(
            "$c = New-Object System.IO.Pipes.NamedPipeClientStream('.', '{leaf}', \
             [System.IO.Pipes.PipeDirection]::InOut); $c.Connect(5000); \
             $c.GetAccessControl().GetSecurityDescriptorSddlForm('Access'); $c.Dispose()"
        );
        let out = std::process::Command::new("powershell.exe")
            .args(["-NoLogo", "-NoProfile", "-Command", &script])
            .output()
            .unwrap();
        server.join().unwrap();
        let sddl = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        assert!(sddl.starts_with("D:P"), "{sddl}");
        let mut trustees: Vec<&str> = sddl
            .split('(')
            .skip(1)
            .filter_map(|ace| ace.trim_end_matches(')').rsplit(';').next())
            .collect();
        // Windows may store the entries in another order.
        trustees.sort_unstable();
        let mut want = [own_user_sid().unwrap(), "SY"];
        want.sort_unstable();
        assert_eq!(trustees, want, "{sddl}");
    }

    #[test]
    fn only_sid_strings_go_into_the_access_list() {
        assert!(is_sid(own_user_sid().unwrap()));
        assert!(is_sid("S-1-5-21-1-2-3-1001"));
        for bad in ["", "S-1-", "S-1-5)(A;;GA;;;WD", "s-1-5-18", "S-1-5-x"] {
            assert!(!is_sid(bad), "{bad}");
        }
        assert!(owner_only(own_user_sid().unwrap()).is_ok());
    }
}
