//! Remote Connecto sessions: wire protocol, authentication and the host and
//! client halves of a terminal stream (ADR 0017).
//!
//! Nothing here touches the UI thread. The transports are blocking traits; the
//! iroh transport keeps its async runtime on one thread of its own.

pub mod auth;
pub mod client;
pub mod guard;
pub mod host;
pub mod hosting;
pub mod iroh_transport;
pub mod manager;
pub mod pairing;
pub mod reconnect;
pub mod relay_access;
pub mod remote_terminals;
pub mod repaint;
pub mod secrets;
pub mod seq;
pub mod staging;
pub mod ticket;
pub mod transport;
pub mod trust;
pub mod wire;
