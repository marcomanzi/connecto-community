//! PTY process hosting: spawn, read until EOF, write, resize and signal.
//!
//! Each session owns three dedicated threads, so nothing here touches the UI
//! thread except the non-blocking [`InputSender`] and [`PtySession::resize`]:
//!
//! - the **reader** drains the master until EOF, even after the child died,
//!   because a child can stall on a full PTY buffer and descendants can keep
//!   the slave open;
//! - the **writer** owns the master's write half, so a child that stops
//!   reading can only ever block this thread;
//! - the **waiter** reaps the child, independently of the reader, so the exit
//!   status is reported even when a descendant keeps the PTY open.

mod env;
mod input;
mod process;
mod session;

pub use env::{ENV_ALLOWLIST, child_env};
pub use input::{InputReceiver, InputSender, MAX_QUEUED_INPUT_BYTES, input_channel};
pub use session::{
    MAX_READ_SLICE, OpenPty, OutputSink, PtyError, PtySession, PtySpec, SIGKILL_GRACE, Wake,
};
