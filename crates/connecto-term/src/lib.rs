//! The terminal emulator: `alacritty_terminal` behind connecto's escape policy.
//!
//! All child output is untrusted. [`policy`] decides what an escape sequence
//! may reach outside the grid: the clipboard never, titles only sanitized, and
//! replies to the child only for queries real TUIs depend on.

mod color;
mod emulator;
mod policy;

pub use emulator::{Emulator, Responder};
pub use policy::{MAX_TITLE_CHARS, sanitize_title};
