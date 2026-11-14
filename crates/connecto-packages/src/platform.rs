//! What differs between the platforms for packages: file modes (Unix has
//! them, Windows does not), how a link is removed, which file names Windows
//! cannot hold, which home folders hold state, and the user's home variable.
//!
//! Each platform has its own small module; the rest of the crate only uses the
//! names re-exported here.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::*;
