#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::sync::atomic::AtomicUsize;

use super::*;

/// Replies in order; records the prompts.
struct Fake {
    projects: Vec<(String, PathBuf)>,
    paused: AtomicBool,
    replies: Mutex<Vec<String>>,
    prompts: Mutex<Vec<String>>,
    asked: AtomicUsize,
    /// How long a model call takes.
    slow: Mutex<Duration>,
    /// Pause Connecto while the model is asked.
    pause_on_ask: AtomicBool,
    /// Done once while the model is asked (the person working meanwhile).
    during_ask: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// What the CLIs offer here, and how often they were asked.
    offered: Mutex<Vec<FmModel>>,
    offered_asked: AtomicUsize,
    /// The model of each call.
    used: Mutex<Vec<FmModel>>,
}

fn claude(model: &str) -> FmModel {
    FmModel::new(connecto_core::memory::Provider::Claude, model)
}

fn codex(model: &str) -> FmModel {
    FmModel::new(connecto_core::memory::Provider::Codex, model)
}

impl FmEnv for Fake {
    fn projects(&self) -> Vec<(String, PathBuf)> {
        self.projects.clone()
    }

    fn paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    fn offered(&self, _: &str, _: &Path) -> Vec<FmModel> {
        self.offered_asked.fetch_add(1, Ordering::SeqCst);
        lock(&self.offered).clone()
    }

    fn ask(
        &self,
        _: &str,
        _: &Path,
        model: &FmModel,
        prompt: String,
        _: Duration,
        _: &AtomicBool,
    ) -> Result<String, String> {
        lock(&self.used).push(model.clone());
        self.asked.fetch_add(1, Ordering::SeqCst);
        lock(&self.prompts).push(prompt);
        let slow = *lock(&self.slow);
        thread::sleep(slow);
        if let Some(work) = lock(&self.during_ask).take() {
            work();
        }
        if self.pause_on_ask.load(Ordering::SeqCst) {
            self.paused.store(true, Ordering::SeqCst);
        }
        let mut replies = lock(&self.replies);
        if replies.is_empty() {
            return Err("no reply scripted".to_owned());
        }
        Ok(replies.remove(0))
    }
}

fn sh(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn put(dir: &Path, rel: &str, text: &str) {
    let path = dir.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn commit_all(dir: &Path, message: &str) {
    sh(dir, &["add", "-A"]);
    sh(dir, &["commit", "-q", "-m", message]);
}

fn head(dir: &Path) -> String {
    sh(dir, &["rev-parse", "HEAD"]).trim().to_owned()
}

struct Setup {
    dir: PathBuf,
    data: PathBuf,
    env: Arc<Fake>,
    host: Arc<FmHost>,
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
        let _ = fs::remove_dir_all(&self.data);
    }
}

fn setup(name: &str) -> Setup {
    setup_with(name, 1)
}

/// `projects` project ids, all on the same repository.
fn setup_with(name: &str, projects: usize) -> Setup {
    let base = std::env::temp_dir().join(format!("fm-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let dir = base.join("repo");
    fs::create_dir_all(&dir).unwrap();
    // A real path: the host compares the repository's top folder.
    let dir = dir.canonicalize().unwrap();
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["config", "user.email", "test@example.invalid"],
        &["config", "user.name", "Test"],
        &["config", "commit.gpgsign", "false"],
    ] {
        sh(&dir, args);
    }
    put(&dir, "src/main.rs", "fn main() {}\n");
    put(&dir, "notes.md", "one\n");
    commit_all(&dir, "base");
    let env = Arc::new(Fake {
        projects: (1..=projects)
            .map(|i| (format!("p{i}"), dir.clone()))
            .collect(),
        paused: AtomicBool::new(false),
        replies: Mutex::new(Vec::new()),
        prompts: Mutex::new(Vec::new()),
        asked: AtomicUsize::new(0),
        slow: Mutex::new(Duration::ZERO),
        pause_on_ask: AtomicBool::new(false),
        during_ask: Mutex::new(None),
        offered: Mutex::new(vec![claude("opus"), claude("sonnet"), codex("gpt-6.2-sol")]),
        offered_asked: AtomicUsize::new(0),
        used: Mutex::new(Vec::new()),
    });
    let data = base.join("data");
    let host = FmHost::new(
        data.clone(),
        Arc::clone(&env) as Arc<dyn FmEnv>,
        None,
        Duration::ZERO,
    );
    Setup {
        dir,
        data,
        env,
        host,
    }
}

impl Setup {
    fn reply(&self, text: &str) {
        lock(&self.env.replies).push(text.to_owned());
    }

    fn asked(&self) -> usize {
        self.env.asked.load(Ordering::SeqCst)
    }

    /// Until no run is in flight.
    fn settle(&self) {
        let until = Instant::now() + Duration::from_secs(20);
        loop {
            let busy = lock(&self.host.projects)
                .get("p1")
                .is_some_and(|p| p.running || p.manual);
            if !busy || Instant::now() > until {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Until no run is in flight, a request by hand left waiting or not.
    fn settle_running(&self) {
        let until = Instant::now() + Duration::from_secs(20);
        while lock(&self.host.projects)
            .get("p1")
            .is_some_and(|p| p.running)
            && Instant::now() < until
        {
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn tick(&self) {
        self.host.tick();
        // A run starts on its own thread; let it start, then finish.
        thread::sleep(Duration::from_millis(50));
        self.settle();
    }

    fn status(&self) -> FmStatus {
        self.host.status("p1")
    }
}

const FULL: &str = "Here are the files.\n```\n<<<FILE README.md>>>\n# Demo\nIndex.\n<<<END FILE>>>\n<<<FILE overview.md>>>\nIt greets.\n<<<END FILE>>>\n```\n";

fn build(s: &Setup) {
    s.reply(FULL);
    s.host.update_now("p1").unwrap();
    thread::sleep(Duration::from_millis(50));
    s.settle();
    assert_eq!(s.status().state, FmState::Idle, "{:?}", s.status());
}

#[test]
fn a_build_by_hand_commits_only_the_folder_and_leaves_the_rest_as_it_was() {
    let s = setup("build");
    // The person's own work: one file staged, one only modified.
    put(&s.dir, "src/main.rs", "fn main() { todo!() }\n");
    sh(&s.dir, &["add", "src/main.rs"]);
    put(&s.dir, "notes.md", "two\n");
    let base = head(&s.dir);
    build(&s);
    assert_eq!(
        sh(&s.dir, &["log", "-1", "--format=%s"]).trim(),
        format!("{MESSAGE} {}", &base[..7])
    );
    let mut files: Vec<String> = sh(&s.dir, &["show", "--name-only", "--format="])
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    files.sort();
    assert_eq!(
        files,
        [
            ".functionalMemory/README.md",
            ".functionalMemory/overview.md",
            ".functionalMemory/state.json"
        ]
    );
    assert_eq!(
        sh(&s.dir, &["status", "--porcelain"]),
        " M notes.md\nM  src/main.rs\n"
    );
    let status = s.status();
    assert_eq!(status.commit.as_deref(), Some(base.as_str()));
    assert!(status.updated_ms.is_some());
    // The prompt was a full build.
    assert!(lock(&s.env.prompts)[0].contains("build the functional memory from the code"));
    // What a peer gets: the index first, then by name, state.json last.
    let got = s.host.files("p1").unwrap();
    let names: Vec<&str> = got.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(names, ["README.md", "overview.md", "state.json"]);
    assert_eq!(got.files[0].text, "# Demo\nIndex.\n");
    assert!(!got.truncated);
}

#[test]
fn a_move_of_main_updates_from_the_diff_and_our_own_commit_does_not_start_another() {
    let s = setup("auto");
    build(&s);
    assert_eq!(s.asked(), 1);
    // First look: main is at our commit, nothing due.
    s.tick();
    assert_eq!(s.asked(), 1);
    put(&s.dir, "src/main.rs", "fn main() { greet_everyone(); }\n");
    commit_all(&s.dir, "Greet everyone");
    let moved = head(&s.dir);
    s.reply("<<<FILE overview.md>>>\nIt greets everyone.\n<<<END FILE>>>\n");
    s.tick();
    assert_eq!(s.asked(), 2, "{:?}", s.status());
    let prompt = lock(&s.env.prompts)[1].clone();
    assert!(prompt.contains("greet_everyone"), "{prompt}");
    assert!(prompt.contains("Greet everyone"));
    assert!(prompt.contains("<<<FILE overview.md>>>\nIt greets.\n<<<END FILE>>>"));
    assert_eq!(
        sh(&s.dir, &["log", "-1", "--format=%s"]).trim(),
        format!("{MESSAGE} {}", &moved[..7])
    );
    assert_eq!(
        fs::read_to_string(s.dir.join(".functionalMemory/overview.md")).unwrap(),
        "It greets everyone.\n"
    );
    assert_eq!(s.status().commit.as_deref(), Some(moved.as_str()));
    // Main moved again, to our own commit: only the folder changed, so no
    // second run (a loop of updates otherwise).
    s.tick();
    s.tick();
    assert_eq!(s.asked(), 2, "{:?}", s.status());
    assert_eq!(s.status().state, FmState::Idle);
}

#[test]
fn a_change_to_the_folder_alone_starts_nothing() {
    let s = setup("only-folder");
    build(&s);
    s.tick();
    put(&s.dir, ".functionalMemory/overview.md", "Edited by hand.\n");
    commit_all(&s.dir, "edit the memory by hand");
    s.tick();
    s.tick();
    assert_eq!(s.asked(), 1);
}

#[test]
fn an_update_waits_while_main_is_not_checked_out_or_a_merge_is_in_progress() {
    let s = setup("defer");
    build(&s);
    s.tick();
    put(&s.dir, "src/lib.rs", "pub fn x() {}\n");
    commit_all(&s.dir, "add x");
    sh(&s.dir, &["checkout", "-q", "-b", "feature"]);
    s.tick();
    assert_eq!(s.asked(), 1);
    let status = s.status();
    assert_eq!(status.state, FmState::Deferred);
    assert!(status.message.unwrap().contains("main is checked out"));
    sh(&s.dir, &["checkout", "-q", "main"]);
    let git_dir = s.dir.join(".git");
    fs::write(git_dir.join("MERGE_HEAD"), head(&s.dir)).unwrap();
    s.tick();
    assert_eq!(s.asked(), 1);
    assert_eq!(s.status().state, FmState::Deferred);
    fs::remove_file(git_dir.join("MERGE_HEAD")).unwrap();
    // Paused: waits too.
    s.env.paused.store(true, Ordering::SeqCst);
    s.tick();
    assert_eq!(s.asked(), 1);
    s.env.paused.store(false, Ordering::SeqCst);
    let described = head(&s.dir);
    s.reply("<<<NO CHANGES>>>\n");
    s.tick();
    assert_eq!(s.asked(), 2);
    assert_eq!(s.status().state, FmState::Idle);
    // No changes still moves state.json on, in a commit of its own.
    assert_eq!(s.status().commit.as_deref(), Some(described.as_str()));
    assert_eq!(
        sh(&s.dir, &["show", "--name-only", "--format="]).trim(),
        ".functionalMemory/state.json"
    );
}

#[test]
fn no_build_at_start_up_but_on_the_first_move_of_main() {
    let s = setup("first");
    s.tick();
    s.tick();
    assert_eq!(s.asked(), 0);
    assert_eq!(s.status().state, FmState::None);
    put(&s.dir, "src/lib.rs", "pub fn x() {}\n");
    commit_all(&s.dir, "add x");
    s.reply(FULL);
    s.tick();
    assert_eq!(s.asked(), 1);
    assert!(s.dir.join(".functionalMemory/README.md").exists());
}

#[test]
fn automatic_updates_can_be_turned_off() {
    let s = setup("off");
    build(&s);
    s.tick();
    s.host.set_auto("p1", false).unwrap();
    put(&s.dir, "src/lib.rs", "pub fn x() {}\n");
    commit_all(&s.dir, "add x");
    s.tick();
    assert_eq!(s.asked(), 1);
    assert_eq!(s.status().state, FmState::Off);
    assert!(!s.status().auto);
    // By hand still works.
    s.reply("<<<NO CHANGES>>>\n");
    s.host.update_now("p1").unwrap();
    thread::sleep(Duration::from_millis(50));
    s.settle();
    assert_eq!(s.asked(), 2);
}

#[test]
fn a_bad_reply_writes_nothing() {
    let s = setup("bad");
    s.reply("<<<FILE ../escape.md>>>\nx\n<<<END FILE>>>\n");
    s.host.update_now("p1").unwrap();
    thread::sleep(Duration::from_millis(50));
    s.settle();
    let status = s.status();
    assert_eq!(status.state, FmState::Failed);
    assert!(status.message.unwrap().contains("not allowed"));
    assert!(!s.dir.join(".functionalMemory").exists());
    assert!(!s.dir.parent().unwrap().join("escape.md").exists());
    assert_eq!(sh(&s.dir, &["log", "-1", "--format=%s"]).trim(), "base");
}

#[test]
fn replies_are_parsed_strictly() {
    let ok = parse_reply(
        "intro\n<<<FILE README.md>>>\n# A\n\n<<<x>>> inside is fine\n<<<END FILE>>>\n<<<DELETE old.md>>>\n",
    )
    .unwrap();
    assert_eq!(ok.write["README.md"], "# A\n\n<<<x>>> inside is fine\n");
    assert_eq!(ok.delete, ["old.md"]);
    assert!(parse_reply("<<<NO CHANGES>>>").unwrap().none);
    for bad in [
        "",
        "just prose",
        "<<<FILE README.md>>>\nno end",
        "<<<FILE a.md>>>\n<<<FILE b.md>>>\n<<<END FILE>>>",
        "<<<FILE state.json>>>\n{}\n<<<END FILE>>>",
        "<<<FILE sub/x.md>>>\nx\n<<<END FILE>>>",
        "<<<DELETE ../x.md>>>",
        "<<<FILE a.md>>>\n1\n<<<END FILE>>>\n<<<FILE a.md>>>\n2\n<<<END FILE>>>",
    ] {
        assert!(parse_reply(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn applied_changes_keep_to_the_limits() {
    let mut current = BTreeMap::new();
    current.insert("README.md".to_owned(), "i\n".to_owned());
    current.insert("old.md".to_owned(), "o\n".to_owned());
    let mut changes = Changes::default();
    changes.write.insert("new.md".into(), "n\n".into());
    changes.delete.push("old.md".into());
    let docs = apply(&current, &changes, false).unwrap();
    assert_eq!(docs.keys().collect::<Vec<_>>(), ["README.md", "new.md"]);
    // A full build replaces the folder: it must bring its index.
    assert!(apply(&current, &changes, true).is_err());
    let mut big = Changes::default();
    big.write
        .insert("README.md".into(), "x".repeat(MAX_FILE_BYTES + 1));
    assert!(apply(&current, &big, false).is_err());
    let mut many = Changes::default();
    for i in 0..MAX_DOCS {
        many.write.insert(format!("f{i}.md"), "x\n".into());
    }
    assert!(apply(&current, &many, false).is_err());
    let mut total = Changes::default();
    for i in 0..4 {
        total
            .write
            .insert(format!("f{i}.md"), "x".repeat(MAX_FILE_BYTES - 10));
    }
    assert!(apply(&current, &total, false).is_err());
}

#[test]
fn files_for_a_peer_are_capped_and_never_read_through_a_link() {
    let s = setup("files");
    assert_eq!(s.host.files("p1").unwrap(), FmFiles::default());
    put(&s.dir, ".functionalMemory/README.md", "index\n");
    put(
        &s.dir,
        ".functionalMemory/big.md",
        &"é".repeat(MAX_FILE_BYTES),
    );
    put(&s.dir, ".functionalMemory/notes.txt", "not markdown\n");
    put(&s.dir, "secret.md", "secret\n");
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        s.dir.join("secret.md"),
        s.dir.join(".functionalMemory/link.md"),
    )
    .unwrap();
    let got = s.host.files("p1").unwrap();
    let names: Vec<&str> = got.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(names, ["README.md", "big.md"]);
    assert!(got.truncated);
    assert!(got.files[1].text.len() <= MAX_FILE_BYTES);
    assert!(s.host.files("nope").is_err());
}

#[test]
fn a_link_at_any_temporary_name_is_never_written_through() {
    let s = setup("link");
    let outside = s.dir.parent().unwrap().join("outside.txt");
    fs::write(&outside, "keep\n").unwrap();
    fs::create_dir_all(s.dir.join(".functionalMemory")).unwrap();
    // The names a temporary file had before, and the file itself.
    #[cfg(unix)]
    for name in [
        ".README.md.connecto-tmp",
        ".state.json.connecto-tmp",
        "README.md",
    ] {
        std::os::unix::fs::symlink(&outside, s.dir.join(".functionalMemory").join(name)).unwrap();
    }
    // Committed: a folder with changes of the person's is never written.
    commit_all(&s.dir, "links");
    build(&s);
    assert_eq!(fs::read_to_string(&outside).unwrap(), "keep\n");
    let readme = s.dir.join(".functionalMemory/README.md");
    assert!(fs::symlink_metadata(&readme).unwrap().is_file());
    assert_eq!(fs::read_to_string(readme).unwrap(), "# Demo\nIndex.\n");
}

#[test]
fn a_commit_that_fails_leaves_nothing_staged_or_written_and_no_status_moved() {
    let s = setup("commit-fails");
    // Signing that cannot work: the commit fails after `git add`.
    sh(&s.dir, &["config", "commit.gpgsign", "true"]);
    sh(&s.dir, &["config", "gpg.program", "false"]);
    put(&s.dir, "src/main.rs", "fn main() { staged(); }\n");
    sh(&s.dir, &["add", "src/main.rs"]);
    s.reply(FULL);
    s.host.update_now("p1").unwrap();
    thread::sleep(Duration::from_millis(50));
    s.settle();
    let status = s.status();
    assert_eq!(status.state, FmState::Failed, "{status:?}");
    assert!(status.message.unwrap().contains("put back"));
    assert_eq!(status.commit, None);
    assert!(!s.dir.join(".functionalMemory").exists());
    // Only the person's own staged work is in the index.
    assert_eq!(sh(&s.dir, &["status", "--porcelain"]), "M  src/main.rs\n");
    // Later, with an update that fails on an existing memory: the files and
    // the index stay as they were.
    sh(&s.dir, &["config", "commit.gpgsign", "false"]);
    sh(&s.dir, &["reset", "-q"]);
    build(&s);
    s.tick();
    put(&s.dir, "src/lib.rs", "pub fn x() {}\n");
    commit_all(&s.dir, "add x");
    sh(&s.dir, &["config", "commit.gpgsign", "true"]);
    s.reply("<<<FILE overview.md>>>\nChanged.\n<<<END FILE>>>\n<<<FILE extra.md>>>\nNew.\n<<<END FILE>>>\n");
    s.tick();
    assert_eq!(s.status().state, FmState::Failed);
    assert_eq!(
        fs::read_to_string(s.dir.join(".functionalMemory/overview.md")).unwrap(),
        "It greets.\n"
    );
    assert!(!s.dir.join(".functionalMemory/extra.md").exists());
    assert_eq!(sh(&s.dir, &["status", "--porcelain"]), "");
}

#[test]
fn one_run_at_a_time_for_projects_sharing_a_repository() {
    let s = setup_with("shared", 2);
    *lock(&s.env.slow) = Duration::from_millis(400);
    s.reply(FULL);
    s.reply(FULL);
    s.host.update_now("p1").unwrap();
    s.host.update_now("p2").unwrap();
    // And the watcher looking at the same time.
    s.host.tick();
    thread::sleep(Duration::from_millis(100));
    assert_eq!(s.asked(), 1);
    thread::sleep(Duration::from_millis(600));
    assert_eq!(s.asked(), 1);
    assert_eq!(s.host.status("p2").state, FmState::Deferred);
    assert_eq!(
        sh(&s.dir, &["log", "--format=%s"])
            .lines()
            .filter(|l| l.starts_with(MESSAGE))
            .count(),
        1
    );
}

#[test]
fn a_request_by_hand_deferred_during_the_run_is_kept_and_runs_later() {
    let s = setup("manual-deferred");
    s.host.set_auto("p1", false).unwrap();
    s.env.pause_on_ask.store(true, Ordering::SeqCst);
    s.reply(FULL);
    s.host.update_now("p1").unwrap();
    let until = Instant::now() + Duration::from_secs(20);
    while s.asked() == 0 && Instant::now() < until {
        thread::sleep(Duration::from_millis(10));
    }
    thread::sleep(Duration::from_millis(50));
    s.settle_running();
    assert_eq!(s.status().state, FmState::Deferred);
    assert!(!s.dir.join(".functionalMemory").exists());
    s.env.pause_on_ask.store(false, Ordering::SeqCst);
    s.env.paused.store(false, Ordering::SeqCst);
    s.reply(FULL);
    s.tick();
    assert_eq!(s.asked(), 2);
    assert_eq!(s.status().state, FmState::Idle);
    assert!(s.dir.join(".functionalMemory/README.md").exists());
}

#[test]
fn the_baseline_is_mains_even_when_another_branch_is_checked_out_at_start() {
    let s = setup("baseline");
    let before_memory = head(&s.dir);
    build(&s);
    put(&s.dir, "src/lib.rs", "pub fn x() {}\n");
    commit_all(&s.dir, "add x");
    // Connecto starts with a branch from before the memory checked out.
    sh(&s.dir, &["checkout", "-q", "-b", "feature", &before_memory]);
    let host = FmHost::new(
        s.data.join("again"),
        Arc::clone(&s.env) as Arc<dyn FmEnv>,
        None,
        Duration::ZERO,
    );
    host.tick();
    assert_eq!(host.status("p1").state, FmState::Deferred);
    sh(&s.dir, &["checkout", "-q", "main"]);
    s.reply("<<<NO CHANGES>>>\n");
    host.tick();
    thread::sleep(Duration::from_millis(300));
    assert_eq!(s.asked(), 2);
}

#[test]
fn names_that_differ_only_in_case_are_refused() {
    let mut changes = Changes::default();
    changes.write.insert("README.md".into(), "a\n".into());
    changes.write.insert("readme.md".into(), "b\n".into());
    assert!(apply(&BTreeMap::new(), &changes, true).is_err());
}

/// The person's own work in the folder (staged and changed again) is never
/// written over, staged or committed: the update waits, and the index
/// blob and the bytes on disk are as they were.
#[test]
fn the_persons_own_changes_in_the_folder_are_left_exactly_as_they_were() {
    let s = setup("own-changes");
    build(&s);
    s.tick();
    put(&s.dir, ".functionalMemory/features.md", "staged version\n");
    sh(&s.dir, &["add", ".functionalMemory/features.md"]);
    put(&s.dir, ".functionalMemory/features.md", "edited again\n");
    put(&s.dir, "src/lib.rs", "pub fn x() {}\n");
    sh(&s.dir, &["add", "src/lib.rs"]);
    sh(&s.dir, &["commit", "-q", "-m", "add x", "--", "src/lib.rs"]);
    let index = sh(&s.dir, &["ls-files", "-s", "--", ".functionalMemory"]);
    s.reply("<<<FILE features.md>>>\nFrom the model.\n<<<END FILE>>>\n");
    s.tick();
    assert_eq!(s.asked(), 1);
    let status = s.status();
    assert_eq!(status.state, FmState::Deferred);
    assert!(
        status
            .message
            .unwrap()
            .contains("changes in .functionalMemory")
    );
    assert_eq!(
        sh(&s.dir, &["ls-files", "-s", "--", ".functionalMemory"]),
        index
    );
    assert_eq!(
        fs::read(s.dir.join(".functionalMemory/features.md")).unwrap(),
        b"edited again\n"
    );
}

/// A file the prompt left out (over its cap) is put back byte for byte when
/// the commit fails after a full build replaced it.
#[test]
fn a_failed_commit_puts_back_files_the_prompt_never_read() {
    let s = setup("big-file");
    let big = format!("# Big\n{}\n", "x".repeat(MAX_FILE_BYTES + 100));
    put(&s.dir, ".functionalMemory/README.md", &big);
    put(&s.dir, ".functionalMemory/other.md", "other\n");
    commit_all(&s.dir, "a memory by hand");
    sh(&s.dir, &["config", "commit.gpgsign", "true"]);
    sh(&s.dir, &["config", "gpg.program", "false"]);
    s.reply(FULL);
    s.host.update_now("p1").unwrap();
    thread::sleep(Duration::from_millis(50));
    s.settle();
    assert_eq!(s.status().state, FmState::Failed);
    assert_eq!(
        fs::read_to_string(s.dir.join(".functionalMemory/README.md")).unwrap(),
        big
    );
    assert_eq!(
        fs::read_to_string(s.dir.join(".functionalMemory/other.md")).unwrap(),
        "other\n"
    );
    assert!(!s.dir.join(".functionalMemory/overview.md").exists());
    assert!(!s.dir.join(".functionalMemory/state.json").exists());
    assert_eq!(sh(&s.dir, &["status", "--porcelain"]), "");
}

#[cfg(unix)]
#[test]
fn a_folder_that_is_a_link_is_never_read_for_a_peer() {
    let s = setup("folder-link");
    let elsewhere = s.dir.parent().unwrap().join("elsewhere");
    put(&elsewhere, "README.md", "not ours\n");
    std::os::unix::fs::symlink(&elsewhere, s.dir.join(".functionalMemory")).unwrap();
    assert_eq!(s.host.files("p1").unwrap(), FmFiles::default());
}

/// A file git ignores in the folder has no copy to put back: the update
/// waits rather than drop it.
#[test]
fn an_ignored_file_in_the_folder_is_never_removed() {
    let s = setup("ignored");
    put(&s.dir, ".gitignore", ".functionalMemory/private.md\n");
    put(&s.dir, ".functionalMemory/README.md", "# Old\n");
    commit_all(&s.dir, "memory by hand");
    put(&s.dir, ".functionalMemory/private.md", "my own notes\n");
    s.reply(FULL);
    s.host.update_now("p1").unwrap();
    // Deferred before any run: the request by hand stays asked for.
    let until = Instant::now() + Duration::from_secs(20);
    while s.status().state != FmState::Deferred && Instant::now() < until {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(s.asked(), 0);
    assert_eq!(s.status().state, FmState::Deferred, "{:?}", s.status());
    assert_eq!(
        fs::read(s.dir.join(".functionalMemory/private.md")).unwrap(),
        b"my own notes\n"
    );
}

/// A correction committed while the model works is not undone by its
/// reply: the reply is dropped and the update starts again from it.
#[test]
fn a_commit_made_while_the_model_works_is_not_undone() {
    let s = setup("moved-during");
    build(&s);
    s.tick();
    put(&s.dir, "src/lib.rs", "pub fn x() {}\n");
    commit_all(&s.dir, "add x");
    let dir = s.dir.clone();
    *lock(&s.env.during_ask) = Some(Box::new(move || {
        put(&dir, ".functionalMemory/overview.md", "Corrected.\n");
        commit_all(&dir, "correct the memory");
    }));
    s.reply("<<<FILE overview.md>>>\nFrom before the correction.\n<<<END FILE>>>\n");
    s.tick();
    assert_eq!(s.asked(), 2);
    assert_eq!(s.status().state, FmState::Deferred, "{:?}", s.status());
    assert_eq!(
        fs::read_to_string(s.dir.join(".functionalMemory/overview.md")).unwrap(),
        "Corrected.\n"
    );
    assert_eq!(
        sh(&s.dir, &["log", "-1", "--format=%s"]).trim(),
        "correct the memory"
    );
    // The next look starts again, from the corrected files.
    s.reply("<<<NO CHANGES>>>\n");
    s.tick();
    assert_eq!(s.asked(), 3);
    assert!(lock(&s.env.prompts)[2].contains("Corrected."));
    assert_eq!(s.status().state, FmState::Idle);
}

/// Updates run on Claude Sonnet by default, on the project's choice once
/// made (kept in the data folder), and on the default again when the chosen
/// CLI can no longer run here. Only what the CLIs offer can be chosen, and
/// they are asked by the watcher, not on every look.
#[test]
fn updates_run_on_the_projects_model_or_the_default() {
    let s = setup("model");
    build(&s);
    assert_eq!(*lock(&s.env.used), [claude("sonnet")]);
    let status = s.status();
    assert_eq!(status.model, Some(claude("sonnet")));
    assert_eq!(status.chosen, None);
    assert_eq!(status.models.len(), 3);
    assert!(status.models_known);
    // A model this machine's CLIs do not offer is refused.
    assert!(s.host.set_model("p1", Some(codex("gpt-9"))).is_err());
    s.host.set_model("p1", Some(codex("gpt-6.2-sol"))).unwrap();
    assert_eq!(s.status().chosen, Some(codex("gpt-6.2-sol")));
    s.reply("<<<NO CHANGES>>>\n");
    s.host.update_now("p1").unwrap();
    thread::sleep(Duration::from_millis(50));
    s.settle();
    assert_eq!(lock(&s.env.used).last(), Some(&codex("gpt-6.2-sol")));
    // Kept: a new host reads it back, with the switch.
    s.host.set_auto("p1", false).unwrap();
    let again = FmHost::new(
        s.data.clone(),
        Arc::clone(&s.env) as Arc<dyn FmEnv>,
        None,
        Duration::ZERO,
    );
    again.tick();
    let status = again.status("p1");
    assert_eq!(
        (status.model, status.chosen, status.auto),
        (
            Some(codex("gpt-6.2-sol")),
            Some(codex("gpt-6.2-sol")),
            false
        )
    );
    // Codex can no longer run here: the default, until it can again.
    *lock(&s.env.offered) = vec![claude("sonnet")];
    let later = FmHost::new(
        s.data.clone(),
        Arc::clone(&s.env) as Arc<dyn FmEnv>,
        None,
        Duration::ZERO,
    );
    later.tick();
    let status = later.status("p1");
    // The choice is still said (so it can be cleared), the default is used.
    assert_eq!(
        (status.model, status.chosen),
        (Some(claude("sonnet")), Some(codex("gpt-6.2-sol")))
    );
    // Cleared while its CLI cannot run: Codex coming back does not bring it
    // back.
    later.set_model("p1", None).unwrap();
    *lock(&s.env.offered) = vec![claude("sonnet"), codex("gpt-6.2-sol")];
    let back = FmHost::new(
        s.data.clone(),
        Arc::clone(&s.env) as Arc<dyn FmEnv>,
        None,
        Duration::ZERO,
    );
    back.tick();
    let status = back.status("p1");
    assert_eq!(
        (status.model, status.chosen),
        (Some(claude("sonnet")), None)
    );
    assert!(!status.auto, "the switch was kept through it");
    // The CLIs are asked once per ten minutes, not on every look.
    let asked = s.env.offered_asked.load(Ordering::SeqCst);
    s.tick();
    s.tick();
    assert_eq!(s.env.offered_asked.load(Ordering::SeqCst), asked);
}

/// With no CLI that can run here, an update says so and nothing is asked.
#[test]
fn no_cli_no_update() {
    let s = setup("no-cli");
    lock(&s.env.offered).clear();
    s.reply(FULL);
    s.host.update_now("p1").unwrap();
    thread::sleep(Duration::from_millis(50));
    s.settle();
    let status = s.status();
    assert_eq!(status.state, FmState::Failed);
    assert!(status.message.unwrap().contains("no CLI"));
    assert_eq!(s.asked(), 0);
    assert_eq!(status.model, None);
}

/// Two changes at once (the window turns the switch off while a peer picks
/// a model) both stay, also in the file a new host reads.
#[test]
fn changes_made_at_once_are_both_kept() {
    let s = setup("prefs-race");
    s.tick();
    for round in 0..40 {
        let data = s.data.join(format!("round-{round}"));
        let host = FmHost::new(
            data.clone(),
            Arc::clone(&s.env) as Arc<dyn FmEnv>,
            None,
            Duration::ZERO,
        );
        host.tick();
        let (a, b) = (Arc::clone(&host), Arc::clone(&host));
        let one = thread::spawn(move || a.set_auto("p1", false).unwrap());
        let two = thread::spawn(move || b.set_model("p1", Some(codex("gpt-6.2-sol"))).unwrap());
        one.join().unwrap();
        two.join().unwrap();
        let again = FmHost::new(
            data,
            Arc::clone(&s.env) as Arc<dyn FmEnv>,
            None,
            Duration::ZERO,
        );
        let status = again.status("p1");
        assert_eq!(
            (status.auto, status.chosen),
            (false, Some(codex("gpt-6.2-sol"))),
            "round {round}"
        );
    }
}

/// Before the CLIs were asked the status says it is not known; after,
/// an empty list means none can run here.
#[test]
fn whether_the_clis_were_asked_is_said() {
    let s = setup("known");
    lock(&s.env.offered).clear();
    assert!(!s.status().models_known);
    s.tick();
    let status = s.status();
    assert!(status.models_known);
    assert!(status.models.is_empty());
}
