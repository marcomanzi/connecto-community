//! The local channel to the app: one connection per request, framed as in
//! the crate root. On Unix it is a Unix domain socket at a path; on Windows it
//! is a named pipe, `\\.\pipe\<name>`. The endpoint reaches the hook the same
//! way on both (`CONNECTO_HOOK_SOCK`, `CONNECTO_BRIDGE_SOCK`); only what the
//! value names differs. Never TCP: the server must know the caller's process.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{Stream, connect};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{PIPE_PREFIX, Stream, connect};
