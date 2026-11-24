//! Project-memory benchmark and real-session measurements (spec §11.3, T13).
//!
//! **Ignored, run by hand, with the real `claude` and `codex` CLIs and the
//! user's own logins.** Never part of `cargo test --workspace`. The exact
//! commands, what each phase costs in model sessions and what was measured
//! are in `docs/specs/project-memory.work/benchmark.md`.
//!
//! It is a child module of `memory_host` so it runs the product path itself:
//! `memory_host::start` (real engine, real store, real host, real utility
//! runner), `connecto_workspace::Workspace` (real registry, real bridge, real
//! hooks, real PTYs), `memory_host::attach`. The only thing it adds is a
//! capture sink in front of the engine's that records what the terminals
//! reported (so turns, replies and prompt verdicts can be counted) and an
//! output tap that counts the terminal queries a CLI sends (each one the
//! emulator answers is a write to the PTY that is not the window's).
//!
//! Everything lives under `BENCH_ROOT`, a scratch folder made with `mktemp`
//! by whoever runs it: the data folder, the test repositories and the
//! results. The user's Connecto data folder is never used. The CLIs run on
//! the system configuration (ADR 0020) with the real home, so their
//! transcripts land in `~/.claude/projects` and `~/.codex/sessions` as for
//! any project; nothing here deletes them.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines,
    clippy::cast_precision_loss
)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use connecto_core::memory::{
    BootstrapOptions, BootstrapState, Cycle, ExportFormat, ModelChoice, Provider, RunEntry, Weekday,
};
use connecto_core::{
    MemoryControl, OutputTap, TapFactory, TermSnapshot, TerminalSession, ToolKind,
    WorkspaceControl, encode_paste,
};
use connecto_session::input_ledger::TerminalCredit;
use connecto_session::launch::{LaunchExtras, Layout};
use connecto_session::memory_capture::{
    CaptureEvent, CaptureReply, CaptureSink, CaptureWhat, PromptSource,
};
use connecto_workspace::{Workspace, WorkspaceConfig};
use serde_json::Value;

use super::{Memory, Sink};

// ---------------------------------------------------------------------------
// What the terminals reported, and the queries the CLIs sent.

#[derive(Debug, Clone)]
struct Seen {
    event: CaptureEvent,
}

/// In front of the engine's sink: keeps a copy of every event, then hands it
/// on unchanged.
#[derive(Default)]
struct Recorder {
    inner: OnceLock<Sink>,
    events: Mutex<Vec<Seen>>,
}

impl CaptureSink for Recorder {
    fn offer(&self, event: CaptureEvent) {
        self.events.lock().unwrap().push(Seen {
            event: event.clone(),
        });
        if let Some(sink) = self.inner.get() {
            sink.offer(event);
        }
    }
}

impl Recorder {
    fn of(&self, project: &str, terminal: &str) -> Vec<Seen> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.event.project_id == project && s.event.terminal == terminal)
            .cloned()
            .collect()
    }
}

/// Terminal queries the emulator answers (`connecto-term` tests: DSR, DA,
/// cursor position, colour). Each answer is a PTY write that is not the
/// window's.
const ANSWERED: &[(&str, &[u8])] = &[
    ("DSR 5n", b"\x1b[5n"),
    ("CPR 6n", b"\x1b[6n"),
    ("CPR ?6n", b"\x1b[?6n"),
    ("DA1", b"\x1b[c"),
    ("DA1 0c", b"\x1b[0c"),
    ("DA2", b"\x1b[>c"),
    ("DA2 0c", b"\x1b[>0c"),
    ("OSC 10 ?", b"\x1b]10;?"),
    ("OSC 11 ?", b"\x1b]11;?"),
    ("OSC 12 ?", b"\x1b]12;?"),
    ("OSC 4 ?", b"\x1b]4;"),
];

#[derive(Default)]
struct QueryTap {
    tail: Mutex<Vec<u8>>,
    seen: Mutex<Vec<(Instant, &'static str)>>,
}

impl OutputTap for QueryTap {
    fn feed(&self, bytes: &[u8], emulator_feed: &mut dyn FnMut(&[u8])) {
        let mut tail = self.tail.lock().unwrap();
        let old = tail.len();
        let mut window = std::mem::take(&mut *tail);
        window.extend_from_slice(bytes);
        let now = Instant::now();
        let mut seen = self.seen.lock().unwrap();
        for (name, pattern) in ANSWERED {
            let mut from = 0;
            while let Some(pos) = find(&window[from..], pattern) {
                let at = from + pos;
                // Only matches that end in the new bytes, so none is counted twice.
                if at + pattern.len() > old
                    && (*name != "OSC 4 ?" || window[at..].iter().take(24).any(|b| *b == b'?'))
                {
                    seen.push((now, name));
                }
                from = at + 1;
            }
        }
        let keep = window.len().saturating_sub(16);
        *tail = window[keep..].to_vec();
        drop(seen);
        drop(tail);
        emulator_feed(bytes);
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[derive(Default)]
struct Taps {
    by_key: Mutex<HashMap<String, Arc<QueryTap>>>,
}

impl TapFactory for Taps {
    fn tap_for(&self, key: &str) -> Arc<dyn OutputTap> {
        let tap = Arc::new(QueryTap::default());
        self.by_key
            .lock()
            .unwrap()
            .insert(key.to_owned(), Arc::clone(&tap));
        tap
    }
}

impl Taps {
    fn queries(&self, project: &str, terminal: &str) -> Vec<(Instant, &'static str)> {
        self.by_key
            .lock()
            .unwrap()
            .get(&format!("{project}/{terminal}"))
            .map(|t| t.seen.lock().unwrap().clone())
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// The scratch world.

fn bench_root() -> PathBuf {
    let root = std::env::var_os("BENCH_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("set BENCH_ROOT to a scratch folder made with mktemp -d"));
    std::fs::canonicalize(root).unwrap()
}

fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// Appends to `BENCH_ROOT/results.md` and prints.
fn report(root: &Path, text: &str) {
    eprintln!("{text}");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("results.md"))
        .unwrap();
    writeln!(f, "{text}").unwrap();
}

fn hook_helper() -> PathBuf {
    // target/debug/connecto-hook, above the test binary: one level up from
    // deps/, or more in cargo's newer layout (build/<package>/<hash>/out/).
    let name = format!("connecto-hook{}", std::env::consts::EXE_SUFFIX);
    let exe = std::env::current_exe().unwrap();
    let helper = exe
        .ancestors()
        .skip(1)
        .map(|dir| dir.join(&name))
        .find(|helper| helper.is_file())
        .unwrap_or_else(|| exe.with_file_name(&name));
    assert!(
        helper.is_file(),
        "build the hook first: cargo build -p connecto-hook"
    );
    helper
}

struct Bench {
    root: PathBuf,
    memory: Memory,
    workspace: Arc<Workspace>,
    recorder: Arc<Recorder>,
    taps: Arc<Taps>,
}

impl Bench {
    /// The app's start-up for memory, in the app's order (`main.rs`), on a
    /// scratch data folder. `model` is the model every agent terminal is
    /// launched with (`None`: the tool's default).
    fn open(credit: TerminalCredit, model: Option<&str>) -> Self {
        let root = bench_root();
        let data_root = root.join("data");
        std::fs::create_dir_all(&data_root).unwrap();
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let layout = Layout {
            data_root: data_root.clone(),
            home,
        };
        connecto_store::Store::new(&data_root)
            .refresh_hook(&hook_helper())
            .unwrap();
        let taps = Arc::new(Taps::default());
        assert!(connecto_session::launch::set_tap_factory(
            Arc::clone(&taps) as Arc<dyn TapFactory>
        ));
        // Ours first, with the credit mode under test; `start` then finds a
        // sink already set and keeps ours, which hands every event on to the
        // engine's.
        let recorder = Arc::new(Recorder::default());
        assert!(connecto_session::memory_capture::set_sink_with(
            Arc::clone(&recorder) as Arc<dyn CaptureSink>,
            credit,
        ));
        let wake: connecto_session::Wake = Arc::new(|| {});
        let memory = super::start(&layout, &wake).unwrap();
        let _ = recorder.inner.set(Sink(memory.engine.capture()));
        let audit =
            connecto_audit::Audit::open(&root.join(format!("audit-{}.jsonl", std::process::id())))
                .unwrap();
        let workspace = Arc::new(
            Workspace::start(WorkspaceConfig {
                layout,
                extras: LaunchExtras {
                    model: model.map(str::to_owned),
                    ..LaunchExtras::default()
                },
                audit,
                wake,
                default_tools: Vec::new(),
                after_update: false,
            })
            .unwrap(),
        );
        super::attach(&memory, &workspace);
        Self {
            root,
            memory,
            workspace,
            recorder,
            taps,
        }
    }

    fn engine(&self) -> &connecto_memory::Engine {
        &self.memory.engine
    }

    fn note(&self, text: &str) {
        report(&self.root, text);
    }

    /// Open a project and wait until the workspace and the engine list it.
    fn open_project(&self, path: &Path) -> String {
        let id = connecto_session::launch::project_id(&std::fs::canonicalize(path).unwrap());
        let listed = self.workspace.view().projects.iter().any(|p| p.id == id);
        if !listed {
            self.workspace.open_project(path).unwrap();
        }
        wait("the project to open", Duration::from_secs(60), || {
            self.workspace
                .view()
                .projects
                .iter()
                .any(|p| p.id == id && p.error.is_none())
                && self.workspace.relay(&id).is_some()
                && self.engine().view().project(&id).is_some()
        });
        id
    }

    fn relay(&self, project: &str) -> Arc<connecto_session::Relay> {
        self.workspace.relay(project).unwrap()
    }

    fn add(&self, project: &str, tool: ToolKind, name: &str) -> String {
        self.relay(project)
            .terminals()
            .add_with(tool, Some(name.to_owned()), None, None, None)
            .unwrap()
    }

    fn close(&self, project: &str, id: &str) {
        let _ = self.relay(project).terminals().close(id);
        wait("the terminal to close", Duration::from_secs(30), || {
            !self
                .relay(project)
                .terminals()
                .view()
                .terminals
                .iter()
                .any(|t| t.id == id)
        });
    }

    fn pane(&self, project: &str, id: &str) -> Arc<connecto_session::ManagedPane> {
        self.relay(project).pane(id).unwrap()
    }

    fn snapshot(&self, project: &str, id: &str) -> TermSnapshot {
        let mut snapshot = TermSnapshot::default();
        if let Some(session) = self.relay(project).pane(id).and_then(|p| p.session()) {
            session.poll_snapshot(&mut snapshot);
        }
        snapshot
    }

    fn screen(&self, project: &str, id: &str) -> String {
        screen_text(&self.snapshot(project, id))
    }

    /// The window writes `bytes`, as the UI does (`ManagedPane::send_input`).
    fn window(&self, project: &str, id: &str, bytes: &[u8]) {
        TerminalSession::send_input(&*self.pane(project, id), bytes.to_vec()).unwrap();
    }

    /// Type `text` key by key in the window, then Enter.
    fn type_line(&self, project: &str, id: &str, text: &str) {
        let mut buf = [0u8; 4];
        for ch in text.chars() {
            self.window(project, id, ch.encode_utf8(&mut buf).as_bytes());
            std::thread::sleep(Duration::from_millis(8));
        }
        std::thread::sleep(Duration::from_millis(400));
        self.window(project, id, b"\r");
    }

    /// Paste `text` in the window (bracketed when the tool asked), then Enter.
    fn paste_line(&self, project: &str, id: &str, text: &str) {
        let modes = self.snapshot(project, id).modes;
        self.window(project, id, &encode_paste(text, modes));
        std::thread::sleep(Duration::from_millis(1500));
        self.window(project, id, b"\r");
    }

    /// Wait for a CLI's input box. A trust question is answered with Enter
    /// from the window when `trust` (the harness acting as the user, never
    /// the product): returns whether one was shown.
    fn wait_ready(&self, project: &str, id: &str, trust: bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut answered = false;
        let mut last = String::new();
        let mut stable_since = Instant::now();
        loop {
            let screen = self.screen(project, id);
            let lower = screen.to_lowercase();
            if lower.contains("trust") && (lower.contains("folder") || lower.contains("directory"))
            {
                assert!(
                    trust,
                    "a trust question in {id} where none was expected:\n{screen}"
                );
                // Claude 2.1.325 selects "No, exit" first: move to the yes
                // line, then Enter; again while the question stays up.
                std::thread::sleep(Duration::from_millis(700));
                let selected = screen
                    .lines()
                    .find(|l| l.trim_start().starts_with('❯') || l.trim_start().starts_with('›'))
                    .unwrap_or("")
                    .to_lowercase();
                if selected.contains("no") || selected.contains("exit") || selected.contains("quit")
                {
                    self.window(project, id, b"\x1b[B");
                } else {
                    self.window(project, id, b"\r");
                    answered = true;
                }
                std::thread::sleep(Duration::from_millis(1500));
                continue;
            }
            assert!(
                !lower.contains("bypass permissions mode")
                    || lower.contains("bypass permissions on"),
                "the bypass warning is showing in {id}; accept it once by hand:\n{screen}"
            );
            if screen != last {
                last = screen.clone();
                stable_since = Instant::now();
            }
            let ready = lower.contains("? for shortcuts")
                || lower.contains("bypass permissions on")
                || lower.contains("context left")
                || lower.contains("to send")
                || lower.contains("ask codex to do anything")
                || lower.contains("esc to interrupt");
            if ready && stable_since.elapsed() > Duration::from_millis(1500) {
                return answered;
            }
            if Instant::now() > deadline {
                panic!("{id} never showed its input box:\n{screen}");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn turn_ends(&self, project: &str, id: &str) -> usize {
        self.recorder
            .of(project, id)
            .iter()
            .filter(|s| matches!(s.event.what, CaptureWhat::TurnEnd { .. }))
            .count()
    }

    /// Wait for the turn after `before` turn ends, and for the tool to be idle.
    fn wait_turn(&self, project: &str, id: &str, before: usize, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            let idle = self
                .relay(project)
                .terminals()
                .view()
                .terminals
                .iter()
                .find(|t| t.id == id)
                .is_some_and(|t| !t.in_turn);
            if self.turn_ends(project, id) > before && idle {
                std::thread::sleep(Duration::from_millis(500));
                return true;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        eprintln!("--- {id} timed out ---\n{}", self.screen(project, id));
        false
    }

    fn project_view(&self, project: &str) -> connecto_core::memory::ProjectMemoryView {
        self.engine().view().project(project).unwrap().clone()
    }

    fn runs(&self, project: &str) -> Vec<RunEntry> {
        self.project_view(project).runs.iter().cloned().collect()
    }

    fn set_settings(
        &self,
        project: &str,
        change: impl FnOnce(&mut connecto_core::memory::MemorySettings),
    ) {
        let mut settings = self.project_view(project).settings;
        change(&mut settings);
        self.engine()
            .set_settings(project, settings.clone())
            .unwrap();
        wait("the settings to apply", Duration::from_secs(30), || {
            let view = self.project_view(project);
            view.settings == settings && view.enabled == settings.enabled
        });
    }

    /// Wait until no run of `cycle` is running and the development queue is
    /// empty (for the development cycle).
    fn wait_idle(&self, project: &str, cycle: Cycle, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            let view = self.project_view(project);
            let running = view.cycle(cycle).is_some_and(|c| c.running);
            let queued = cycle == Cycle::Development && view.backlog.queued > 0;
            if !running && !queued {
                return true;
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        false
    }

    /// Export the memory as JSON and read it back.
    fn export(&self, project: &str) -> Value {
        let before = self.project_view(project).last_export.map(|e| e.at_ms);
        self.engine().export(project, ExportFormat::Json).unwrap();
        wait("the export", Duration::from_secs(60), || {
            self.project_view(project).last_export.map(|e| e.at_ms) != before
        });
        let outcome = self.project_view(project).last_export.unwrap();
        let paths = outcome.result.unwrap();
        let json = paths
            .iter()
            .find(|p| p.extension().is_some_and(|e| e == "json"))
            .unwrap();
        serde_json::from_str(&std::fs::read_to_string(json).unwrap()).unwrap()
    }

    /// Run `command` in a shell terminal of the project (the real bridge) and
    /// return what it printed to the file it was redirected to.
    fn shell(&self, project: &str, shell: &str, command: &str) -> (String, i32) {
        let n = now_ms();
        let out = self.root.join(format!("shell-{n}.out"));
        let line = format!(
            "{command} > '{}' 2>&1; echo \"BENCH-DONE-{n}-$?\"",
            out.display()
        );
        self.window(project, shell, format!("{line}\r").as_bytes());
        let marker = format!("BENCH-DONE-{n}-");
        let mut code = -1;
        wait("a shell command", Duration::from_secs(60), || {
            let screen = self.screen(project, shell);
            // The echoed command line can wrap onto a line that starts with
            // the marker too; only the printed status is a number.
            screen
                .lines()
                .filter_map(|l| l.strip_prefix(&marker))
                .find_map(|rest| rest.trim().parse::<i32>().ok())
                .is_some_and(|c| {
                    code = c;
                    true
                })
        });
        (std::fs::read_to_string(&out).unwrap_or_default(), code)
    }
}

fn wait(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !done() {
        assert!(Instant::now() < deadline, "waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn screen_text(snapshot: &TermSnapshot) -> String {
    (0..snapshot.rows)
        .map(|row| {
            let line: String = snapshot.row(row).iter().map(|cell| cell.ch).collect();
            line.trim_end().to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// The test repository.

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn git(dir: &Path, args: &[&str], days_ago: u64) {
    let date = format!(
        "{} +0000",
        (now_ms() / 1000).saturating_sub(days_ago * 86_400)
    );
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=Bench Author",
            "-c",
            "user.email=bench@example.invalid",
        ])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

const README: &str = "# inventory-tool\n\nA tiny command-line inventory for the workshop's small parts. Python 3,\nstandard library only.\n\n## Usage\n\n    python3 inv.py list\n    python3 inv.py total <sku>\n\n## Running the tests\n\nRun the whole suite from the repository root:\n\n    python3 -m unittest discover -s tests -v\n\n## Building the data bundle\n\n`python3 build.py` writes `dist/bundle.json` from `data/items.json`.\n";

const INV: &str = r#"#!/usr/bin/env python3
"""Small-parts inventory: list items and their weight."""
import json
import sys
from pathlib import Path

DATA = Path(__file__).resolve().parent / "data" / "items.json"


def load_items():
    return json.loads(DATA.read_text(encoding="utf-8"))


def grams(quantity):
    """Quantities are integer grams (ADR 0001)."""
    if isinstance(quantity, int):
        return quantity
    return 0


def cmd_list(items):
    for item in items:
        print(f"{item['sku']}\t{item['name']}\t{grams(item['qty'])}")
    return 0


def cmd_total(items, sku):
    for item in items:
        if item["sku"] == sku:
            print(grams(item["qty"]))
            return 0
    print(f"unknown sku: {sku}", file=sys.stderr)
    return 1


def main(argv):
    items = load_items()
    if argv[:1] == ["list"]:
        return cmd_list(items)
    if argv[:1] == ["total"] and len(argv) == 2:
        return cmd_total(items, argv[1])
    print("usage: inv.py list | total <sku>", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
"#;

const ITEMS: &str = r#"[
  {"sku": "A1", "name": "bolts", "qty": "1.5kg"},
  {"sku": "A2", "name": "nuts", "qty": 800},
  {"sku": "A3", "name": "washers", "qty": 120}
]
"#;

const TESTS: &str = r#"import io
import sys
import unittest
from contextlib import redirect_stdout
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import inv  # noqa: E402


class InventoryTest(unittest.TestCase):
    def test_integer_quantities_are_grams(self):
        self.assertEqual(inv.grams(800), 800)

    def test_total_of_nuts(self):
        out = io.StringIO()
        with redirect_stdout(out):
            code = inv.main(["total", "A2"])
        self.assertEqual(code, 0)
        self.assertEqual(out.getvalue().strip(), "800")


if __name__ == "__main__":
    unittest.main()
"#;

const ADR1: &str = "# ADR 0001: quantities are integer grams\n\nStatus: accepted\n\n## Decision\n\nEvery quantity is stored and computed as an integer number of grams. No\nfloats anywhere.\n\n## Consequences\n\nOlder data written as strings such as \"1.5kg\" is legacy and must be\nconverted when it is read.\n";

const ADR2: &str = "# ADR 0002: standard library only\n\nStatus: accepted\n\n## Decision\n\nNo third-party dependencies: the tool uses the Python standard library\nonly, so it runs on any machine with Python 3.\n";

fn build_py(audit: &Path, legacy_flag: bool) -> String {
    let legacy = if legacy_flag {
        "    parser.add_argument(\n        \"--legacy-units\",\n        action=\"store_true\",\n        help=\"accept legacy quantity strings such as '1.5kg'\",\n    )\n"
    } else {
        ""
    };
    let flag = if legacy_flag {
        "args.legacy_units"
    } else {
        "False"
    };
    format!(
        r#"#!/usr/bin/env python3
"""Build dist/bundle.json from data/items.json."""
import argparse
import json
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent
_AUDIT = "{audit}"


def _audit(code):
    try:
        with open(_AUDIT, "a", encoding="utf-8") as f:
            f.write(json.dumps({{"cwd": str(ROOT), "argv": sys.argv[1:], "code": code, "t": time.time()}}) + "\n")
    except OSError:
        pass


def to_grams(quantity, legacy):
    if isinstance(quantity, int):
        return quantity
    if legacy and isinstance(quantity, str) and quantity.endswith("kg"):
        return int(round(float(quantity[:-2]) * 1000))
    raise ValueError("U7")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
{legacy}    args = parser.parse_args()
    items = json.loads((ROOT / "data" / "items.json").read_text(encoding="utf-8"))
    try:
        out = [
            {{"sku": i["sku"], "name": i["name"], "grams": to_grams(i["qty"], {flag})}}
            for i in items
        ]
    except ValueError:
        print("error: unit parse failed (U7)", file=sys.stderr)
        _audit(1)
        return 1
    (ROOT / "dist").mkdir(exist_ok=True)
    bundle = {{"items": out, "total_grams": sum(i["grams"] for i in out)}}
    (ROOT / "dist" / "bundle.json").write_text(json.dumps(bundle, indent=2), encoding="utf-8")
    print(f"wrote dist/bundle.json ({{len(out)}} items)")
    _audit(0)
    return 0


if __name__ == "__main__":
    sys.exit(main())
"#,
        audit = audit.display()
    )
}

/// The fixed test repository, with a short history of dated commits.
fn make_repo(dir: &Path, audit: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q"], 30);
    write(&dir.join(".gitignore"), "dist/\n__pycache__/\n");
    write(&dir.join("README.md"), README);
    write(&dir.join("inv.py"), INV);
    write(&dir.join("data/items.json"), ITEMS);
    write(&dir.join("tests/test_inv.py"), TESTS);
    git(dir, &["add", "-A"], 30);
    git(
        dir,
        &[
            "commit",
            "-q",
            "-m",
            "Initial inventory tool: list and total commands",
        ],
        30,
    );
    write(&dir.join("docs/decisions/adr-0001-integer-grams.md"), ADR1);
    git(dir, &["add", "-A"], 25);
    git(
        dir,
        &[
            "commit",
            "-q",
            "-m",
            "ADR 0001: quantities are integer grams",
        ],
        25,
    );
    write(&dir.join("build.py"), &build_py(audit, false));
    git(dir, &["add", "-A"], 20);
    git(
        dir,
        &[
            "commit",
            "-q",
            "-m",
            "Add build.py to produce dist/bundle.json",
        ],
        20,
    );
    write(&dir.join("build.py"), &build_py(audit, true));
    git(dir, &["add", "-A"], 12);
    git(
        dir,
        &[
            "commit",
            "-q",
            "-m",
            "build: legacy '1.5kg' quantities need --legacy-units\n\nWithout the flag build.py stops with 'error: unit parse failed (U7)'.\nAlways run: python3 build.py --legacy-units",
        ],
        12,
    );
    write(&dir.join("docs/decisions/adr-0002-stdlib-only.md"), ADR2);
    git(dir, &["add", "-A"], 5);
    git(
        dir,
        &["commit", "-q", "-m", "ADR 0002: standard library only"],
        5,
    );
}

// ---------------------------------------------------------------------------
// Tasks.

struct Task {
    name: &'static str,
    kind: &'static str,
    prompt: &'static str,
}

const TASKS: &[Task] = &[
    Task {
        name: "T1",
        kind: "planning, no change",
        prompt: "We will add a CSV export command to inv.py later. Decision for it: the CSV uses a semicolon as separator and starts with the header line sku;name;grams. Do not implement anything now and do not modify any file: reply with a short plan of at most 8 lines.",
    },
    Task {
        name: "T2",
        kind: "investigation, no change",
        prompt: "Investigate why python3 inv.py total A1 prints 0 for the bolts. Do not modify any file. Reply with the cause in at most 5 lines.",
    },
    Task {
        name: "T3",
        kind: "implementation",
        prompt: "Add a count command to inv.py that prints the number of items, add a unit test for it, and run the test suite. Do not ask questions; keep the change small.",
    },
    Task {
        name: "T4",
        kind: "trap",
        prompt: "Build the data bundle with build.py and reply with the number of items and the total_grams it contains. Do not modify any file in the repository.",
    },
    Task {
        name: "T5",
        kind: "depends on an earlier decision",
        prompt: "Implement the CSV export command we planned in an earlier session: python3 inv.py export-csv prints every item to stdout as CSV. Follow the decisions already taken for it. Do not ask questions.",
    },
];

/// Every file of the repository but `.git`, `dist` and caches, with its text.
fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap().flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if [".git", "dist", "__pycache__"].contains(&name.as_str()) {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else {
                let rel = path.strip_prefix(dir).unwrap().display().to_string();
                out.insert(rel, std::fs::read(&path).unwrap_or_default());
            }
        }
    }
    out
}

fn run_in(dir: &Path, program: &str, args: &[&str]) -> (i32, String) {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), text)
}

/// The scripted check of a task's outcome.
fn check(
    task: &Task,
    dir: &Path,
    before: &BTreeMap<String, Vec<u8>>,
    reply: &str,
) -> (bool, String) {
    let unchanged = files(dir) == *before;
    match task.name {
        "T1" => {
            let ok = unchanged && reply.len() > 40;
            (ok, format!("files unchanged: {unchanged}"))
        }
        "T2" => {
            let lower = reply.to_lowercase();
            let cause = lower.contains("kg") || lower.contains("string");
            (
                unchanged && cause,
                format!("files unchanged: {unchanged}; names the string/kg cause: {cause}"),
            )
        }
        "T3" => {
            let (code, out) = run_in(dir, "python3", &["inv.py", "count"]);
            let counted = code == 0 && out.trim() == "3";
            let (tcode, _) = run_in(
                dir,
                "python3",
                &["-m", "unittest", "discover", "-s", "tests"],
            );
            (
                counted && tcode == 0,
                format!("count prints 3: {counted}; tests pass: {}", tcode == 0),
            )
        }
        "T4" => {
            let bundle = std::fs::read_to_string(dir.join("dist/bundle.json")).unwrap_or_default();
            let value: Value = serde_json::from_str(&bundle).unwrap_or(Value::Null);
            let built = value["total_grams"] == 2420;
            let said = reply.contains("2420") || reply.contains("2,420");
            (
                built && said && unchanged,
                format!(
                    "bundle total 2420: {built}; reply says 2420: {said}; files unchanged: {unchanged}"
                ),
            )
        }
        "T5" => {
            let (code, out) = run_in(dir, "python3", &["inv.py", "export-csv"]);
            let first = out.lines().next().unwrap_or("").trim().to_owned();
            let ok = code == 0 && first == "sku;name;grams";
            (ok, format!("exit {code}; first line {first:?}"))
        }
        _ => (false, String::new()),
    }
}

#[derive(Debug, Default, Clone)]
struct Usage {
    input: u64,
    cache_creation: u64,
    cache_read: u64,
    output: u64,
    memory_commands: u32,
    shell_commands: u32,
}

/// Token use and shell commands from a Claude transcript (assistant records,
/// one per message id).
fn claude_usage(log: &Path) -> Usage {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let mut by_id: BTreeMap<String, Value> = BTreeMap::new();
    let mut usage = Usage::default();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v["type"] != "assistant" {
            continue;
        }
        let msg = &v["message"];
        if let Some(id) = msg["id"].as_str() {
            by_id.insert(id.to_owned(), msg["usage"].clone());
        }
        if let Some(parts) = msg["content"].as_array() {
            for part in parts {
                if part["type"] == "tool_use" && part["name"] == "Bash" {
                    usage.shell_commands += 1;
                    if part["input"]["command"]
                        .as_str()
                        .is_some_and(|c| c.contains("connecto memory"))
                    {
                        usage.memory_commands += 1;
                    }
                }
            }
        }
    }
    for u in by_id.values() {
        usage.input += u["input_tokens"].as_u64().unwrap_or(0);
        usage.cache_creation += u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
        usage.cache_read += u["cache_read_input_tokens"].as_u64().unwrap_or(0);
        usage.output += u["output_tokens"].as_u64().unwrap_or(0);
    }
    usage
}

#[derive(Debug, Default, Clone)]
struct TaskResult {
    side: &'static str,
    task: String,
    ok: bool,
    detail: String,
    turns: usize,
    chars_in: usize,
    chars_out: usize,
    secs: f64,
    usage: Usage,
    trap_failures: usize,
    log: Option<PathBuf>,
    terminal: String,
}

fn trap_lines(root: &Path) -> Vec<Value> {
    std::fs::read_to_string(root.join("build-audit.log"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

impl Bench {
    /// One task in a fresh terminal: type the prompt, wait for the turn;
    /// when the reply is a question, answer once with a fixed line.
    fn run_task(
        &self,
        side: &'static str,
        project: &str,
        dir: &Path,
        task: &Task,
        keep_open: bool,
    ) -> TaskResult {
        let name = format!("{}-{}-{}", side, task.name, now_ms() % 100_000);
        let id = self.add(project, ToolKind::Claude, &name);
        self.wait_ready(project, &id, false);
        let before = files(dir);
        let trap_before = trap_lines(&self.root).len();
        let started = Instant::now();
        let mut turns = 0;
        let mut chars_in = 0;
        let prompts = [
            task.prompt,
            "Proceed with your best judgement; do not ask further questions.",
        ];
        for (n, prompt) in prompts.iter().enumerate() {
            let ends = self.turn_ends(project, &id);
            self.type_line(project, &id, prompt);
            chars_in += prompt.chars().count();
            let finished = self.wait_turn(project, &id, ends, Duration::from_secs(900));
            turns += 1;
            if !finished {
                break;
            }
            let reply = last_reply(&self.recorder.of(project, &id));
            let asks = reply.trim_end().ends_with('?');
            if n == 0 && !asks {
                break;
            }
            if n == 0 && asks {
                continue;
            }
        }
        let secs = started.elapsed().as_secs_f64();
        let events = self.recorder.of(project, &id);
        let reply = last_reply(&events);
        let chars_out: usize = events
            .iter()
            .filter_map(|s| match &s.event.what {
                CaptureWhat::TurnEnd {
                    reply: CaptureReply::Text(t),
                } => Some(t.chars().count()),
                _ => None,
            })
            .sum();
        let log = events.iter().rev().find_map(|s| s.event.log.clone());
        let usage = log.as_deref().map(claude_usage).unwrap_or_default();
        let (ok, detail) = check(task, dir, &before, &reply);
        let trap_failures = trap_lines(&self.root)[trap_before..]
            .iter()
            .filter(|l| l["code"] != 0 && l["cwd"].as_str().is_some_and(|c| Path::new(c) == dir))
            .count();
        if !keep_open {
            self.close(project, &id);
        }
        let result = TaskResult {
            side,
            task: task.name.to_owned(),
            ok,
            detail,
            turns,
            chars_in,
            chars_out,
            secs,
            usage,
            trap_failures,
            log,
            terminal: id,
        };
        self.note(&format!(
            "task {side} {} ({}): ok={} turns={} chars_in={} chars_out={} secs={:.1} tokens in={} cache_create={} cache_read={} out={} shell_cmds={} memory_cmds={} trap_failures={} ({}) reply: {:?}",
            task.name,
            task.kind,
            result.ok,
            result.turns,
            result.chars_in,
            result.chars_out,
            result.secs,
            result.usage.input,
            result.usage.cache_creation,
            result.usage.cache_read,
            result.usage.output,
            result.usage.shell_commands,
            result.usage.memory_commands,
            result.trap_failures,
            result.detail,
            reply.chars().take(300).collect::<String>()
        ));
        result
    }
}

fn last_reply(events: &[Seen]) -> String {
    events
        .iter()
        .rev()
        .find_map(|s| match &s.event.what {
            CaptureWhat::TurnEnd {
                reply: CaptureReply::Text(t),
            } => Some(t.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn run_line(r: &RunEntry) -> String {
    format!(
        "run {} {} {}/{}/{} started={} secs={:.1} chars_in={} chars_out={} outcome={:?}",
        r.id,
        r.cycle.word(),
        r.choice.provider,
        r.choice.model,
        r.choice.effort,
        r.started_ms,
        r.duration_ms.unwrap_or(0) as f64 / 1000.0,
        r.chars_in,
        r.chars_out,
        r.outcome
    )
}

fn haiku() -> ModelChoice {
    ModelChoice::new(Provider::Claude, "haiku", "low")
}

/// Counts of memories by kind, standing and status, and every claim
/// stronger than the default whose sources cannot support it in an initial
/// build (`verificato`/`accertato` need a user statement or a code
/// observation, §5.9).
fn census(root: &Path, export: &Value) -> Vec<String> {
    let mut kinds: BTreeMap<String, u32> = BTreeMap::new();
    let mut problems = Vec::new();
    let memories = export["memories"].as_array().cloned().unwrap_or_default();
    for m in memories.iter().filter(|m| m["live"] != false) {
        let key = format!(
            "{} / {} / {} / {}",
            m["room"].as_str().unwrap_or("?"),
            m["kind"].as_str().unwrap_or("?"),
            m["standing"].as_str().unwrap_or("?"),
            m["status"].as_str().unwrap_or("?")
        );
        *kinds.entry(key).or_default() += 1;
        let source_kinds: BTreeSet<String> = m["sources"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|s| s["kind"].as_str().map(str::to_owned))
            .collect();
        let strong = m["status"] == "verificato" || m["standing"] == "accertato";
        let backed = source_kinds.contains("dichiarazione_utente")
            || source_kinds.contains("osservazione_codice");
        if strong && !backed {
            problems.push(format!(
                "{} {} is {}/{} with sources {:?}",
                m["id"], m["summary"], m["status"], m["standing"], source_kinds
            ));
        }
    }
    report(
        root,
        &format!(
            "memories (room / kind / standing / status: count): {}",
            kinds
                .iter()
                .map(|(k, v)| format!("{k}: {v}"))
                .collect::<Vec<_>>()
                .join("; ")
        ),
    );
    let annotations = export["annotations"].as_array().map_or(0, Vec::len);
    report(root, &format!("annotations in the export: {annotations}"));
    problems
}

fn projects_dir(root: &Path) -> PathBuf {
    root.join("projects")
}

fn repo_a(root: &Path) -> PathBuf {
    projects_dir(root).join("inventory-a")
}

fn repo_b(root: &Path) -> PathBuf {
    projects_dir(root).join("inventory-b")
}

fn repo_c(root: &Path) -> PathBuf {
    projects_dir(root).join("inventory-c")
}

fn weekday_today() -> Weekday {
    use chrono::Datelike;
    match chrono::Local::now().weekday() {
        chrono::Weekday::Mon => Weekday::Mon,
        chrono::Weekday::Tue => Weekday::Tue,
        chrono::Weekday::Wed => Weekday::Wed,
        chrono::Weekday::Thu => Weekday::Thu,
        chrono::Weekday::Fri => Weekday::Fri,
        chrono::Weekday::Sat => Weekday::Sat,
        chrono::Weekday::Sun => Weekday::Sun,
    }
}

// ---------------------------------------------------------------------------
// Phases. Run each by itself, in order, with the same BENCH_ROOT.

/// Phase 1: the repositories, the CLIs' trust in their folders (answered by
/// the harness in a first terminal, as a user does once), memory on for A,
/// the initial build of A. Model sessions: the build's runs only.
#[test]
#[ignore = "real CLIs and logins; run by hand (docs/specs/project-memory.work/benchmark.md)"]
fn bench_1_setup_and_bootstrap() {
    let root = bench_root();
    let audit = root.join("build-audit.log");
    for dir in [repo_a(&root), repo_b(&root), repo_c(&root)] {
        if !dir.join(".git").exists() {
            make_repo(&dir, &audit);
        }
    }
    let bench = Bench::open(TerminalCredit::Off, Some("haiku"));
    bench.note(&format!(
        "## phase 1 at {} (unix ms {})",
        chrono::Local::now().to_rfc3339(),
        now_ms()
    ));
    let a = bench.open_project(&repo_a(&root));
    let b = bench.open_project(&repo_b(&root));
    for (pid, label) in [(&a, "A"), (&b, "B")] {
        let id = bench.add(pid, ToolKind::Claude, "trust");
        let t0 = Instant::now();
        let answered = bench.wait_ready(pid, &id, true);
        bench.note(&format!(
            "trust {label}: question shown and answered from the window: {answered}; ready after {:.1}s",
            t0.elapsed().as_secs_f64()
        ));
        bench.close(pid, &id);
    }
    bench.set_settings(&a, |s| {
        s.enabled = true;
        s.development.choice = haiku();
        s.dream.choice = haiku();
        s.dream.days = BTreeSet::new();
    });
    let options = BootstrapOptions::default();
    wait("the plan to be accepted", Duration::from_secs(60), || {
        bench.engine().plan_bootstrap(&a, options.clone()).is_ok()
    });
    wait("the build plan", Duration::from_secs(120), || {
        bench.project_view(&a).bootstrap.plan.is_some()
    });
    let plan = bench.project_view(&a).bootstrap.plan.unwrap();
    bench.note(&format!(
        "bootstrap plan: {} runs, {} chars, coverage {:?}, gaps {:?}",
        plan.runs, plan.chars, plan.coverage, plan.gaps
    ));
    bench.note(&format!(
        "PLANNED model sessions for phase 1: {} (initial build of A, haiku/low)",
        plan.runs
    ));
    let t0 = Instant::now();
    bench.engine().bootstrap(&a, options).unwrap();
    wait("the initial build", Duration::from_secs(3600), || {
        matches!(
            bench.project_view(&a).bootstrap.state,
            BootstrapState::Completed | BootstrapState::Failed { .. }
        )
    });
    let view = bench.project_view(&a);
    bench.note(&format!(
        "bootstrap: state {:?} after {:.1}s; packets {}/{}; coverage {:?}; gaps {:?}; version {}",
        view.bootstrap.state,
        t0.elapsed().as_secs_f64(),
        view.bootstrap.packets_done,
        view.bootstrap.packets_total,
        view.bootstrap.coverage,
        view.bootstrap.gaps,
        view.version
    ));
    for r in view.runs.iter().filter(|r| r.cycle == Cycle::Bootstrap) {
        bench.note(&run_line(r));
    }
    let export = bench.export(&a);
    let problems = census(&root, &export);
    bench.note(&format!(
        "unsupported strong claims after the build: {problems:?}"
    ));
    for m in export["memories"].as_array().cloned().unwrap_or_default() {
        bench.note(&format!(
            "  {} {} [{} {} {}] {}",
            m["id"].as_str().unwrap_or(""),
            m["address"].as_str().unwrap_or(""),
            m["kind"].as_str().unwrap_or(""),
            m["standing"].as_str().unwrap_or(""),
            m["status"].as_str().unwrap_or(""),
            m["summary"].as_str().unwrap_or("")
        ));
    }
    bench.engine().shutdown();
    assert!(problems.is_empty(), "{problems:?}");
}

/// Phase 2: the tasks with memory (A) and without (B), interleaved; the
/// development runs they cause; a second terminal reading a note; one
/// dream; the version notice at the next prompt; retrieval; the trap again.
#[test]
#[ignore = "real CLIs and logins; run by hand (docs/specs/project-memory.work/benchmark.md)"]
fn bench_2_tasks() {
    let root = bench_root();
    let bench = Bench::open(TerminalCredit::Off, Some("haiku"));
    bench.note(&format!(
        "## phase 2 at {} (unix ms {})",
        chrono::Local::now().to_rfc3339(),
        now_ms()
    ));
    bench.note(&format!(
        "PLANNED model sessions for phase 2: {} task sessions (5 tasks x 2 sides) + 4 trap repeats + 1 notice turn in an open session + development runs (expected 2-6) + 1 dream (1-3 sessions)",
        TASKS.len() * 2
    ));
    let a = bench.open_project(&repo_a(&root));
    let b = bench.open_project(&repo_b(&root));
    let (dir_a, dir_b) = (repo_a(&root), repo_b(&root));
    let first_run = bench.runs(&a).first().map_or(0, |r| r.id);
    let tasks_started = now_ms();
    let mut results = Vec::new();
    let mut kept = None;
    for task in TASKS {
        if task.name == "T5" {
            // The decision of T1 must have reached the memory as a note.
            let t0 = Instant::now();
            let idle = bench.wait_idle(&a, Cycle::Development, Duration::from_secs(600));
            bench.note(&format!(
                "waited {:.1}s for A's development queue before T5 (drained: {idle})",
                t0.elapsed().as_secs_f64()
            ));
        }
        let with = bench.run_task("with", &a, &dir_a, task, task.name == "T5");
        if task.name == "T5" {
            kept = Some(with.terminal.clone());
        }
        results.push(with);
        results.push(bench.run_task("without", &b, &dir_b, task, false));
    }
    let tasks_ended = now_ms();
    let t0 = Instant::now();
    let idle = bench.wait_idle(&a, Cycle::Development, Duration::from_secs(900));
    bench.note(&format!(
        "after the tasks, A's development queue drained in {:.1}s ({idle})",
        t0.elapsed().as_secs_f64()
    ));
    let dev_runs: Vec<RunEntry> = bench
        .runs(&a)
        .into_iter()
        .filter(|r| r.id > first_run && r.cycle == Cycle::Development)
        .collect();
    for r in &dev_runs {
        bench.note(&run_line(r));
    }

    // 6a: a note written by the development cycle, read from a second
    // terminal through the real bridge.
    let export = bench.export(&a);
    let note = export["annotations"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .rev()
        .find(|n| n["origin"] == "ciclo_sviluppo");
    let shell = bench.add(&a, ToolKind::Shell, "reader");
    std::thread::sleep(Duration::from_secs(3));
    if let Some(note) = &note {
        let id = note["id"].as_str().unwrap_or("");
        let t0 = Instant::now();
        let (out, code) = bench.shell(&a, &shell, &format!("connecto memory read {id}"));
        bench.note(&format!(
            "6a: second terminal `connecto memory read {id}`: exit {code}, {:.1}s, shows the note: {}; created_ms {} vs tasks started {}",
            t0.elapsed().as_secs_f64(),
            out.contains(id),
            note["created_ms"],
            tasks_started
        ));
        bench.note(&format!(
            "6a output (first 600 chars): {:?}",
            out.chars().take(600).collect::<String>()
        ));
    } else {
        bench.note("6a: no note from the development cycle was found to read");
    }
    let (status, _) = bench.shell(&a, &shell, "connecto memory status");
    bench.note(&format!("status before the dream: {status:?}"));

    // One dream at the end.
    let version_before = bench.project_view(&a).version;
    let t0 = Instant::now();
    bench.engine().run_dream_now(&a).unwrap();
    wait("the dream to start", Duration::from_secs(60), || {
        bench
            .project_view(&a)
            .cycle(Cycle::Dream)
            .is_some_and(|c| c.running)
    });
    let done = bench.wait_idle(&a, Cycle::Dream, Duration::from_secs(1800));
    let view = bench.project_view(&a);
    bench.note(&format!(
        "dream: finished {done} after {:.1}s; version {} -> {}; report {:?}",
        t0.elapsed().as_secs_f64(),
        version_before,
        view.version,
        view.last_report
    ));
    let dream_runs: Vec<RunEntry> = bench
        .runs(&a)
        .into_iter()
        .filter(|r| r.id > first_run && r.cycle == Cycle::Dream)
        .collect();
    for r in &dream_runs {
        bench.note(&run_line(r));
    }

    // 6a: the version notice at the next prompt of a terminal that was open.
    if let Some(term) = &kept {
        let ends = bench.turn_ends(&a, term);
        let t0 = Instant::now();
        bench.type_line(&a, term, "Reply with only the word DONE, nothing else.");
        bench.wait_turn(&a, term, ends, Duration::from_secs(300));
        let log = bench
            .recorder
            .of(&a, term)
            .iter()
            .rev()
            .find_map(|s| s.event.log.clone());
        let raw = log
            .as_deref()
            .and_then(|l| std::fs::read_to_string(l).ok())
            .unwrap_or_default();
        let notice = format!("project memory updated to version {}", view.version);
        bench.note(&format!(
            "6a: version notice in the next prompt's context: {} ({:.1}s turn); transcript {:?}",
            raw.contains(&notice),
            t0.elapsed().as_secs_f64(),
            log
        ));
        bench.close(&a, term);
    }

    // Totals.
    let sum = |side: &str| {
        let rows: Vec<&TaskResult> = results.iter().filter(|r| r.side == side).collect();
        (
            rows.iter().filter(|r| r.ok).count(),
            rows.iter().map(|r| r.turns).sum::<usize>(),
            rows.iter().map(|r| r.chars_in).sum::<usize>(),
            rows.iter().map(|r| r.chars_out).sum::<usize>(),
            rows.iter().map(|r| r.secs).sum::<f64>(),
            rows.iter()
                .map(|r| r.usage.input + r.usage.cache_creation + r.usage.cache_read)
                .sum::<u64>(),
            rows.iter().map(|r| r.usage.output).sum::<u64>(),
        )
    };
    for side in ["with", "without"] {
        let (ok, turns, cin, cout, secs, tin, tout) = sum(side);
        bench.note(&format!(
            "TOTAL tasks {side}: ok {ok}/5, turns {turns}, chars in {cin}, chars out {cout}, secs {secs:.1}, tokens in (incl. cache) {tin}, tokens out {tout}"
        ));
    }
    let cycle_in: u64 = dev_runs.iter().chain(&dream_runs).map(|r| r.chars_in).sum();
    let cycle_out: u64 = dev_runs
        .iter()
        .chain(&dream_runs)
        .map(|r| r.chars_out)
        .sum();
    let cycle_secs: f64 = dev_runs
        .iter()
        .chain(&dream_runs)
        .map(|r| r.duration_ms.unwrap_or(0) as f64 / 1000.0)
        .sum();
    bench.note(&format!(
        "TOTAL cycles for the with side: {} development runs + {} dream sessions, chars in {cycle_in}, chars out {cycle_out}, secs {cycle_secs:.1}; tasks ran from {tasks_started} to {tasks_ended}",
        dev_runs.len(),
        dream_runs.len()
    ));

    // 4: retrieval at the default budgets, through the bridge.
    retrieval(&bench, &a, &shell);

    // 5: the trap again, with the trap in memory (A) and without memory (B).
    let export = bench.export(&a);
    let trap_known = export["memories"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .any(|m| {
            m["room"] == "problemi"
                && m["live"] != false
                && format!("{}{}", m["summary"], m["details"]).contains("legacy-units")
        });
    bench.note(&format!(
        "trap recorded in problemi before the repeats: {trap_known}"
    ));
    let trap = TASKS.iter().find(|t| t.name == "T4").unwrap();
    for rep in 1..=2 {
        for (side, pid, dir) in [("with", &a, &dir_a), ("without", &b, &dir_b)] {
            let bundle = dir.join("dist/bundle.json");
            if bundle.exists() {
                std::fs::rename(
                    &bundle,
                    dir.join(format!("dist/bundle.prev-{side}-{rep}-{}.json", now_ms())),
                )
                .unwrap();
            }
            let r = bench.run_task(side, pid, dir, trap, false);
            bench.note(&format!(
                "trap repeat {rep} {side}: failing build attempts {}",
                r.trap_failures
            ));
            results.push(r);
        }
    }
    let after_trap: Vec<RunEntry> = bench
        .runs(&a)
        .into_iter()
        .filter(|r| r.id > first_run && r.started_ms > tasks_ended)
        .collect();
    for r in &after_trap {
        bench.note(&format!("after the tasks: {}", run_line(r)));
    }
    // Dropped-reply path: read a real reply back from its session log.
    if let Some(r) = results.iter().find(|r| r.side == "with" && r.log.is_some()) {
        let log = r.log.clone().unwrap();
        let task = TASKS.iter().find(|t| t.name == r.task).unwrap();
        let t0 = Instant::now();
        let logged = super::logged_reply(Provider::Claude, &log, Some(task.prompt));
        bench.note(&format!(
            "6d: reply recovered from the real session log of {} {} in {:.0} ms: {}",
            r.side,
            r.task,
            t0.elapsed().as_secs_f64() * 1000.0,
            logged.as_ref().map_or_else(
                || "none".to_owned(),
                |l| format!("{} chars", l.text.chars().count())
            )
        ));
    }
    bench.close(&a, &shell);
    bench.engine().shutdown();
}

/// Question, search words, words a right answer holds (any of them).
const QUESTIONS: &[(&str, &str, &[&str])] = &[
    ("How are the tests run?", "tests", &["unittest"]),
    (
        "Why does build.py fail with unit parse failed?",
        "build",
        &["legacy-units"],
    ),
    (
        "Which separator does the CSV export use?",
        "csv",
        &["semicolon", "sku;name;grams", "';'"],
    ),
    ("How are quantities stored?", "quantities", &["integer"]),
    ("Why does total print 0 for the bolts?", "bolts", &["kg"]),
    (
        "May the tool use third-party packages?",
        "dependencies",
        &["standard library", "stdlib", "third-party"],
    ),
    ("Is there a count command?", "count", &["count"]),
    (
        "Where does the data bundle go?",
        "bundle",
        &["dist/bundle.json", "bundle.json"],
    ),
];

fn ids_in_order(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let t = line.trim_start();
        let t = t.strip_prefix("altro:").map_or(t, str::trim_start);
        let t = t.strip_prefix("! ").unwrap_or(t);
        let word: String = t.chars().take(10).collect();
        if (word.starts_with("m-") || word.starts_with("p-"))
            && word.len() == 10
            && word[2..].chars().all(|c| c.is_ascii_alphanumeric())
            && !out.contains(&word)
        {
            out.push(word);
        }
    }
    out
}

fn retrieval(bench: &Bench, project: &str, shell: &str) {
    let export = bench.export(project);
    let mut texts: HashMap<String, String> = HashMap::new();
    for m in export["memories"].as_array().cloned().unwrap_or_default() {
        texts.insert(
            m["id"].as_str().unwrap_or("").to_owned(),
            format!("{} {}", m["summary"], m["details"]).to_lowercase(),
        );
    }
    for a in export["annotations"]
        .as_array()
        .cloned()
        .unwrap_or_default()
    {
        texts.insert(
            a["id"].as_str().unwrap_or("").to_owned(),
            a["text"].as_str().unwrap_or("").to_lowercase(),
        );
    }
    let (brief, _) = bench.shell(project, shell, "connecto memory brief");
    bench.note(&format!("brief: {} chars", brief.chars().count()));
    let (mut hit1, mut hit3, mut stored, mut in_brief) = (0, 0, 0, 0);
    for (question, words, answers) in QUESTIONS {
        let holds = |text: &str| answers.iter().any(|a| text.contains(&a.to_lowercase()));
        let exists = texts.values().any(|t| holds(t));
        let (out, code) = bench.shell(project, shell, &format!("connecto memory search {words}"));
        let ranked = ids_in_order(&out);
        let pos = ranked
            .iter()
            .position(|id| texts.get(id).is_some_and(|t| holds(t)));
        let brief_has = holds(&brief.to_lowercase());
        stored += usize::from(exists);
        hit1 += usize::from(pos == Some(0));
        hit3 += usize::from(pos.is_some_and(|p| p < 3));
        in_brief += usize::from(brief_has);
        bench.note(&format!(
            "retrieval {question:?} search {words:?} (exit {code}): answer stored {exists}; rank {:?} of {} listed; in brief {brief_has}",
            pos.map(|p| p + 1),
            ranked.len()
        ));
    }
    bench.note(&format!(
        "RETRIEVAL {} questions: answer stored {stored}, search hit@1 {hit1}, hit@3 {hit3}, answer in brief {in_brief}",
        QUESTIONS.len()
    ));
}

/// Phase 3 (6b, Claude): how often a prompt typed in the window is credited
/// to the user by the quarantine rule. Project C, memory off (the verdict is
/// the ledger's and does not need the engine). The credit mode is
/// `Quarantine` for this process only; the app ships `Off`.
#[test]
#[ignore = "real CLIs and logins; run by hand (docs/specs/project-memory.work/benchmark.md)"]
fn bench_3_credit_claude() {
    let root = bench_root();
    let bench = Bench::open(TerminalCredit::Quarantine, Some("haiku"));
    bench.note(&format!(
        "## phase 3 (credit, Claude) at {}",
        chrono::Local::now().to_rfc3339()
    ));
    bench
        .note("PLANNED model sessions for phase 3: 2 Claude haiku sessions (about 10 short turns)");
    let c = bench.open_project(&repo_c(&root));
    credit_scenarios(&bench, &c, ToolKind::Claude);
    bench.engine().shutdown();
}

fn verdicts(bench: &Bench, project: &str, id: &str) -> Vec<(String, PromptSource, bool)> {
    bench
        .recorder
        .of(project, id)
        .iter()
        .filter_map(|s| match &s.event.what {
            CaptureWhat::Prompt {
                text,
                source,
                truncated,
            } => Some((text.chars().take(60).collect(), *source, *truncated)),
            _ => None,
        })
        .collect()
}

/// The scripted window sequences of 6b, in one session that opens on the
/// trust question (when the folder is new to the CLI), then in a second
/// session with no question.
fn credit_scenarios(bench: &Bench, project: &str, tool: ToolKind) {
    let label = format!("{tool:?}");
    let shell = bench.add(project, ToolKind::Shell, "sender");
    for session in ["first", "second"] {
        let id = bench.add(project, tool, &format!("credit-{session}"));
        let opened = Instant::now();
        let trust = bench.wait_ready(project, &id, true);
        bench.note(&format!(
            "{label} {session} session: trust question answered from the window: {trust}"
        ));
        let mut steps: Vec<(&str, String)> = Vec::new();
        let typed = |n: usize| format!("Reply with only the word OK{n}, nothing else.");
        for n in 1..=3 {
            steps.push(("typed", typed(n)));
        }
        if session == "first" {
            steps.push((
                "pasted",
                "Reply with only the word PASTED, nothing else.".to_owned(),
            ));
            steps.push(("typed after paste", typed(4)));
            steps.push((
                "connecto send from a shell",
                "Reply with only the word SENT, nothing else.".to_owned(),
            ));
            steps.push(("typed after send", typed(5)));
            steps.push(("typed", typed(6)));
        }
        for (what, text) in steps {
            let ends = bench.turn_ends(project, &id);
            let t0 = Instant::now();
            match what {
                "pasted" => bench.paste_line(project, &id, &text),
                "connecto send from a shell" => {
                    let (out, code) =
                        bench.shell(project, &shell, &format!("connecto send {id} '{text}'"));
                    bench.note(&format!("send: exit {code} {:?}", out.trim()));
                }
                _ => bench.type_line(project, &id, &text),
            }
            let ok = bench.wait_turn(project, &id, ends, Duration::from_secs(300));
            let v = verdicts(bench, project, &id);
            let last = v.last().cloned();
            let q = bench.taps.queries(project, &id);
            let since: Vec<&str> = q
                .iter()
                .filter(|(at, _)| *at >= t0)
                .map(|(_, n)| *n)
                .collect();
            bench.note(&format!(
                "{label} {session} {what}: turn ended {ok}; verdict {:?}; answered queries during this step {:?}",
                last.as_ref().map(|(_, s, t)| (*s, *t)),
                since
            ));
        }
        let v = verdicts(bench, project, &id);
        let credited = v
            .iter()
            .filter(|(_, s, _)| *s == PromptSource::User)
            .count();
        let q = bench.taps.queries(project, &id);
        let mut by_name: BTreeMap<&str, usize> = BTreeMap::new();
        for (_, n) in &q {
            *by_name.entry(n).or_default() += 1;
        }
        bench.note(&format!(
            "CREDIT {label} {session} session: {credited}/{} prompts credited to the user; answered queries over {:.0}s: {by_name:?}; verdicts {:?}",
            v.len(),
            opened.elapsed().as_secs_f64(),
            v.iter().map(|(t, s, _)| format!("{s:?}:{t}")).collect::<Vec<_>>()
        ));
        bench.close(project, &id);
    }
    bench.close(project, &shell);
}

/// Phase 4 (6b and 6c, Codex): the credit sequences in a Codex terminal of
/// A, with A's development cycle and dream on Codex (its default model, low
/// effort): one development run from those events, one dream.
#[test]
#[ignore = "real CLIs and logins; run by hand (docs/specs/project-memory.work/benchmark.md)"]
fn bench_4_codex() {
    let root = bench_root();
    let bench = Bench::open(TerminalCredit::Quarantine, None);
    bench.note(&format!(
        "## phase 4 (Codex) at {}",
        chrono::Local::now().to_rfc3339()
    ));
    bench.note("PLANNED model sessions for phase 4: 2 Codex terminal sessions (about 10 short turns), development runs on Codex (expected 1-2), 1 Codex dream (1-3 sessions)");
    let a = bench.open_project(&repo_a(&root));
    let codex = ModelChoice::new(Provider::Codex, "default", "low");
    bench.set_settings(&a, |s| {
        s.development.choice = codex.clone();
        s.dream.choice = codex.clone();
    });
    let first_run = bench.runs(&a).first().map_or(0, |r| r.id);
    credit_scenarios(&bench, &a, ToolKind::Codex);
    let t0 = Instant::now();
    let idle = bench.wait_idle(&a, Cycle::Development, Duration::from_secs(900));
    bench.note(&format!(
        "Codex development queue drained {idle} after {:.1}s",
        t0.elapsed().as_secs_f64()
    ));
    let t0 = Instant::now();
    bench.engine().run_dream_now(&a).unwrap();
    wait("the dream to start", Duration::from_secs(60), || {
        bench
            .project_view(&a)
            .cycle(Cycle::Dream)
            .is_some_and(|c| c.running)
    });
    let done = bench.wait_idle(&a, Cycle::Dream, Duration::from_secs(1800));
    bench.note(&format!(
        "Codex dream finished {done} after {:.1}s; report {:?}",
        t0.elapsed().as_secs_f64(),
        bench.project_view(&a).last_report
    ));
    for r in bench.runs(&a).iter().filter(|r| r.id > first_run) {
        bench.note(&run_line(r));
    }
    bench.set_settings(&a, |s| {
        s.development.choice = haiku();
        s.dream.choice = haiku();
    });
    bench.engine().shutdown();
}

/// Phase 5 (6c, 6d): the real 30 s scheduler tick firing a due dream once;
/// a prompt over 64 KiB typed into a real session of A.
#[test]
#[ignore = "real CLIs and logins; run by hand (docs/specs/project-memory.work/benchmark.md)"]
fn bench_5_scheduler_and_long_prompt() {
    let root = bench_root();
    let bench = Bench::open(TerminalCredit::Off, Some("haiku"));
    bench.note(&format!(
        "## phase 5 at {}",
        chrono::Local::now().to_rfc3339()
    ));
    bench.note("PLANNED model sessions for phase 5: 1 Claude haiku session (one long-prompt turn), 1 scheduled dream (1-3 sessions), development runs from that turn (expected 1)");
    let a = bench.open_project(&repo_a(&root));
    let first_run = bench.runs(&a).first().map_or(0, |r| r.id);

    // 6d: a prompt over 64 KiB.
    let id = bench.add(&a, ToolKind::Claude, "long");
    bench.wait_ready(&a, &id, false);
    let filler: Vec<String> = (0..2600)
        .map(|n| format!("{n:05} filler line, ignore it"))
        .collect();
    let prompt = format!(
        "Ignore the filler below and reply with only the word ORCHID.\n{}\nEND",
        filler.join("\n")
    );
    let ends = bench.turn_ends(&a, &id);
    let t0 = Instant::now();
    bench.paste_line(&a, &id, &prompt);
    let ok = bench.wait_turn(&a, &id, ends, Duration::from_secs(600));
    let v = verdicts(&bench, &a, &id);
    bench.note(&format!(
        "6d: long prompt {} chars: turn ended {ok} in {:.1}s; capture: {:?}; reply {:?}",
        prompt.len(),
        t0.elapsed().as_secs_f64(),
        v.last().map(|(_, s, t)| (*s, *t)),
        last_reply(&bench.recorder.of(&a, &id))
    ));
    bench.close(&a, &id);
    bench.wait_idle(&a, Cycle::Development, Duration::from_secs(600));
    let export = bench.export(&a);
    let leaked = export["evidence"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .any(|e| {
            e["excerpt"]
                .as_str()
                .is_some_and(|x| x.contains("filler line"))
        });
    bench.note(&format!(
        "6d: any stored evidence holds the long prompt's text: {leaked}"
    ));

    // 6c: the scheduler. A new note first, so the dream's minimum is met.
    let shell = bench.add(&a, ToolKind::Shell, "scheduler");
    std::thread::sleep(Duration::from_secs(3));
    let (out, code) = bench.shell(
        &a,
        &shell,
        "printf '%s' 'Benchmark probe: the scheduler test added this note.' | connecto memory propose --room lavori-aperti",
    );
    bench.note(&format!("propose: exit {code} {:?}", out.trim()));
    let now = chrono::Local::now();
    let due = now + chrono::Duration::minutes(2);
    use chrono::Timelike;
    let time = connecto_core::memory::LocalTime::new(
        u8::try_from(due.hour()).unwrap(),
        u8::try_from(due.minute()).unwrap(),
    )
    .unwrap();
    let set_at = Instant::now();
    bench.set_settings(&a, |s| {
        s.dream.days = BTreeSet::from([weekday_today()]);
        s.dream.time = time;
        s.dream.choice = haiku();
    });
    let due_ms = {
        let d = due.with_second(0).unwrap().with_nanosecond(0).unwrap();
        u64::try_from(d.timestamp_millis()).unwrap()
    };
    bench.note(&format!(
        "scheduler: dream set for {:02}:{:02} today (due at unix ms {due_ms}); next_dream_ms in the view: {:?}",
        due.hour(),
        due.minute(),
        bench.project_view(&a).next_dream_ms
    ));
    let before = bench.runs(&a).first().map_or(0, |r| r.id);
    let started = (|| {
        let deadline = Instant::now() + Duration::from_secs(420);
        while Instant::now() < deadline {
            if let Some(r) = bench
                .runs(&a)
                .into_iter()
                .find(|r| r.id > before && r.cycle == Cycle::Dream)
            {
                return Some(r);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        None
    })();
    match &started {
        Some(r) => bench.note(&format!(
            "6c: the scheduled dream started at unix ms {} = {:.1}s after it was due ({:.1}s after the setting was saved)",
            r.started_ms,
            (r.started_ms as f64 - due_ms as f64) / 1000.0,
            set_at.elapsed().as_secs_f64()
        )),
        None => bench.note("6c: no scheduled dream started within 7 minutes"),
    }
    let done = bench.wait_idle(&a, Cycle::Dream, Duration::from_secs(1800));
    std::thread::sleep(Duration::from_secs(65));
    let dreams_today = bench
        .runs(&a)
        .into_iter()
        .filter(|r| r.id > before && r.cycle == Cycle::Dream)
        .count();
    bench.note(&format!("6c: dream finished {done}; dream runs started by the schedule after one more minute: {dreams_today}"));
    bench.set_settings(&a, |s| s.dream.days = BTreeSet::new());
    for r in bench.runs(&a).iter().filter(|r| r.id > first_run) {
        bench.note(&run_line(r));
    }
    bench.close(&a, &shell);
    bench.engine().shutdown();
}

/// Reason of a rejection or adjustment without the operation number, so the
/// same cause groups across operations ("session 1, operation 7: into: …").
fn reason_key(text: &str) -> String {
    text.split_once("operation ")
        .and_then(|(_, rest)| rest.split_once(": "))
        .map_or_else(|| text.to_owned(), |(_, reason)| reason.to_owned())
}

fn grouped(items: &[String]) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for item in items {
        *out.entry(reason_key(item)).or_default() += 1;
    }
    out
}

impl Bench {
    /// Revert `version`; true when a new version appeared, false when the
    /// engine refused (the reason is noted).
    fn revert(&self, project: &str, version: u64) -> bool {
        let before = self.project_view(project);
        if let Err(e) = self.engine().revert(project, version) {
            self.note(&format!("revert {version}: refused at once: {e}"));
            return false;
        }
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            let view = self.project_view(project);
            if view.version > before.version {
                self.note(&format!("revert {version}: now version {}", view.version));
                return true;
            }
            if view.last_error.is_some() && view.last_error != before.last_error {
                self.note(&format!("revert {version}: refused: {:?}", view.last_error));
                return false;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        self.note(&format!("revert {version}: no answer in 60 s"));
        false
    }

    /// One "Dream now" with `choice`; returns the version it published.
    fn dream_with(&self, project: &str, choice: &ModelChoice) -> Option<u64> {
        self.set_settings(project, |s| s.dream.choice = choice.clone());
        wait("the runs to load", Duration::from_secs(60), || {
            !self.runs(project).is_empty()
        });
        let before = self.runs(project).first().map_or(0, |r| r.id);
        let open = self.project_view(project).open_annotations;
        let t0 = Instant::now();
        self.engine().run_dream_now(project).unwrap();
        wait("the dream to start", Duration::from_secs(60), || {
            self.project_view(project)
                .cycle(Cycle::Dream)
                .is_some_and(|c| c.running)
        });
        let done = self.wait_idle(project, Cycle::Dream, Duration::from_secs(2400));
        let view = self.project_view(project);
        let runs: Vec<RunEntry> = self
            .runs(project)
            .into_iter()
            .filter(|r| r.id > before && r.cycle == Cycle::Dream)
            .collect();
        let report = view.last_report.clone();
        self.note(&format!(
            "DREAM {}/{}/{}: finished {done} in {:.1}s wall; open notes before {open}; version now {}",
            choice.provider,
            choice.model,
            choice.effort,
            t0.elapsed().as_secs_f64(),
            view.version
        ));
        for r in &runs {
            self.note(&format!("  {}", run_line(r)));
        }
        if let Some(report) = &report {
            self.note(&format!(
                "  report: version {:?}; rejected {}; adjusted {}; lowered {}",
                report.version,
                report.rejected.len(),
                report.adjusted.len(),
                report.lowered.len()
            ));
            self.note(&format!(
                "  rejected by reason: {:?}",
                grouped(&report.rejected)
            ));
            self.note(&format!(
                "  adjusted by reason: {:?}",
                grouped(&report.adjusted)
            ));
            for r in &report.rejected {
                self.note(&format!("    rejected: {r}"));
            }
            for a in &report.adjusted {
                self.note(&format!("    adjusted: {a}"));
            }
        }
        report.and_then(|r| r.version)
    }
}

/// Phase 6, after the prompt fix: three dreams from the same state of A
/// (Claude haiku, Codex, Claude sonnet/medium), each undone before the next
/// so they start alike; then an initial build of B, which has real past
/// conversations. Model sessions: three dreams (one or more each) and the
/// build's runs (refused here above 4).
#[test]
#[ignore = "real CLIs and logins; run by hand (docs/specs/project-memory.work/benchmark.md)"]
fn bench_6_after_the_prompt_fix() {
    let root = bench_root();
    let bench = Bench::open(TerminalCredit::Off, Some("haiku"));
    bench.note(&format!(
        "## phase 6 (after the prompt fix) at {}",
        chrono::Local::now().to_rfc3339()
    ));
    bench.note("PLANNED model sessions for phase 6: 3 dreams (about 1 session each) + the initial build of B (at most 4 runs)");
    let a = bench.open_project(&repo_a(&root));
    // Back to the state the first dream saw: undo the dream versions newest
    // first, as far as the engine allows (no model call).
    let versions: Vec<u64> = bench
        .project_view(&a)
        .versions
        .iter()
        .filter(|v| v.cycle == Cycle::Dream && v.reverts.is_none())
        .map(|v| v.version)
        .collect();
    bench.note(&format!(
        "dream versions to undo, newest first: {versions:?}"
    ));
    for v in versions {
        if !bench.revert(&a, v) {
            break;
        }
    }
    let start = bench.project_view(&a);
    bench.note(&format!(
        "start state: version {}, memories {:?}, open notes {}",
        start.version, start.totals, start.open_annotations
    ));
    let choices = [
        haiku(),
        ModelChoice::new(Provider::Codex, "default", "low"),
        ModelChoice::new(Provider::Claude, "sonnet", "medium"),
    ];
    for (n, choice) in choices.iter().enumerate() {
        let published = bench.dream_with(&a, choice);
        if n + 1 < choices.len() {
            if let Some(v) = published {
                bench.revert(&a, v);
            }
            let now = bench.project_view(&a);
            bench.note(&format!(
                "after undo: version {}, memories {:?}, open notes {}",
                now.version, now.totals, now.open_annotations
            ));
        }
    }
    bench.set_settings(&a, |s| s.dream.choice = haiku());

    // The initial build of B: its closed terminals are real past
    // conversations.
    let b = bench.open_project(&repo_b(&root));
    bench.set_settings(&b, |s| {
        s.enabled = true;
        s.development.choice = haiku();
        s.dream.choice = haiku();
        s.dream.days = BTreeSet::new();
    });
    let options = BootstrapOptions::default();
    wait("the plan to be accepted", Duration::from_secs(60), || {
        bench.engine().plan_bootstrap(&b, options.clone()).is_ok()
    });
    wait("the build plan", Duration::from_secs(120), || {
        bench.project_view(&b).bootstrap.plan.is_some()
    });
    let plan = bench.project_view(&b).bootstrap.plan.unwrap();
    bench.note(&format!(
        "B bootstrap plan: {} runs, {} chars, coverage {:?}, gaps {:?}",
        plan.runs, plan.chars, plan.coverage, plan.gaps
    ));
    assert!(plan.runs <= 4, "over the agreed budget: {} runs", plan.runs);
    let t0 = Instant::now();
    bench.engine().bootstrap(&b, options).unwrap();
    wait("the initial build", Duration::from_secs(3600), || {
        matches!(
            bench.project_view(&b).bootstrap.state,
            BootstrapState::Completed | BootstrapState::Failed { .. }
        )
    });
    let view = bench.project_view(&b);
    bench.note(&format!(
        "B bootstrap: state {:?} after {:.1}s; packets {}/{}; gaps {:?}; version {}; report {:?}",
        view.bootstrap.state,
        t0.elapsed().as_secs_f64(),
        view.bootstrap.packets_done,
        view.bootstrap.packets_total,
        view.bootstrap.gaps,
        view.version,
        view.last_report
    ));
    for r in view.runs.iter().filter(|r| r.cycle == Cycle::Bootstrap) {
        bench.note(&run_line(r));
    }
    let export = bench.export(&b);
    let problems = census(&root, &export);
    let mut evidence: BTreeMap<String, usize> = BTreeMap::new();
    for e in export["evidence"].as_array().cloned().unwrap_or_default() {
        *evidence
            .entry(e["kind"].as_str().unwrap_or("?").to_owned())
            .or_default() += 1;
    }
    bench.note(&format!("B evidence by kind: {evidence:?}"));
    for m in export["memories"].as_array().cloned().unwrap_or_default() {
        let kinds: BTreeSet<String> = m["sources"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|s| s["kind"].as_str().map(str::to_owned))
            .collect();
        bench.note(&format!(
            "  {} {} [{} {} {}] sources {:?} {}",
            m["id"].as_str().unwrap_or(""),
            m["address"].as_str().unwrap_or(""),
            m["kind"].as_str().unwrap_or(""),
            m["standing"].as_str().unwrap_or(""),
            m["status"].as_str().unwrap_or(""),
            kinds,
            m["summary"].as_str().unwrap_or("")
        ));
    }
    bench.note(&format!("B unsupported strong claims: {problems:?}"));
    bench.engine().shutdown();
    assert!(problems.is_empty(), "{problems:?}");
}

/// Phase 7, after the prompt fix: undo every live dream version of A (no
/// model call) back to the state the first dream saw, then the same three
/// dreams from it, each undone before the next. No dream runs unless the
/// undo reaches the initial build's version.
#[test]
#[ignore = "real CLIs and logins; run by hand (docs/specs/project-memory.work/benchmark.md)"]
fn bench_7_dreams_from_the_first_dream_state() {
    let root = bench_root();
    let bench = Bench::open(TerminalCredit::Off, Some("haiku"));
    bench.note(&format!(
        "## phase 7 (after the prompt fix, from the first dream's state) at {}",
        chrono::Local::now().to_rfc3339()
    ));
    bench.note("PLANNED model sessions for phase 7: 3 dreams (about 1 session each), only if the undo reaches version 1's state");
    let a = bench.open_project(&repo_a(&root));
    wait("A's memory view to load", Duration::from_secs(60), || {
        bench.project_view(&a).version > 0 && !bench.project_view(&a).versions.is_empty()
    });
    let view = bench.project_view(&a);
    let undone: BTreeSet<u64> = view.versions.iter().filter_map(|v| v.reverts).collect();
    let live: Vec<u64> = view
        .versions
        .iter()
        .filter(|v| v.cycle == Cycle::Dream && v.reverts.is_none() && !undone.contains(&v.version))
        .map(|v| v.version)
        .collect();
    bench.note(&format!(
        "live dream versions to undo, newest first: {live:?}"
    ));
    let mut reached = true;
    for v in live {
        if !bench.revert(&a, v) {
            reached = false;
            break;
        }
    }
    let start = bench.project_view(&a);
    bench.note(&format!(
        "start state: version {}, memories {:?}, open notes {}, reached the initial build's state: {reached}",
        start.version, start.totals, start.open_annotations
    ));
    assert!(reached, "the undo stopped early: no dream was run");
    let choices = [
        haiku(),
        ModelChoice::new(Provider::Codex, "default", "low"),
        ModelChoice::new(Provider::Claude, "sonnet", "medium"),
    ];
    for (n, choice) in choices.iter().enumerate() {
        let published = bench.dream_with(&a, choice);
        if n + 1 < choices.len() {
            if let Some(v) = published {
                bench.revert(&a, v);
            }
            let now = bench.project_view(&a);
            bench.note(&format!(
                "after undo: version {}, memories {:?}, open notes {}",
                now.version, now.totals, now.open_annotations
            ));
        }
    }
    bench.set_settings(&a, |s| s.dream.choice = haiku());
    bench.engine().shutdown();
}

/// Not a measurement: the parsing helpers, so the ignored phases do not fail
/// on a helper after an hour of model calls. Runs in the ordinary gate (no
/// CLI, no network).
#[test]
fn bench_helpers_parse_what_they_will_see() {
    let out = "m-abcdefgh  officina/test\n  > run tests\n  altro: p-12345678 note\n! p-zzzzzzzz contraddizione\n";
    assert_eq!(
        ids_in_order(out),
        vec!["m-abcdefgh", "p-12345678", "p-zzzzzzzz"]
    );
    let tap = QueryTap::default();
    let mut fed = Vec::new();
    tap.feed(b"abc\x1b[", &mut |b| fed.extend_from_slice(b));
    tap.feed(b"6n\x1b]11;?\x07", &mut |b| fed.extend_from_slice(b));
    let names: Vec<&str> = tap.seen.lock().unwrap().iter().map(|(_, n)| *n).collect();
    assert_eq!(names, vec!["CPR 6n", "OSC 11 ?"]);
    assert_eq!(fed, b"abc\x1b[6n\x1b]11;?\x07");
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("s.jsonl");
    std::fs::write(
        &log,
        concat!(
            r#"{"type":"assistant","message":{"id":"a","usage":{"input_tokens":2,"cache_read_input_tokens":10,"output_tokens":5},"content":[{"type":"tool_use","name":"Bash","input":{"command":"connecto memory brief"}}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"a","usage":{"input_tokens":2,"cache_read_input_tokens":10,"output_tokens":7},"content":[]}}"#,
            "\n"
        ),
    )
    .unwrap();
    let usage = claude_usage(&log);
    assert_eq!((usage.input, usage.cache_read, usage.output), (2, 10, 7));
    assert_eq!((usage.shell_commands, usage.memory_commands), (1, 1));
    let reasons = grouped(&[
        "session 1, operation 7: into: not a memory id".to_owned(),
        "session 1, operation 8: into: not a memory id".to_owned(),
    ]);
    assert_eq!(reasons.get("into: not a memory id"), Some(&2));
}
