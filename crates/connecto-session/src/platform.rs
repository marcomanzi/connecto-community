//! What differs between the platforms for launching: private folders, the
//! socket folder, random bytes, whether a process is gone, the inherited
//! environment, the shell and its flags, and which home folders hold state.
//!
//! Each platform has its own small module; the rest of the crate only uses the
//! names re-exported here. The process tree is in [`crate::process_tree`] and
//! the hook and bridge sockets in [`crate::local_socket`].

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::SHELL;
#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::SHELL;
#[cfg(windows)]
pub(crate) use windows::*;
