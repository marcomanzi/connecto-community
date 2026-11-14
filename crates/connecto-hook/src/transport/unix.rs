//! Unix: a Unix domain socket at a path, with read and write timeouts.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

pub type Stream = UnixStream;

/// Connect to the socket at `endpoint`; every read and write waits at most
/// `timeout`.
pub fn connect(endpoint: &Path, timeout: Duration) -> io::Result<Stream> {
    let stream = UnixStream::connect(endpoint)?;
    stream.set_write_timeout(Some(timeout))?;
    stream.set_read_timeout(Some(timeout))?;
    Ok(stream)
}
