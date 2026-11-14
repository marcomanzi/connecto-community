//! What differs between the platforms for the workspace: how a project's path
//! is shown, which saved folder setting counts as absolute, and how a saved
//! file is replaced.
//!
//! A project's identity is the same everywhere: the id of its real path
//! (`fs::canonicalize`). Only what the user sees and how files are replaced
//! differ, each in its own small module.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::*;
