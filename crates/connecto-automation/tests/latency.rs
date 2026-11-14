//! How long an automation takes from the moment its trigger text is on a
//! terminal to the moment its keys are typed, with the real engine threads and
//! real timers, a stand-in host that stamps every step, and a stand-in Jev that
//! answers after a chosen delay. Nothing leaves the machine: no key, no network.
//!
//! The engine's own part (noticing, settling, queueing, typing) is what is
//! measured here; the time Jev takes is the delay the stand-in is given.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::expect_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use connecto_automation::jev::{Answer, Exchange, Response, Usage};
use connecto_automation::{Asker, Automations, Config, Host, MemoryKeys, ProjectInfo, TermInfo};
use connecto_core::{TermActivity, ToolKind};
use serde_json::{Value, json};

#[derive(Default)]
struct Stamps {
    changed: Option<Instant>,
    seen: Option<Instant>,
    asked: Option<Instant>,
    answered: Option<Instant>,
    typed: Option<Instant>,
    reads: usize,
}

struct Rig {
    root: PathBuf,
    screens: Mutex<HashMap<String, String>>,
    state: Mutex<TermActivity>,
    stamps: Mutex<Stamps>,
    trigger: String,
    jev_delay: Duration,
}

impl Host for Rig {
    fn terminals(&self) -> Vec<TermInfo> {
        vec![TermInfo {
            project_id: "p1".into(),
            project_name: "proj".into(),
            project_path: PathBuf::from("/tmp/proj"),
            id: "claude-1".into(),
            name: None,
            tool: ToolKind::Claude,
            state: *self.state.lock().unwrap(),
        }]
    }
    fn screen(&self, _p: &str, t: &str) -> Option<String> {
        let text = self.screens.lock().unwrap().get(t).cloned()?;
        let mut stamps = self.stamps.lock().unwrap();
        stamps.reads += 1;
        if text.contains(&self.trigger) && stamps.seen.is_none() {
            stamps.seen = Some(Instant::now());
        }
        Some(text)
    }
    fn paused(&self) -> bool {
        false
    }
    fn keys(&self, _p: &str, _t: &str, _keys: &[String]) -> Result<(), String> {
        self.stamps.lock().unwrap().typed = Some(Instant::now());
        Ok(())
    }
    fn open_project(&self, _dir: &Path) -> Result<(), String> {
        Err("no".into())
    }
    fn project_at(&self, _dir: &Path) -> Option<ProjectInfo> {
        None
    }
    fn add_terminal(
        &self,
        _p: &str,
        _t: ToolKind,
        _n: Option<String>,
        _pr: Option<String>,
        _r: Option<String>,
        _rn: Option<String>,
    ) -> Result<String, String> {
        Err("no".into())
    }
    fn terminal_session(&self, _p: &str, _t: &str) -> Option<String> {
        None
    }
    fn show(&self, _p: &str, _t: Option<&str>) {}
    fn saved_root(&self) -> Option<PathBuf> {
        Some(self.root.clone())
    }
    fn save_root(&self, _root: Option<PathBuf>) -> Result<(), String> {
        Ok(())
    }
}

struct FakeJev {
    rig: Arc<Rig>,
    sizes: Mutex<Vec<usize>>,
}

impl Asker for FakeJev {
    fn ask(&self, _key: &str, model: &str, state: &str, q: &[(String, Value)]) -> Exchange {
        self.rig.stamps.lock().unwrap().asked = Some(Instant::now());
        let questions: serde_json::Map<String, Value> = q.iter().cloned().collect();
        let request = json!({"model": model, "state": state, "questions": questions}).to_string();
        self.sizes.lock().unwrap().push(request.len());
        std::thread::sleep(self.rig.jev_delay);
        self.rig.stamps.lock().unwrap().answered = Some(Instant::now());
        Exchange {
            result: Ok(Response {
                answers: q
                    .iter()
                    .map(|(n, _)| {
                        (
                            n.clone(),
                            Answer::Choice {
                                choice: "proceed".into(),
                                confidence: 0.97,
                            },
                        )
                    })
                    .collect(),
                usage: Usage {
                    input_tokens: 1800,
                    output_tokens: 5,
                },
            }),
            request,
            response: "{}".into(),
        }
    }
}

/// One run: the trigger appears on a terminal in `state`; returns the stage
/// times in milliseconds (change to first read, to the call, Jev, to the
/// keys, total) and the request size.
fn run(state: TermActivity, jev_delay: Duration) -> (u128, u128, u128, u128, u128, usize) {
    let dir = std::env::temp_dir().join(format!(
        "connecto-latency-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let root = dir.join("automations");
    std::fs::create_dir_all(root.join("accept")).unwrap();
    let trigger = "Do you want to proceed";
    std::fs::write(
        root.join("accept").join("automation.json"),
        json!({
            "version": 1, "name": "accept", "enabled": true, "agent": "claude",
            "model": "jev-1.14.0",
            "question": {"type": "choice", "instructions": "Is it the prompt?",
                "criteria": {"proceed": "Do you want to proceed", "other": "else"}},
            "match": "proceed", "keys": ["enter"], "threshold": 0.8,
            "trigger": [trigger]
        })
        .to_string(),
    )
    .unwrap();
    let rig = Arc::new(Rig {
        root: std::fs::canonicalize(&root).unwrap(),
        screens: Mutex::new(HashMap::from([(
            "claude-1".to_owned(),
            "starting…".to_owned(),
        )])),
        state: Mutex::new(state),
        stamps: Mutex::default(),
        trigger: trigger.to_owned(),
        jev_delay,
    });
    let jev = Arc::new(FakeJev {
        rig: Arc::clone(&rig),
        sizes: Mutex::default(),
    });
    let engine = Automations::with(Config {
        seed_defaults: false,
        data_root: dir.join("data"),
        home: dir.join("home"),
        host: rig.clone(),
        keys: Arc::new(MemoryKeys::with("test-key-not-real")),
        asker: jev.clone(),
        wake: Arc::new(|| {}),
    })
    .unwrap();
    // The engine settles on the first screen, then the trigger text arrives.
    std::thread::sleep(Duration::from_millis(1500));
    let t0 = Instant::now();
    rig.stamps.lock().unwrap().changed = Some(t0);
    rig.screens.lock().unwrap().insert(
        "claude-1".to_owned(),
        format!("Some header\n{trigger}?\n 1. Yes\n 2. No\n"),
    );
    for _ in 0..200 {
        if rig.stamps.lock().unwrap().typed.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let s = rig.stamps.lock().unwrap();
    let ms = |a: Option<Instant>, b: Option<Instant>| {
        b.zip(a)
            .map_or(u128::MAX, |(b, a)| b.duration_since(a).as_millis())
    };
    let out = (
        ms(s.changed, s.seen),
        ms(s.seen, s.asked),
        ms(s.asked, s.answered),
        ms(s.answered, s.typed),
        ms(s.changed, s.typed),
        jev.sizes.lock().unwrap().first().copied().unwrap_or(0),
    );
    drop(s);
    drop(engine);
    let _ = std::fs::remove_dir_all(&dir);
    out
}

#[test]
fn from_the_trigger_on_the_screen_to_the_keys() {
    let jev = Duration::from_millis(200);
    println!("stage times in ms (Jev answers in {} ms)", jev.as_millis());
    println!("state        noticed  settle+queue  jev  answer->keys  TOTAL  request bytes");
    let mut totals = Vec::new();
    let mut answer_to_keys = Vec::new();
    for (name, state) in [
        ("NeedsUser", TermActivity::NeedsUser),
        ("Starting", TermActivity::Starting),
        ("Idle", TermActivity::Idle),
    ] {
        let (a, b, c, d, total, bytes) = run(state, jev);
        println!("{name:<11} {a:>8} {b:>13} {c:>5} {d:>13} {total:>6} {bytes:>10}");
        totals.push((name, total));
        answer_to_keys.push(d);
    }
    // Before the engine acted on a result at once, read idle screens every
    // second and settled for 1.5 s: 1.6 s, 3.2 s and 3.2 s. These limits leave
    // room for a slow machine and still fail if those waits come back.
    for ((name, total), limit) in totals.into_iter().zip([1300, 1900, 1900]) {
        assert!(total < limit, "{name}: {total} ms");
    }
    for ms in answer_to_keys {
        assert!(ms < 150, "the answer waited {ms} ms for the next tick");
    }
}
