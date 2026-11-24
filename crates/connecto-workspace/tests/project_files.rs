//! A new project cloned from a git repository, and a project deleted with
//! its folder, through the real workspace. Everything lives in a temp folder:
//! the repository is a local bare one, and the "trash" is a folder of the
//! test's own, so no test touches the person's Trash. The temp folder is
//! Cargo's for this crate's tests (under `target/`): the system's is under
//! `/var` on macOS, where a deletion is refused.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use connecto_audit::Audit;
use connecto_core::WorkspaceControl;
use connecto_core::project_files::{ProjectTask, TaskState};
use connecto_session::launch::{LaunchExtras, Layout, project_id};
use connecto_workspace::{Workspace, WorkspaceConfig};

struct Rig {
    root: PathBuf,
    layout: Layout,
}

impl Rig {
    fn new(name: &str) -> Self {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("mg-pf-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for dir in ["home", "data", "work", "trash"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let root = fs::canonicalize(root).unwrap();
        let layout = Layout {
            data_root: root.join("data"),
            home: root.join("home"),
        };
        Self { root, layout }
    }

    fn at(&self, path: &str) -> PathBuf {
        self.root.join(path)
    }

    fn start(&self) -> Workspace {
        let ws = Workspace::start(WorkspaceConfig {
            layout: self.layout.clone(),
            extras: LaunchExtras::default(),
            audit: Audit::open(&self.root.join(format!("audit-{}.jsonl", unique()))).unwrap(),
            wake: Arc::new(|| {}),
            default_tools: Vec::new(),
            after_update: false,
        })
        .unwrap();
        ws.use_trash_folder(self.at("trash"));
        ws
    }

    /// A bare repository with one commit holding `README.md`.
    fn bare_repo(&self) -> PathBuf {
        let source = self.at("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("README.md"), "# Fixture\n").unwrap();
        git(&source, &["init", "-q"]);
        git(&source, &["add", "."]);
        git(
            &source,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "first",
            ],
        );
        let bare = self.at("fixture.git");
        let status = Command::new("git")
            .args(["clone", "-q", "--bare"])
            .arg(&source)
            .arg(&bare)
            .status()
            .unwrap();
        assert!(status.success());
        bare
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}

/// The names in a folder, sorted.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A server that takes connections and never answers: a clone from it
/// waits until cancelled.
fn silent_server() -> u16 {
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = server.local_addr().unwrap().port();
    thread::spawn(move || {
        let mut held = Vec::new();
        for stream in server.incoming().flatten() {
            held.push(stream);
        }
    });
    port
}

/// Why a clone was refused: at once, or by its task (the disk checks run
/// off the caller's thread). Empty when it was not refused.
fn refused(ws: &Workspace, url: &str, parent: &Path) -> String {
    match ws.clone_project(url, parent) {
        Err(why) => why,
        Ok(cloning) => match finish(&cloning.task) {
            TaskState::Failed(why) => why,
            TaskState::Cancelled => "cancelled".to_owned(),
            _ => String::new(),
        },
    }
}

/// Why a deletion was refused: at once, or (the folder's checks run off
/// the window's thread) through its task.
fn delete_refused(ws: &Workspace, plan: &connecto_core::project_files::DeletePlan) -> String {
    match ws.delete_project(plan) {
        Err(why) => why,
        Ok(task) => match finish(&task) {
            TaskState::Failed(why) => why,
            other => panic!("not refused: {other:?}"),
        },
    }
}

fn has_git() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

fn wait_for(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

fn finish(task: &Arc<dyn ProjectTask>) -> TaskState {
    wait_for("the task", || !task.state().is_running());
    task.state()
}

fn shown(path: &Path) -> String {
    let text = path.display().to_string();
    #[cfg(windows)]
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return rest.to_owned();
    }
    text
}

#[test]
fn a_project_cloned_from_a_repository_opens_with_its_files() {
    if !has_git() {
        return;
    }
    let rig = Rig::new("clone");
    let bare = rig.bare_repo();
    let ws = rig.start();
    let cloning = ws
        .clone_project(&bare.display().to_string(), &rig.at("work"))
        .unwrap();
    // A folder of its own, named after the repository.
    assert_eq!(cloning.target, rig.at("work/fixture"));
    assert_eq!(finish(&cloning.task), TaskState::Done);
    assert_eq!(
        fs::read_to_string(rig.at("work/fixture/README.md")).unwrap(),
        "# Fixture\n"
    );
    assert_eq!(entries(&rig.at("work")), ["fixture"], "no staging left");
    // Opened where it was published (its real path), as the window does.
    let published = cloning
        .task
        .published()
        .expect("a finished clone says where it went");
    assert_eq!(published, fs::canonicalize(rig.at("work/fixture")).unwrap());
    ws.open_project(&published).unwrap();
    wait_for("the project", || {
        ws.view()
            .projects
            .iter()
            .any(|p| p.path == shown(&published))
    });

    // Never into a folder that is already there.
    let again = refused(&ws, &bare.display().to_string(), &rig.at("work"));
    assert!(
        again.contains("already has something named \"fixture\""),
        "{again}"
    );
    assert!(rig.at("work/fixture/README.md").exists(), "left as it was");
}

#[test]
fn a_failed_clone_leaves_no_folder_and_says_why() {
    if !has_git() {
        return;
    }
    let rig = Rig::new("clone-fail");
    let ws = rig.start();
    let missing = rig.at("nowhere/repo.git");
    let cloning = ws
        .clone_project(&missing.display().to_string(), &rig.at("work"))
        .unwrap();
    let TaskState::Failed(why) = finish(&cloning.task) else {
        panic!("the clone did not fail");
    };
    assert!(why.starts_with("No repository was found"), "{why}");
    assert!(!rig.at("work/repo").exists(), "no half-made project");
    assert!(entries(&rig.at("work")).is_empty(), "no staging left");
    // An address that cannot be one is refused before git runs.
    assert!(!refused(&ws, "--upload-pack=x", &rig.at("work")).is_empty());
    assert!(!refused(&ws, "https://h/x.git", &rig.at("nope")).is_empty());
    // Nor into Connecto's own data.
    let into_data = refused(&ws, "https://h/x.git", &rig.layout.data_root);
    assert!(into_data.contains("cannot be a project"), "{into_data}");
    assert!(!rig.layout.data_root.join("x").exists());
}

#[test]
fn a_cancelled_clone_stops_git_and_removes_its_folder() {
    if !has_git() {
        return;
    }
    let rig = Rig::new("clone-cancel");
    // A server that takes the connection and never answers: the clone waits.
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = server.local_addr().unwrap().port();
    thread::spawn(move || {
        let mut held = Vec::new();
        for stream in server.incoming().flatten() {
            held.push(stream);
        }
    });
    let ws = rig.start();
    let cloning = ws
        .clone_project(
            &format!("http://127.0.0.1:{port}/slow.git"),
            &rig.at("work"),
        )
        .unwrap();
    thread::sleep(Duration::from_millis(500));
    assert!(
        cloning.task.state().is_running(),
        "{:?}",
        cloning.task.state()
    );
    // git works in a private folder; the target appears only when done.
    assert!(!rig.at("work/slow").exists());
    let staged = entries(&rig.at("work"));
    assert!(
        staged.len() == 1 && staged[0].starts_with(".slow.connecto-clone-"),
        "{staged:?}"
    );
    cloning.task.cancel();
    assert_eq!(finish(&cloning.task), TaskState::Cancelled);
    assert!(entries(&rig.at("work")).is_empty(), "no half-made project");
    assert!(ws.view().projects.is_empty());
}

#[test]
fn deleting_a_project_closes_it_and_moves_its_folder_to_the_trash() {
    let rig = Rig::new("delete");
    let folder = rig.at("work/doomed");
    fs::create_dir_all(folder.join("src")).unwrap();
    fs::write(folder.join("src/main.rs"), "fn main() {}\n").unwrap();
    let keep = rig.at("work/kept");
    fs::create_dir_all(&keep).unwrap();
    let ws = rig.start();
    ws.open_project(&keep).unwrap();
    ws.open_project(&folder).unwrap();
    let id = project_id(&folder);
    wait_for("both projects", || ws.view().projects.len() == 2);
    // What Connecto keeps for it (its terminal list, history).
    let data = rig.layout.data_root.join("projects").join(&id);
    fs::create_dir_all(data.join("history")).unwrap();
    fs::write(data.join("history/one.json"), "{}").unwrap();

    let plan = ws.plan_delete(&id).unwrap();
    assert_eq!(plan.name, "doomed");
    assert_eq!(plan.path, folder.display().to_string());
    assert!(
        plan.contents.starts_with("1 file in 1 folder"),
        "{}",
        plan.contents
    );
    assert_eq!(plan.trash, "the Trash");

    let task = ws.delete_project(&plan).unwrap();
    assert_eq!(finish(&task), TaskState::Done);
    assert!(!folder.exists(), "the folder is gone");
    assert!(
        rig.at("trash/doomed/src/main.rs").exists(),
        "and can be put back"
    );
    let view = ws.view();
    assert_eq!(view.projects.len(), 1);
    assert_eq!(view.projects[0].id, project_id(&keep));
    assert!(
        view.recents.iter().all(|r| r.path != shown(&folder)),
        "out of the recents: {:?}",
        view.recents
    );
    assert!(!data.exists(), "its list and history are gone");
    assert!(keep.exists());
    assert!(
        !fs::read_to_string(rig.layout.data_root.join("workspace.json"))
            .unwrap()
            .contains("doomed")
    );
}

#[test]
fn a_project_holding_another_or_connectos_data_is_not_deleted() {
    let rig = Rig::new("refuse");
    let outer = rig.at("work/outer");
    let inner = outer.join("inner");
    fs::create_dir_all(&inner).unwrap();
    let ws = rig.start();
    ws.open_project(&outer).unwrap();
    ws.open_project(&inner).unwrap();
    wait_for("both projects", || ws.view().projects.len() == 2);
    let outer_id = project_id(&outer);
    let why = ws.plan_delete(&outer_id).unwrap_err();
    assert!(why.contains("holds another project"), "{why}");
    let inner_plan = ws.plan_delete(&project_id(&inner)).unwrap();
    let forged = connecto_core::project_files::DeletePlan {
        project: outer_id.clone(),
        ..inner_plan
    };
    let why = delete_refused(&ws, &forged);
    assert!(why.contains("holds another project"), "{why}");
    thread::sleep(Duration::from_millis(200));
    assert_eq!(ws.view().projects.len(), 2, "nothing closed");
    assert!(inner.exists());

    // The Automations folder is Connecto's own.
    let automations = rig.at("work/automations");
    fs::create_dir_all(&automations).unwrap();
    ws.set_automations_root(Some(automations.clone())).unwrap();
    ws.open_project(&automations).unwrap();
    wait_for("the automations project", || ws.view().projects.len() == 3);
    let why = ws.plan_delete(&project_id(&automations)).unwrap_err();
    assert!(why.contains("Automations folder"), "{why}");
    assert!(automations.exists());
    let missing = connecto_core::project_files::DeletePlan {
        project: "no-such-project".into(),
        ..ws.plan_delete(&project_id(&inner)).unwrap()
    };
    assert!(ws.delete_project(&missing).is_err());
}

#[cfg(unix)]
#[test]
fn a_project_opened_through_a_link_is_never_deleted() {
    let rig = Rig::new("linked");
    let target = rig.at("work/real-folder");
    fs::create_dir_all(&target).unwrap();
    std::os::unix::fs::symlink(&target, rig.at("work/shortcut")).unwrap();
    // A link to a parent counts too.
    let deeper = rig.at("work/real-parent/app");
    fs::create_dir_all(&deeper).unwrap();
    std::os::unix::fs::symlink(rig.at("work/real-parent"), rig.at("work/parent-link")).unwrap();
    let ws = rig.start();
    ws.open_project(&rig.at("work/shortcut")).unwrap();
    ws.open_project(&rig.at("work/parent-link/app")).unwrap();
    wait_for("both projects", || ws.view().projects.len() == 2);
    for folder in [&target, &deeper] {
        let why = ws.plan_delete(&project_id(folder)).unwrap_err();
        assert!(why.contains("opened through a link"), "{why}");
        assert!(folder.exists());
    }
    // And after a relaunch, which only knows the real folders.
    drop(ws);
    let ws = rig.start();
    wait_for("the projects again", || ws.view().projects.len() == 2);
    let why = ws.plan_delete(&project_id(&target)).unwrap_err();
    assert!(why.contains("opened through a link"), "{why}");
}

#[test]
fn a_folder_swapped_after_the_confirmation_is_not_deleted() {
    let rig = Rig::new("swapped");
    let folder = rig.at("work/app");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("mine.txt"), "confirmed").unwrap();
    let ws = rig.start();
    ws.open_project(&folder).unwrap();
    let id = project_id(&folder);
    wait_for("the project", || ws.view().projects.len() == 1);
    let plan = ws.plan_delete(&id).unwrap();
    // Another folder takes its place at the same path.
    fs::rename(&folder, rig.at("work/app-original")).unwrap();
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("theirs.txt"), "not confirmed").unwrap();
    let why = delete_refused(&ws, &plan);
    assert!(why.contains("changed since"), "{why}");
    assert!(folder.join("theirs.txt").exists());
    assert!(fs::read_dir(rig.at("trash")).unwrap().next().is_none());
    assert_eq!(ws.view().projects.len(), 1, "not even closed");
}

#[test]
fn a_failed_trash_keeps_the_folder_recents_and_data_and_releases_the_folder() {
    let rig = Rig::new("trash-fails");
    let folder = rig.at("work/app");
    fs::create_dir_all(&folder).unwrap();
    let ws = rig.start();
    // A trash the folder cannot be moved into.
    ws.use_trash_folder(rig.at("no/such/trash"));
    ws.open_project(&folder).unwrap();
    let id = project_id(&folder);
    wait_for("the project", || ws.view().projects.len() == 1);
    let data = rig.layout.data_root.join("projects").join(&id);
    fs::create_dir_all(&data).unwrap();
    let plan = ws.plan_delete(&id).unwrap();
    let task = ws.delete_project(&plan).unwrap();
    let TaskState::Failed(why) = finish(&task) else {
        panic!("the deletion did not fail");
    };
    assert!(why.contains("Nothing was moved"), "{why}");
    assert!(folder.exists());
    assert!(data.exists(), "Connecto's data for it stays");
    assert!(
        ws.view().recents.iter().any(|r| r.path == shown(&folder)),
        "it stays in the recents"
    );
    // Released: it opens again.
    ws.open_project(&folder).unwrap();
    wait_for("the project again", || ws.view().projects.len() == 1);
}

#[test]
fn a_clone_is_refused_outside_the_transport_policy() {
    let rig = Rig::new("policy");
    let ws = rig.start();
    for url in [
        "file::/srv/x.git",
        "ext::sh -c id",
        "https://ghp_secret@github.com/owner/x.git",
        "ftp://host/x.git",
    ] {
        assert!(!refused(&ws, url, &rig.at("work")).is_empty(), "{url}");
    }
    assert!(fs::read_dir(rig.at("work")).unwrap().next().is_none());
}

#[test]
fn nothing_is_trashed_unless_the_teardown_stopped_everything() {
    let rig = Rig::new("teardown-fails");
    let folder = rig.at("work/app");
    fs::create_dir_all(&folder).unwrap();
    let ws = rig.start();
    ws.on_teardown(Arc::new(|| false));
    ws.open_project(&folder).unwrap();
    let id = project_id(&folder);
    wait_for("the project", || ws.view().projects.len() == 1);
    let plan = ws.plan_delete(&id).unwrap();
    let task = ws.delete_project(&plan).unwrap();
    let TaskState::Failed(why) = finish(&task) else {
        panic!("the deletion did not fail");
    };
    assert!(why.contains("did not all stop"), "{why}");
    assert!(folder.exists(), "left alone");
    assert!(fs::read_dir(rig.at("trash")).unwrap().next().is_none());
    assert!(ws.view().recents.iter().any(|r| r.path == shown(&folder)));
}

#[test]
fn a_folder_being_deleted_cannot_be_opened_until_the_outcome_is_known() {
    let rig = Rig::new("reserved");
    let folder = rig.at("work/app");
    let child = folder.join("child");
    fs::create_dir_all(&child).unwrap();
    let ws = rig.start();
    // The teardown waits until the test lets it go.
    let (go, wait) = std::sync::mpsc::channel::<()>();
    let wait = std::sync::Mutex::new(wait);
    ws.on_teardown(Arc::new(move || {
        let _ = wait.lock().unwrap().recv_timeout(Duration::from_secs(20));
        true
    }));
    ws.open_project(&folder).unwrap();
    let id = project_id(&folder);
    wait_for("the project", || ws.view().projects.len() == 1);
    let plan = ws.plan_delete(&id).unwrap();
    let task = ws.delete_project(&plan).unwrap();
    wait_for("the close", || ws.view().projects.is_empty());
    for path in [&folder, &child] {
        ws.open_project(path).unwrap();
        wait_for("the refusal", || {
            ws.view()
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("is being deleted"))
        });
        assert!(ws.view().projects.is_empty(), "{} opened", path.display());
    }
    assert!(!refused(&ws, "https://h/x.git", &child).is_empty());
    assert!(task.state().is_running());
    go.send(()).unwrap();
    assert_eq!(finish(&task), TaskState::Done);
    assert!(rig.at("trash/app/child").exists());
}

#[test]
fn a_project_opened_since_the_plan_inside_the_folder_stops_the_deletion() {
    let rig = Rig::new("child-opened");
    let folder = rig.at("work/app");
    let child = folder.join("child");
    fs::create_dir_all(&child).unwrap();
    let ws = rig.start();
    ws.open_project(&folder).unwrap();
    let id = project_id(&folder);
    wait_for("the project", || ws.view().projects.len() == 1);
    let plan = ws.plan_delete(&id).unwrap();
    // Queued before the deletion: the worker opens it first.
    ws.open_project(&child).unwrap();
    // Refused by the worker's checks, or (should the view lag) its own
    // check of the live list just before the teardown.
    let why = delete_refused(&ws, &plan);
    assert!(
        why.contains("holds another project") || why.contains("holds another open project"),
        "{why}"
    );
    assert!(child.exists());
    assert!(fs::read_dir(rig.at("trash")).unwrap().next().is_none());
}

#[test]
fn a_clone_and_a_deletion_never_overlap() {
    if !has_git() {
        return;
    }
    let rig = Rig::new("overlap");
    let folder = rig.at("work/app");
    fs::create_dir_all(&folder).unwrap();
    let ws = rig.start();
    ws.open_project(&folder).unwrap();
    let id = project_id(&folder);
    wait_for("the project", || ws.view().projects.len() == 1);
    let plan = ws.plan_delete(&id).unwrap();
    let port = silent_server();
    let url = format!("http://127.0.0.1:{port}/slow.git");
    let cloning = ws.clone_project(&url, &folder).unwrap();
    // Admitted (its folder held) on a thread of its own: past that before
    // the next one is asked, so the two do not race for the hold.
    wait_for("the clone's admission", || {
        cloning.task.state() != TaskState::Running("Checking the folder…".into())
    });
    // A second clone to the same place waits its turn too.
    let twice = refused(&ws, &url, &folder);
    assert!(
        twice.contains("already") || twice.contains("cloned into"),
        "{twice}"
    );
    let task = ws.delete_project(&plan).unwrap();
    let TaskState::Failed(why) = finish(&task) else {
        panic!("deleted while a clone was going into it");
    };
    assert!(why.contains("being cloned into"), "{why}");
    assert!(folder.exists());
    cloning.task.cancel();
    assert_eq!(finish(&cloning.task), TaskState::Cancelled);
    // Released: the deletion goes now (the project closed with the failed
    // attempt, so it is opened again).
    ws.open_project(&folder).unwrap();
    wait_for("the project again", || ws.view().projects.len() == 1);
    let plan = ws.plan_delete(&id).unwrap();
    assert_eq!(finish(&ws.delete_project(&plan).unwrap()), TaskState::Done);
}

#[cfg(unix)]
#[test]
fn a_linked_open_is_remembered_in_the_recents() {
    let rig = Rig::new("linked-recent");
    let target = rig.at("work/real-folder");
    fs::create_dir_all(&target).unwrap();
    std::os::unix::fs::symlink(&target, rig.at("work/shortcut")).unwrap();
    let ws = rig.start();
    ws.open_project(&rig.at("work/shortcut")).unwrap();
    wait_for("the project", || ws.view().projects.len() == 1);
    let id = project_id(&target);
    ws.close_project(&id).unwrap();
    wait_for("closed", || ws.view().projects.is_empty());
    // Open Recent opens the recent's path: the real folder.
    let recent = ws.view().recents[0].path.clone();
    assert_eq!(recent, shown(&target));
    ws.open_project(Path::new(&recent)).unwrap();
    wait_for("reopened", || ws.view().projects.len() == 1);
    let why = ws.plan_delete(&id).unwrap_err();
    assert!(why.contains("opened through a link"), "{why}");
    // Also after a relaunch with it closed.
    ws.close_project(&id).unwrap();
    wait_for("closed again", || ws.view().projects.is_empty());
    drop(ws);
    let ws = rig.start();
    ws.open_project(Path::new(&recent)).unwrap();
    wait_for("reopened after relaunch", || ws.view().projects.len() == 1);
    let why = ws.plan_delete(&id).unwrap_err();
    assert!(why.contains("opened through a link"), "{why}");
}

#[cfg(unix)]
#[test]
fn a_saved_folder_that_became_a_link_while_closed_is_never_deleted() {
    let rig = Rig::new("became-link");
    let folder = rig.at("work/app");
    fs::create_dir_all(&folder).unwrap();
    let ws = rig.start();
    ws.open_project(&folder).unwrap();
    wait_for("the project", || ws.view().projects.len() == 1);
    drop(ws);
    // While Connecto is closed the folder moves and a link takes its place.
    let moved = rig.at("work/elsewhere");
    fs::rename(&folder, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, &folder).unwrap();
    let ws = rig.start();
    wait_for("the project restored", || {
        ws.view().projects.iter().any(|p| p.terminals.is_some())
    });
    let id = ws.view().projects[0].id.clone();
    let why = ws.plan_delete(&id).unwrap_err();
    assert!(why.contains("opened through a link"), "{why}");
    assert!(moved.exists());
}

#[test]
fn dropping_the_workspace_ends_its_worker() {
    let rig = Rig::new("drop");
    let ws = rig.start();
    // The worker holds the shared Pause switch through the workspace's
    // state: once the worker has ended, only this handle is left.
    let pause = ws.pause_switch();
    assert!(Arc::strong_count(&pause) > 1);
    drop(ws);
    wait_for("the worker to end", || Arc::strong_count(&pause) == 1);
}

#[test]
fn a_clone_into_a_forbidden_place_is_refused_before_git_writes_anything() {
    if !has_git() {
        return;
    }
    let rig = Rig::new("forbidden-clone");
    // A repository whose name is the CLIs' state folder in the home folder.
    let source = rig.bare_repo();
    let named = rig.at(".codex.git");
    fs::rename(&source, &named).unwrap();
    let ws = rig.start();
    let why = refused(&ws, &named.display().to_string(), &rig.layout.home);
    assert!(why.contains("Nothing was cloned"), "{why}");
    assert!(
        entries(&rig.layout.home).is_empty(),
        "{:?}",
        entries(&rig.layout.home)
    );
    // And it is not held: an allowed clone of it goes ahead.
    let cloning = ws
        .clone_project(&named.display().to_string(), &rig.at("work"))
        .unwrap();
    assert_eq!(finish(&cloning.task), TaskState::Done);
    assert!(rig.at("work/.codex/README.md").exists());
}

#[test]
fn a_confirmation_holds_its_folder_open_until_it_is_used_or_closed() {
    let rig = Rig::new("held");
    let folder = rig.at("work/app");
    fs::create_dir_all(&folder).unwrap();
    let ws = rig.start();
    ws.open_project(&folder).unwrap();
    let id = project_id(&folder);
    wait_for("the project", || ws.view().projects.len() == 1);
    let plan = ws.plan_delete(&id).unwrap();
    assert_eq!(ws.held_plans(), 1, "held while the dialog shows it");
    ws.release_delete_plan(&plan);
    assert_eq!(ws.held_plans(), 0, "let go when the dialog closes");
    let why = ws.delete_project(&plan).err().unwrap();
    assert!(why.contains("expired"), "{why}");
    assert!(folder.exists());
    // A few at most.
    for _ in 0..12 {
        let _ = ws.plan_delete(&id).unwrap();
    }
    assert_eq!(ws.held_plans(), 8);
    // Used by the deletion, and let go when it ends.
    let plan = ws.plan_delete(&id).unwrap();
    let task = ws.delete_project(&plan).unwrap();
    assert_eq!(finish(&task), TaskState::Done);
    // The ninth let the oldest go, and the deletion took its own.
    assert_eq!(ws.held_plans(), 7, "only the one used went");
    assert!(rig.at("trash/app").exists());
}

#[test]
fn closing_connecto_stops_a_clone_and_removes_its_staging_folder() {
    if !has_git() {
        return;
    }
    let rig = Rig::new("clone-shutdown");
    let port = silent_server();
    let ws = rig.start();
    let cloning = ws
        .clone_project(
            &format!("http://127.0.0.1:{port}/slow.git"),
            &rig.at("work"),
        )
        .unwrap();
    wait_for("git to start", || !entries(&rig.at("work")).is_empty());
    assert!(ws.shut_down(Duration::from_secs(10)), "a clean shutdown");
    assert_eq!(cloning.task.state(), TaskState::Cancelled);
    assert!(
        entries(&rig.at("work")).is_empty(),
        "{:?}",
        entries(&rig.at("work"))
    );
}

#[test]
fn a_protected_name_in_another_case_is_refused_before_git_writes_anything() {
    if !has_git() {
        return;
    }
    let rig = Rig::new("mixed-case-clone");
    // `~/.claude` is not there; on a case-insensitive volume `.ClAuDe` would
    // become it.
    let source = rig.bare_repo();
    let named = rig.at(".ClAuDe.git");
    fs::rename(&source, &named).unwrap();
    let ws = rig.start();
    let why = refused(&ws, &named.display().to_string(), &rig.layout.home);
    assert!(why.contains("Nothing was cloned"), "{why}");
    assert!(
        entries(&rig.layout.home).is_empty(),
        "{:?}",
        entries(&rig.layout.home)
    );
}

#[test]
fn closing_connecto_while_a_clone_is_starting_stops_it_too() {
    if !has_git() {
        return;
    }
    let rig = Rig::new("clone-starting");
    let port = silent_server();
    let ws = Arc::new(rig.start());
    // The clone pauses in its start; the shutdown comes then.
    let (admitted, wait_admitted) = std::sync::mpsc::channel::<()>();
    let admitted = std::sync::Mutex::new(admitted);
    ws.on_clone_admission(Arc::new(move || {
        let _ = admitted.lock().unwrap().send(());
        thread::sleep(Duration::from_millis(300));
    }));
    let starting = {
        let ws = Arc::clone(&ws);
        let work = rig.at("work");
        thread::spawn(move || ws.clone_project(&format!("http://127.0.0.1:{port}/slow.git"), &work))
    };
    wait_admitted.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(ws.shut_down(Duration::from_secs(10)), "a clean shutdown");
    // Once the shutdown has returned, the clone that was starting has ended
    // and left nothing behind; git never ran.
    match starting.join().unwrap() {
        Ok(cloning) => assert_eq!(cloning.task.state(), TaskState::Cancelled),
        Err(why) => assert!(why.contains("closing"), "{why}"),
    }
    assert!(
        entries(&rig.at("work")).is_empty(),
        "{:?}",
        entries(&rig.at("work"))
    );
    // And none is admitted after it.
    assert!(!refused(&ws, "https://h/x.git", &rig.at("work")).is_empty());
}

#[test]
fn a_clone_stalled_in_its_start_does_not_hold_the_shutdown_past_its_limit() {
    let rig = Rig::new("clone-stalled");
    let ws = Arc::new(rig.start());
    // Its folder is on a volume that stalls for seconds.
    ws.on_clone_admission(Arc::new(|| thread::sleep(Duration::from_secs(4))));
    let starting = {
        let ws = Arc::clone(&ws);
        let work = rig.at("work");
        thread::spawn(move || ws.clone_project("https://h/owner/stalled.git", &work))
    };
    // Registered before its disk work begins.
    thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    let clean = ws.shut_down(Duration::from_millis(500));
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(2),
        "the shutdown waited {took:?}"
    );
    assert!(!clean, "it says the clone did not finish in time");
    // When the volume answers, the clone stops before git, with nothing left.
    let cloning = starting.join().unwrap().unwrap();
    assert_eq!(finish(&cloning.task), TaskState::Cancelled);
    assert!(
        entries(&rig.at("work")).is_empty(),
        "{:?}",
        entries(&rig.at("work"))
    );
}

#[test]
fn a_clone_on_a_stalled_volume_returns_at_once_and_can_be_cancelled() {
    let rig = Rig::new("clone-pending");
    let ws = rig.start();
    // Its folder is on a volume that stalls for seconds.
    ws.on_clone_admission(Arc::new(|| thread::sleep(Duration::from_secs(3))));
    let asked = Instant::now();
    let cloning = ws
        .clone_project("https://h/owner/stalled.git", &rig.at("work"))
        .unwrap();
    let took = asked.elapsed();
    // The window gets its task at once and goes on drawing.
    assert!(took < Duration::from_millis(500), "the call took {took:?}");
    assert!(cloning.task.state().is_running());
    assert_eq!(cloning.target, rig.at("work/stalled"));
    // Cancel reaches it while the volume is stalled; it ends, nothing left.
    cloning.task.cancel();
    assert_eq!(
        cloning.task.state(),
        TaskState::Running("Cancelling…".into())
    );
    assert_eq!(finish(&cloning.task), TaskState::Cancelled);
    assert!(
        entries(&rig.at("work")).is_empty(),
        "{:?}",
        entries(&rig.at("work"))
    );
    // Released: the same clone may be asked again.
    ws.on_clone_admission(Arc::new(|| {}));
    let again = ws
        .clone_project("https://h/owner/stalled.git", &rig.at("work"))
        .unwrap();
    again.task.cancel();
    let _ = finish(&again.task);
}

#[test]
fn a_deletion_on_a_stalled_volume_returns_at_once_and_is_checked_on_the_worker() {
    let rig = Rig::new("delete-stalled");
    let folder = rig.at("work/app");
    fs::create_dir_all(&folder).unwrap();
    let ws = rig.start();
    ws.open_project(&folder).unwrap();
    let id = project_id(&folder);
    wait_for("the project", || ws.view().projects.len() == 1);
    let plan = ws.plan_delete(&id).unwrap();
    // The volume stalls once the person confirms.
    ws.on_delete_check(Arc::new(|| thread::sleep(Duration::from_secs(3))));
    let asked = Instant::now();
    let task = ws.delete_project(&plan).unwrap();
    let took = asked.elapsed();
    // The window gets its task at once and goes on drawing.
    assert!(took < Duration::from_millis(500), "the call took {took:?}");
    assert!(task.state().is_running());
    assert_eq!(ws.held_plans(), 0, "the confirmation's hold went with it");
    assert_eq!(finish(&task), TaskState::Done);
    assert!(rig.at("trash/app").exists());
    assert!(!folder.exists());

    // A refusal found by those checks comes through the task.
    let other = rig.at("work/other");
    fs::create_dir_all(&other).unwrap();
    ws.open_project(&other).unwrap();
    let other_id = project_id(&other);
    wait_for("the other project", || {
        ws.view().projects.iter().any(|p| p.id == other_id)
    });
    let plan = ws.plan_delete(&other_id).unwrap();
    fs::rename(&other, rig.at("work/other-original")).unwrap();
    fs::create_dir_all(&other).unwrap();
    ws.on_delete_check(Arc::new(|| {}));
    let task = ws.delete_project(&plan).unwrap();
    let TaskState::Failed(why) = finish(&task) else {
        panic!("a swapped folder was deleted");
    };
    assert!(why.contains("changed since"), "{why}");
    assert!(other.exists());
    assert!(
        ws.view().projects.iter().any(|p| p.id == other_id),
        "not even closed"
    );
}

#[test]
fn closing_connecto_while_a_deletion_is_being_checked_leaves_the_folder() {
    let rig = Rig::new("delete-closing");
    let folder = rig.at("work/app");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("mine.txt"), "kept").unwrap();
    let ws = rig.start();
    ws.open_project(&folder).unwrap();
    let id = project_id(&folder);
    wait_for("the project", || ws.view().projects.len() == 1);
    let plan = ws.plan_delete(&id).unwrap();
    // The check pauses (a stalled volume) until released.
    let (entered_tx, entered) = std::sync::mpsc::channel::<()>();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let hook = std::sync::Mutex::new((entered_tx, released));
    ws.on_delete_check(Arc::new(move || {
        let hook = hook.lock().unwrap();
        let _ = hook.0.send(());
        let _ = hook.1.recv();
    }));
    let task = ws.delete_project(&plan).unwrap();
    entered.recv_timeout(Duration::from_secs(10)).unwrap();
    // Connecto closes while the deletion is still being checked: the
    // shutdown does not say it is done while the deletion may still go.
    let clean = ws.shut_down(Duration::from_millis(300));
    assert!(
        !clean,
        "the shutdown said it was done with a deletion pending"
    );
    // The check ends after the shutdown began: nothing is deleted.
    release.send(()).unwrap();
    let TaskState::Failed(why) = finish(&task) else {
        panic!("deleted after the shutdown");
    };
    assert!(why.contains("closing"), "{why}");
    thread::sleep(Duration::from_millis(300));
    assert_eq!(fs::read_to_string(folder.join("mine.txt")).unwrap(), "kept");
    assert!(fs::read_dir(rig.at("trash")).unwrap().next().is_none());
}
