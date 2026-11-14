//! The server end of the hook and bridge channels, and who is calling.
//!
//! On Unix it is a Unix domain socket at a path, and the caller's pid comes
//! from the socket (`LOCAL_PEERPID`). On Windows it is a named pipe
//! (`\\.\pipe\<name>`), and the caller's pid comes from
//! `GetNamedPipeClientProcessId`. Never TCP: the server must know the caller's
//! process, which a loopback port cannot tell.
//!
//! Each platform's module offers the same names: `Listener` (with
//! `incoming`), `Stream` (`Read`, `Write`, read and write timeouts), `bind`,
//! `peer_pid`, `wake` and `remove`.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::*;
