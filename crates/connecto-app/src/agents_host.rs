//! The Agents section of Settings: every supported agent with its state, and
//! Activate / Deactivate. The work (finding the program, asking it for its
//! version, asking whether the user is signed in, installing) runs on threads of
//! its own; the window reads the rows each frame.

use std::fs;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use connecto_core::{AgentBusy, AgentRow, AgentsControl, AgentsView, AuthState, ToolKind};
use connecto_session::Wake;
use connecto_session::launch::{Layout, system_signin};
use connecto_store::{Codesign, Failure, Signature, Store, Tool};

/// A shared signature check, usable where a sized one is asked for.
struct Shared(Arc<dyn Signature + Send + Sync>);

impl Signature for Shared {
    fn verify(&self, file: &Path, team: Option<&str>) -> Result<(), String> {
        self.0.verify(file, team)
    }

    fn check(&self, file: &Path, team: Option<&str>) -> Result<(), Failure> {
        self.0.check(file, team)
    }

    /// Passed on, so a checker that does a batch in one go (Windows: one
    /// PowerShell for all of Codex's files) still does.
    fn check_all(&self, files: &[(&Path, Option<&str>)]) -> Result<(), Failure> {
        self.0.check_all(files)
    }

    fn checks_together(&self) -> bool {
        self.0.checks_together()
    }
}

use crate::agent_install::{AgentInstaller, source_of, version_of_install};
use crate::platform::THIS_COMPUTER;
use crate::setup::{AGENTS, runnable_entry};

/// Agents switched off in Settings, comma separated, in the data folder.
pub const INACTIVE_FILE: &str = ".agents-off";

/// The agents switched off in Settings.
#[must_use]
pub fn inactive_agents(data_root: &Path) -> Vec<Tool> {
    let Ok(text) = fs::read_to_string(data_root.join(INACTIVE_FILE)) else {
        return Vec::new();
    };
    AGENTS
        .into_iter()
        .filter(|tool| text.split(',').any(|word| word.trim() == tool.name()))
        .collect()
}

fn write_inactive(data_root: &Path, list: &[Tool]) {
    let names: Vec<&str> = list.iter().map(|t| t.name()).collect();
    let _ = fs::create_dir_all(data_root);
    let _ = fs::write(data_root.join(INACTIVE_FILE), names.join(","));
}

fn kind_of(tool: Tool) -> ToolKind {
    match tool {
        Tool::Claude => ToolKind::Claude,
        Tool::Codex => ToolKind::Codex,
        Tool::Shell => ToolKind::Shell,
    }
}

fn tool_of(kind: ToolKind) -> Option<Tool> {
    match kind {
        ToolKind::Claude => Some(Tool::Claude),
        ToolKind::Codex => Some(Tool::Codex),
        ToolKind::Shell => None,
    }
}

fn name_of(tool: Tool) -> &'static str {
    source_of(tool).map_or("Shell", |s| s.name)
}

pub struct AgentsHost {
    layout: Layout,
    installer: Arc<dyn AgentInstaller>,
    signature: Arc<dyn Signature + Send + Sync>,
    rows: Mutex<Vec<AgentRow>>,
    wake: Wake,
}

impl AgentsHost {
    /// Rows at once (the program is looked for, nothing is run); the version
    /// and sign-in follow from a thread.
    pub fn start(
        data_root: &Path,
        home: &Path,
        installer: Arc<dyn AgentInstaller>,
        wake: Wake,
    ) -> Arc<Self> {
        Self::start_with(data_root, home, installer, Arc::new(Codesign), wake)
    }

    pub fn start_with(
        data_root: &Path,
        home: &Path,
        installer: Arc<dyn AgentInstaller>,
        signature: Arc<dyn Signature + Send + Sync>,
        wake: Wake,
    ) -> Arc<Self> {
        let layout = Layout {
            data_root: data_root.to_owned(),
            home: home.to_owned(),
        };
        let off = inactive_agents(data_root);
        let rows = AGENTS
            .into_iter()
            .map(|tool| AgentRow {
                tool: kind_of(tool),
                name: name_of(tool).to_owned(),
                source: source_of(tool).map_or_else(String::new, |s| {
                    format!("{}'s installer at {}", s.vendor, s.host)
                }),
                installed: Store::find_installed(tool, home).is_some(),
                version: None,
                signed_in: None,
                active: !off.contains(&tool),
                busy: Some(AgentBusy::Checking),
                error: None,
                signin_requested: false,
            })
            .collect();
        let host = Arc::new(Self {
            layout,
            installer,
            signature,
            rows: Mutex::new(rows),
            wake,
        });
        host.refresh_in_background();
        host
    }

    fn lock(&self) -> MutexGuard<'_, Vec<AgentRow>> {
        self.rows.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn change(&self, tool: Tool, edit: impl FnOnce(&mut AgentRow)) {
        if let Some(row) = self.lock().iter_mut().find(|r| r.tool == kind_of(tool)) {
            edit(row);
        }
        (self.wake)();
    }

    fn refresh_in_background(self: &Arc<Self>) {
        let me = Arc::clone(self);
        if let Err(err) = std::thread::Builder::new()
            .name("agents-check".to_owned())
            .spawn(move || {
                for tool in AGENTS {
                    me.look(tool);
                }
            })
        {
            eprintln!("the agents check did not start: {err}");
        }
    }

    /// Find the program, its version, and whether the user is signed in. The
    /// verified stored copy is asked, as setup asks it (each check verifies it
    /// in full, as each launch does); the user's own install only when there is
    /// no usable stored copy (a Node launcher cannot run from here).
    fn look(&self, tool: Tool) {
        let (found, problem) = runnable_entry(&self.layout.data_root, &self.layout.home, tool);
        let version = found
            .as_deref()
            .and_then(|path| version_of_install(tool, path, &self.layout.home));
        let signed_in = found
            .as_deref()
            // The program itself (an npm Codex's `codex.cmd` is not run).
            .and_then(|path| crate::platform::runnable(tool, path))
            .and_then(|path| match system_signin(&self.layout, tool, &path) {
                AuthState::LoggedIn { .. } => Some(true),
                AuthState::LoggedOut => Some(false),
                AuthState::Unknown => None,
            });
        let off = inactive_agents(&self.layout.data_root);
        self.change(tool, |row| {
            row.installed = found.is_some();
            row.version = version;
            row.signed_in = signed_in;
            // Said, not silently replaced by the user's own install.
            row.error = problem;
            row.active = !off.contains(&tool);
            if matches!(row.busy, Some(AgentBusy::Checking)) {
                row.busy = None;
            }
        });
    }

    fn install_and_set_up(&self, tool: Tool) {
        let name = name_of(tool);
        let mut installed_now = false;
        let unusable = Store::unusable_installs(tool, &self.layout.home);
        if Store::find_installed(tool, &self.layout.home).is_none() && !unusable.is_empty() {
            let found = unusable
                .iter()
                .map(|(path, why)| format!("{}: {why}", path.display()))
                .collect::<Vec<_>>()
                .join("; ");
            self.change(tool, |row| {
                row.busy = None;
                row.error = Some(format!(
                    "{name} is on {THIS_COMPUTER} but Connecto cannot use this copy ({found}). Install it from its official page, or fix that install."
                ));
            });
            return;
        }
        if Store::find_installed(tool, &self.layout.home).is_none() {
            let say = |line: &str| {
                let line = line.to_owned();
                self.change(tool, |row| row.busy = Some(AgentBusy::Installing(line)));
            };
            match self.installer.install(tool, &self.layout.home, &say) {
                Ok(_) => installed_now = true,
                Err(failure) => {
                    self.change(tool, |row| {
                        row.busy = None;
                        row.error = Some(format!("{} {}", failure.what, failure.next));
                    });
                    return;
                }
            }
        }
        // Made ready for the isolated sessions: a verified copy in Connecto's store.
        if Store::find_installed(tool, &self.layout.home).is_some() {
            let store = Store::new(&self.layout.data_root);
            if let Err(err) = store.provision_found(
                tool,
                &self.layout.home,
                &Shared(Arc::clone(&self.signature)),
            ) && store
                .newest(tool)
                .ok()
                .and_then(|v| store.verify(tool, &v).ok())
                .is_none()
            {
                self.change(tool, |row| {
                    row.busy = None;
                    row.error = Some(format!(
                        "{name} could not be made ready ({err}). Try again, or reinstall it from its official page."
                    ));
                });
                return;
            }
        }
        crate::setup::forget_skipped(&self.layout.data_root, tool);
        let mut off = inactive_agents(&self.layout.data_root);
        off.retain(|t| *t != tool);
        write_inactive(&self.layout.data_root, &off);
        self.look(tool);
        self.change(tool, |row| {
            row.busy = None;
            row.error = None;
            // After an install, or when not signed in, the person goes on to sign in.
            row.signin_requested = installed_now || row.signed_in != Some(true);
        });
    }
}

/// What the window holds.
pub struct Handle(pub Arc<AgentsHost>);

impl AgentsControl for Handle {
    fn view(&self) -> Arc<AgentsView> {
        Arc::new(AgentsView {
            rows: self.0.lock().clone(),
        })
    }

    fn activate(&self, kind: ToolKind) {
        let Some(tool) = tool_of(kind) else {
            return;
        };
        {
            let mut rows = self.0.lock();
            let Some(row) = rows.iter_mut().find(|r| r.tool == kind) else {
                return;
            };
            if row.busy.is_some() {
                return;
            }
            row.busy = Some(AgentBusy::Installing("Starting…".to_owned()));
            row.error = None;
        }
        (self.0.wake)();
        let host = Arc::clone(&self.0);
        if let Err(err) = std::thread::Builder::new()
            .name("agents-activate".to_owned())
            .spawn(move || host.install_and_set_up(tool))
        {
            self.0.change(tool, |row| {
                row.busy = None;
                row.error = Some(format!("Could not start: {err}"));
            });
        }
    }

    fn deactivate(&self, kind: ToolKind) {
        let Some(tool) = tool_of(kind) else {
            return;
        };
        let mut off = inactive_agents(&self.0.layout.data_root);
        if !off.contains(&tool) {
            off.push(tool);
        }
        write_inactive(&self.0.layout.data_root, &off);
        self.0.change(tool, |row| row.active = false);
    }

    fn refresh(&self) {
        self.0.refresh_in_background();
    }

    fn signin_started(&self, kind: ToolKind) {
        if let Some(tool) = tool_of(kind) {
            self.0.change(tool, |row| row.signin_requested = false);
        }
    }

    fn inactive(&self) -> Vec<ToolKind> {
        inactive_agents(&self.0.layout.data_root)
            .into_iter()
            .map(kind_of)
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::agent_install::{Failure, Installed};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    struct AnySignature;
    impl Signature for AnySignature {
        fn verify(&self, _file: &Path, _team: Option<&str>) -> Result<(), String> {
            Ok(())
        }
    }

    /// Installs a fake Claude into the home it is given, or fails when told to.
    struct Fake {
        offline: AtomicBool,
        calls: Mutex<Vec<Tool>>,
    }

    impl AgentInstaller for Fake {
        fn install(
            &self,
            tool: Tool,
            home: &Path,
            say: &dyn Fn(&str),
        ) -> Result<Installed, Failure> {
            self.calls.lock().unwrap().push(tool);
            say("Downloading the installer…");
            if self.offline.load(Ordering::SeqCst) {
                return Err(Failure {
                    what: "Connecto could not reach claude.ai. Is this Mac online?".to_owned(),
                    next: "Check your internet connection, then try again.".to_owned(),
                    details: String::new(),
                });
            }
            // `claude.exe` on Windows, where discovery looks for that name.
            let path = home.join(".local/bin").join(format!(
                "{}{}",
                tool.name(),
                std::env::consts::EXE_SUFFIX
            ));
            match tool {
                // Claude is taken only as a native program, so its fake is one.
                Tool::Claude => crate::test_support::link_native("4.5.6 (Fake)", &path),
                _ => crate::test_support::link_fake("#!/bin/sh\necho '4.5.6 (Fake)'\n", &path),
            }
            Ok(Installed {
                path,
                version: "4.5.6 (Fake)".to_owned(),
            })
        }
    }

    fn setup(name: &str, offline: bool) -> (PathBuf, Arc<Fake>, Handle) {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root = fs::canonicalize({
            let dir = std::env::temp_dir()
                .join(format!("connecto-agents-{name}-{}-{n}", std::process::id()));
            fs::create_dir_all(dir.join("home")).unwrap();
            fs::create_dir_all(dir.join("data")).unwrap();
            dir
        })
        .unwrap();
        let fake = Arc::new(Fake {
            offline: AtomicBool::new(offline),
            calls: Mutex::default(),
        });
        let host = AgentsHost::start_with(
            &root.join("data"),
            &root.join("home"),
            Arc::clone(&fake) as Arc<dyn AgentInstaller>,
            Arc::new(AnySignature),
            Arc::new(|| {}),
        );
        (root, fake, Handle(host))
    }

    /// Waits for the rows to reach `done`. Returns as soon as they do; the limit
    /// is generous, because a loaded machine (a parallel test run) is slow.
    fn wait(handle: &Handle, done: impl Fn(&AgentsView) -> bool) -> Arc<AgentsView> {
        let deadline = Instant::now() + crate::test_support::scaled(Duration::from_secs(60));
        loop {
            let view = handle.view();
            if done(&view) {
                return view;
            }
            assert!(Instant::now() < deadline, "waiting: {:?}", handle.view());
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn row(view: &AgentsView, tool: ToolKind) -> AgentRow {
        view.rows.iter().find(|r| r.tool == tool).unwrap().clone()
    }

    #[test]
    fn every_supported_agent_is_listed_with_its_state_even_when_none_is_installed() {
        let (_root, fake, handle) = setup("list", false);
        let view = wait(&handle, |v| v.rows.iter().all(|r| r.busy.is_none()));
        assert_eq!(view.rows.len(), 2);
        let claude = row(&view, ToolKind::Claude);
        assert_eq!(claude.name, "Claude Code");
        assert!(!claude.installed && claude.version.is_none() && claude.active);
        assert!(claude.source.contains("claude.ai"));
        assert!(
            fake.calls.lock().unwrap().is_empty(),
            "looking installs nothing"
        );
    }

    // Installs a shell-script agent: Unix only.
    #[cfg(unix)]
    #[test]
    fn activating_a_missing_agent_installs_it_shows_its_version_and_asks_for_sign_in_once() {
        let (root, fake, handle) = setup("activate", false);
        wait(&handle, |v| v.rows.iter().all(|r| r.busy.is_none()));
        handle.activate(ToolKind::Claude);
        let view = wait(&handle, |v| {
            let r = row(v, ToolKind::Claude);
            r.installed && r.busy.is_none()
        });
        let claude = row(&view, ToolKind::Claude);
        assert_eq!(claude.version.as_deref(), Some("4.5.6 (Fake)"));
        assert!(claude.active && claude.error.is_none());
        assert!(claude.signin_requested, "the person goes on to sign in");
        assert_eq!(*fake.calls.lock().unwrap(), [Tool::Claude]);
        assert!(root.join("home/.local/bin/claude").is_file());
        // Acknowledged, it is not asked again.
        handle.signin_started(ToolKind::Claude);
        assert!(!row(&handle.view(), ToolKind::Claude).signin_requested);
        // Activating what is already installed installs nothing.
        handle.activate(ToolKind::Claude);
        wait(&handle, |v| row(v, ToolKind::Claude).busy.is_none());
        assert_eq!(fake.calls.lock().unwrap().len(), 1);
    }

    // Installs a shell-script agent: Unix only.
    #[cfg(unix)]
    #[test]
    fn an_install_that_fails_says_why_in_plain_words_and_can_be_tried_again() {
        let (_root, fake, handle) = setup("fail", true);
        wait(&handle, |v| v.rows.iter().all(|r| r.busy.is_none()));
        handle.activate(ToolKind::Claude);
        let view = wait(&handle, |v| row(v, ToolKind::Claude).error.is_some());
        let claude = row(&view, ToolKind::Claude);
        assert!(claude.error.unwrap().contains("Is this Mac online?"));
        assert!(!claude.installed && claude.busy.is_none() && !claude.signin_requested);
        fake.offline.store(false, Ordering::SeqCst);
        handle.activate(ToolKind::Claude);
        let view = wait(&handle, |v| row(v, ToolKind::Claude).installed);
        assert!(row(&view, ToolKind::Claude).error.is_none());
    }

    // Installs a shell-script agent: Unix only.
    #[cfg(unix)]
    #[test]
    fn deactivating_hides_the_agent_and_removes_nothing_and_activating_brings_it_back() {
        let (root, _fake, handle) = setup("hide", false);
        wait(&handle, |v| v.rows.iter().all(|r| r.busy.is_none()));
        handle.activate(ToolKind::Claude);
        wait(&handle, |v| {
            row(v, ToolKind::Claude).installed && row(v, ToolKind::Claude).busy.is_none()
        });
        handle.deactivate(ToolKind::Claude);
        assert!(!row(&handle.view(), ToolKind::Claude).active);
        assert_eq!(handle.inactive(), [ToolKind::Claude]);
        assert!(
            root.join("home/.local/bin/claude").is_file(),
            "nothing is uninstalled"
        );
        assert_eq!(inactive_agents(&root.join("data")), [Tool::Claude]);
        // The choice outlasts a restart.
        let again = AgentsHost::start_with(
            &root.join("data"),
            &root.join("home"),
            Arc::new(Fake {
                offline: AtomicBool::new(false),
                calls: Mutex::default(),
            }),
            Arc::new(AnySignature),
            Arc::new(|| {}),
        );
        assert!(
            !Handle(again)
                .view()
                .rows
                .iter()
                .find(|r| r.tool == ToolKind::Claude)
                .unwrap()
                .active
        );
        handle.activate(ToolKind::Claude);
        wait(&handle, |v| {
            row(v, ToolKind::Claude).active && row(v, ToolKind::Claude).busy.is_none()
        });
        assert!(handle.inactive().is_empty());
    }

    /// A stand-in Codex: signed in, version 2.0.0.
    #[cfg(unix)]
    const CODEX_SCRIPT: &str = r#"#!/bin/sh
case "$*" in
  "login status") echo "Logged in using ChatGPT" ;;
  *) echo "codex-cli 2.0.0" ;;
esac
"#;

    #[cfg(unix)]
    fn write_script(path: &Path, text: &str) {
        crate::test_support::link_fake(text, path);
    }

    /// Claude Code as a native program, signed out, reporting version 9.9.9.
    fn write_native_claude(path: &Path) {
        crate::test_support::link_native_source(
            r#"fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.join(" ") == "auth status" {
        println!("{{\"loggedIn\":false,\"authMethod\":\"none\"}}");
    } else {
        println!("9.9.9 (Claude Code)");
    }
}
"#,
            path,
        );
    }

    /// Codex as `npm i -g @openai/codex` lays it out: `~/.npm-global/bin/codex`
    /// is a `#!/usr/bin/env node` launcher, and the native release is in the
    /// platform package beside it. The launcher's interpreter is not on the
    /// test's `PATH`, as for a Finder-started app. Gives the native entry.
    #[cfg(unix)]
    fn npm_codex(home: &Path) -> PathBuf {
        let (triple, platform) = if cfg!(target_arch = "x86_64") {
            ("x86_64-apple-darwin", "codex-darwin-x64")
        } else {
            ("aarch64-apple-darwin", "codex-darwin-arm64")
        };
        let package = home.join(".npm-global/lib/node_modules/@openai/codex");
        let launcher = package.join("bin/codex.js");
        write_script(&launcher, "#!/usr/bin/env connecto-no-such-node\n");
        fs::write(package.join("package.json"), r#"{"name":"@openai/codex"}"#).unwrap();
        let release = package
            .join("node_modules/@openai")
            .join(platform)
            .join("vendor")
            .join(triple);
        for tree in ["codex-path", "codex-resources"] {
            fs::create_dir_all(release.join(tree)).unwrap();
            fs::write(release.join(tree).join("data"), "x").unwrap();
        }
        fs::write(release.join("codex-package.json"), "{}").unwrap();
        let entry = release.join("bin/codex");
        write_script(&entry, CODEX_SCRIPT);
        let link = home.join(".npm-global/bin/codex");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&launcher, link).unwrap();
        entry
    }

    fn fresh_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "connecto-agents-check-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        fs::create_dir_all(root.join("home")).unwrap();
        fs::create_dir_all(root.join("data")).unwrap();
        fs::canonicalize(root).unwrap()
    }

    /// Starts the host on `root` and waits until every row has been checked.
    fn checked_host(root: &Path) -> Arc<AgentsView> {
        let fake = Arc::new(Fake {
            offline: AtomicBool::new(true),
            calls: Mutex::default(),
        });
        let host = AgentsHost::start_with(
            &root.join("data"),
            &root.join("home"),
            fake as Arc<dyn AgentInstaller>,
            Arc::new(AnySignature),
            Arc::new(|| {}),
        );
        wait(&Handle(host), |v| v.rows.iter().all(|r| r.busy.is_none()))
    }

    // A shell-script Codex in a macOS npm layout: Unix only.
    #[cfg(unix)]
    #[test]
    fn a_verified_stored_copy_is_checked_and_the_discovered_launcher_is_not() {
        let root = fresh_root("stored");
        let native = npm_codex(&root.join("home"));
        // What setup keeps: a verified copy of the same release.
        Store::new(&root.join("data"))
            .provision(Tool::Codex, &native, &AnySignature)
            .unwrap();
        let view = checked_host(&root);
        let codex = row(&view, ToolKind::Codex);
        assert!(codex.installed);
        assert_eq!(codex.version.as_deref(), Some("codex-cli 2.0.0"));
        assert_eq!(codex.signed_in, Some(true));
    }

    #[test]
    fn without_a_stored_copy_the_users_own_install_is_checked() {
        let root = fresh_root("own-install");
        write_native_claude(&root.join(format!(
            "home/.local/bin/claude{}",
            std::env::consts::EXE_SUFFIX
        )));
        let view = checked_host(&root);
        let claude = row(&view, ToolKind::Claude);
        assert!(claude.installed);
        assert_eq!(claude.version.as_deref(), Some("9.9.9 (Claude Code)"));
        assert_eq!(claude.signed_in, Some(false));
    }

    // Shell scripts and Unix modes: Unix only.
    #[cfg(unix)]
    #[test]
    fn a_tampered_stored_copy_is_not_run_is_marked_for_repair_and_is_said() {
        let root = fresh_root("tampered");
        write_script(
            &root.join("release/claude"),
            r#"#!/bin/sh
case "$*" in
  "auth status") echo '{"loggedIn":true,"authMethod":"claude.ai"}' ;;
  *) echo "2.0.0 (Claude Code)" ;;
esac
"#,
        );
        let store = Store::new(&root.join("data"));
        store
            .provision(Tool::Claude, &root.join("release/claude"), &AnySignature)
            .unwrap();
        let version = store.newest(Tool::Claude).unwrap();
        let entry = store.verify(Tool::Claude, &version).unwrap().entry;
        // Changed after it was checked in.
        fs::set_permissions(&entry, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(&entry, "#!/bin/sh\necho 'changed'\n").unwrap();
        write_native_claude(&root.join("home/.local/bin/claude"));
        let view = checked_host(&root);
        let claude = row(&view, ToolKind::Claude);
        assert_eq!(
            claude.version.as_deref(),
            Some("9.9.9 (Claude Code)"),
            "the changed copy is not run; the user's own install is"
        );
        assert!(
            claude
                .error
                .as_deref()
                .is_some_and(|e| e.contains("did not pass its check")),
            "said to the person: {:?}",
            claude.error
        );
        assert!(
            Store::new(&root.join("data")).take_repair(),
            "marked for repair at the next start"
        );
    }

    // A macOS npm layout with a `#!` launcher: Unix only.
    #[cfg(unix)]
    #[test]
    fn an_npm_launcher_that_cannot_run_is_installed_but_claims_no_sign_in_state() {
        let root = fresh_root("launcher-only");
        npm_codex(&root.join("home"));
        let view = checked_host(&root);
        let codex = row(&view, ToolKind::Codex);
        assert!(codex.installed, "the launcher is the user's install");
        assert_eq!(codex.version, None);
        assert_eq!(codex.signed_in, None, "unknown, not signed out");
    }

    // Windows (M3): activating a missing agent runs its installer, as on
    // macOS; a failure is said in plain words.
    #[cfg(windows)]
    #[test]
    fn on_windows_activating_a_missing_agent_runs_its_installer() {
        let (_root, fake, handle) = setup("windows", true);
        wait(&handle, |v| v.rows.iter().all(|r| r.busy.is_none()));
        handle.activate(ToolKind::Codex);
        let view = wait(&handle, |v| row(v, ToolKind::Codex).error.is_some());
        let codex = row(&view, ToolKind::Codex);
        assert!(codex.error.unwrap().contains("online?"));
        assert!(codex.busy.is_none() && !codex.signin_requested);
        assert_eq!(*fake.calls.lock().unwrap(), [Tool::Codex]);
        assert!(codex.source.contains("chatgpt.com"), "{}", codex.source);
    }
}
