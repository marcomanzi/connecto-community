//! What `connecto agents` is told of each coding CLI (ADR 0045): decided as
//! an Agentic Works job's agent is (ADR 0041), from the settings (installed,
//! offered, not switched off) and then a look (Pi's providers, the sign-in in
//! the profile the project uses, Claude's TogetherLink). The project's Tools
//! are applied again by the bridge, as `connecto add` applies them.
//!
//! It only reads, since it is allowed under Pause: a project's Pi providers
//! host is never started for it (its first check may save a moved LM Studio
//! address), so a Pi whose providers are not known yet is unverified; and
//! Antigravity, whose check only sees that `agy` answers, is never more than
//! unverified.
//!
//! A look runs the CLI's sign-in check, so the looks run side by side, one
//! per project, CLI and profile at a time however many calls ask (a call
//! that comes while one runs waits for it), each answer is kept a short
//! while, and a CLI whose look is not back within [`LOOK_WAIT`] of the call
//! is said to be still being checked.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use connecto_core::ToolKind;
use connecto_core::packages::ProjectTools;
use connecto_core::roster::{AgentState, RosterAgent, RosterBridge};

use crate::agentic_works::agents::{AgentCheck, Availability, Profile};
use crate::agents_host::{AgentsHost, JobAgents};

/// How long `connecto agents` waits for the looks, from the call (the
/// command itself waits 40 s).
const LOOK_WAIT: Duration = Duration::from_secs(20);
/// How long a look's answer is kept.
const KEEP: Duration = Duration::from_secs(30);
/// How long a look may run before the next call starts another: one that
/// hangs (a CLI's helper holding its output open) would otherwise keep its
/// CLI `checking` until Connecto restarts.
const STALE: Duration = Duration::from_secs(60);

/// What a look found of one CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub state: AgentState,
    /// The provider whose own model it runs (TogetherLink for Claude).
    pub via: Option<String>,
}

impl Found {
    const fn plain(state: AgentState) -> Self {
        Self { state, via: None }
    }
}

fn state(availability: Availability) -> AgentState {
    match availability {
        Availability::Usable => AgentState::Ready,
        Availability::Unusable(why) => AgentState::Unusable(why),
        Availability::Pending(what) => AgentState::Checking(what),
    }
}

/// Where the roster looks. Every method only reads.
pub trait Probe: Send + Sync + 'static {
    /// The project's Tools, as saved. Cheap.
    fn allows(&self, project: &str, tool: ToolKind) -> bool;
    /// Installed, offered, not switched off. Cheap.
    fn settings(&self, project: &str, tool: ToolKind) -> Availability;
    /// The profile the look depends on. Cheap.
    fn profile(&self, project: &str, tool: ToolKind) -> Profile;
    /// The version the background check found. Cheap.
    fn version(&self, tool: ToolKind) -> Option<String>;
    /// What only the CLI can say. Slow.
    fn look(&self, project: &str, root: &Path, tool: ToolKind, profile: &Profile) -> Found;
}

/// The app's agents, read only.
pub struct HostProbe(pub Arc<AgentsHost>);

impl HostProbe {
    fn check(&self) -> JobAgents {
        JobAgents(Arc::clone(&self.0))
    }
}

impl Probe for HostProbe {
    fn allows(&self, project: &str, tool: ToolKind) -> bool {
        self.check().allows(project, tool)
    }

    fn settings(&self, project: &str, tool: ToolKind) -> Availability {
        self.check().settings(project, tool)
    }

    fn profile(&self, project: &str, tool: ToolKind) -> Profile {
        self.check().profile(project, tool)
    }

    fn version(&self, tool: ToolKind) -> Option<String> {
        self.check().version(tool)
    }

    fn look(&self, _: &str, root: &Path, tool: ToolKind, profile: &Profile) -> Found {
        let check = self.check();
        let sign_in = || state(check.sign_in(root, tool, profile));
        match tool {
            ToolKind::Pi => match profile.scope.as_ref().map(|s| check.pi_known(s)) {
                Some(None) => Found::plain(AgentState::Unverified(
                    "Pi's providers for this project are read when its first Pi terminal starts"
                        .into(),
                )),
                Some(Some(Availability::Usable)) | None => Found::plain(sign_in()),
                Some(Some(other)) => Found::plain(state(other)),
            },
            ToolKind::Antigravity => match sign_in() {
                AgentState::Ready => Found::plain(AgentState::Unverified(
                    "Antigravity has no sign-in status: only that it is installed was checked"
                        .into(),
                )),
                other => Found::plain(other),
            },
            ToolKind::Claude => match check.claude_through_together(profile) {
                Err(why) => Found::plain(AgentState::Unusable(why)),
                Ok(true) => Found {
                    state: AgentState::Ready,
                    via: Some("Together AI".into()),
                },
                // The providers were read once, just above: the sign-in
                // check is given a profile without them, so it runs the
                // status command alone and cannot read them again (a
                // TogetherLink turned on in between would then make it
                // usable while this look says Opus).
                Ok(false) => Found::plain(state(check.sign_in(
                    root,
                    tool,
                    &Profile {
                        scope: None,
                        ..profile.clone()
                    },
                ))),
            },
            _ => Found::plain(sign_in()),
        }
    }
}

type Key = (String, ToolKind, Profile);

/// A look under way: its answer, once in.
#[derive(Default)]
struct Pending {
    answer: Mutex<Option<Found>>,
    done: Condvar,
}

enum Slot {
    Running {
        pending: Arc<Pending>,
        started: Instant,
    },
    Done(Found, Instant),
}

/// The most look threads of one key alive at once, hung ones included.
const MAX_LIVE: usize = 2;

/// The slots, and how many look threads of each key are still alive,
/// counted apart from the slot: a hung thread stays counted after a newer
/// look has replaced its slot, until it really ends.
#[derive(Default)]
struct Table {
    slots: HashMap<Key, Slot>,
    live: HashMap<Key, usize>,
}

type Slots = Arc<Mutex<Table>>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Ends a look however its thread ends (a panic included): the answer is
/// given to whoever waits, kept when it is final, and its slot let go.
struct LookEnd {
    slots: Slots,
    key: Key,
    pending: Arc<Pending>,
    answer: Option<Found>,
}

impl Drop for LookEnd {
    fn drop(&mut self) {
        let answer = self.answer.take().unwrap_or_else(|| {
            Found::plain(AgentState::Checking("its check stopped; ask again".into()))
        });
        {
            // Only the key's current look settles its slot: a stale one
            // that answers late, after it was replaced, never overwrites
            // the newer look's slot or answer.
            let mut table = lock(&self.slots);
            if let Some(live) = table.live.get_mut(&self.key) {
                *live = live.saturating_sub(1);
                if *live == 0 {
                    table.live.remove(&self.key);
                }
            }
            let current = matches!(
                table.slots.get(&self.key),
                Some(Slot::Running { pending, .. }) if Arc::ptr_eq(pending, &self.pending)
            );
            if current {
                if matches!(answer.state, AgentState::Checking(_)) {
                    table.slots.remove(&self.key);
                } else {
                    table
                        .slots
                        .insert(self.key.clone(), Slot::Done(answer.clone(), Instant::now()));
                }
            }
        }
        *lock(&self.pending.answer) = Some(answer);
        self.pending.done.notify_all();
    }
}

pub struct Roster<P: Probe> {
    probe: Arc<P>,
    slots: Slots,
    wait: Duration,
    keep: Duration,
    stale: Duration,
}

impl Roster<HostProbe> {
    pub fn new(host: Arc<AgentsHost>) -> Self {
        Self::with(HostProbe(host), LOOK_WAIT, KEEP, STALE)
    }
}

impl<P: Probe> Roster<P> {
    fn with(probe: P, wait: Duration, keep: Duration, stale: Duration) -> Self {
        Self {
            probe: Arc::new(probe),
            slots: Arc::default(),
            wait,
            keep,
            stale,
        }
    }

    /// The look of `key`: kept, joined if one runs, else started. A look
    /// running past `stale` is replaced by a new one. No look is started
    /// while [`MAX_LIVE`] threads of the key are alive (hung ones count until
    /// they really end): the key is then reported as hung, without waiting.
    fn ask(&self, key: Key, root: &Path) -> Result<Found, Arc<Pending>> {
        let mut table = lock(&self.slots);
        match table.slots.get(&key) {
            Some(Slot::Done(found, at)) if at.elapsed() < self.keep => return Ok(found.clone()),
            Some(Slot::Running { pending, started }) if started.elapsed() < self.stale => {
                return Err(Arc::clone(pending));
            }
            _ => {}
        }
        let live = table.live.entry(key.clone()).or_default();
        if *live >= MAX_LIVE {
            return Ok(Found::plain(AgentState::Checking(
                "its check has hung; Connecto tries again once that ends".into(),
            )));
        }
        *live += 1;
        let pending = Arc::new(Pending::default());
        table.slots.insert(
            key.clone(),
            Slot::Running {
                pending: Arc::clone(&pending),
                started: Instant::now(),
            },
        );
        drop(table);
        let mut end = LookEnd {
            slots: Arc::clone(&self.slots),
            key,
            pending: Arc::clone(&pending),
            answer: None,
        };
        let probe = Arc::clone(&self.probe);
        let root: PathBuf = root.to_owned();
        // If no thread can be made, the closure and its `LookEnd` are
        // dropped: the slot is let go and the answer says to ask again.
        let _ = thread::Builder::new()
            .name("roster-look".into())
            .spawn(move || {
                let (project, tool, profile) = &end.key;
                let found = probe.look(project, &root, *tool, profile);
                // Read when `end` drops, right after.
                end.answer.get_or_insert(found);
            });
        Err(pending)
    }
}

impl<P: Probe> RosterBridge for Roster<P> {
    fn agents(&self, project_id: &str, folder: &Path) -> Vec<RosterAgent> {
        let deadline = Instant::now() + self.wait;
        let mut found: HashMap<ToolKind, Result<Found, Arc<Pending>>> = HashMap::new();
        for tool in ProjectTools::CHOICES {
            let settings = self.probe.settings(project_id, tool);
            if settings != Availability::Usable {
                found.insert(tool, Ok(Found::plain(state(settings))));
                continue;
            }
            let profile = self.probe.profile(project_id, tool);
            found.insert(
                tool,
                self.ask((project_id.to_owned(), tool, profile), folder),
            );
        }
        ProjectTools::CHOICES
            .into_iter()
            .map(|tool| {
                let found = match found.remove(&tool) {
                    Some(Ok(found)) => found,
                    Some(Err(pending)) => {
                        let mut answer = lock(&pending.answer);
                        while answer.is_none() {
                            let left = deadline.saturating_duration_since(Instant::now());
                            if left.is_zero() {
                                break;
                            }
                            answer = pending
                                .done
                                .wait_timeout(answer, left)
                                .unwrap_or_else(PoisonError::into_inner)
                                .0;
                        }
                        answer.clone().unwrap_or_else(|| {
                            Found::plain(AgentState::Checking(
                                "its sign-in is still being checked".into(),
                            ))
                        })
                    }
                    None => Found::plain(AgentState::Checking("not checked".into())),
                };
                RosterAgent {
                    tool,
                    allowed: self.probe.allows(project_id, tool),
                    state: found.state,
                    version: self.probe.version(tool),
                    via: found.via,
                }
            })
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Codex alone is usable by the settings; the looks `hangs` picks (by
    /// their number, from 0) are held until the test lets them go, the rest
    /// answer at once. Every look is counted, and so are the ones alive.
    struct Held {
        looks: AtomicUsize,
        alive: AtomicUsize,
        most_alive: AtomicUsize,
        hangs: fn(usize) -> bool,
        open: Mutex<bool>,
        opened: Condvar,
    }

    impl Held {
        fn new(hangs: fn(usize) -> bool) -> Arc<Self> {
            Arc::new(Self {
                looks: AtomicUsize::new(0),
                alive: AtomicUsize::new(0),
                most_alive: AtomicUsize::new(0),
                hangs,
                open: Mutex::new(false),
                opened: Condvar::new(),
            })
        }

        fn hanging(first: usize) -> Arc<Self> {
            match first {
                1 => Self::new(|n| n < 1),
                _ => Self::new(|_| true),
            }
        }

        fn release(&self) {
            *lock(&self.open) = true;
            self.opened.notify_all();
        }
    }

    impl Probe for Arc<Held> {
        fn allows(&self, _: &str, _: ToolKind) -> bool {
            true
        }
        fn settings(&self, _: &str, tool: ToolKind) -> Availability {
            if tool == ToolKind::Codex {
                Availability::Usable
            } else {
                Availability::Unusable("not installed".into())
            }
        }
        fn profile(&self, _: &str, _: ToolKind) -> Profile {
            Profile::default()
        }
        fn version(&self, _: ToolKind) -> Option<String> {
            None
        }
        fn look(&self, _: &str, _: &Path, _: ToolKind, _: &Profile) -> Found {
            let alive = self.alive.fetch_add(1, Ordering::SeqCst) + 1;
            self.most_alive.fetch_max(alive, Ordering::SeqCst);
            if (self.hangs)(self.looks.fetch_add(1, Ordering::SeqCst)) {
                let mut open = lock(&self.open);
                while !*open {
                    open = self.opened.wait(open).unwrap();
                }
            }
            self.alive.fetch_sub(1, Ordering::SeqCst);
            Found::plain(AgentState::Ready)
        }
    }

    fn codex(list: &[RosterAgent]) -> AgentState {
        list.iter()
            .find(|a| a.tool == ToolKind::Codex)
            .unwrap()
            .state
            .clone()
    }

    /// Two calls at once, and a call again after one gave up, share the one
    /// look that runs: the CLI's check is not run twice, and every caller
    /// gets its answer.
    #[test]
    fn calls_while_a_look_runs_wait_for_it_instead_of_starting_another() {
        let held = Held::hanging(usize::MAX);
        let roster = Arc::new(Roster::with(
            Arc::clone(&held),
            Duration::from_millis(200),
            Duration::from_secs(30),
            Duration::from_secs(60),
        ));
        // The first call gives up while the look is held.
        let first = roster.agents("p", Path::new("/p"));
        assert!(matches!(codex(&first), AgentState::Checking(_)));
        // Two more calls come while it still runs.
        let slow = Arc::new(Roster {
            probe: Arc::clone(&roster.probe),
            slots: Arc::clone(&roster.slots),
            wait: Duration::from_secs(10),
            keep: Duration::from_secs(30),
            stale: Duration::from_secs(60),
        });
        let callers: Vec<_> = (0..2)
            .map(|_| {
                let slow = Arc::clone(&slow);
                thread::spawn(move || slow.agents("p", Path::new("/p")))
            })
            .collect();
        thread::sleep(Duration::from_millis(100));
        held.release();
        for caller in callers {
            assert_eq!(codex(&caller.join().unwrap()), AgentState::Ready);
        }
        assert_eq!(held.looks.load(Ordering::SeqCst), 1, "one look only");
        // Kept: asked again, nothing runs.
        assert_eq!(
            codex(&roster.agents("p", Path::new("/p"))),
            AgentState::Ready
        );
        assert_eq!(held.looks.load(Ordering::SeqCst), 1);
    }

    fn quick(held: &Arc<Held>) -> Roster<Arc<Held>> {
        Roster::with(
            Arc::clone(held),
            Duration::from_millis(50),
            Duration::from_secs(30),
            Duration::from_millis(150),
        )
    }

    /// Second review, finding 1: a look that hangs forever (a CLI's helper
    /// holding its output open) does not keep the CLI `checking` until
    /// Connecto restarts. Once it is stale, the next call starts a new look
    /// and gets its answer.
    #[test]
    fn a_hung_look_is_replaced_once_stale_and_a_later_call_recovers() {
        let held = Held::hanging(1);
        let roster = quick(&held);
        let first = roster.agents("p", Path::new("/p"));
        assert!(matches!(codex(&first), AgentState::Checking(_)));
        // Not stale yet: the call waits for the same look, nothing new runs.
        let again = roster.agents("p", Path::new("/p"));
        assert!(matches!(codex(&again), AgentState::Checking(_)));
        assert_eq!(held.looks.load(Ordering::SeqCst), 1);
        thread::sleep(Duration::from_millis(200));
        let later = roster.agents("p", Path::new("/p"));
        assert_eq!(codex(&later), AgentState::Ready, "a new look answered");
        assert_eq!(held.looks.load(Ordering::SeqCst), 2);
        // The hung look ending late changes nothing: the newer answer stays.
        held.release();
        thread::sleep(Duration::from_millis(50));
        assert_eq!(
            codex(&roster.agents("p", Path::new("/p"))),
            AgentState::Ready
        );
        assert_eq!(held.looks.load(Ordering::SeqCst), 2);
    }

    /// While a replaced look still hangs, a replacement that hangs as well
    /// is not replaced again: at most one stale worker per key, however
    /// many calls come. Once the stale one ends, a new look may start.
    #[test]
    fn at_most_one_stale_look_is_left_behind_per_key() {
        let held = Held::hanging(usize::MAX);
        let roster = quick(&held);
        for _ in 0..6 {
            let list = roster.agents("p", Path::new("/p"));
            assert!(matches!(codex(&list), AgentState::Checking(_)));
            thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(
            held.looks.load(Ordering::SeqCst),
            2,
            "one stale, one current"
        );
        let hung = roster.agents("p", Path::new("/p"));
        assert!(
            matches!(codex(&hung), AgentState::Checking(ref why) if why.contains("hung")),
            "{hung:?}"
        );
        held.release();
        thread::sleep(Duration::from_millis(50));
        assert_eq!(
            codex(&roster.agents("p", Path::new("/p"))),
            AgentState::Ready
        );
    }

    /// Third review, finding 1: the live threads are counted apart from the
    /// cached answer. A hangs; B replaces it and answers; the answer expires;
    /// C hangs; D would start while A and C still hang, but is not: never
    /// more than two threads of a key alive. Once they end, E starts.
    #[test]
    fn a_hung_look_stays_counted_after_its_replacement_answers() {
        let held = Held::new(|n| n == 0 || n == 2);
        let roster = Roster::with(
            Arc::clone(&held),
            Duration::from_millis(50),
            Duration::from_millis(100),
            Duration::from_millis(150),
        );
        let ask = || codex(&roster.agents("p", Path::new("/p")));
        assert!(matches!(ask(), AgentState::Checking(_)), "A hangs");
        thread::sleep(Duration::from_millis(200));
        assert_eq!(ask(), AgentState::Ready, "B replaced A and answered");
        thread::sleep(Duration::from_millis(150));
        assert!(matches!(ask(), AgentState::Checking(_)), "C hangs");
        thread::sleep(Duration::from_millis(200));
        let d = ask();
        assert!(
            matches!(d, AgentState::Checking(ref why) if why.contains("hung")),
            "{d:?}"
        );
        assert_eq!(held.looks.load(Ordering::SeqCst), 3, "D never started");
        assert!(held.most_alive.load(Ordering::SeqCst) <= MAX_LIVE);
        held.release();
        thread::sleep(Duration::from_millis(50));
        // C, still the current look, answers when it ends; A's late answer
        // changes nothing.
        assert_eq!(ask(), AgentState::Ready, "C answered once let go");
        assert_eq!(held.looks.load(Ordering::SeqCst), 3);
        // Both ended and uncounted: once the answer expires, E starts.
        thread::sleep(Duration::from_millis(150));
        assert_eq!(ask(), AgentState::Ready, "E answered");
        assert_eq!(held.looks.load(Ordering::SeqCst), 4);
    }
}
