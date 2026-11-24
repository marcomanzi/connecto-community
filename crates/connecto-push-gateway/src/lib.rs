//! Connecto's push gateway: the one service that holds the team's APNs key,
//! taking each enrolled host's Siri notifications and handing them to APNs
//! (research_notes/siri-intents/push-gateway.md).
//!
//! [`auth`] is all a host needs (it signs its requests); the gateway itself
//! is behind the `server` feature.

pub mod auth;

/// The SSH key types a host signs with.
pub use ssh_key;

#[cfg(feature = "server")]
pub mod apns;
#[cfg(feature = "fake-apns")]
pub mod fake_apns;
#[cfg(feature = "server")]
pub mod server;
