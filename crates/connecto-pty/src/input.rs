//! The bounded queue between the UI thread and a session's writer thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use connecto_core::InputRefused;
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};

/// Bytes that may wait for the writer thread before input is refused.
pub const MAX_QUEUED_INPUT_BYTES: usize = 1024 * 1024;

/// Separate writes that may wait, whatever their size.
const MAX_QUEUED_WRITES: usize = 4096;

#[derive(Debug, Default)]
struct QueueState {
    queued_bytes: AtomicUsize,
    closed: AtomicBool,
}

/// Non-blocking producer side. Cheap to clone.
#[derive(Debug, Clone)]
pub struct InputSender {
    tx: Sender<Vec<u8>>,
    state: Arc<QueueState>,
}

/// Consumer side, owned by the writer thread.
#[derive(Debug)]
pub struct InputReceiver {
    rx: Receiver<Vec<u8>>,
    state: Arc<QueueState>,
}

/// Create a bounded input queue.
#[must_use]
pub fn input_channel() -> (InputSender, InputReceiver) {
    let (tx, rx) = bounded(MAX_QUEUED_WRITES);
    let state = Arc::new(QueueState::default());
    (
        InputSender {
            tx,
            state: Arc::clone(&state),
        },
        InputReceiver { rx, state },
    )
}

impl InputSender {
    /// Queue bytes without ever blocking.
    ///
    /// A write is accepted whole or refused whole, so a paste is never cut in
    /// half by a full queue.
    pub fn try_send(&self, bytes: Vec<u8>) -> Result<(), InputRefused> {
        if self.state.closed.load(Ordering::Acquire) {
            return Err(InputRefused::Closed);
        }
        let len = bytes.len();
        let reserved = self.state.queued_bytes.fetch_add(len, Ordering::AcqRel);
        if reserved.saturating_add(len) > MAX_QUEUED_INPUT_BYTES {
            self.state.queued_bytes.fetch_sub(len, Ordering::AcqRel);
            return Err(InputRefused::QueueFull);
        }
        self.tx.try_send(bytes).map_err(|err| {
            self.state.queued_bytes.fetch_sub(len, Ordering::AcqRel);
            match err {
                TrySendError::Full(_) => InputRefused::QueueFull,
                TrySendError::Disconnected(_) => InputRefused::Closed,
            }
        })
    }

    /// Refuse all further input.
    pub fn close(&self) {
        self.state.closed.store(true, Ordering::Release);
    }
}

impl InputReceiver {
    /// Wait for the next write. `None` once every sender is gone.
    pub(crate) fn recv(&self) -> Option<Vec<u8>> {
        self.rx.recv().ok()
    }

    /// Record that a write left the queue.
    pub(crate) fn mark_written(&self, len: usize) {
        self.state.queued_bytes.fetch_sub(len, Ordering::AcqRel);
    }

    /// Refuse all further input, because the PTY no longer accepts writes.
    pub(crate) fn close(&self) {
        self.state.closed.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_past_the_byte_limit_without_blocking() {
        let (tx, _rx) = input_channel();
        assert_eq!(tx.try_send(vec![0; MAX_QUEUED_INPUT_BYTES]), Ok(()));
        assert_eq!(tx.try_send(vec![0]), Err(InputRefused::QueueFull));
    }

    #[test]
    fn refuses_past_the_write_count_limit_without_blocking() {
        let (tx, _rx) = input_channel();
        for _ in 0..MAX_QUEUED_WRITES {
            assert_eq!(tx.try_send(vec![0]), Ok(()));
        }
        assert_eq!(tx.try_send(vec![0]), Err(InputRefused::QueueFull));
    }

    #[test]
    fn a_refused_write_releases_its_reservation() {
        let (tx, _rx) = input_channel();
        assert_eq!(
            tx.try_send(vec![0; MAX_QUEUED_INPUT_BYTES + 1]),
            Err(InputRefused::QueueFull)
        );
        assert_eq!(tx.try_send(vec![0; MAX_QUEUED_INPUT_BYTES]), Ok(()));
    }

    #[test]
    fn written_bytes_free_capacity() {
        let (tx, rx) = input_channel();
        assert_eq!(tx.try_send(vec![0; MAX_QUEUED_INPUT_BYTES]), Ok(()));
        let write = rx.recv().unwrap_or_default();
        rx.mark_written(write.len());
        assert_eq!(tx.try_send(vec![0; MAX_QUEUED_INPUT_BYTES]), Ok(()));
    }

    #[test]
    fn closing_refuses_input() {
        let (tx, _rx) = input_channel();
        tx.close();
        assert_eq!(tx.try_send(vec![0]), Err(InputRefused::Closed));
    }

    #[test]
    fn a_dropped_receiver_refuses_input() {
        let (tx, rx) = input_channel();
        drop(rx);
        assert_eq!(tx.try_send(vec![0]), Err(InputRefused::Closed));
    }
}
