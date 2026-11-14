//! The only way to an action.
//!
//! Everything that changes a session goes through [`Gate`], in a fixed order:
//! pause, target current, policy, limits, durable audit intent. Success yields
//! an [`Authorized`] token, which nothing else can construct.

mod policy;

use std::collections::{HashMap, HashSet, VecDeque};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use connecto_audit::{Audit, Record};
use connecto_core::{PauseSwitch, Readiness, Redacted, SessionRef};

pub use policy::{
    Actor, AgentAction, Decision, POLICY_REVISION, TargetState, decide_agent, decide_deliver,
};

/// How long a gated action waits for the audit log before it is denied.
pub const AUDIT_WAIT: Duration = Duration::from_millis(200);
pub const MAX_HOPS: u32 = 6;
pub const RATE_LIMIT: usize = 4;
pub const RATE_WINDOW: Duration = Duration::from_secs(60);
const MAX_REMEMBERED: usize = 1024;

/// Why an action was not authorized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Denial {
    #[error("relay is paused")]
    Paused,
    #[error("the target restarted; this was for an older attempt")]
    StaleTarget,
    #[error("only a user can start a delivery")]
    NotUser,
    #[error("the target is not running")]
    NotRunning,
    #[error("the target is in manual mode: typing works and the relay is denied")]
    ManualMode,
    #[error(
        "this is the target's own reply, so it was not sent; clear the message to send your own text"
    )]
    OwnReply,
    #[error("the target is busy with a turn")]
    Busy,
    #[error("the target may not be ready; confirm to send anyway")]
    NeedsConfirmation,
    #[error("hop limit reached")]
    HopLimit,
    #[error("rate limit: 4 deliveries a minute between these two")]
    RateLimit,
    #[error("the target already has an open delivery")]
    OneOpen,
    #[error("already sent in this trace")]
    Duplicate,
    #[error("the audit log failed, so nothing was sent")]
    AuditFailed,
}

/// The delivery action.
#[derive(Debug)]
pub struct Deliver;

/// A stop or restart.
#[derive(Debug)]
pub struct Control;

/// An agent's action through the bridge.
#[derive(Debug)]
pub struct Agent;

/// A remote peer's action.
#[derive(Debug)]
pub struct Remote;

/// A remote peer's action, as the gate sees it.
#[derive(Debug, Clone)]
pub struct RemoteRequest {
    /// The peer, as `SHA256:<fingerprint>@<endpoint id>`.
    pub peer: String,
    /// `attach`, `input`, `resize`, `connect`.
    pub action: &'static str,
    /// The terminal's name, or empty.
    pub target_name: String,
    pub attempt: u64,
    /// Of the text or bytes, when the action carries any. Never the bytes.
    pub digest: String,
    /// How many bytes the action carried, recorded as the message.
    pub bytes: u64,
}

/// Captured when the bridge accepts an action, before any queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Admitted {
    pub pause_epoch: u64,
    pub attempt: u64,
}

/// An agent's action. Limits do not apply to it (R3, R7, A16).
#[derive(Debug, Clone)]
pub struct AgentRequest {
    pub action: AgentAction,
    /// The calling terminal's id, or `connecto` for a wake.
    pub source: String,
    pub target: SessionRef,
    pub target_name: String,
    /// A request id, key id or wake id.
    pub message: String,
    /// Of the text as typed.
    pub digest: String,
    /// The redacted copy for the audit. Absent keeps the hash only.
    pub audit_body: Option<Redacted<String>>,
    pub admitted: Admitted,
}

/// Proof that the gate allowed an action. Fields are private and there is no
/// public constructor, so holding one means the gate ran.
#[derive(Debug)]
pub struct Authorized<A> {
    epoch: u64,
    target: SessionRef,
    digest: String,
    action: PhantomData<A>,
}

impl<A> Authorized<A> {
    #[must_use]
    pub const fn target(&self) -> SessionRef {
        self.target
    }

    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }
}

/// A delivery someone wants.
#[derive(Debug, Clone)]
pub struct DeliverRequest {
    pub actor: Actor,
    pub target: SessionRef,
    pub source: String,
    pub target_name: String,
    pub trace: String,
    pub message: String,
    pub hop: u32,
    pub body: Redacted<String>,
    pub digest: String,
    /// The user confirmed on a second click.
    pub confirmed: bool,
}

#[derive(Default)]
struct Limits {
    sent: HashMap<(String, usize), VecDeque<Instant>>,
    seen: HashSet<(String, usize, String)>,
    seen_order: VecDeque<(String, usize, String)>,
}

impl Limits {
    fn check(
        &mut self,
        request: &DeliverRequest,
        state: &TargetState,
        now: Instant,
    ) -> Option<Denial> {
        if request.hop >= MAX_HOPS {
            return Some(Denial::HopLimit);
        }
        if state.open_delivery {
            return Some(Denial::OneOpen);
        }
        let key = (
            request.trace.clone(),
            request.target.session,
            request.digest.clone(),
        );
        if self.seen.contains(&key) {
            return Some(Denial::Duplicate);
        }
        let pair = (request.source.clone(), request.target.session);
        let times = self.sent.entry(pair).or_default();
        while times
            .front()
            .is_some_and(|sent| now.duration_since(*sent) >= RATE_WINDOW)
        {
            times.pop_front();
        }
        (times.len() >= RATE_LIMIT).then_some(Denial::RateLimit)
    }

    fn remember(&mut self, request: &DeliverRequest, now: Instant) {
        let pair = (request.source.clone(), request.target.session);
        self.sent.entry(pair).or_default().push_back(now);
        let key = (
            request.trace.clone(),
            request.target.session,
            request.digest.clone(),
        );
        if self.seen.insert(key.clone()) {
            self.seen_order.push_back(key);
        }
        if self.seen_order.len() > MAX_REMEMBERED
            && let Some(oldest) = self.seen_order.pop_front()
        {
            self.seen.remove(&oldest);
        }
    }
}

/// The gate.
pub struct Gate {
    pause: Arc<PauseSwitch>,
    audit: Audit,
    limits: Mutex<Limits>,
    evidence_class: &'static str,
    /// Stamped on every record, so one log can serve several projects (A5).
    project: String,
}

impl Gate {
    #[must_use]
    pub fn new(pause: Arc<PauseSwitch>, audit: Audit, evidence_class: &'static str) -> Self {
        Self {
            pause,
            audit,
            limits: Mutex::new(Limits::default()),
            evidence_class,
            project: String::new(),
        }
    }

    /// The same gate, with every record it writes naming `project`.
    #[must_use]
    pub fn with_project(mut self, project: &str) -> Self {
        project.clone_into(&mut self.project);
        self
    }

    fn record(&self, request: &DeliverRequest, decision: &str, reason: &str) -> Record {
        Record {
            trace: request.trace.clone(),
            message: request.message.clone(),
            actor: request.actor.name().to_owned(),
            source: request.source.clone(),
            target: request.target_name.clone(),
            attempt: request.target.attempt,
            action: "deliver".to_owned(),
            decision: decision.to_owned(),
            reason: reason.to_owned(),
            policy_revision: POLICY_REVISION,
            pause_epoch: self.pause.epoch(),
            body_hash: request.digest.clone(),
            redacted_body: Some(request.body.clone()),
            outcome: String::new(),
            evidence_class: self.evidence_class.to_owned(),
            project: self.project.clone(),
        }
    }

    fn checked(
        &self,
        request: &DeliverRequest,
        state: &TargetState,
        now: Instant,
    ) -> Result<(), Denial> {
        if self.pause.is_paused() {
            return Err(Denial::Paused);
        }
        if request.target.attempt != state.attempt {
            return Err(Denial::StaleTarget);
        }
        // `source` names a session only through the user's own Use click.
        let own_reply = request.source == request.target_name;
        match decide_deliver(request.actor, state, request.confirmed, own_reply) {
            Decision::Allow => {}
            Decision::Deny(denial) => return Err(denial),
        }
        let mut limits = self.limits.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(denial) = limits.check(request, state, now) {
            return Err(denial);
        }
        // Hold the limits until the intent is durable: two deliveries racing
        // for one target must not both pass.
        let intent = self.record(request, "allow", confirmed_reason(request));
        self.audit
            .append_durable(intent, AUDIT_WAIT)
            .map_err(|_| Denial::AuditFailed)?;
        limits.remember(request, now);
        Ok(())
    }

    /// Decide a delivery. Blocks up to [`AUDIT_WAIT`], so never call it on the
    /// UI thread.
    pub fn submit(
        &self,
        request: &DeliverRequest,
        state: &TargetState,
    ) -> Result<Authorized<Deliver>, Denial> {
        self.submit_at(request, state, Instant::now())
    }

    fn submit_at(
        &self,
        request: &DeliverRequest,
        state: &TargetState,
        now: Instant,
    ) -> Result<Authorized<Deliver>, Denial> {
        let epoch = self.pause.epoch();
        match self.checked(request, state, now) {
            Ok(()) => Ok(Authorized {
                epoch,
                target: request.target,
                digest: request.digest.clone(),
                action: PhantomData,
            }),
            Err(denial) => {
                self.audit
                    .append_nowait(self.record(request, "deny", &denial.to_string()));
                Err(denial)
            }
        }
    }

    /// Whether an action may still take its next step.
    #[must_use]
    pub fn still_valid<A>(&self, authorized: &Authorized<A>) -> bool {
        !self.pause.is_paused() && self.pause.epoch() == authorized.epoch
    }

    fn agent_record(&self, request: &AgentRequest, decision: &str, reason: &str) -> Record {
        Record {
            trace: String::new(),
            message: request.message.clone(),
            actor: Actor::Agent.name().to_owned(),
            source: request.source.clone(),
            target: request.target_name.clone(),
            attempt: request.target.attempt,
            action: request.action.name().to_owned(),
            decision: decision.to_owned(),
            reason: reason.to_owned(),
            policy_revision: POLICY_REVISION,
            pause_epoch: self.pause.epoch(),
            body_hash: request.digest.clone(),
            redacted_body: request.audit_body.clone(),
            outcome: String::new(),
            evidence_class: self.evidence_class.to_owned(),
            project: self.project.clone(),
        }
    }

    /// Decide an agent's action, in this order: paused now, pause epoch equal
    /// to the admitted one, target attempt equal to the admitted one,
    /// [`decide_agent`], durable audit intent. There is no call to the limits
    /// (R3, R7). Blocks up to [`AUDIT_WAIT`], so never call it on the UI thread.
    pub fn agent_submit(
        &self,
        request: &AgentRequest,
        state: &TargetState,
    ) -> Result<Authorized<Agent>, Denial> {
        let checked = (|| {
            if self.pause.is_paused() {
                return Err(Denial::Paused);
            }
            if self.pause.epoch() != request.admitted.pause_epoch {
                return Err(Denial::Paused);
            }
            if request.admitted.attempt != state.attempt
                || request.admitted.attempt != request.target.attempt
            {
                return Err(Denial::StaleTarget);
            }
            match decide_agent(request.action, state) {
                Decision::Allow => {}
                Decision::Deny(denial) => return Err(denial),
            }
            self.audit
                .append_durable(self.agent_record(request, "allow", ""), AUDIT_WAIT)
                .map_err(|_| Denial::AuditFailed)
        })();
        match checked {
            Ok(()) => Ok(Authorized {
                epoch: request.admitted.pause_epoch,
                target: request.target,
                digest: request.digest.clone(),
                action: PhantomData,
            }),
            Err(denial) => {
                self.audit
                    .append_nowait(self.agent_record(request, "deny", &denial.to_string()));
                Err(denial)
            }
        }
    }

    /// Record how an agent action ended. Never waits.
    pub fn agent_outcome(&self, request: &AgentRequest, outcome: &str, reason: &str) -> bool {
        let mut record = self.agent_record(request, "outcome", reason);
        record.outcome = outcome.to_owned();
        self.audit.append_nowait(record)
    }

    /// Record an agent's read of a terminal: who read which, never what. It
    /// never waits and is allowed under Pause (A33).
    pub fn agent_note(&self, source: &str, target_name: &str, attempt: u64) -> bool {
        self.audit.append_nowait(Record {
            actor: Actor::Agent.name().to_owned(),
            source: source.to_owned(),
            target: target_name.to_owned(),
            attempt,
            action: "read".to_owned(),
            decision: "allow".to_owned(),
            policy_revision: POLICY_REVISION,
            pause_epoch: self.pause.epoch(),
            evidence_class: self.evidence_class.to_owned(),
            project: self.project.clone(),
            ..Record::default()
        })
    }

    /// Whether Pause is on now.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.pause.is_paused()
    }

    /// Decide a remote peer's action: paused now, then (for `durable`) an
    /// audit intent that must reach the log. A remote peer can type into a
    /// terminal, so Pause stops it as it stops an agent; reading is not
    /// gated, so `attach` passes `durable` false. Blocks up to [`AUDIT_WAIT`]
    /// when durable, so never call it on the UI thread.
    pub fn remote_submit(
        &self,
        request: &RemoteRequest,
        durable: bool,
    ) -> Result<Authorized<Remote>, Denial> {
        let paused = self.pause.is_paused() && request.action != "attach";
        let checked = if paused {
            Err(Denial::Paused)
        } else if durable {
            self.audit
                .append_durable(self.remote_record(request, "allow", ""), AUDIT_WAIT)
                .map_err(|_| Denial::AuditFailed)
        } else {
            let _ = self
                .audit
                .append_nowait(self.remote_record(request, "allow", ""));
            Ok(())
        };
        match checked {
            Ok(()) => Ok(Authorized {
                epoch: self.pause.epoch(),
                target: SessionRef {
                    session: 0,
                    attempt: request.attempt,
                },
                digest: request.digest.clone(),
                action: PhantomData,
            }),
            Err(denial) => {
                self.audit
                    .append_nowait(self.remote_record(request, "deny", &denial.to_string()));
                Err(denial)
            }
        }
    }

    /// Record something about a remote peer that is not a gated action, such
    /// as a refused connection. Never waits.
    pub fn remote_note(&self, request: &RemoteRequest, decision: &str, reason: &str) -> bool {
        self.audit
            .append_nowait(self.remote_record(request, decision, reason))
    }

    fn remote_record(&self, request: &RemoteRequest, decision: &str, reason: &str) -> Record {
        Record {
            message: if request.bytes > 0 {
                format!("bytes={}", request.bytes)
            } else {
                String::new()
            },
            actor: Actor::Remote.name().to_owned(),
            source: request.peer.clone(),
            target: request.target_name.clone(),
            attempt: request.attempt,
            action: request.action.to_owned(),
            decision: decision.to_owned(),
            reason: reason.to_owned(),
            policy_revision: POLICY_REVISION,
            pause_epoch: self.pause.epoch(),
            body_hash: request.digest.clone(),
            evidence_class: self.evidence_class.to_owned(),
            project: self.project.clone(),
            ..Record::default()
        }
    }

    /// A human stop or restart. Always allowed, and it never waits on the log.
    #[must_use]
    pub fn control(
        &self,
        action: &str,
        target: SessionRef,
        target_name: &str,
    ) -> Authorized<Control> {
        self.audit.append_nowait(Record {
            actor: Actor::User.name().to_owned(),
            target: target_name.to_owned(),
            attempt: target.attempt,
            action: action.to_owned(),
            decision: "allow".to_owned(),
            policy_revision: POLICY_REVISION,
            pause_epoch: self.pause.epoch(),
            evidence_class: self.evidence_class.to_owned(),
            project: self.project.clone(),
            ..Record::default()
        });
        Authorized {
            epoch: self.pause.epoch(),
            target,
            digest: String::new(),
            action: PhantomData,
        }
    }

    /// Record how a delivery ended. Never waits. False means the record was lost.
    pub fn outcome(&self, request: &DeliverRequest, outcome: &str, reason: &str) -> bool {
        let mut record = self.record(request, "outcome", reason);
        record.outcome = outcome.to_owned();
        self.audit.append_nowait(record)
    }

    /// Record how a delivery ended while the app closes, when nothing can retry:
    /// waits up to [`AUDIT_WAIT`] for the log. False means the record was lost.
    /// Never call it on the UI thread.
    #[must_use]
    pub fn final_outcome(&self, request: &DeliverRequest, outcome: &str, reason: &str) -> bool {
        let mut record = self.record(request, "outcome", reason);
        record.outcome = outcome.to_owned();
        self.audit.append_durable(record, AUDIT_WAIT).is_ok()
    }

    /// Wait for the audit log to take everything.
    pub fn flush(&self, timeout: Duration) -> bool {
        self.audit.flush(timeout)
    }
}

const fn confirmed_reason(request: &DeliverRequest) -> &'static str {
    if request.confirmed {
        "confirmed by the user on a second click"
    } else {
        ""
    }
}

impl TargetState {
    /// A running, ready, idle target.
    #[must_use]
    pub const fn ready(attempt: u64) -> Self {
        Self {
            attempt,
            running: true,
            readiness: Readiness::Ready,
            busy: false,
            manual: false,
            open_delivery: false,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::io;
    use std::sync::Arc;

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

    struct Broken;

    impl Sink for Broken {
        fn write_line(&mut self, _line: &str) -> io::Result<()> {
            Err(io::Error::other("disk full"))
        }
    }

    fn gate() -> (Gate, Lines, Arc<PauseSwitch>) {
        let lines = Lines::default();
        let pause = Arc::new(PauseSwitch::default());
        let audit = Audit::with_sink(lines.clone()).unwrap();
        (Gate::new(Arc::clone(&pause), audit, "UNIT"), lines, pause)
    }

    fn request(trace: &str, body: &str) -> DeliverRequest {
        DeliverRequest {
            actor: Actor::User,
            target: SessionRef {
                session: 1,
                attempt: 1,
            },
            source: "user".to_owned(),
            target_name: "Codex".to_owned(),
            trace: trace.to_owned(),
            message: "m1".to_owned(),
            hop: 0,
            body: Redacted::assume(body.to_owned()),
            digest: body.to_owned(),
            confirmed: false,
        }
    }

    fn denied(result: Result<Authorized<Deliver>, Denial>) -> Denial {
        result.err().unwrap()
    }

    /// Holds the writer until told to go on.
    struct Held(std::sync::mpsc::Receiver<()>);

    impl Sink for Held {
        fn write_line(&mut self, _line: &str) -> io::Result<()> {
            let _ = self.0.recv();
            Ok(())
        }
    }

    #[test]
    fn a_final_outcome_waits_for_a_full_queue_and_says_when_it_was_lost() {
        let (release, held) = std::sync::mpsc::channel();
        let audit = Audit::with_sink(Held(held)).unwrap();
        let gate = Gate::new(Arc::new(PauseSwitch::default()), audit, "UNIT");
        let request = request("t", "b");
        while gate.outcome(&request, "uncertain", "") {}
        let started = Instant::now();
        assert!(!gate.final_outcome(&request, "uncertain", "the app closed"));
        assert!(started.elapsed() >= AUDIT_WAIT, "it did wait");
        assert!(started.elapsed() < AUDIT_WAIT * 5, "and not for long");
        drop(release);
    }

    #[test]
    fn a_final_outcome_is_on_disk_when_it_returns() {
        let (gate, lines, _) = gate();
        assert!(gate.final_outcome(&request("t", "b"), "uncertain", "the app closed"));
        let lines = lines.0.lock().unwrap();
        assert!(lines[0].contains(r#""outcome":"uncertain""#), "{lines:?}");
    }

    #[test]
    fn an_own_reply_is_denied_and_audited_and_other_sources_go_through() {
        let (gate, lines, _) = gate();
        let mut own = request("t", "b");
        own.source = "Codex".to_owned();
        own.confirmed = true;
        assert_eq!(
            denied(gate.submit(&own, &TargetState::ready(1))),
            Denial::OwnReply
        );
        assert!(gate.audit.flush(Duration::from_secs(1)));
        let text = lines.0.lock().unwrap().join("\n");
        assert!(text.contains(r#""decision":"deny""#), "{text}");
        assert!(text.contains("own reply"), "{text}");
        assert!(text.contains(r#""policy_revision":4"#), "{text}");

        let mut other = request("t2", "b");
        other.source = "Claude".to_owned();
        assert!(gate.submit(&other, &TargetState::ready(1)).is_ok());
        assert!(
            gate.submit(&request("t3", "b"), &TargetState::ready(1))
                .is_ok()
        );
    }

    #[test]
    fn a_ready_target_is_authorized_with_a_durable_intent_first() {
        let (gate, lines, _) = gate();
        let token = gate
            .submit(&request("t", "b"), &TargetState::ready(1))
            .unwrap();
        assert_eq!(token.digest(), "b");
        let lines = lines.0.lock().unwrap();
        assert_eq!(lines.len(), 1, "the intent was durable before the token");
        assert!(lines[0].contains(r#""decision":"allow""#));
        assert!(lines[0].contains(r#""redacted_body":"b""#));
    }

    #[test]
    fn pause_comes_first_and_is_audited() {
        let (gate, lines, pause) = gate();
        pause.pause();
        let request = request("t", "b");
        assert_eq!(
            denied(gate.submit(&request, &TargetState::ready(1))),
            Denial::Paused
        );
        assert!(gate.flush(Duration::from_secs(1)));
        assert!(lines.0.lock().unwrap()[0].contains(r#""decision":"deny""#));
        pause.resume();
        assert!(gate.submit(&request, &TargetState::ready(1)).is_ok());
    }

    #[test]
    fn a_stale_attempt_is_refused() {
        let (gate, _, _) = gate();
        let result = gate.submit(&request("t", "b"), &TargetState::ready(2));
        assert_eq!(denied(result), Denial::StaleTarget);
    }

    #[test]
    fn nothing_but_a_user_can_deliver() {
        let (gate, _, _) = gate();
        let mut request = request("t", "b");
        request.actor = Actor::Agent;
        request.confirmed = true;
        let result = gate.submit(&request, &TargetState::ready(1));
        assert_eq!(denied(result), Denial::NotUser);
    }

    #[test]
    fn hops_stop_at_six() {
        let (gate, _, _) = gate();
        let mut request = request("t", "b");
        request.hop = 5;
        assert!(gate.submit(&request, &TargetState::ready(1)).is_ok());
        request.hop = 6;
        request.digest = "other".to_owned();
        let result = gate.submit(&request, &TargetState::ready(1));
        assert_eq!(denied(result), Denial::HopLimit);
    }

    #[test]
    fn the_rate_limit_is_per_pair_and_slides() {
        let (gate, _, _) = gate();
        let start = Instant::now();
        let state = TargetState::ready(1);
        for index in 0..RATE_LIMIT {
            let request = request(&index.to_string(), "b");
            assert!(gate.submit_at(&request, &state, start).is_ok());
        }
        let extra = request("extra", "b");
        assert_eq!(
            denied(gate.submit_at(&extra, &state, start)),
            Denial::RateLimit
        );

        let mut other_pair = request("other", "b");
        other_pair.source = "Claude".to_owned();
        assert!(gate.submit_at(&other_pair, &state, start).is_ok());

        let later = start + RATE_WINDOW;
        assert!(gate.submit_at(&extra, &state, later).is_ok());
    }

    #[test]
    fn one_open_delivery_per_target() {
        let (gate, _, _) = gate();
        let state = TargetState {
            open_delivery: true,
            ..TargetState::ready(1)
        };
        assert_eq!(
            denied(gate.submit(&request("t", "b"), &state)),
            Denial::OneOpen
        );
    }

    #[test]
    fn the_same_body_in_the_same_trace_is_a_duplicate_only_for_that_target() {
        let (gate, _, _) = gate();
        let first = request("t", "b");
        assert!(gate.submit(&first, &TargetState::ready(1)).is_ok());
        assert_eq!(
            denied(gate.submit(&first, &TargetState::ready(1))),
            Denial::Duplicate
        );
        let mut elsewhere = first.clone();
        elsewhere.target.session = 0;
        assert!(gate.submit(&elsewhere, &TargetState::ready(1)).is_ok());
        assert!(
            gate.submit(&request("new", "b"), &TargetState::ready(1))
                .is_ok()
        );
    }

    #[test]
    fn a_denied_delivery_uses_up_no_limit() {
        let (gate, _, pause) = gate();
        let request = request("t", "b");
        pause.pause();
        let _ = gate.submit(&request, &TargetState::ready(1));
        pause.resume();
        assert!(gate.submit(&request, &TargetState::ready(1)).is_ok());
    }

    #[test]
    fn an_audit_failure_denies_delivery_and_never_stop() {
        let pause = Arc::new(PauseSwitch::default());
        let audit = Audit::with_sink(Broken).unwrap();
        let gate = Gate::new(pause, audit, "UNIT");
        let request = request("t", "b");
        assert_eq!(
            denied(gate.submit(&request, &TargetState::ready(1))),
            Denial::AuditFailed
        );
        let token = gate.control("stop", request.target, "Codex");
        assert_eq!(token.target(), request.target);
    }

    #[test]
    fn a_pause_after_authorization_invalidates_it_even_if_resumed() {
        let (gate, _, pause) = gate();
        let token = gate
            .submit(&request("t", "b"), &TargetState::ready(1))
            .unwrap();
        assert!(gate.still_valid(&token));
        pause.pause();
        assert!(!gate.still_valid(&token));
        pause.resume();
        assert!(!gate.still_valid(&token));
    }

    #[test]
    fn an_outcome_is_recorded_after_its_intent() {
        let (gate, lines, _) = gate();
        let request = request("t", "b");
        let _ = gate.submit(&request, &TargetState::ready(1)).unwrap();
        gate.outcome(&request, "completed", "");
        assert!(gate.flush(Duration::from_secs(1)));
        let lines = lines.0.lock().unwrap();
        assert_eq!(lines.len(), 2);
        assert!(lines[1].contains(r#""outcome":"completed""#));
    }

    fn agent_request(pause: &PauseSwitch, text: &str) -> AgentRequest {
        AgentRequest {
            action: AgentAction::Type,
            source: "claude".to_owned(),
            target: SessionRef {
                session: 1,
                attempt: 4,
            },
            target_name: "codex".to_owned(),
            message: "r1".to_owned(),
            digest: text.to_owned(),
            audit_body: Some(Redacted::assume(format!("redacted {text}"))),
            admitted: Admitted {
                pause_epoch: pause.epoch(),
                attempt: 4,
            },
        }
    }

    /// AC4: an allowed agent action is in the audit with its source and hash.
    #[test]
    fn an_agent_action_is_audited_with_source_hash_and_redacted_body() {
        let (gate, lines, pause) = gate();
        let request = agent_request(&pause, "hash-of-text");
        assert!(gate.agent_submit(&request, &TargetState::ready(4)).is_ok());
        assert!(gate.audit.flush(Duration::from_secs(1)));
        let text = lines.0.lock().unwrap().join("\n");
        assert!(text.contains(r#""actor":"agent""#), "{text}");
        assert!(text.contains(r#""source":"claude""#), "{text}");
        assert!(text.contains(r#""action":"type""#), "{text}");
        assert!(text.contains(r#""body_hash":"hash-of-text""#), "{text}");
        assert!(text.contains("redacted hash-of-text"), "{text}");
    }

    /// R3, R7, A16: the user's limits do not reach agents.
    #[test]
    fn agents_are_not_rate_limited_or_deduplicated() {
        let (gate, _lines, pause) = gate();
        let request = agent_request(&pause, "same");
        for _ in 0..30 {
            let busy = TargetState {
                busy: true,
                open_delivery: true,
                readiness: Readiness::NotReady,
                ..TargetState::ready(4)
            };
            assert!(gate.agent_submit(&request, &busy).is_ok());
        }
    }

    /// T3: what was admitted before a Pause is refused after the Resume, and
    /// what was admitted before a restart types nothing in the new attempt.
    #[test]
    fn an_admission_does_not_survive_a_pause_or_a_restart() {
        let (gate, _lines, pause) = gate();
        let request = agent_request(&pause, "x");
        pause.pause();
        assert_eq!(
            gate.agent_submit(&request, &TargetState::ready(4)).err(),
            Some(Denial::Paused)
        );
        pause.resume();
        assert_eq!(
            gate.agent_submit(&request, &TargetState::ready(4)).err(),
            Some(Denial::Paused),
            "after Resume the old admission is still stale"
        );
        let fresh = agent_request(&pause, "x");
        assert_eq!(
            gate.agent_submit(&fresh, &TargetState::ready(5)).err(),
            Some(Denial::StaleTarget),
            "the target restarted"
        );
        let authorized = gate.agent_submit(&fresh, &TargetState::ready(4)).unwrap();
        assert!(gate.still_valid(&authorized));
        pause.pause();
        assert!(!gate.still_valid(&authorized));
        pause.resume();
        assert!(!gate.still_valid(&authorized), "a Pause ended it for good");
    }

    #[test]
    fn a_read_note_records_who_read_which_and_never_waits_or_refuses() {
        let (gate, lines, pause) = gate();
        pause.pause();
        assert!(gate.agent_note("claude", "codex", 2));
        assert!(gate.audit.flush(Duration::from_secs(1)));
        let text = lines.0.lock().unwrap().join("\n");
        assert!(text.contains(r#""action":"read""#) && text.contains(r#""source":"claude""#));
        assert!(!text.contains("redacted_body\":\""), "{text}");
    }

    fn remote(action: &'static str) -> RemoteRequest {
        RemoteRequest {
            peer: "SHA256:abc@node".to_owned(),
            action,
            target_name: "claude-1".to_owned(),
            attempt: 1,
            digest: "d".to_owned(),
            bytes: 3,
        }
    }

    #[test]
    fn a_remote_input_is_audited_as_the_remote_actor() {
        let (gate, lines, _pause) = gate();
        assert!(gate.remote_submit(&remote("input"), true).is_ok());
        assert!(gate.flush(Duration::from_secs(2)));
        let log = lines.0.lock().unwrap().join("\n");
        assert!(log.contains("\"actor\":\"remote\""), "{log}");
        assert!(log.contains("SHA256:abc@node"));
        assert!(log.contains("bytes=3"));
    }

    #[test]
    fn pause_blocks_remote_input_and_resize_but_not_a_look() {
        let (gate, lines, pause) = gate();
        pause.pause();
        assert_eq!(
            gate.remote_submit(&remote("input"), false).unwrap_err(),
            Denial::Paused
        );
        assert_eq!(
            gate.remote_submit(&remote("resize"), false).unwrap_err(),
            Denial::Paused
        );
        assert!(gate.remote_submit(&remote("attach"), true).is_ok());
        assert!(gate.flush(Duration::from_secs(2)));
        assert!(
            lines
                .0
                .lock()
                .unwrap()
                .join("\n")
                .contains("relay is paused")
        );
        pause.resume();
        assert!(gate.remote_submit(&remote("input"), false).is_ok());
    }

    #[test]
    fn a_failed_audit_denies_a_durable_remote_action() {
        let pause = Arc::new(PauseSwitch::default());
        let gate = Gate::new(pause, Audit::with_sink(Broken).unwrap(), "UNIT");
        assert_eq!(
            gate.remote_submit(&remote("attach"), true).unwrap_err(),
            Denial::AuditFailed
        );
    }

    #[test]
    fn a_remote_actor_cannot_use_the_delivery_path() {
        let (gate, _lines, _pause) = gate();
        let mut request = request("t", "hello");
        request.actor = Actor::Remote;
        assert!(gate.submit(&request, &TargetState::ready(1)).is_err());
    }
}
