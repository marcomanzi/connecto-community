//! An Antigravity terminal under Connecto's own launch path, with a stand-in
//! for `agy` (`fixtures/fake_agy.py`) that runs hooks the way Antigravity's
//! documentation says it does.
//!
//! Evidence class FAKE-CLI: it proves the hooks file Connecto keeps, the
//! helper's turning of Antigravity's payloads into Connecto's events, and the
//! reply read from the transcript. It proves nothing about the real `agy`,
//! which was never run signed in (spec L13). Unix only: the stand-in is a
//! Python script run through `sh`.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{hook_binary, no_wake, scaled};
use connecto_core::{GridSize, Reply, TerminalSession, ToolEvent};
use connecto_session::LiveSession;
use connecto_session::launch::{BridgeInfo, LaunchExtras, Layout, start_resuming};
use connecto_store::{Codesign, Installed, Store, Tool};

const SIZE: GridSize = GridSize {
    cols: 100,
    rows: 30,
};

struct Scene {
    root: PathBuf,
    layout: Layout,
    project: PathBuf,
    installed: Installed,
}

impl Drop for Scene {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Scene {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("mg-agy-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for dir in ["home/project", "home/.local/bin", "data"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let root = fs::canonicalize(root).unwrap();
        let layout = Layout {
            data_root: root.join("data"),
            home: root.join("home"),
        };
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_agy.py");
        let entry = root.join("home/.local/bin/agy");
        fs::write(
            &entry,
            format!(
                "#!/bin/sh\nexec /usr/bin/python3 '{}' \"$@\"\n",
                fixture.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&entry, fs::Permissions::from_mode(0o755)).unwrap();
        let store = Store::new(&layout.data_root);
        let installed = store
            .provision(Tool::Antigravity, &entry, &Codesign)
            .unwrap();
        // Run where it is: nothing was copied.
        assert_eq!(installed.entry, entry);
        store.install_hook(&hook_binary()).unwrap();
        Self {
            project: root.join("home/project"),
            root,
            layout,
            installed,
        }
    }

    fn hooks_file(&self) -> PathBuf {
        self.layout.home.join(".gemini/config/hooks.json")
    }

    fn start(&self) -> LiveSession {
        let bridge = BridgeInfo {
            terminal: "antigravity".to_owned(),
            text: Some("LAUNCH TEXT for antigravity".to_owned()),
            ..BridgeInfo::absent()
        };
        start_resuming(
            &self.layout,
            &self.project,
            Tool::Antigravity,
            &self.installed,
            SIZE,
            no_wake(),
            &LaunchExtras::default(),
            &bridge,
            None,
        )
        .unwrap()
    }
}

fn next_event(session: &LiveSession, what: &str) -> ToolEvent {
    let hook = session.hook().unwrap();
    let deadline = Instant::now() + scaled(Duration::from_secs(20));
    while Instant::now() < deadline {
        if let Some(event) = hook.events().recv_timeout(Duration::from_millis(200)) {
            return event;
        }
    }
    panic!("no {what} event; rejected={}", hook.rejected());
}

fn type_line(session: &LiveSession, text: &str) {
    session
        .send_input(format!("{text}\r").into_bytes())
        .unwrap();
}

fn answers(project: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(project.join("answers.log"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// The first `count` answers, once they are written.
fn answers_once(project: &Path, count: usize) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + scaled(Duration::from_secs(20));
    loop {
        let found = answers(project);
        if found.len() >= count {
            return found;
        }
        assert!(Instant::now() < deadline, "only {} answers", found.len());
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn turns_are_reported_through_the_global_hooks_file_with_the_reply_from_the_transcript() {
    let scene = Scene::new("turns");
    // The user's own hook stays.
    fs::create_dir_all(scene.hooks_file().parent().unwrap()).unwrap();
    fs::write(
        scene.hooks_file(),
        r#"{"mine":{"Stop":[{"command":"true"}]}}"#,
    )
    .unwrap();
    let session = scene.start();

    let hooks: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(scene.hooks_file()).unwrap()).unwrap();
    assert_eq!(hooks["mine"]["Stop"][0]["command"], "true");
    for event in ["SessionStart", "PreInvocation", "Stop"] {
        let command = hooks["connecto"][event][0]["command"].as_str().unwrap();
        assert!(
            command.ends_with(&format!("antigravity --event {event}")),
            "{command}"
        );
    }
    // It may read the files a terminal is given by path, and only those.
    let settings: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(
            scene
                .layout
                .home
                .join(".gemini/antigravity-cli/settings.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        settings["permissions"]["allow"],
        serde_json::json!([format!(
            "read_file({})",
            scene.layout.data_root.join("chat-attachments").display()
        )])
    );

    let ToolEvent::Started {
        tool_session,
        turn_log,
    } = next_event(&session, "start")
    else {
        panic!("the first event is not the start");
    };
    assert_eq!(
        tool_session.expose(),
        "ec33ebf9-0cba-4100-8142-c61503f6c587"
    );
    assert!(
        turn_log
            .unwrap()
            .expose()
            .ends_with(".system_generated/logs/transcript.jsonl")
    );
    // The launch text reached the model as a system message.
    // Written once the handler has answered, a moment after its report.
    let started = answers_once(&scene.project, 1);
    assert_eq!(
        started[0]["answer"]["injectSteps"][0]["systemMessage"]["systemMessage"],
        "LAUNCH TEXT for antigravity"
    );

    type_line(&session, "hello there");
    assert_eq!(
        next_event(&session, "prompt"),
        ToolEvent::PromptSubmitted {
            prompt: connecto_core::EventText::new("hello there".to_owned()),
            turn: None,
        }
    );
    // The second model call of the same turn is not a second prompt.
    match next_event(&session, "end") {
        ToolEvent::TurnEnded {
            reply: Reply::Text(text),
            failure: None,
            lifecycle_known: true,
            ..
        } => assert_eq!(text.expose(), "REPLY hello there"),
        other => panic!("the turn ended as {other:?}"),
    }

    type_line(&session, "please CANCEL");
    assert!(matches!(
        next_event(&session, "prompt"),
        ToolEvent::PromptSubmitted { .. }
    ));
    match next_event(&session, "end") {
        ToolEvent::TurnEnded {
            reply: Reply::None,
            failure: Some(failure),
            ..
        } => assert_eq!(failure.expose(), "user_canceled"),
        other => panic!("the cancelled turn ended as {other:?}"),
    }
    // Every answer Antigravity read was JSON.
    assert!(
        answers(&scene.project)
            .iter()
            .all(|line| line["answer"].get("not_json").is_none())
    );
    session.terminate();
}

#[test]
fn a_hooks_file_that_is_not_an_object_is_left_alone_and_the_start_is_refused() {
    let scene = Scene::new("broken");
    fs::create_dir_all(scene.hooks_file().parent().unwrap()).unwrap();
    fs::write(scene.hooks_file(), "[1, 2").unwrap();
    let bridge = BridgeInfo::absent();
    let started = start_resuming(
        &scene.layout,
        &scene.project,
        Tool::Antigravity,
        &scene.installed,
        SIZE,
        no_wake(),
        &LaunchExtras::default(),
        &bridge,
        None,
    );
    assert!(started.is_err());
    assert_eq!(fs::read_to_string(scene.hooks_file()).unwrap(), "[1, 2");
}
