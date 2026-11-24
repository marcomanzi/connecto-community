//! Updating Connecto itself: what the window shows and asks for. The updater
//! (`connecto-update`) does the work.

use std::sync::Arc;

/// Where the updater stands.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UpdateStatus {
    /// Nothing checked yet this run.
    #[default]
    Idle,
    Checking,
    /// The newest release is the one running.
    UpToDate,
    /// A newer release is being downloaded and checked.
    Downloading {
        version: String,
    },
    /// A newer release is downloaded and checked: it can be installed with a
    /// restart.
    Ready {
        version: String,
    },
    /// A newer release exists, but this copy cannot install it itself (why):
    /// it is downloaded from the site instead.
    Manual {
        version: String,
        why: String,
    },
    /// The last check or download failed.
    Failed(String),
    /// Updates are off in this build (no update key, an unsupported platform).
    Unavailable(String),
}

/// What the window shows.
#[derive(Debug, Clone, Default)]
pub struct UpdateView {
    /// The version running.
    pub current: String,
    pub status: UpdateStatus,
    /// Check at launch and then every minute.
    pub auto_check: bool,
    /// Install a ready update by itself as soon as nothing blocks a restart,
    /// and carry on the agent turns the restart cut.
    pub auto_install: bool,
    /// What holds back the automatic install of a ready update now (a
    /// pairing, a file transfer...), if anything.
    pub held: Option<String>,
    /// The status comes from a check the person asked for (so a result such as
    /// "up to date" is worth showing).
    pub asked: bool,
    /// Said once, at launch: e.g. that an update did not start and the
    /// previous version was put back.
    pub notice: Option<String>,
}

/// The updater, as the window drives it. Never blocks.
pub trait UpdateControl: Send + Sync {
    fn view(&self) -> Arc<UpdateView>;

    /// Check now (Connecto > Check for Updates…).
    fn check_now(&self);

    /// Check at launch and every minute, or not. Saved.
    fn set_auto_check(&self, on: bool);

    /// Install ready updates by themselves, or wait for the person. Saved.
    fn set_auto_install(&self, _on: bool) {}

    /// Hand the downloaded release over to the installer, which waits for this
    /// process to end. `Ok`: the app must now quit; it is started again on the
    /// new version, and its terminals are restored as at any start.
    ///
    /// # Errors
    /// When nothing is ready, or the hand-over cannot be started.
    fn install_and_restart(&self) -> Result<(), String>;

    /// The notice was read.
    fn dismiss_notice(&self) {}
}

/// No updater: a static shell, or tests.
#[derive(Debug, Default)]
pub struct NoUpdates;

impl UpdateControl for NoUpdates {
    fn view(&self) -> Arc<UpdateView> {
        Arc::new(UpdateView::default())
    }
    fn check_now(&self) {}
    fn set_auto_check(&self, _on: bool) {}
    fn install_and_restart(&self) -> Result<(), String> {
        Err("updates are not available".to_owned())
    }
}
