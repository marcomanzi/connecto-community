//! New Project (a folder, and optionally a git repository to clone into it)
//! and Delete Project (close it and move its folder to the trash), against
//! a fake workspace: nothing here runs git or touches a folder.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use connecto_core::project_files::{Cloning, DeletePlan, ProjectTask, TaskState};
use egui::{Event, Pos2};

use super::{W, two_projects};
use crate::commands::Command;
use crate::tree::Hit;

/// A clone or a deletion whose end the test decides.
pub(super) struct FakeTask {
    state: Mutex<TaskState>,
    cancels: Mutex<usize>,
}

impl FakeTask {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(TaskState::Running("Receiving objects: 40%".into())),
            cancels: Mutex::new(0),
        })
    }

    fn set(&self, state: TaskState) {
        *self.state.lock().unwrap() = state;
    }
}

impl ProjectTask for FakeTask {
    fn state(&self) -> TaskState {
        self.state.lock().unwrap().clone()
    }

    fn cancel(&self) {
        *self.cancels.lock().unwrap() += 1;
    }
}

/// What the window asked of the workspace's New Project and Delete Project.
#[derive(Default)]
pub(super) struct FakeFiles {
    clones: Mutex<Vec<(String, PathBuf)>>,
    clone_task: Mutex<Option<Arc<FakeTask>>>,
    clone_refusal: Mutex<Option<String>>,
    plans: Mutex<Vec<String>>,
    plan: Mutex<Option<Result<DeletePlan, String>>>,
    deletes: Mutex<Vec<DeletePlan>>,
    releases: Mutex<Vec<u64>>,
    /// Holds the next plan back until the test lets it go.
    plan_gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    delete_task: Mutex<Option<Arc<FakeTask>>>,
}

impl FakeFiles {
    pub(super) fn clone_project(&self, url: &str, parent: &Path) -> Result<Cloning, String> {
        self.clones
            .lock()
            .unwrap()
            .push((url.to_owned(), parent.to_owned()));
        if let Some(why) = self.clone_refusal.lock().unwrap().clone() {
            return Err(why);
        }
        let task = FakeTask::new();
        *self.clone_task.lock().unwrap() = Some(Arc::clone(&task));
        Ok(Cloning {
            target: parent.join("project"),
            task,
        })
    }

    pub(super) fn plan_delete(&self, project: &str) -> Result<DeletePlan, String> {
        let gate = self.plan_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            let _ = gate.recv_timeout(std::time::Duration::from_secs(10));
        }
        self.plans.lock().unwrap().push(project.to_owned());
        self.plan
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| Ok(plan(project)))
    }

    pub(super) fn release_delete_plan(&self, plan: &DeletePlan) {
        self.releases.lock().unwrap().push(plan.token);
    }

    pub(super) fn delete_project(&self, plan: &DeletePlan) -> Result<Arc<dyn ProjectTask>, String> {
        self.deletes.lock().unwrap().push(plan.clone());
        let task = FakeTask::new();
        *self.delete_task.lock().unwrap() = Some(Arc::clone(&task));
        Ok(task)
    }
}

fn plan(project: &str) -> DeletePlan {
    DeletePlan {
        project: project.to_owned(),
        name: "quill".to_owned(),
        path: "/work/quill".to_owned(),
        contents: "12 files in 3 folders, 4.1 KB".to_owned(),
        trash: "the Trash".to_owned(),
        warnings: vec!["2 uncommitted changes (changed or untracked files) will be lost.".into()],
        identity: vec![1, 2, 3, 4],
        token: 7,
    }
}

// ---------- helpers ----------

fn new_hits(w: &W) -> &crate::forms::Marks {
    &w.app.hits.new_project
}

fn delete_hits(w: &W) -> &crate::forms::Marks {
    &w.app.hits.delete_project
}

fn at(marks: &crate::forms::Marks, key: &str) -> Pos2 {
    let rect = marks.rects.get(key);
    assert!(
        rect.is_some(),
        "{key} was not drawn: {:?}",
        marks.rects.keys()
    );
    rect.unwrap().center()
}

fn said(marks: &crate::forms::Marks, text: &str) -> bool {
    marks.texts.iter().any(|t| t.contains(text))
}

fn frames(w: &mut W, n: usize) {
    for _ in 0..n {
        w.frame(vec![]);
    }
}

fn type_into(w: &mut W, at: Pos2, text: &str) {
    w.click(at);
    w.frame(vec![Event::Text(text.to_owned())]);
    w.frame(vec![]);
}

fn open_new_project(w: &mut W) {
    w.app.queue_for_tests(Command::NewProject);
    frames(w, 2);
    assert!(
        new_hits(w).rects.contains_key("new-project-create"),
        "the dialog is open"
    );
}

/// Ask to delete quill, and wait for its plan.
fn open_delete(w: &mut W) {
    w.app
        .queue_for_tests(Command::DeleteProjectAt("quill".into()));
    let came = |w: &W| {
        delete_hits(w).rects.contains_key("delete-confirm")
            || said(delete_hits(w), "cannot")
            || said(delete_hits(w), "will not")
    };
    for _ in 0..200 {
        w.frame(vec![]);
        if came(w) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(came(w), "the plan never came: {:?}", delete_hits(w).texts);
}

// ---------- New Project ----------

#[test]
fn the_plus_menu_offers_new_project() {
    let (_quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.click_hit(&Hit::HeaderPlus);
    w.click_hit(&Hit::PlusNewProject);
    frames(&mut w, 2);
    assert!(new_hits(&w).rects.contains_key("new-project-url"));
    assert!(ws.opened.lock().unwrap().is_empty(), "nothing opened yet");
}

#[test]
fn a_new_project_with_no_repository_opens_the_chosen_folder() {
    let (_quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    open_new_project(&mut w);
    // No folder yet: nothing to open.
    w.click(at(new_hits(&w), "new-project-create"));
    assert!(ws.opened.lock().unwrap().is_empty());
    w.click(at(new_hits(&w), "new-project-choose"));
    assert!(said(new_hits(&w), "/picked/folder"));
    w.click(at(new_hits(&w), "new-project-create"));
    frames(&mut w, 1);
    assert_eq!(
        *ws.opened.lock().unwrap(),
        [PathBuf::from("/picked/folder")]
    );
    assert!(ws.files.clones.lock().unwrap().is_empty(), "nothing cloned");
    assert!(new_hits(&w).rects.is_empty(), "the dialog closed");
}

#[test]
fn a_new_project_from_a_repository_is_cloned_then_opened() {
    let (_quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    open_new_project(&mut w);
    w.click(at(new_hits(&w), "new-project-choose"));
    let url = at(new_hits(&w), "new-project-url");
    type_into(&mut w, url, "git@github.com:owner/project.git");
    w.click(at(new_hits(&w), "new-project-create"));
    assert_eq!(
        *ws.files.clones.lock().unwrap(),
        [(
            "git@github.com:owner/project.git".to_owned(),
            PathBuf::from("/picked/folder")
        )]
    );
    frames(&mut w, 1);
    assert!(said(new_hits(&w), "Cloning into /picked/folder/project"));
    assert!(
        said(new_hits(&w), "Receiving objects: 40%"),
        "progress shows"
    );
    assert!(
        ws.opened.lock().unwrap().is_empty(),
        "not before it is done"
    );
    let task = ws.files.clone_task.lock().unwrap().clone().unwrap();
    task.set(TaskState::Done);
    frames(&mut w, 2);
    assert_eq!(
        *ws.opened.lock().unwrap(),
        [PathBuf::from("/picked/folder/project")]
    );
    assert!(new_hits(&w).rects.is_empty(), "the dialog closed");
}

#[test]
fn cancel_stops_the_clone_and_opens_nothing() {
    let (_quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    open_new_project(&mut w);
    w.click(at(new_hits(&w), "new-project-choose"));
    let url = at(new_hits(&w), "new-project-url");
    type_into(&mut w, url, "https://example.com/owner/project.git");
    w.click(at(new_hits(&w), "new-project-create"));
    frames(&mut w, 1);
    let task = ws.files.clone_task.lock().unwrap().clone().unwrap();
    w.click(at(new_hits(&w), "new-project-cancel"));
    assert_eq!(*task.cancels.lock().unwrap(), 1, "git is told to stop");
    // Open until git has stopped and its folder is gone.
    assert!(new_hits(&w).rects.contains_key("new-project-cancel"));
    task.set(TaskState::Cancelled);
    frames(&mut w, 2);
    assert!(new_hits(&w).rects.is_empty(), "the dialog closed");
    assert!(ws.opened.lock().unwrap().is_empty(), "no project");
}

#[test]
fn a_failed_clone_says_why_and_stays_open() {
    let (_quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    open_new_project(&mut w);
    w.click(at(new_hits(&w), "new-project-choose"));
    let url = at(new_hits(&w), "new-project-url");
    type_into(&mut w, url, "https://example.com/owner/private.git");
    w.click(at(new_hits(&w), "new-project-create"));
    let task = ws.files.clone_task.lock().unwrap().clone().unwrap();
    task.set(TaskState::Failed("The repository refused access.".into()));
    frames(&mut w, 2);
    assert!(said(new_hits(&w), "The repository refused access."));
    assert!(ws.opened.lock().unwrap().is_empty());
    // A refusal before git runs (a folder already there) says so too.
    *ws.files.clone_refusal.lock().unwrap() = Some("already has something named".into());
    w.click(at(new_hits(&w), "new-project-create"));
    frames(&mut w, 1);
    assert!(said(new_hits(&w), "already has something named"));
}

// ---------- Delete Project ----------

#[test]
fn delete_does_nothing_until_the_name_is_typed() {
    let (_quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    open_delete(&mut w);
    let hits = delete_hits(&w);
    assert!(said(hits, "Delete quill and its folder?"));
    assert!(said(hits, "/work/quill"), "the full path");
    assert!(said(hits, "12 files in 3 folders"), "what goes");
    assert!(said(hits, "the Trash"));
    assert!(
        said(hits, "2 uncommitted changes"),
        "unsaved work is warned of"
    );
    // Not typed: the button does nothing.
    w.click(at(delete_hits(&w), "delete-confirm"));
    let field = at(delete_hits(&w), "delete-confirm-name");
    type_into(&mut w, field, "quil");
    w.click(at(delete_hits(&w), "delete-confirm"));
    w.frame(enter_events());
    assert!(ws.files.deletes.lock().unwrap().is_empty());
    // Keep it: nothing happened.
    w.click(at(delete_hits(&w), "delete-keep"));
    frames(&mut w, 1);
    assert!(delete_hits(&w).rects.is_empty(), "the dialog closed");
    assert!(ws.files.deletes.lock().unwrap().is_empty());
    assert!(ws.closed.lock().unwrap().is_empty(), "not even closed");
}

#[test]
fn delete_after_typing_the_name_closes_and_trashes_the_project() {
    let (_quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    open_delete(&mut w);
    let field = at(delete_hits(&w), "delete-confirm-name");
    type_into(&mut w, field, "quill");
    w.click(at(delete_hits(&w), "delete-confirm"));
    // The plan the person confirmed goes back, identity and all.
    assert_eq!(*ws.files.deletes.lock().unwrap(), [plan("quill")]);
    frames(&mut w, 1);
    // It stays to say how it goes, and Esc does not hide it.
    w.frame(super::esc());
    assert!(said(delete_hits(&w), "Receiving objects"), "progress shows");
    let task = ws.files.delete_task.lock().unwrap().clone().unwrap();
    task.set(TaskState::Done);
    frames(&mut w, 2);
    assert!(delete_hits(&w).rects.is_empty(), "the dialog closed");
}

#[test]
fn a_failed_deletion_ends_with_its_reason_and_can_be_closed() {
    let (_quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    open_delete(&mut w);
    let field = at(delete_hits(&w), "delete-confirm-name");
    type_into(&mut w, field, "quill");
    w.click(at(delete_hits(&w), "delete-confirm"));
    let task = ws.files.delete_task.lock().unwrap().clone().unwrap();
    task.set(TaskState::Failed(
        "The project was closed, but its folder was not moved to the Trash.".into(),
    ));
    frames(&mut w, 2);
    assert!(said(delete_hits(&w), "was not moved to the Trash"));
    assert!(
        !delete_hits(&w).rects.contains_key("delete-confirm"),
        "nothing more to confirm"
    );
    w.click(at(delete_hits(&w), "delete-keep"));
    frames(&mut w, 1);
    assert!(delete_hits(&w).rects.is_empty(), "not stuck open");
    assert_eq!(ws.files.deletes.lock().unwrap().len(), 1, "asked once");
}

#[test]
fn a_refused_folder_says_why_and_offers_nothing_to_confirm() {
    let (_quill, _connecto, ws) = two_projects();
    *ws.files.plan.lock().unwrap() = Some(Err(
        "Connecto will not delete /Users/me: it is your home folder, or holds it.".into(),
    ));
    let mut w = W::new(&ws);
    open_delete(&mut w);
    assert!(said(delete_hits(&w), "it is your home folder"));
    assert!(!delete_hits(&w).rects.contains_key("delete-confirm"));
    w.click(at(delete_hits(&w), "delete-keep"));
    frames(&mut w, 1);
    assert!(delete_hits(&w).rects.is_empty());
    assert!(ws.files.deletes.lock().unwrap().is_empty());
}

#[test]
fn file_delete_project_asks_about_the_project_shown() {
    let (_quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.app.queue_for_tests(Command::DeleteProject);
    for _ in 0..200 {
        w.frame(vec![]);
        if !ws.files.plans.lock().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(*ws.files.plans.lock().unwrap(), ["quill"]);
}

fn enter_events() -> Vec<Event> {
    super::enter()
}

#[test]
fn a_folder_name_ending_in_a_space_can_be_typed_exactly() {
    let (_quill, _connecto, ws) = two_projects();
    let named = DeletePlan {
        name: "quill ".into(),
        ..plan("quill")
    };
    *ws.files.plan.lock().unwrap() = Some(Ok(named.clone()));
    let mut w = W::new(&ws);
    open_delete(&mut w);
    // Quoted, so the space shows.
    assert!(
        said(delete_hits(&w), "\"quill \""),
        "{:?}",
        delete_hits(&w).texts
    );
    let field = at(delete_hits(&w), "delete-confirm-name");
    type_into(&mut w, field, "quill");
    w.click(at(delete_hits(&w), "delete-confirm"));
    assert!(ws.files.deletes.lock().unwrap().is_empty(), "not the name");
    let field = at(delete_hits(&w), "delete-confirm-name");
    type_into(&mut w, field, " ");
    w.click(at(delete_hits(&w), "delete-confirm"));
    assert_eq!(*ws.files.deletes.lock().unwrap(), [named]);
}

#[test]
fn closing_the_confirmation_gives_its_plan_back() {
    let (_quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    open_delete(&mut w);
    w.click(at(delete_hits(&w), "delete-keep"));
    frames(&mut w, 1);
    assert_eq!(*ws.files.releases.lock().unwrap(), [7], "Keep it");
    open_delete(&mut w);
    w.frame(super::esc());
    frames(&mut w, 1);
    assert_eq!(*ws.files.releases.lock().unwrap(), [7, 7], "Esc");
    // Used by a deletion: not given back.
    open_delete(&mut w);
    let field = at(delete_hits(&w), "delete-confirm-name");
    type_into(&mut w, field, "quill");
    w.click(at(delete_hits(&w), "delete-confirm"));
    let task = ws.files.delete_task.lock().unwrap().clone().unwrap();
    task.set(TaskState::Done);
    frames(&mut w, 2);
    assert_eq!(ws.files.releases.lock().unwrap().len(), 2);
}

#[test]
fn a_plan_that_arrives_after_its_dialog_has_gone_is_given_back() {
    let (_quill, _connecto, ws) = two_projects();
    let (go, gate) = std::sync::mpsc::channel();
    *ws.files.plan_gate.lock().unwrap() = Some(gate);
    let mut w = W::new(&ws);
    w.app
        .queue_for_tests(Command::DeleteProjectAt("quill".into()));
    frames(&mut w, 2);
    // The plan is made and waits in the channel, unread.
    go.send(()).unwrap();
    for _ in 0..200 {
        if !ws.files.plans.lock().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    // Dismissed after its last poll found nothing: the plan is never read.
    w.app.dismiss_delete_for_tests();
    w.frame(vec![]);
    assert_eq!(
        *ws.files.releases.lock().unwrap(),
        [7],
        "the unread plan is given back"
    );
}
