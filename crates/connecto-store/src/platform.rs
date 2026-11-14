//! What differs between the platforms: where the user's home and the data
//! folder are, file modes, whether a process is alive, how the `connecto`
//! command is made beside the helper, and the names of the programs.
//!
//! Each platform has its own small module; the rest of the crate only uses
//! the names re-exported here.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;
