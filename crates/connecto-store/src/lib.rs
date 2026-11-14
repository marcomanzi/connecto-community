//! Private, immutable copies of the CLIs.
//!
//! Sessions run these copies by absolute path, never the shared install. They
//! are copied read-only from locally installed, signed binaries. Credentials
//! and profiles are never copied: for Codex the release folder sits under
//! `~/.codex`, so only the entries in [`Tool::release_entries`] are touched.
//!
//! `claude install`, `claude update` and `codex update` are never run.

mod discover;
mod hash;
mod platform;
mod signature;
mod store;

pub use discover::{is_launcher, prefetch_login_path, run_path};
pub use signature::{Codesign, Failure, Signature};
pub use store::{
    Installed, Store, StoreError, Tool, data_dir_override, data_root, describe_list, home_dir,
    shown_path,
};
