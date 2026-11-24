//! The publication gate shared by every cycle (development, dream, initial
//! build): whether a run may still write what it produced.
//!
//! A run takes a [`RunGate`] when it starts (it records the pause epoch) and
//! calls [`RunGate::check`] **inside its writer closure, in the transaction,
//! right before the commit**. Checking on the run's own thread is not
//! enough: the write may wait behind other writer jobs while a pause or Stop
//! Everything happens.
//!
//! The gate is closed when:
//! - the app is paused, or was paused and resumed since the run started (the
//!   pause epoch changed);
//! - the run was cancelled: Stop Everything, the app closing, memory turned
//!   off for the project, or the project closed (the engine sets the run's
//!   cancel flag in every one of those cases).
//!
//! On [`StoreError::Aborted`] the caller rolls back (dropping the
//! transaction does) and puts its work back with no attempt used.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::StoreError;
use crate::host::Host;

/// Why the gate is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Closed {
    /// Paused now, or a pause came and went during the run.
    Paused,
    /// Stop Everything, closing, memory off, project closed.
    Cancelled,
}

/// A run's right to publish. Cheap to clone; `Send + Sync + 'static`, so it
/// can move into a writer closure.
#[derive(Clone)]
pub struct RunGate {
    host: Arc<dyn Host>,
    epoch: u64,
    cancel: Arc<AtomicBool>,
}

impl RunGate {
    /// Open now: records the current pause epoch.
    #[must_use]
    pub fn new(host: Arc<dyn Host>, cancel: Arc<AtomicBool>) -> Self {
        let epoch = host.pause_epoch();
        Self {
            host,
            epoch,
            cancel,
        }
    }

    /// The epoch the run started under.
    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The run's cancel flag (what the model call watches too).
    #[must_use]
    pub fn cancel_flag(&self) -> &AtomicBool {
        &self.cancel
    }

    /// Why the run may no longer publish, if it may not.
    #[must_use]
    pub fn closed(&self) -> Option<Closed> {
        if self.cancel.load(Ordering::SeqCst) {
            Some(Closed::Cancelled)
        } else if self.host.paused() || self.host.pause_epoch() != self.epoch {
            Some(Closed::Paused)
        } else {
            None
        }
    }

    /// `Err(StoreError::Aborted)` when closed. Call it inside the writer
    /// closure, in the transaction, just before the commit.
    pub fn check(&self) -> Result<(), StoreError> {
        match self.closed() {
            None => Ok(()),
            Some(_) => Err(StoreError::Aborted),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeHost;

    #[test]
    fn a_pause_or_a_cancel_closes_the_gate_for_good() {
        let host = Arc::new(FakeHost::new());
        let cancel = Arc::new(AtomicBool::new(false));
        let gate = RunGate::new(Arc::clone(&host) as Arc<dyn Host>, Arc::clone(&cancel));
        assert_eq!(gate.closed(), None);
        assert!(gate.check().is_ok());
        host.set_paused(true);
        assert_eq!(gate.closed(), Some(Closed::Paused));
        host.set_paused(false);
        // Resumed, but the epoch moved: still closed.
        assert_eq!(gate.closed(), Some(Closed::Paused));
        let gate = RunGate::new(Arc::clone(&host) as Arc<dyn Host>, Arc::clone(&cancel));
        cancel.store(true, Ordering::SeqCst);
        assert!(matches!(gate.check(), Err(StoreError::Aborted)));
    }
}
