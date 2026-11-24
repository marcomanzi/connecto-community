//! Automations: Connecto watches terminals and, when a screen matches a
//! user-defined automation according to TypeSafe's Jev model, types the
//! automation's keys into that terminal.
//!
//! The UI holds an `Arc<dyn connecto_core::AutomationsControl>` (see
//! [`Automations`]); everything here runs on its own threads.

mod decide;
pub mod defaults;
mod engine;
mod groups;
mod guide;
mod history;
mod host;
pub mod jev;
pub mod keystore;
pub mod store;

use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use connecto_core::{AutomationsControl, AutomationsView, ToolKind};
use connecto_hook::automation::Agent;
use connecto_session::Wake;
use crossbeam_channel::Sender;

pub use engine::{Asker, Config, MAX_CALLS_PER_MINUTE};
pub use host::{Host, ProjectInfo, SCREEN_ROWS, TermInfo};
pub use keystore::{FileKeys, KeyStore, MemoryKeys, StoredKeys};

/// How many entries `history_for` returns at most.
const HISTORY_FOR_LIMIT: usize = 200;

/// The running engine, as the UI's handle.
pub struct Automations {
    view: Arc<ArcSwap<AutomationsView>>,
    tx: Sender<engine::Cmd>,
    /// The chosen folder, set at once by `set_root`.
    root: engine::SharedRoot,
    groups: groups::SharedGroups,
    data_root: PathBuf,
    home: PathBuf,
}

impl Automations {
    /// Start the engine for the app: the given host (the app's workspace), the secret store and Jev.
    /// `wake` is called whenever the view changes.
    pub fn start(data_root: PathBuf, host: Arc<dyn Host>, wake: Wake) -> std::io::Result<Self> {
        Self::start_with_keys(data_root, host, wake, Arc::new(StoredKeys))
    }

    /// As [`Automations::start`], with the key kept where `keys` says (a file, for
    /// a build run with a data folder of its own).
    pub fn start_with_keys(
        data_root: PathBuf,
        host: Arc<dyn Host>,
        wake: Wake,
        keys: Arc<dyn KeyStore>,
    ) -> std::io::Result<Self> {
        // `HOME`; `USERPROFILE` on Windows.
        let home = connecto_store::home_dir().ok_or_else(|| {
            std::io::Error::other(if cfg!(windows) {
                "USERPROFILE is not set"
            } else {
                "HOME is not set"
            })
        })?;
        Self::with(Config {
            seed_defaults: true,
            data_root,
            home,
            host,
            keys,
            asker: Arc::new(jev::Client::new(jev::DEFAULT_URL)),
            wake,
        })
    }

    /// Start with parts of your own (tests).
    pub fn with(config: Config) -> std::io::Result<Self> {
        let (data_root, home) = (config.data_root.clone(), config.home.clone());
        let (view, tx, root, groups) = engine::start(config)?;
        Ok(Self {
            view,
            tx,
            root,
            groups,
            data_root,
            home,
        })
    }

    fn has_root(&self) -> Result<(), String> {
        let chosen = self
            .root
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some();
        if chosen {
            Ok(())
        } else {
            Err(engine::NO_ROOT.to_owned())
        }
    }

    fn groups(&self) -> std::sync::MutexGuard<'_, groups::GroupStore> {
        self.groups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Ask the engine to show the groups as they are now. If its queue is
    /// full the next change of any kind shows them, and they are already saved.
    fn groups_changed(&self) {
        let _ = self.tx.try_send(engine::Cmd::GroupsChanged);
    }

    fn send(&self, cmd: engine::Cmd) -> Result<(), String> {
        self.tx
            .try_send(cmd)
            .map_err(|_| "busy: try again".to_owned())
    }

    /// Stop the engine's control thread. Also happens on drop.
    pub fn stop(&self) {
        let _ = self.tx.try_send(engine::Cmd::Stop);
    }
}

impl Drop for Automations {
    fn drop(&mut self) {
        self.stop();
    }
}

fn agent_of(tool: ToolKind) -> Agent {
    match tool {
        // Automations answer the screens of Claude and Codex only: there is
        // none for Pi, Antigravity or OpenCode, and a shell is never an agent.
        ToolKind::Claude
        | ToolKind::Pi
        | ToolKind::Antigravity
        | ToolKind::OpenCode
        | ToolKind::Shell => Agent::Claude,
        ToolKind::Codex => Agent::Codex,
    }
}

impl AutomationsControl for Automations {
    fn view(&self) -> Arc<AutomationsView> {
        self.view.load_full()
    }

    fn set_api_key(&self, key: &str) -> Result<(), String> {
        let key = keystore::clean_key(key)?;
        self.send(engine::Cmd::SetKey(key))
    }

    fn clear_api_key(&self) {
        let _ = self.send(engine::Cmd::ClearKey);
    }

    fn set_root(&self, path: PathBuf) -> Result<(), String> {
        let real = store::validate_root(&path, &self.data_root, &self.home)?;
        self.send(engine::Cmd::SetRoot(real.clone()))?;
        // At once, so a create right after is not refused for want of a folder.
        *self
            .root
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(real);
        Ok(())
    }

    fn open_login(&self, tool: ToolKind) -> Result<(), String> {
        self.has_root()?;
        self.send(engine::Cmd::OpenLogin(tool))
    }

    fn create(&self, name: &str, agent: ToolKind) -> Result<(), String> {
        self.has_root()?;
        let name = store::check_name(name)?;
        self.send(engine::Cmd::Create {
            name: name.to_owned(),
            agent: agent_of(agent),
        })
    }

    fn create_described(
        &self,
        name: &str,
        tool: ToolKind,
        screen: &str,
        action: &str,
    ) -> Result<(), String> {
        self.has_root()?;
        let name = store::check_name(name)?;
        if screen.trim().is_empty() {
            return Err("describe what the terminal shows".to_owned());
        }
        if action.trim().is_empty() {
            return Err("describe what should happen".to_owned());
        }
        self.send(engine::Cmd::CreateDescribed {
            name: name.to_owned(),
            agent: agent_of(tool),
            screen: screen.to_owned(),
            action: action.to_owned(),
        })
    }

    fn open_session(&self, slug: &str) -> Result<(), String> {
        self.has_root()?;
        if !store::valid_slug(slug) {
            return Err("not an automation".to_owned());
        }
        self.send(engine::Cmd::OpenSession(slug.to_owned()))
    }

    fn set_enabled(&self, slug: &str, enabled: bool) {
        let _ = self.send(engine::Cmd::SetEnabled(slug.to_owned(), enabled));
    }

    fn delete(&self, slug: &str) {
        let _ = self.send(engine::Cmd::Delete(slug.to_owned()));
    }

    fn set_threshold(&self, slug: &str, threshold: f64) {
        let _ = self.send(engine::Cmd::SetThreshold(slug.to_owned(), threshold));
    }

    fn approve(&self, pending: u64) {
        let _ = self.send(engine::Cmd::Approve(pending));
    }

    fn dismiss(&self, pending: u64) {
        let _ = self.send(engine::Cmd::Dismiss(pending));
    }

    fn history_for(&self, slug: &str) -> Vec<connecto_core::HistoryRecord> {
        if !store::valid_slug(slug) {
            return Vec::new();
        }
        let path = store::automations_dir(&self.data_root).join(store::HISTORY_FILE);
        history::read_automation(&path, slug, HISTORY_FOR_LIMIT)
    }

    fn clear_history_for(&self, slug: &str) -> Result<(), String> {
        if !store::valid_slug(slug) {
            return Err("not an automation".to_owned());
        }
        self.send(engine::Cmd::ClearHistoryFor(slug.to_owned()))
    }

    fn clear_history(&self) {
        let _ = self.send(engine::Cmd::ClearHistory);
    }

    fn create_group(&self, title: &str) -> Result<String, String> {
        let id = self.groups().create(title, &defaults::group_titles())?;
        self.groups_changed();
        Ok(id)
    }

    fn rename_group(&self, id: &str, title: &str) -> Result<(), String> {
        check_custom(id)?;
        self.groups().rename(id, title, &defaults::group_titles())?;
        self.groups_changed();
        Ok(())
    }

    fn delete_group(&self, id: &str) -> Result<(), String> {
        check_custom(id)?;
        self.groups().delete(id)?;
        self.groups_changed();
        Ok(())
    }

    fn set_group(&self, slug: &str, group: Option<&str>) -> Result<(), String> {
        if !self.view().automations.iter().any(|a| a.slug == slug) {
            return Err("no such automation".to_owned());
        }
        if let Some(id) = group {
            check_custom(id)?;
        }
        if defaults::is_member(slug) {
            return Err("this automation's group is fixed".to_owned());
        }
        self.groups().set_member(slug, group)?;
        self.groups_changed();
        Ok(())
    }
}

/// `Err` when `id` names a group that ships: those cannot be changed.
fn check_custom(id: &str) -> Result<(), String> {
    if defaults::group_ids().iter().any(|g| g == id) {
        Err("the groups that come with Connecto cannot be changed".to_owned())
    } else {
        Ok(())
    }
}
