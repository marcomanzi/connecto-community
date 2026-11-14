//! Windows: a named pipe client, opened as a file (safe std only).
//!
//! The endpoint must be a local pipe, `\\.\pipe\<name>` with `<name>` one path
//! component, so a bad environment value can never open or write a real file.
//! The pipe is opened at the identification level: the server may learn who
//! the caller is (and its process id with `GetNamedPipeClientProcessId`) but
//! may not act as the caller. It must be a byte-mode pipe.
//!
//! A `File` has no read or write timeout. Each read and write therefore runs
//! on a short-lived helper thread and the caller waits at most `timeout`; on
//! time-out the caller gets `TimedOut` and the helper stays blocked until the
//! server closes the pipe or the process exits (the hook and the `connecto`
//! command are short-lived processes, so nothing piles up).

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Every endpoint starts with this (compared ignoring ASCII case).
pub const PIPE_PREFIX: &str = r"\\.\pipe\";

/// All instances of the pipe are taken: wait and try again.
const ERROR_PIPE_BUSY: i32 = 231;
/// The longest a busy pipe is retried (and never longer than the timeout).
const BUSY_WAIT: Duration = Duration::from_secs(2);
const BUSY_STEP: Duration = Duration::from_millis(20);

/// `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION` (WinBase.h).
const IDENTIFICATION_ONLY: u32 = 0x0010_0000 | 0x0001_0000;

/// A connected pipe; reads and writes wait at most the timeout given to
/// [`connect`].
#[derive(Debug)]
pub struct Stream {
    pipe: File,
    timeout: Duration,
}

fn check_endpoint(endpoint: &Path) -> io::Result<()> {
    let text = endpoint.as_os_str().to_string_lossy();
    let name = text
        .get(..PIPE_PREFIX.len())
        .filter(|prefix| prefix.eq_ignore_ascii_case(PIPE_PREFIX))
        .and_then(|_| text.get(PIPE_PREFIX.len()..))
        .unwrap_or("");
    if name.is_empty() || name.contains(['\\', '/']) || name == "." || name == ".." {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a local named pipe ({PIPE_PREFIX}<name>): {text}"),
        ));
    }
    Ok(())
}

/// Connect to the named pipe `endpoint`. A busy pipe is retried for a short,
/// bounded time; a missing one fails at once.
pub fn connect(endpoint: &Path, timeout: Duration) -> io::Result<Stream> {
    check_endpoint(endpoint)?;
    let deadline = Instant::now() + timeout.min(BUSY_WAIT);
    loop {
        match OpenOptions::new()
            .read(true)
            .write(true)
            .security_qos_flags(IDENTIFICATION_ONLY)
            .open(endpoint)
        {
            Ok(pipe) => return Ok(Stream { pipe, timeout }),
            Err(err)
                if err.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < deadline =>
            {
                thread::sleep(BUSY_STEP);
            }
            Err(err) => return Err(err),
        }
    }
}

/// Run `op` on a helper thread and wait for it at most `timeout`.
fn within<T: Send + 'static>(
    timeout: Duration,
    op: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    let (done, result) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("connecto-pipe-io".to_owned())
        .spawn(move || {
            let _ = done.send(op());
        })?;
    match result.recv_timeout(timeout) {
        Ok(outcome) => outcome,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the named pipe did not answer in time",
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(io::Error::other("the named pipe helper thread failed"))
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut pipe = self.pipe.try_clone()?;
        let want = buf.len();
        let data = within(self.timeout, move || {
            let mut data = vec![0_u8; want];
            let got = pipe.read(&mut data)?;
            data.truncate(got);
            Ok(data)
        })?;
        let into = buf
            .get_mut(..data.len())
            .ok_or_else(|| io::Error::other("the named pipe read too much"))?;
        into.copy_from_slice(&data);
        Ok(data.len())
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut pipe = self.pipe.try_clone()?;
        let data = buf.to_vec();
        within(self.timeout, move || pipe.write(&data))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use tokio::runtime::Runtime;

    use super::*;
    use crate::bridge::{Op, Reply};

    fn pipe_name(what: &str) -> String {
        format!(r"\\.\pipe\connecto-test-{what}-{}", std::process::id())
    }

    /// A runtime and a first pipe instance, created before any client tries.
    fn server(name: &str) -> (Runtime, NamedPipeServer) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .unwrap();
        let server = {
            let _inside = runtime.enter();
            ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create(name)
                .unwrap()
        };
        (runtime, server)
    }

    /// Serve one request: read a frame, answer with a reply, then close.
    async fn answer(server: &mut NamedPipeServer, reply: &Reply) -> Vec<u8> {
        server.connect().await.unwrap();
        let mut len = [0_u8; 4];
        server.read_exact(&mut len).await.unwrap();
        let mut body = vec![0_u8; usize::try_from(u32::from_be_bytes(len)).unwrap()];
        server.read_exact(&mut body).await.unwrap();
        let mut bytes = Vec::new();
        reply.write_to(&mut bytes).unwrap();
        server.write_all(&bytes).await.unwrap();
        body
    }

    #[test]
    fn a_request_goes_over_the_pipe_and_the_whole_reply_comes_back() {
        let name = pipe_name("request");
        let (runtime, mut server) = server(&name);
        let reply = Reply::ok(serde_json::json!({ "id": "r1" })).with_body(vec![7; 100_000]);
        let sent = reply.clone();
        let (done, client_done) = mpsc::sync_channel::<()>(1);
        let serving = thread::spawn(move || {
            runtime.block_on(async move {
                let body = answer(&mut server, &sent).await;
                drop(server);
                // A large write is still in flight: the pipe closes (end of
                // stream for the client) only once the runtime has seen it
                // finish, so keep the runtime running until the client is done.
                let _ = tokio::task::spawn_blocking(move || client_done.recv()).await;
                body
            })
        });
        let got = crate::client::request(Path::new(&name), &Op::List).unwrap();
        done.send(()).unwrap();
        assert_eq!(got, reply);
        let body = serving.join().unwrap();
        let frame = crate::decode(&body).unwrap();
        assert_eq!(frame.token, b"");
        assert_eq!(Op::parse(frame.payload).unwrap(), Op::List);
    }

    #[test]
    fn a_busy_pipe_is_retried_until_an_instance_is_free() {
        let name = pipe_name("busy");
        let (runtime, first) = server(&name);
        let (taken, take) = mpsc::sync_channel(1);
        let pipe = name.clone();
        let serving = thread::spawn(move || {
            runtime.block_on(async move {
                first.connect().await.unwrap();
                taken.send(()).unwrap();
                // Every instance is in use: a second client sees the pipe busy
                // until this one exists.
                tokio::time::sleep(Duration::from_millis(300)).await;
                let mut second = ServerOptions::new().create(&pipe).unwrap();
                let reply = Reply::ok(serde_json::json!({}));
                answer(&mut second, &reply).await;
                drop(second);
                drop(first);
            });
        });
        let _holder = connect(Path::new(&name), Duration::from_secs(5)).unwrap();
        take.recv().unwrap();
        let busy = OpenOptions::new().read(true).write(true).open(&name);
        assert_eq!(busy.unwrap_err().raw_os_error(), Some(ERROR_PIPE_BUSY));
        assert!(crate::client::request(Path::new(&name), &Op::List).is_ok());
        serving.join().unwrap();
    }

    #[test]
    fn a_silent_server_times_out_the_read() {
        let name = pipe_name("silent");
        let (runtime, server) = server(&name);
        let (finish, finished) = mpsc::sync_channel::<()>(1);
        let serving = thread::spawn(move || {
            runtime.block_on(async move {
                server.connect().await.unwrap();
                let _ = tokio::task::spawn_blocking(move || finished.recv()).await;
                drop(server);
            });
        });
        let mut stream = connect(Path::new(&name), Duration::from_millis(200)).unwrap();
        stream.write_all(b"hello").unwrap();
        let started = Instant::now();
        let err = stream.read(&mut [0_u8; 1]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
        finish.send(()).unwrap();
        serving.join().unwrap();
    }

    #[test]
    fn only_a_local_pipe_with_a_one_component_name_is_an_endpoint() {
        for good in [r"\\.\pipe\connecto-hook-1", r"\\.\PIPE\connecto-bridge"] {
            assert!(check_endpoint(Path::new(good)).is_ok(), "{good}");
        }
        for bad in [
            "/nonexistent/bridge.sock",
            r"C:\Users\x\bridge.sock",
            r"\\.\pipe\",
            r"\\.\pipe\..",
            r"\\.\pipe\..\C:\x",
            r"\\.\pipe\a/b",
            r"\\server\pipe\connecto",
            r"\\?\pipe\connecto",
            "",
        ] {
            let err = check_endpoint(Path::new(bad)).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{bad}");
        }
    }

    #[test]
    fn a_missing_pipe_fails_at_once() {
        let started = Instant::now();
        let name = format!(r"\\.\pipe\connecto-test-missing-{}", std::process::id());
        assert!(connect(Path::new(&name), Duration::from_secs(5)).is_err());
        assert!(started.elapsed() < BUSY_WAIT);
    }
}
