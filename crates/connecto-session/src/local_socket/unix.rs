//! Unix side of [`crate::local_socket`]: Unix domain sockets.

use std::fs;
use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use nix::sys::socket::{getsockopt, sockopt::LocalPeerPid};

pub(crate) type Listener = UnixListener;
pub(crate) type Stream = UnixStream;

/// Bind the socket at `path`. A stale file from a crashed run is replaced.
pub(crate) fn bind(path: &Path) -> io::Result<Listener> {
    match fs::remove_file(path) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
        _ => {}
    }
    UnixListener::bind(path)
}

/// The pid of the process on the other end.
pub(crate) fn peer_pid(stream: &Stream) -> io::Result<i32> {
    getsockopt(stream, LocalPeerPid).map_err(io::Error::from)
}

/// Wake a blocking accept on `path`, which then sees its stop flag.
pub(crate) fn wake(path: &Path) {
    let _ = UnixStream::connect(path);
}

/// Remove the socket's file.
pub(crate) fn remove(path: &Path) {
    let _ = fs::remove_file(path);
}
