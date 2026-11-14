//! The Gate's door for a remote peer (ADR 0017): every action a peer takes
//! passes the Gate as the `remote` actor, so it is audited and Pause stops it,
//! and hosting is off until someone turns it on.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use connecto_audit::{Audit, sha256_hex};
use connecto_core::{GuardDenied, PauseSwitch, RemoteGuard, RemotePeer};
use connecto_gate::{Denial, Gate, RemoteRequest};

/// How often typed input is summed into one audit record.
pub const INPUT_AUDIT_EVERY: Duration = Duration::from_secs(1);

/// What the audit log's records for remote peers are marked as.
const EVIDENCE_CLASS: &str = "REMOTE";

struct Pending {
    bytes: u64,
    /// A running digest: each chunk's hash folded into the last.
    digest: String,
    since: Instant,
}

/// The guard backed by the gate and the audit log.
pub struct RemoteGate {
    gate: Arc<Gate>,
    enabled: AtomicBool,
    pending: Mutex<HashMap<String, Pending>>,
}

fn label(peer: &RemotePeer) -> String {
    let id: String = peer
        .endpoint
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{}@{id}", peer.fingerprint)
}

impl RemoteGate {
    /// Hosting starts off.
    #[must_use]
    pub fn new(pause: Arc<PauseSwitch>, audit: Audit) -> Self {
        Self::with_gate(Arc::new(Gate::new(pause, audit, EVIDENCE_CLASS)))
    }

    #[must_use]
    pub fn with_gate(gate: Arc<Gate>) -> Self {
        Self {
            gate,
            enabled: AtomicBool::new(false),
            pending: Mutex::new(HashMap::new()),
        }
    }

    fn request(
        peer: &RemotePeer,
        action: &'static str,
        terminal: &str,
        attempt: u64,
        digest: String,
        bytes: u64,
    ) -> RemoteRequest {
        RemoteRequest {
            peer: label(peer),
            action,
            target_name: terminal.to_owned(),
            attempt,
            digest,
            bytes,
        }
    }

    fn map(denial: Denial) -> GuardDenied {
        match denial {
            Denial::Paused => GuardDenied::Paused,
            _ => GuardDenied::Refused,
        }
    }

    fn flush(&self, peer: &RemotePeer, terminal: &str, attempt: u64, pending: Pending) {
        let request = Self::request(
            peer,
            "input",
            terminal,
            attempt,
            pending.digest,
            pending.bytes,
        );
        // Already allowed when it was typed; this is the record of how much.
        let _ = self.gate.remote_submit(&request, false);
    }
}

impl RemoteGuard for RemoteGate {
    fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::SeqCst);
    }

    fn connected(&self, peer: &RemotePeer, outcome: Result<(), &str>) {
        let request = Self::request(peer, "connect", "", 0, String::new(), 0);
        match outcome {
            Ok(()) => self.gate.remote_note(&request, "allow", ""),
            Err(reason) => self.gate.remote_note(&request, "deny", reason),
        };
    }

    fn attach(&self, peer: &RemotePeer, terminal: &str, attempt: u64) -> Result<(), GuardDenied> {
        let request = Self::request(peer, "attach", terminal, attempt, String::new(), 0);
        self.gate
            .remote_submit(&request, true)
            .map(drop)
            .map_err(Self::map)
    }

    fn input(
        &self,
        peer: &RemotePeer,
        terminal: &str,
        attempt: u64,
        bytes: &[u8],
    ) -> Result<(), GuardDenied> {
        if self.gate.is_paused() {
            // Recorded as a denial, then refused.
            let request = Self::request(
                peer,
                "input",
                terminal,
                attempt,
                sha256_hex(bytes),
                bytes.len() as u64,
            );
            return self
                .gate
                .remote_submit(&request, false)
                .map(drop)
                .map_err(Self::map);
        }
        let key = format!("{}|{terminal}", label(peer));
        let due = {
            let mut map = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
            let entry = map.entry(key.clone()).or_insert_with(|| Pending {
                bytes: 0,
                digest: String::new(),
                since: Instant::now(),
            });
            entry.bytes += bytes.len() as u64;
            entry.digest = sha256_hex(format!("{}{}", entry.digest, sha256_hex(bytes)).as_bytes());
            if entry.since.elapsed() >= INPUT_AUDIT_EVERY {
                map.remove(&key)
            } else {
                None
            }
        };
        if let Some(pending) = due {
            self.flush(peer, terminal, attempt, pending);
        }
        Ok(())
    }

    fn resize(&self, peer: &RemotePeer, terminal: &str, attempt: u64) -> Result<(), GuardDenied> {
        let request = Self::request(peer, "resize", terminal, attempt, String::new(), 0);
        self.gate
            .remote_submit(&request, false)
            .map(drop)
            .map_err(Self::map)
    }

    fn manage(
        &self,
        peer: &RemotePeer,
        action: &'static str,
        terminal: &str,
        attempt: u64,
    ) -> Result<(), GuardDenied> {
        let request = Self::request(peer, action, terminal, attempt, String::new(), 0);
        self.gate
            .remote_submit(&request, true)
            .map(drop)
            .map_err(Self::map)
    }

    fn detach(&self, peer: &RemotePeer, terminal: &str, attempt: u64) {
        let key = format!("{}|{terminal}", label(peer));
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&key);
        if let Some(pending) = pending {
            self.flush(peer, terminal, attempt, pending);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::io;

    use connecto_audit::Sink;

    use super::*;

    #[derive(Clone, Default)]
    struct Lines(Arc<Mutex<Vec<String>>>);

    impl Sink for Lines {
        fn write_line(&mut self, line: &str) -> io::Result<()> {
            self.0.lock().unwrap().push(line.to_owned());
            Ok(())
        }
    }

    fn guard() -> (RemoteGate, Lines, Arc<PauseSwitch>, Arc<Gate>) {
        let lines = Lines::default();
        let pause = Arc::new(PauseSwitch::default());
        let gate = Arc::new(Gate::new(
            Arc::clone(&pause),
            Audit::with_sink(lines.clone()).unwrap(),
            "UNIT",
        ));
        (RemoteGate::with_gate(Arc::clone(&gate)), lines, pause, gate)
    }

    fn peer() -> RemotePeer {
        RemotePeer {
            fingerprint: "SHA256:abc".into(),
            endpoint: [7; 32],
        }
    }

    #[test]
    fn hosting_is_off_until_enabled() {
        let (guard, ..) = guard();
        assert!(!guard.enabled());
        guard.set_enabled(true);
        assert!(guard.enabled());
    }

    #[test]
    fn input_is_summed_into_one_record_and_never_logs_the_bytes() {
        let (guard, lines, _pause, gate) = guard();
        for chunk in [&b"hunter"[..], b"2", b"\r"] {
            guard.input(&peer(), "claude-1", 1, chunk).unwrap();
        }
        guard.detach(&peer(), "claude-1", 1);
        assert!(gate.flush(Duration::from_secs(2)));
        let log = lines.0.lock().unwrap().join("\n");
        assert_eq!(log.matches("\"action\":\"input\"").count(), 1, "{log}");
        assert!(log.contains("bytes=8"));
        assert!(log.contains("\"actor\":\"remote\""));
        assert!(!log.contains("hunter"));
    }

    #[test]
    fn pause_refuses_input_and_resize_and_audits_the_refusal() {
        let (guard, lines, pause, gate) = guard();
        pause.pause();
        assert_eq!(guard.input(&peer(), "t", 1, b"x"), Err(GuardDenied::Paused));
        assert_eq!(guard.resize(&peer(), "t", 1), Err(GuardDenied::Paused));
        // Looking is still allowed.
        assert_eq!(guard.attach(&peer(), "t", 1), Ok(()));
        assert!(gate.flush(Duration::from_secs(2)));
        let log = lines.0.lock().unwrap().join("\n");
        assert_eq!(log.matches("\"decision\":\"deny\"").count(), 2, "{log}");
        pause.resume();
        assert_eq!(guard.input(&peer(), "t", 1, b"x"), Ok(()));
    }

    #[test]
    fn managing_terminals_is_audited_and_pause_refuses_it() {
        let (guard, lines, pause, gate) = guard();
        for action in ["add", "close", "restart", "rename"] {
            assert_eq!(guard.manage(&peer(), action, "t", 1), Ok(()));
        }
        pause.pause();
        for action in ["add", "close", "restart", "rename"] {
            assert_eq!(
                guard.manage(&peer(), action, "t", 1),
                Err(GuardDenied::Paused)
            );
        }
        assert!(gate.flush(Duration::from_secs(2)));
        let log = lines.0.lock().unwrap().join("\n");
        for action in ["add", "close", "restart", "rename"] {
            assert!(log.contains(&format!("\"action\":\"{action}\"")), "{log}");
        }
        assert_eq!(log.matches("\"decision\":\"deny\"").count(), 4, "{log}");
        assert_eq!(log.matches("\"actor\":\"remote\"").count(), 8);
    }

    #[test]
    fn a_refused_handshake_is_recorded() {
        let (guard, lines, _pause, gate) = guard();
        guard.connected(&peer(), Err("not authorized"));
        assert!(gate.flush(Duration::from_secs(2)));
        let log = lines.0.lock().unwrap().join("\n");
        assert!(log.contains("\"action\":\"connect\"") && log.contains("not authorized"));
    }
}
