//! Independent UNIT policy/audit acceptance probes. No CLI/model calls.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use connecto_audit::{Audit, Record, Sink, verify};
use connecto_core::{PauseSwitch, Redacted, SessionRef};
use connecto_gate::{Actor, DeliverRequest, Denial, Gate, TargetState};
use std::{
    fs, io,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
struct Scratch(PathBuf);
impl Scratch {
    fn new(name: &str) -> Self {
        let p =
            std::env::temp_dir().join(format!("mg-tester-policy-{name}-{}", std::process::id()));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn request(n: usize) -> DeliverRequest {
    DeliverRequest {
        actor: Actor::User,
        target: SessionRef {
            session: 0,
            attempt: 7,
        },
        source: "user".into(),
        target_name: "Claude".into(),
        trace: "t".into(),
        message: format!("m{n}"),
        hop: 0,
        body: Redacted::assume(format!("body {n}")),
        digest: format!("digest{n}"),
        confirmed: false,
    }
}
#[test]
fn gate_boundaries_audit_and_epoch() {
    let scratch = Scratch::new("limits");
    let file = scratch.0.join("audit.jsonl");
    let pause = Arc::new(PauseSwitch::default());
    let gate = Gate::new(pause.clone(), Audit::open(&file).unwrap(), "UNIT");
    let state = TargetState::ready(7);
    let mut r = request(0);
    r.actor = Actor::Agent;
    assert_eq!(gate.submit(&r, &state).unwrap_err(), Denial::NotUser);
    r.actor = Actor::User;
    r.target.attempt = 6;
    assert_eq!(gate.submit(&r, &state).unwrap_err(), Denial::StaleTarget);
    r.target.attempt = 7;
    r.hop = 6;
    assert_eq!(gate.submit(&r, &state).unwrap_err(), Denial::HopLimit);
    r.hop = 0;
    let mut occupied = state;
    occupied.open_delivery = true;
    assert_eq!(gate.submit(&r, &occupied).unwrap_err(), Denial::OneOpen);
    let token = gate.submit(&r, &state).unwrap();
    gate.outcome(&r, "completed", "");
    assert_eq!(gate.submit(&r, &state).unwrap_err(), Denial::Duplicate);
    pause.pause();
    assert!(!gate.still_valid(&token));
    assert_eq!(
        gate.submit(&request(1), &state).unwrap_err(),
        Denial::Paused
    );
    pause.resume();
    assert!(!gate.still_valid(&token), "old epoch revived");
    for n in 1..4 {
        let r = request(n);
        gate.submit(&r, &state).unwrap();
        gate.outcome(&r, "completed", "");
    }
    assert_eq!(
        gate.submit(&request(4), &state).unwrap_err(),
        Denial::RateLimit
    );
    assert!(gate.flush(Duration::from_secs(2)));
    let count = verify(&file).unwrap();
    let text = fs::read_to_string(&file).unwrap();
    assert_eq!(
        text.lines()
            .filter(|l| l.contains("\"decision\":\"allow\""))
            .count(),
        4
    );
    assert_eq!(
        text.lines()
            .filter(|l| l.contains("\"outcome\":\"completed\""))
            .count(),
        4
    );
    let mut bytes = text.into_bytes();
    let ix = bytes.iter().position(|b| *b == b'u').unwrap();
    bytes[ix] = b'v';
    fs::write(&file, bytes).unwrap();
    assert!(verify(&file).is_err());
    println!(
        "UNIT policy denies actor/stale/hop/open/duplicate/pause/rate; 4 durable intents + outcomes; {count} verified records; mutation detected"
    );
}
struct Broken;
impl Sink for Broken {
    fn write_line(&mut self, _: &str) -> io::Result<()> {
        Err(io::Error::other("synthetic disk failure"))
    }
}
#[test]
fn failed_audit_refuses_delivery_but_stop_is_available() {
    let pause = Arc::new(PauseSwitch::default());
    let gate = Gate::new(pause, Audit::with_sink(Broken).unwrap(), "UNIT");
    let r = request(0);
    assert_eq!(
        gate.submit(&r, &TargetState::ready(7)).unwrap_err(),
        Denial::AuditFailed
    );
    let start = Instant::now();
    let stop = gate.control("stop", r.target, "Claude");
    let elapsed = start.elapsed();
    assert_eq!(stop.target(), r.target);
    println!(
        "UNIT failing audit denied delivery; stop token returned in {} us",
        elapsed.as_micros()
    );
}
#[test]
fn reopening_existing_audit_cannot_silently_restart_chain() {
    let scratch = Scratch::new("reopen");
    let file = scratch.0.join("log");
    let audit = Audit::open(&file).unwrap();
    audit
        .append_durable(Record::default(), Duration::from_secs(2))
        .unwrap();
    drop(audit);
    assert!(Audit::open(&file).is_err());
    assert_eq!(verify(&file).unwrap(), 1);
    println!("UNIT existing audit refuses reopening; original chain retained");
}
