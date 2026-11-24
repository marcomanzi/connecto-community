//! The workspace with no tools installed: every pane ends at "the private copy
//! is missing", so nothing runs, and what is under test is the bookkeeping:
//! the saved file, the open projects, their independent registries, and the one
//! Pause switch over them.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use connecto_audit::Audit;
use connecto_core::{RelayControl, TermActivity, ToolKind, WorkspaceControl, WorkspaceView};
use connecto_session::launch::{LaunchExtras, Layout, project_id};
use connecto_store::Tool;
use connecto_workspace::{Workspace, WorkspaceConfig};

struct Rig {
    root: PathBuf,
    layout: Layout,
    /// Held for the test: every workspace start sets the launcher's
    /// process-wide launch modes, which one test here checks.
    _serial: Option<std::sync::MutexGuard<'static, ()>>,
}

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl Rig {
    fn new(name: &str) -> Self {
        let serial = SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::made(name, Some(serial))
    }

    /// A second rig in a test that already holds one, and so the lock.
    fn alongside(_first: &Rig, name: &str) -> Self {
        Self::made(name, None)
    }

    fn made(name: &str, serial: Option<std::sync::MutexGuard<'static, ()>>) -> Self {
        let root = std::env::temp_dir().join(format!("mg-ws-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for dir in ["home", "data", "work"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let root = fs::canonicalize(root).unwrap();
        let layout = Layout {
            data_root: root.join("data"),
            home: root.join("home"),
        };
        Self {
            root,
            layout,
            _serial: serial,
        }
    }

    /// A project folder under `work`.
    fn folder(&self, path: &str) -> PathBuf {
        let folder = self.root.join("work").join(path);
        fs::create_dir_all(&folder).unwrap();
        fs::canonicalize(folder).unwrap()
    }

    fn start(&self) -> Workspace {
        self.launch(false)
    }

    fn launch(&self, after_update: bool) -> Workspace {
        Workspace::start(WorkspaceConfig {
            layout: self.layout.clone(),
            extras: LaunchExtras::default(),
            audit: Audit::open(&self.root.join(format!("audit-{}.jsonl", unique()))).unwrap(),
            wake: Arc::new(|| {}),
            default_tools: vec![Tool::Claude, Tool::Codex],
            after_update,
        })
        .unwrap()
    }

    /// Save a project's terminal list as an update's quit leaves it: one
    /// idle terminal, saved running.
    fn save_idle_running(&self, folder: &Path) {
        let dir = self
            .layout
            .data_root
            .join("projects")
            .join(project_id(folder));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("terminals.json"),
            r#"{"version":1,"next":{"claude":2},"terminals":[{"id":"claude","tool":"claude","desired":"running"}]}"#,
        )
        .unwrap();
    }

    fn audit_start(&self, file: &Path) -> Workspace {
        Workspace::start(WorkspaceConfig {
            layout: self.layout.clone(),
            extras: LaunchExtras::default(),
            audit: Audit::open(file).unwrap(),
            wake: Arc::new(|| {}),
            default_tools: vec![Tool::Claude, Tool::Codex],
            after_update: false,
        })
        .unwrap()
    }

    fn saved(&self) -> String {
        fs::read_to_string(self.root.join("data/workspace.json")).unwrap_or_default()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}

fn wait_for(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

/// A real path as the workspace shows it: as it is, or on Windows without
/// the `\\?\` prefix (the test folders are short drive paths).
fn shown(path: &Path) -> String {
    let text = path.display().to_string();
    #[cfg(windows)]
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return rest.to_owned();
    }
    text
}

fn names(view: &WorkspaceView) -> Vec<String> {
    view.projects.iter().map(|p| p.name.clone()).collect()
}

fn ready(ws: &Workspace, count: usize) {
    wait_for("the projects to start", || {
        let view = ws.view();
        view.projects.len() == count && view.projects.iter().all(|p| p.terminals.is_some())
    });
}

fn terminal_ids(ws: &Workspace, project: &str) -> Vec<String> {
    let view = ws.view();
    let open = view.projects.iter().find(|p| p.id == project).unwrap();
    open.terminals
        .as_ref()
        .unwrap()
        .view()
        .terminals
        .iter()
        .map(|t| t.id.clone())
        .collect()
}

fn desired_running(ws: &Workspace, project: &str) -> Option<bool> {
    let view = ws.view();
    let open = view.projects.iter().find(|p| p.id == project)?;
    let terminals = open.terminals.as_ref()?.view();
    terminals.terminals.first().map(|t| t.desired_running)
}

/// ADR 0030: the launch after an update brings back the idle terminals of the
/// projects open at its quit; a project opened afterwards restores as after a
/// quit.
#[test]
fn after_an_update_only_the_projects_open_at_the_quit_bring_back_idle_terminals() {
    let rig = Rig::new("after-update");
    let (a, b) = (rig.folder("a"), rig.folder("b"));
    let ws = rig.start();
    ws.open_project(&a).unwrap();
    ready(&ws, 1);
    ws.begin_shutdown();
    assert!(ws.shut_down(Duration::from_secs(5)));
    drop(ws);
    rig.save_idle_running(&a);
    rig.save_idle_running(&b);

    let ws = rig.launch(true);
    ready(&ws, 1);
    assert_eq!(desired_running(&ws, &project_id(&a)), Some(true));
    ws.open_project(&b).unwrap();
    ready(&ws, 2);
    wait_for("b restored as after a quit", || {
        desired_running(&ws, &project_id(&b)) == Some(false)
    });
    ws.begin_shutdown();
    assert!(ws.shut_down(Duration::from_secs(5)));
}

/// R1: the open projects, their order and names, which one is shown, each one's
/// shown terminal and expanded flag, the sidebar and the recents come back.
#[test]
fn the_workspace_round_trips_across_a_relaunch() {
    let rig = Rig::new("roundtrip");
    let (a, b) = (rig.folder("a/app"), rig.folder("b/tool"));
    let ws = rig.start();
    ws.open_project(&a).unwrap();
    ws.open_project(&b).unwrap();
    ready(&ws, 2);
    let (ida, idb) = (project_id(&a), project_id(&b));
    ws.rename_project(&ida, Some("Alpha".to_owned())).unwrap();
    ws.set_expanded(&ida, false);
    ws.set_active_terminal(&idb, "codex");
    ws.set_sidebar_collapsed(true);
    ws.activate(&ida);
    wait_for("the saved file", || {
        let text = rig.saved();
        text.contains("Alpha")
            && text.contains("\"active_terminal\"")
            && text.contains("collapsed\": true")
    });
    // Order of opening is the sidebar's order.
    assert_eq!(ws.view().projects[0].id, ida);
    ws.begin_shutdown();
    assert!(ws.shut_down(Duration::from_secs(5)));
    drop(ws);

    let again = rig.start();
    // The projects show in the sidebar before their terminals are up.
    assert_eq!(again.view().projects.len(), 2);
    ready(&again, 2);
    let view = again.view();
    assert_eq!(names(&view), ["Alpha", "tool"]);
    assert_eq!(view.active.as_deref(), Some(ida.as_str()));
    assert!(!view.projects[0].expanded && view.projects[1].expanded);
    assert_eq!(view.projects[1].active_terminal.as_deref(), Some("codex"));
    assert!(view.sidebar_collapsed);
    assert_eq!(view.recents.len(), 2);
    assert_eq!(view.recents[0].path, shown(&b), "newest first");
    assert!(view.warning.is_none());
}

/// Sidebar groups: made, named, given projects, kept across a relaunch, and
/// removed without closing their projects.
#[test]
fn project_groups_are_saved_and_a_deleted_group_frees_its_projects() {
    let rig = Rig::new("groups");
    let (a, b) = (rig.folder("a/app"), rig.folder("b/tool"));
    let ws = rig.start();
    ws.open_project(&a).unwrap();
    ws.open_project(&b).unwrap();
    ready(&ws, 2);
    let (ida, idb) = (project_id(&a), project_id(&b));
    assert!(ws.create_group("  ").is_err(), "a group needs a name");
    ws.create_group("Clients").unwrap();
    wait_for("the group", || ws.view().groups.len() == 1);
    let group = ws.view().groups[0].id.clone();
    assert_eq!(group, "group-1");
    assert!(ws.set_project_group(&ida, Some("group-9")).is_err());
    ws.set_project_group(&ida, Some(&group)).unwrap();
    ws.rename_group(&group, "Customers").unwrap();
    wait_for("the saved groups", || {
        let text = rig.saved();
        text.contains("Customers") && text.contains("\"group\": \"group-1\"")
    });
    ws.begin_shutdown();
    assert!(ws.shut_down(Duration::from_secs(5)));
    drop(ws);

    let again = rig.start();
    ready(&again, 2);
    let view = again.view();
    assert_eq!(view.groups.len(), 1);
    assert_eq!(view.groups[0].name, "Customers");
    let group_of = |id: &str| {
        again
            .view()
            .projects
            .iter()
            .find(|p| p.id == id)
            .and_then(|p| p.group.clone())
    };
    assert_eq!(group_of(&ida).as_deref(), Some("group-1"));
    assert_eq!(group_of(&idb), None);
    again.delete_group(&group).unwrap();
    wait_for("the group to go", || again.view().groups.is_empty());
    assert_eq!(again.view().projects.len(), 2, "its projects stay open");
    assert_eq!(group_of(&ida), None);
}

/// Two workspaces in one process keep their own Automations folder: one's
/// registration is not the other's, and settings without a folder clear an
/// older registration for the same data folder.
#[test]
fn workspaces_register_their_own_automations_folder_and_cleared_settings_clear_it() {
    let one = Rig::new("auto-one");
    let two = Rig::alongside(&one, "auto-two");
    let folder = |rig: &Rig| {
        let dir = rig.root.join("data/automations");
        fs::create_dir_all(dir.join("x")).unwrap();
        fs::canonicalize(dir).unwrap()
    };
    let b = folder(&two);
    let registered =
        |rig: &Rig| connecto_session::project_folder::allowed_root(&rig.root.join("data"));
    let ws_one = one.start();
    let ws_two = two.start();
    // One registers a folder inside the other's data folder, which the other
    // would refuse as a project. It is the other's registration that counts.
    ws_one.set_automations_root(Some(b.clone())).unwrap();
    assert_eq!(registered(&one), Some(b.clone()));
    assert_eq!(registered(&two), None);
    ws_two.open_project(&b).unwrap();
    wait_for("the refusal", || ws_two.view().last_error.is_some());
    assert!(ws_two.view().projects.is_empty());
    ws_two.set_automations_root(Some(b.clone())).unwrap();
    assert_eq!(registered(&two), Some(b.clone()));
    ws_two.open_project(&b).unwrap();
    ready(&ws_two, 1);
    assert_eq!(registered(&one), Some(b));
    // Settings that carry no folder clear the registration on the next start.
    wait_for("the folder saved", || {
        fs::read_to_string(one.root.join("data/settings.json"))
            .unwrap_or_default()
            .contains("automations_root")
    });
    ws_one.begin_shutdown();
    assert!(ws_one.shut_down(Duration::from_secs(5)));
    drop(ws_one);
    fs::write(one.root.join("data/settings.json"), r#"{"version":1}"#).unwrap();
    let again = one.start();
    assert_eq!(again.automations_root(), None);
    assert_eq!(registered(&one), None);
    again.open_project(&folder(&one)).unwrap();
    wait_for("the refusal", || again.view().last_error.is_some());
    assert!(again.view().projects.is_empty());
    // The other one is untouched.
    assert!(registered(&two).is_some());
}

/// The Automations folder is saved in settings.json, survives a relaunch and a
/// later change of another setting, is allowed to be a project even inside the
/// data folder, and stays optional for files from before it existed.
#[test]
fn the_automations_folder_is_saved_allowed_and_kept_by_other_settings_writes() {
    use connecto_core::{UtilityEffort, UtilitySettings};
    let rig = Rig::new("auto-root");
    let settings = rig.root.join("data/settings.json");
    // A settings file from before the field existed loads and gets no folder.
    fs::write(
        &settings,
        r#"{"version":1,"utility":{"model":"opus","effort":"low"}}"#,
    )
    .unwrap();
    let ws = rig.start();
    assert_eq!(ws.automations_root(), None);
    // A folder inside the data folder is otherwise refused as a project.
    let auto = rig.root.join("data/automations");
    fs::create_dir_all(auto.join("approve")).unwrap();
    let auto = fs::canonicalize(auto).unwrap();
    ws.open_project(&auto).unwrap();
    wait_for("the refusal", || ws.view().last_error.is_some());
    ws.set_automations_root(Some(auto.clone())).unwrap();
    ws.open_project(&auto).unwrap();
    ready(&ws, 1);
    // Another setting changes: the folder is still in the file.
    let mut chosen = ws.utility_settings();
    chosen.effort = UtilityEffort::High;
    ws.set_utility_settings(UtilitySettings { ..chosen });
    wait_for("both settings saved", || {
        let text = fs::read_to_string(&settings).unwrap_or_default();
        text.contains("automations_root") && text.contains("high") && text.contains("opus")
    });
    ws.begin_shutdown();
    assert!(ws.shut_down(Duration::from_secs(5)));
    drop(ws);
    // After a relaunch the folder is back, before any project is restored.
    let again = rig.start();
    assert_eq!(again.automations_root(), Some(auto.clone()));
    assert_eq!(
        connecto_session::project_folder::allowed_root(&rig.root.join("data")),
        Some(auto)
    );
    ready(&again, 1);
}

/// R1: an unreadable file is kept as `workspace.json.bad`, a warning shows, the
/// app opens with an empty workspace, and the next change writes a good file.
#[test]
fn an_unreadable_workspace_is_kept_aside_with_a_warning() {
    let rig = Rig::new("bad");
    fs::write(rig.root.join("data/workspace.json"), "{ not json").unwrap();
    let ws = rig.start();
    let view = ws.view();
    assert!(view.projects.is_empty());
    assert!(
        view.warning
            .as_deref()
            .unwrap()
            .contains("workspace.json.bad"),
        "{:?}",
        view.warning
    );
    assert!(rig.root.join("data/workspace.json.bad").exists());
    assert!(!rig.root.join("data/workspace.json").exists());
    ws.open_project(&rig.folder("p")).unwrap();
    wait_for("a good file", || rig.saved().contains("\"projects\""));
    assert_eq!(
        fs::read_to_string(rig.root.join("data/workspace.json.bad")).unwrap(),
        "{ not json"
    );
}

/// Settings that cannot be read are not taken for missing ones: the file is
/// never written this run, a warning shows, and a folder choice is refused.
#[test]
fn settings_that_cannot_be_read_are_never_written_over() {
    use connecto_core::{UtilityEffort, UtilitySettings};
    let rig = Rig::new("settings-unreadable");
    // A folder where the file should be: reading fails, and not with "not found".
    let settings = rig.root.join("data/settings.json");
    fs::create_dir_all(settings.join("inner")).unwrap();
    let ws = rig.start();
    let warning = ws.view().warning.clone().unwrap_or_default();
    assert!(warning.contains("settings could not be read"), "{warning}");
    let home = rig.folder("home-choice");
    assert!(ws.set_connecto_home(Some(home)).is_err());
    assert_eq!(ws.connecto_home(), None);
    let mut chosen = ws.utility_settings();
    chosen.effort = UtilityEffort::High;
    ws.set_utility_settings(UtilitySettings { ..chosen });
    wait_for("the note that it is not saved", || {
        ws.view()
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("not saved this run"))
    });
    assert!(settings.join("inner").is_dir(), "left as it was");
    assert!(!rig.root.join("data/settings.json.tmp").exists());
}

/// Unusable settings are kept as `settings.json.bad` with a warning, and the
/// next change writes a good file.
#[test]
fn unusable_settings_are_kept_aside_with_a_warning() {
    let rig = Rig::new("settings-bad");
    let settings = rig.root.join("data/settings.json");
    fs::write(&settings, "{ not json").unwrap();
    let ws = rig.start();
    let warning = ws.view().warning.clone().unwrap_or_default();
    assert!(warning.contains("settings.json.bad"), "{warning}");
    assert_eq!(
        fs::read_to_string(rig.root.join("data/settings.json.bad")).unwrap(),
        "{ not json"
    );
    let home = rig.folder("home-choice");
    ws.set_connecto_home(Some(home)).unwrap();
    wait_for("a good file", || {
        fs::read_to_string(&settings)
            .unwrap_or_default()
            .contains("connecto_home")
    });
}

/// R2: two projects run at once, each with its own registry and its own
/// terminal list and history on disk.
#[test]
fn two_projects_have_independent_registries() {
    let rig = Rig::new("two");
    let (a, b) = (rig.folder("a"), rig.folder("b"));
    let ws = rig.start();
    ws.open_project(&a).unwrap();
    ws.open_project(&b).unwrap();
    ready(&ws, 2);
    let (ida, idb) = (project_id(&a), project_id(&b));
    assert_eq!(terminal_ids(&ws, &ida), ["claude", "codex"]);
    assert_eq!(terminal_ids(&ws, &idb), ["claude", "codex"]);

    let view = ws.view();
    let in_a = view.projects[0].terminals.clone().unwrap();
    in_a.close("claude").unwrap();
    in_a.add(ToolKind::Codex, Some("extra".to_owned())).unwrap();
    wait_for("a's changes", || {
        terminal_ids(&ws, &ida) == ["codex", "codex-2"]
    });
    assert_eq!(
        terminal_ids(&ws, &idb),
        ["claude", "codex"],
        "b is untouched"
    );
    assert_eq!(in_a.view().history.len(), 1);
    let b_view = view.projects[1].terminals.clone().unwrap().view();
    assert!(b_view.history.is_empty());
    // Each has its own files under projects/<id>.
    let data = rig.root.join("data/projects");
    assert!(data.join(&ida).join("terminals.json").exists());
    assert!(data.join(&ida).join("history/index.json").exists());
    assert!(
        !data.join(&idb).join("terminals.json").exists(),
        "b has nothing to save"
    );
    assert!(!data.join(&idb).join("history").exists());
}

/// R2: showing another project starts, stops and types into nothing.
#[test]
fn switching_projects_changes_nothing_in_either() {
    let rig = Rig::new("switch");
    let (a, b) = (rig.folder("a"), rig.folder("b"));
    let ws = rig.start();
    ws.open_project(&a).unwrap();
    ws.open_project(&b).unwrap();
    ready(&ws, 2);
    let (ida, idb) = (project_id(&a), project_id(&b));
    let snapshot = |ws: &Workspace| -> Vec<String> {
        ws.view()
            .projects
            .iter()
            .flat_map(|p| {
                p.terminals
                    .as_ref()
                    .unwrap()
                    .view()
                    .terminals
                    .iter()
                    .map(|t| {
                        let pane = ws.relay(&p.id).unwrap().pane(&t.id).unwrap();
                        format!(
                            "{}/{} {:?} attempt={} activity={} desired={}",
                            p.id,
                            t.id,
                            t.state,
                            pane.attempt(),
                            t.activity,
                            t.desired_running
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    };
    wait_for("preflights to end", || {
        ws.view().projects.iter().all(|p| {
            p.terminals
                .as_ref()
                .unwrap()
                .view()
                .terminals
                .iter()
                .all(|t| t.state == TermActivity::Failed)
        })
    });
    let before = snapshot(&ws);
    for _ in 0..3 {
        ws.activate(&idb);
        wait_for("b to show", || {
            ws.view().active.as_deref() == Some(idb.as_str())
        });
        ws.activate(&ida);
        wait_for("a to show", || {
            ws.view().active.as_deref() == Some(ida.as_str())
        });
    }
    thread::sleep(Duration::from_millis(600));
    assert_eq!(snapshot(&ws), before);
    assert!(ws.view().last_error.is_none());
}

/// Opening a folder that is open, by any path to it, only shows it.
#[cfg(unix)]
#[test]
fn opening_the_same_folder_twice_just_activates_it() {
    let rig = Rig::new("twice");
    let (a, b) = (rig.folder("a"), rig.folder("b"));
    let link = rig.root.join("work/link-to-a");
    symlink(&a, &link).unwrap();
    let ws = rig.start();
    ws.open_project(&a).unwrap();
    ws.open_project(&b).unwrap();
    ready(&ws, 2);
    assert_eq!(ws.view().active.as_deref(), Some(project_id(&b).as_str()));
    ws.open_project(&link).unwrap();
    wait_for("a to be shown", || {
        ws.view().active.as_deref() == Some(project_id(&a).as_str())
    });
    assert_eq!(ws.view().projects.len(), 2);
    assert_eq!(ws.view().recents.len(), 2);
    assert_eq!(ws.view().recents[0].path, shown(&a));
    assert_eq!(terminal_ids(&ws, &project_id(&a)), ["claude", "codex"]);
}

/// Two open folders with one name get their parent folder appended; a name the
/// user chose is left alone.
#[test]
fn duplicate_folder_names_get_the_parent_appended() {
    let rig = Rig::new("dupes");
    let (a, b, c) = (
        rig.folder("one/app"),
        rig.folder("two/app"),
        rig.folder("three/other"),
    );
    let ws = rig.start();
    for folder in [&a, &b, &c] {
        ws.open_project(folder).unwrap();
    }
    ready(&ws, 3);
    assert_eq!(names(&ws.view()), ["app (one)", "app (two)", "other"]);
    ws.rename_project(&project_id(&b), Some("mine".to_owned()))
        .unwrap();
    wait_for("the rename", || names(&ws.view())[1] == "mine");
    assert_eq!(names(&ws.view())[0], "app", "the twin is gone");
    ws.rename_project(&project_id(&b), None).unwrap();
    wait_for("the name to clear", || names(&ws.view())[1] == "app (two)");
    assert!(
        ws.rename_project(&project_id(&b), Some("x".repeat(65)))
            .is_err()
    );
    assert!(ws.rename_project("nope", Some("x".to_owned())).is_err());
}

/// The recents hold ten, newest first, include closed projects, and clear.
#[test]
fn recents_hold_ten_and_survive_a_close() {
    let rig = Rig::new("recents");
    let ws = rig.start();
    let folders: Vec<PathBuf> = (0..12).map(|n| rig.folder(&format!("p{n}"))).collect();
    for folder in &folders {
        ws.open_project(folder).unwrap();
    }
    wait_for("twelve projects", || ws.view().projects.len() == 12);
    wait_for("the last one shown", || {
        ws.view().active.as_deref() == Some(project_id(&folders[11]).as_str())
    });
    let view = ws.view();
    assert_eq!(view.recents.len(), 10);
    assert_eq!(view.recents[0].path, shown(&folders[11]));
    assert_eq!(view.recents[9].path, shown(&folders[2]));
    ws.close_project(&project_id(&folders[11])).unwrap();
    wait_for("the close", || ws.view().projects.len() == 11);
    assert_eq!(
        ws.view().recents.len(),
        10,
        "a closed project stays in the recents"
    );
    assert_eq!(ws.view().recents[0].path, shown(&folders[11]));
    ws.clear_recents();
    wait_for("the recents to clear", || ws.view().recents.is_empty());
}

/// Reorder, close the shown project (a neighbour is shown), and a refused
/// folder (home, Connecto's data, a missing one).
#[test]
fn projects_move_close_and_refuse_forbidden_folders() {
    let rig = Rig::new("moves");
    let (a, b, c) = (rig.folder("a"), rig.folder("b"), rig.folder("c"));
    let ws = rig.start();
    for folder in [&a, &b, &c] {
        ws.open_project(folder).unwrap();
    }
    ready(&ws, 3);
    let (ida, idb, idc) = (project_id(&a), project_id(&b), project_id(&c));
    ws.move_project(&idc, 0);
    wait_for("the move", || ws.view().projects[0].id == idc);
    assert_eq!(
        ws.view()
            .projects
            .iter()
            .map(|p| p.id.clone())
            .collect::<Vec<_>>(),
        [idc.clone(), ida.clone(), idb.clone()]
    );
    ws.activate(&ida);
    wait_for("a shown", || {
        ws.view().active.as_deref() == Some(ida.as_str())
    });
    ws.close_project(&ida).unwrap();
    wait_for("a closed", || ws.view().projects.len() == 2);
    assert_eq!(
        ws.view().active.as_deref(),
        Some(idb.as_str()),
        "the next one"
    );
    assert!(ws.close_project(&ida).is_err());
    // Refusals are decided on the worker and show in the view.
    for refused in [
        rig.layout.home.clone(),
        rig.layout.data_root.clone(),
        rig.root.join("work/missing"),
    ] {
        assert_eq!(
            ws.open_project(&refused),
            Ok(()),
            "queued, not checked here"
        );
        wait_for("the refusal", || ws.view().last_error.is_some());
        ws.activate(&idb);
        wait_for("the error to clear", || ws.view().last_error.is_none());
    }
    assert_eq!(ws.view().projects.len(), 2);
    // Opening it again brings it back, with its terminals.
    ws.open_project(&a).unwrap();
    wait_for("a to come back", || {
        ws.view()
            .projects
            .iter()
            .any(|p| p.id == ida && p.terminals.is_some())
    });
    assert_eq!(terminal_ids(&ws, &ida), ["claude", "codex"]);
    assert_eq!(ws.view().active.as_deref(), Some(ida.as_str()));
}

/// A saved project whose folder is gone stays in the sidebar with the reason,
/// and has no registry.
#[test]
fn a_saved_project_whose_folder_is_gone_shows_why() {
    let rig = Rig::new("gone");
    let (a, b) = (rig.folder("a"), rig.folder("b"));
    let ws = rig.start();
    ws.open_project(&a).unwrap();
    ws.open_project(&b).unwrap();
    ready(&ws, 2);
    assert!(ws.shut_down(Duration::from_secs(5)));
    drop(ws);
    fs::remove_dir_all(&a).unwrap();

    let again = rig.start();
    wait_for("the other project to start", || {
        again
            .view()
            .projects
            .get(1)
            .is_some_and(|p| p.terminals.is_some())
    });
    let view = again.view();
    assert_eq!(view.projects.len(), 2);
    assert!(view.projects[0].terminals.is_none());
    assert!(view.projects[0].error.is_some());
    assert!(view.projects[1].error.is_none());
}

/// R12: one switch covers every project. Pausing blocks an agent's writes in
/// both, and Stop Everything pauses and closes nothing.
#[test]
fn one_pause_switch_covers_every_project() {
    let rig = Rig::new("pause");
    let (a, b) = (rig.folder("a"), rig.folder("b"));
    let ws = rig.start();
    ws.open_project(&a).unwrap();
    ws.open_project(&b).unwrap();
    ready(&ws, 2);
    let relays = [
        ws.relay(&project_id(&a)).unwrap(),
        ws.relay(&project_id(&b)).unwrap(),
    ];
    let switch = ws.pause_switch();
    for relay in &relays {
        assert!(Arc::ptr_eq(&relay.pause_switch(), &switch), "one switch");
        assert!(relay.agent_add(ToolKind::Claude).is_ok(), "not paused yet");
    }
    switch.pause();
    for relay in &relays {
        assert_eq!(
            relay.agent_add(ToolKind::Claude),
            Err(connecto_session::TermError::Paused)
        );
    }
    switch.resume();
    ws.stop_everything();
    assert!(ws.pause_switch().is_paused(), "Stop Everything pauses");
    for relay in &relays {
        assert_eq!(
            relay.agent_add(ToolKind::Claude),
            Err(connecto_session::TermError::Paused)
        );
    }
    thread::sleep(Duration::from_millis(300));
    for id in [project_id(&a), project_id(&b)] {
        assert_eq!(terminal_ids(&ws, &id).len(), 3, "nothing was closed");
    }
}

/// A5: every audit record of a project's action names the project.
#[test]
fn audit_records_name_their_project() {
    let rig = Rig::new("audit");
    let (a, b) = (rig.folder("a"), rig.folder("b"));
    let file = rig.root.join("audit.jsonl");
    let ws = rig.audit_start(&file);
    ws.open_project(&a).unwrap();
    ws.open_project(&b).unwrap();
    ready(&ws, 2);
    // A user's stop is audited by the Gate of the project it is in.
    wait_for("preflights to end", || {
        ws.view().projects.iter().all(|p| {
            p.terminals
                .as_ref()
                .unwrap()
                .view()
                .terminals
                .iter()
                .all(|t| t.state == TermActivity::Failed)
        })
    });
    ws.stop_everything();
    thread::sleep(Duration::from_millis(300));
    ws.begin_shutdown();
    assert!(ws.shut_down(Duration::from_secs(5)));
    let text = fs::read_to_string(&file).unwrap();
    for folder in [&a, &b] {
        let id = project_id(folder);
        assert!(
            text.contains(&format!("\"project\":\"{id}\"")),
            "no record names {id}:\n{text}"
        );
    }
    assert!(connecto_audit::verify(&file).is_ok());
}

/// Item 3: saved A, B, C where A is a link to B is two projects, not a panic,
/// and the workspace still takes commands afterwards.
#[cfg(unix)]
#[test]
fn two_saved_paths_to_one_folder_are_one_project() {
    let rig = Rig::new("linked");
    let (b, c) = (rig.folder("b"), rig.folder("c"));
    let a = rig.root.join("work/a");
    symlink(&b, &a).unwrap();
    let list = format!(
        r#"{{"version":1,"active":"{}","projects":[{{"path":"{}"}},{{"path":"{}"}},{{"path":"{}"}}]}}"#,
        c.display(),
        a.display(),
        b.display(),
        c.display()
    );
    fs::write(rig.root.join("data/workspace.json"), list).unwrap();
    let ws = rig.start();
    ready(&ws, 2);
    let view = ws.view();
    assert_eq!(
        view.projects
            .iter()
            .map(|p| p.id.clone())
            .collect::<Vec<_>>(),
        [project_id(&b), project_id(&c)]
    );
    assert_eq!(view.active.as_deref(), Some(project_id(&c).as_str()));
    ws.activate(&project_id(&b));
    wait_for("the worker to take a command", || {
        ws.view().active.as_deref() == Some(project_id(&b).as_str())
    });
}

/// Item 1: a project still being set up when the app shuts down is stopped
/// before the shutdown returns, and never installed after it. The list read is
/// held up with a named pipe. Unix only: a FIFO at a file path has no
/// Windows equivalent (named pipes live in `\\.\pipe\`, not in a folder).
#[cfg(unix)]
#[test]
fn a_setup_in_flight_is_stopped_before_shutdown_returns() {
    let rig = Rig::new("inflight");
    let a = rig.folder("a");
    let dir = rig.root.join("data/projects").join(project_id(&a));
    fs::create_dir_all(&dir).unwrap();
    let fifo = dir.join("terminals.json");
    assert!(
        std::process::Command::new("/usr/bin/mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let ws = Arc::new(rig.start());
    ws.open_project(&a).unwrap();
    thread::sleep(Duration::from_millis(500));
    assert!(
        ws.relay(&project_id(&a)).is_none(),
        "still reading its list"
    );

    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutting = {
        let (ws, done) = (Arc::clone(&ws), Arc::clone(&done));
        thread::spawn(move || {
            ws.begin_shutdown();
            let clean = ws.shut_down(Duration::from_secs(20));
            done.store(true, std::sync::atomic::Ordering::SeqCst);
            clean
        })
    };
    thread::sleep(Duration::from_millis(700));
    assert!(
        !done.load(std::sync::atomic::Ordering::SeqCst),
        "shutdown returned with a setup still running"
    );
    // Let the read finish.
    fs::write(&fifo, r#"{"version":1,"terminals":[]}"#).unwrap();
    assert!(shutting.join().unwrap());
    assert!(
        ws.relay(&project_id(&a)).is_none(),
        "nothing installed after it"
    );
    assert!(ws.panes().is_empty());
}

/// Connecto Home is saved, allowed as a project (with the Automations folder in
/// it), and kept by other settings writes; an old file with only the
/// automations folder still registers that folder.
#[test]
fn connecto_home_is_saved_allowed_and_the_old_folder_still_registers() {
    let rig = Rig::new("home");
    let home = rig.root.join("data/connecto-home");
    fs::create_dir_all(home.join("automations")).unwrap();
    let home = fs::canonicalize(home).unwrap();
    let ws = rig.start();
    assert_eq!(ws.connecto_home(), None);
    ws.set_connecto_home(Some(home.clone())).unwrap();
    ws.open_project(&home.join("automations")).unwrap();
    ready(&ws, 1);
    wait_for("the home saved", || {
        fs::read_to_string(rig.root.join("data/settings.json"))
            .unwrap_or_default()
            .contains("connecto_home")
    });
    ws.begin_shutdown();
    assert!(ws.shut_down(Duration::from_secs(5)));
    drop(ws);
    let again = rig.start();
    assert_eq!(again.connecto_home(), Some(home.clone()));
    assert_eq!(
        connecto_session::project_folder::allowed_root(&rig.root.join("data")),
        Some(home)
    );
    ready(&again, 1);
    again.begin_shutdown();
    assert!(again.shut_down(Duration::from_secs(5)));
    drop(again);
    // Only the old setting: it is the allowed folder.
    let old = rig.root.join("data/auto-old");
    fs::create_dir_all(&old).unwrap();
    let old = fs::canonicalize(old).unwrap();
    fs::write(
        rig.root.join("data/settings.json"),
        format!(
            r#"{{"version":1,"automations_root":{:?}}}"#,
            old.to_string_lossy()
        ),
    )
    .unwrap();
    let legacy = rig.start();
    assert_eq!(legacy.connecto_home(), None);
    assert_eq!(
        connecto_session::project_folder::allowed_root(&rig.root.join("data")),
        Some(old)
    );
}

/// A project that is set up is held at the install gate until the packages
/// manager lets it go, and one that is not gated is not held.
#[test]
fn a_new_project_waits_at_the_install_gate_for_its_packages() {
    let rig = Rig::new("gate");
    let data = rig.root.join("data");
    connecto_session::install_gate::enable(&data);
    let ws = rig.start();
    let folder = rig.folder("gated");
    ws.open_project(&folder).unwrap();
    ready(&ws, 1);
    let id = project_id(&folder);
    assert!(connecto_session::install_gate::is_pending(&data, &id));
    assert!(
        connecto_session::install_gate::permit(&data, &id, Duration::from_millis(30)).is_none()
    );
    connecto_session::install_gate::finish(&data, &id);
    assert!(
        connecto_session::install_gate::permit(&data, &id, Duration::from_millis(30)).is_some()
    );
    connecto_session::install_gate::disable(&data);
}

/// The launch choice is saved, reaches the launcher at once, survives a
/// relaunch before any terminal is restored, and an old file keeps today's
/// behaviour (bypass) for both tools.
#[test]
fn the_launch_modes_are_saved_applied_to_the_launcher_and_restored() {
    use connecto_core::{AgentMode, LaunchModes};
    use connecto_session::launch::{launch_modes, set_launch_modes};
    let rig = Rig::new("modes");
    let settings = rig.root.join("data/settings.json");
    fs::write(
        &settings,
        r#"{"version":1,"utility":{"model":"haiku","effort":"low"}}"#,
    )
    .unwrap();
    set_launch_modes(LaunchModes {
        claude: AgentMode::Prompt,
        codex: AgentMode::Prompt,
        pi: AgentMode::Prompt,
        antigravity: AgentMode::Prompt,
        opencode: AgentMode::Prompt,
    });
    let ws = rig.start();
    // An old file: both tools keep today's behaviour, and the launcher is told.
    assert_eq!(ws.launch_modes(), LaunchModes::default());
    assert_eq!(launch_modes(), LaunchModes::default());
    let chosen = LaunchModes {
        claude: AgentMode::Auto,
        codex: AgentMode::Prompt,
        pi: AgentMode::Bypass,
        antigravity: AgentMode::Auto,
        opencode: AgentMode::Prompt,
    };
    ws.set_launch_modes(chosen);
    assert_eq!(launch_modes(), chosen, "the next start or restart uses it");
    wait_for("the choice saved", || {
        let text = fs::read_to_string(&settings).unwrap_or_default();
        text.contains("\"claude\": \"auto\"") && text.contains("\"codex\": \"prompt\"")
    });
    ws.begin_shutdown();
    assert!(ws.shut_down(Duration::from_secs(5)));
    drop(ws);
    // After a relaunch it is back, and the launcher has it before any terminal
    // is restored.
    set_launch_modes(LaunchModes::default());
    let again = rig.start();
    assert_eq!(again.launch_modes(), chosen);
    assert_eq!(launch_modes(), chosen);
    set_launch_modes(LaunchModes::default());
}

/// A second path to `target` that is the same folder: a directory junction,
/// which any user may make (a symbolic link needs a privilege).
#[cfg(windows)]
fn junction(target: &Path, link: &Path) {
    let made = std::process::Command::new("cmd")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .unwrap();
    assert!(made.status.success(), "mklink: {made:?}");
}

/// Windows: a folder given as a plain `C:\...` path, with spaces and letters
/// outside ASCII, opens once; any other spelling of it (forward slashes,
/// another letter case, `.` and `..`, the verbatim `\\?\` form, a junction)
/// only shows it. The sidebar and the recents show it without the `\\?\`
/// prefix, its id is that of its real path, and it comes back after a
/// relaunch as the same project.
#[cfg(windows)]
#[test]
fn a_windows_folder_opens_once_by_any_spelling_and_comes_back() {
    let rig = Rig::new("win-spelling");
    let real = rig.folder(r"My Projects\Ünïcødé app ✓");
    let other = rig.folder("other");
    let plain = shown(&real);
    assert!(
        !plain.starts_with(r"\\?\") && plain.contains(r"\My Projects\"),
        "{plain}"
    );
    let id = project_id(&real);
    let link = rig.root.join("work").join("link to app");
    junction(Path::new(&plain), &link);
    let ws = rig.start();
    ws.open_project(Path::new(&plain)).unwrap();
    ws.open_project(&other).unwrap();
    ready(&ws, 2);
    let view = ws.view();
    assert_eq!(view.projects[0].id, id);
    assert_eq!(view.projects[0].path, plain);
    assert_eq!(view.projects[0].name, "Ünïcødé app ✓");
    assert_eq!(view.recents[1].path, plain);
    let spellings = [
        plain.replace('\\', "/"),
        plain.to_uppercase(),
        plain.to_lowercase(),
        format!(r"{plain}\..\.\Ünïcødé app ✓"),
        real.display().to_string(),
        link.display().to_string(),
        shown(&link),
    ];
    for spelling in &spellings {
        ws.activate(&project_id(&other));
        wait_for("the other one shown", || {
            ws.view().active.as_deref() == Some(project_id(&other).as_str())
        });
        ws.open_project(Path::new(spelling)).unwrap();
        wait_for(spelling, || {
            ws.view().active.as_deref() == Some(id.as_str())
        });
        let view = ws.view();
        assert!(
            view.last_error.is_none(),
            "{spelling}: {:?}",
            view.last_error
        );
        assert_eq!(view.projects.len(), 2, "{spelling}");
        assert_eq!(view.recents.len(), 2, "{spelling}");
        assert_eq!(view.recents[0].path, plain, "{spelling}");
    }
    wait_for("the saved file", || rig.saved().contains("other"));
    ws.begin_shutdown();
    assert!(ws.shut_down(Duration::from_secs(5)));
    drop(ws);

    // The file is JSON a person can read back, and holds the real path, so
    // the id of a project whose folder is away at the next start is the same.
    let saved: serde_json::Value = serde_json::from_str(&rig.saved()).unwrap();
    assert_eq!(saved["projects"][0]["path"], real.display().to_string());
    assert_eq!(saved["recents"][0]["path"], plain);
    let again = rig.start();
    ready(&again, 2);
    let view = again.view();
    assert_eq!(view.projects[0].id, id);
    assert_eq!(view.projects[0].path, plain);
    assert_eq!(view.active.as_deref(), Some(id.as_str()));
    assert_eq!(view.recents.len(), 2);
    assert!(view.warning.is_none());
}

/// Windows: a saved project whose folder was away at the start keeps its id,
/// so opening the folder once it is back is the same project, not a second.
#[cfg(windows)]
#[test]
fn a_windows_project_whose_folder_comes_back_is_the_same_project() {
    let rig = Rig::new("win-back");
    let a = rig.folder("Väter Ordner");
    let id = project_id(&a);
    let ws = rig.start();
    ws.open_project(&a).unwrap();
    ready(&ws, 1);
    ws.begin_shutdown();
    assert!(ws.shut_down(Duration::from_secs(5)));
    drop(ws);
    let away = rig.root.join("work").join("away");
    fs::rename(&a, &away).unwrap();
    let again = rig.start();
    wait_for("the reason", || {
        again
            .view()
            .projects
            .first()
            .is_some_and(|p| p.error.is_some())
    });
    assert_eq!(again.view().projects[0].id, id);
    fs::rename(&away, &a).unwrap();
    again.open_project(Path::new(&shown(&a))).unwrap();
    ready(&again, 1);
    assert_eq!(again.view().projects[0].id, id);
}

/// Windows: saved A, B, C where A is a junction to B and C is saved in
/// another letter case with forward slashes is two projects, and the shown
/// one follows its id.
#[cfg(windows)]
#[test]
fn two_saved_windows_spellings_of_one_folder_are_one_project() {
    let rig = Rig::new("win-linked");
    let (b, c) = (rig.folder("b"), rig.folder("c"));
    let a = rig.root.join("work").join("a");
    junction(Path::new(&shown(&b)), &a);
    let odd_c = shown(&c).to_uppercase().replace('\\', "/");
    let list = serde_json::json!({
        "version": 1,
        "active": odd_c,
        "projects": [
            { "path": a.display().to_string() },
            { "path": shown(&b) },
            { "path": odd_c },
            { "path": c.display().to_string() },
        ],
    });
    fs::write(rig.root.join("data/workspace.json"), list.to_string()).unwrap();
    let ws = rig.start();
    ready(&ws, 2);
    let view = ws.view();
    assert_eq!(
        view.projects
            .iter()
            .map(|p| p.id.clone())
            .collect::<Vec<_>>(),
        [project_id(&b), project_id(&c)]
    );
    assert_eq!(view.projects[1].path, shown(&c));
    assert_eq!(view.active.as_deref(), Some(project_id(&c).as_str()));
    ws.activate(&project_id(&b));
    wait_for("the worker to take a command", || {
        ws.view().active.as_deref() == Some(project_id(&b).as_str())
    });
}

/// The Automations folder and the agentic jobs folder open in the Connecto
/// Internal group, made once; other projects and a project the user moved
/// elsewhere are left alone.
#[test]
fn connecto_folders_open_in_the_internal_group() {
    let rig = Rig::new("internal");
    let home = rig.root.join("data/connecto-home");
    let jobs = home.join(connecto_workspace::AGENTIC_JOBS_FOLDER);
    fs::create_dir_all(home.join("automations")).unwrap();
    fs::create_dir_all(&jobs).unwrap();
    let home = fs::canonicalize(home).unwrap();
    let ws = rig.start();
    ws.set_connecto_home(Some(home.clone())).unwrap();
    ws.set_automations_root(Some(home.join("automations")))
        .unwrap();
    let plain = rig.folder("plain");
    for dir in [home.join("automations"), jobs.clone(), plain.clone()] {
        ws.open_project(&dir).unwrap();
    }
    ready(&ws, 3);
    let view = ws.view();
    let internal: Vec<_> = view
        .groups
        .iter()
        .filter(|g| g.name == connecto_workspace::INTERNAL_GROUP)
        .collect();
    assert_eq!(internal.len(), 1);
    let group_of = |dir: &Path| {
        let id = project_id(&fs::canonicalize(dir).unwrap());
        view.projects
            .iter()
            .find(|p| p.id == id)
            .unwrap()
            .group
            .clone()
    };
    assert_eq!(
        group_of(&home.join("automations")),
        Some(internal[0].id.clone())
    );
    assert_eq!(group_of(&jobs), Some(internal[0].id.clone()));
    assert_eq!(group_of(&plain), None);
    // Moved out by the user, it stays where it was put when opened again.
    ws.create_group("Mine").unwrap();
    wait_for("the group", || ws.view().groups.len() == 2);
    let mine = ws
        .view()
        .groups
        .iter()
        .find(|g| g.name == "Mine")
        .unwrap()
        .id
        .clone();
    let jobs_id = project_id(&fs::canonicalize(&jobs).unwrap());
    ws.set_project_group(&jobs_id, Some(&mine)).unwrap();
    ws.open_project(&jobs).unwrap();
    wait_for("the move", || {
        ws.view()
            .projects
            .iter()
            .any(|p| p.id == jobs_id && p.group.as_deref() == Some(mine.as_str()))
    });
    thread::sleep(Duration::from_millis(200));
    assert_eq!(
        ws.view()
            .projects
            .iter()
            .find(|p| p.id == jobs_id)
            .unwrap()
            .group
            .as_deref(),
        Some(mine.as_str())
    );
    ws.begin_shutdown();
    assert!(ws.shut_down(Duration::from_secs(5)));
}
