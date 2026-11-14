//! What a host asks before it lets a remote peer do anything. The trait lives
//! in `connecto-core`, and the Gate-backed implementation in `connecto-session`
//! (only the session crate may depend on the Gate), so this crate sees only
//! the contract.

pub use connecto_core::{GuardDenied, RemoteGuard, RemotePeer as Peer};

use crate::wire::DenyReason;

/// The reason a peer is told for a refusal by the guard.
#[must_use]
pub const fn deny_reason(denied: GuardDenied) -> DenyReason {
    match denied {
        GuardDenied::Paused => DenyReason::Paused,
        GuardDenied::Refused => DenyReason::Protocol,
    }
}
