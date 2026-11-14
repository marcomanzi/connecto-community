//! The updater as the app runs it: a background thread that checks the feed
//! at launch and daily (or when asked), downloads and checks a newer release,
//! and hands it over to the installer when the person says so.

use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use connecto_core::{UpdateControl, UpdateStatus, UpdateView};
use ed25519_dalek::VerifyingKey;

use crate::feed::{self, Platform};
use crate::helper::{self, MacHandOver, WindowsHandOver};
use crate::install;
use crate::state::{self, HAND_OVER_WAIT_SECS, Pending, State};
use crate::version::Version;

/// The first automatic check waits this long, so a launch is not slowed.
const FIRST_CHECK_AFTER: Duration = Duration::from_secs(20);
/// How often the thread looks whether a daily check is due.
const LOOK_EVERY: Duration = Duration::from_secs(60 * 60);

/// What the updater needs from the app.
pub struct Config {
    pub data_dir: PathBuf,
    pub current: Version,
    pub feed_url: String,
    /// `None`: this build has no update key, and updates are off.
    pub key: Option<VerifyingKey>,
    /// The program running now.
    pub exe: PathBuf,
    /// Called whenever the view changes.
    pub wake: Arc<dyn Fn() + Send + Sync>,
    /// Said once, at launch (see [`crate::start_of_app`]).
    pub notice: Option<String>,
}

struct Ready {
    version: Version,
    staged: PathBuf,
}

/// The running updater.
pub struct Updater {
    cfg: Config,
    view: ArcSwap<UpdateView>,
    tx: Mutex<Option<Sender<bool>>>,
    ready: Mutex<Option<Ready>>,
    state: Mutex<State>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Updater {
    /// Start the background thread.
    #[must_use]
    pub fn start(cfg: Config) -> Arc<Self> {
        let state = State::load(&cfg.data_dir);
        let view = UpdateView {
            current: cfg.current.to_string(),
            status: UpdateStatus::Idle,
            auto_check: state.auto_check,
            asked: false,
            notice: cfg.notice.clone(),
        };
        let updater = Arc::new(Self {
            cfg,
            view: ArcSwap::from_pointee(view),
            tx: Mutex::new(None),
            ready: Mutex::new(None),
            state: Mutex::new(state),
        });
        let (tx, rx) = mpsc::channel::<bool>();
        *lock(&updater.tx) = Some(tx);
        let worker = Arc::clone(&updater);
        let spawned = std::thread::Builder::new()
            .name("updater".into())
            .spawn(move || {
                let mut wait = FIRST_CHECK_AFTER;
                loop {
                    match rx.recv_timeout(wait) {
                        Ok(asked) => worker.check(asked),
                        Err(RecvTimeoutError::Timeout) => {
                            if lock(&worker.state).check_due(now_ms()) {
                                worker.check(false);
                            }
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                    wait = LOOK_EVERY;
                }
            });
        if spawned.is_err() {
            updater.set_status(
                UpdateStatus::Unavailable("the updater could not start".to_owned()),
                false,
            );
        }
        updater
    }

    fn set_status(&self, status: UpdateStatus, asked: bool) {
        let mut view = (**self.view.load()).clone();
        view.status = status;
        view.asked = asked;
        self.view.store(Arc::new(view));
        (self.cfg.wake)();
    }

    fn updates_dir(&self) -> PathBuf {
        state::dir(&self.cfg.data_dir)
    }

    /// Why this copy cannot install an update itself, if it cannot; with the
    /// macOS team that must sign the new one.
    fn install_problem(&self) -> Result<Option<String>, String> {
        if cfg!(target_os = "macos") {
            let Some(bundle) = install::bundle_of(&self.cfg.exe) else {
                return Err(
                    "Connecto is not running from an app bundle, so it cannot update itself."
                        .to_owned(),
                );
            };
            if let Some(why) = install::bundle_problem(&bundle) {
                return Err(why);
            }
            return match install::team_id(&bundle) {
                Some(team) => Ok(Some(team)),
                None => Err("This copy of Connecto is not signed by its developer, so it cannot update itself.".to_owned()),
            };
        }
        Ok(None)
    }

    fn check(&self, asked: bool) {
        let Some(key) = self.cfg.key else {
            self.set_status(
                UpdateStatus::Unavailable(
                    "This build has no update key; new versions are on connecto.studio.".to_owned(),
                ),
                asked,
            );
            return;
        };
        let Some(platform) = Platform::current() else {
            self.set_status(
                UpdateStatus::Unavailable("Updates are not offered on this platform.".to_owned()),
                asked,
            );
            return;
        };
        if let Some(ready) = lock(&self.ready).as_ref() {
            // Already downloaded: say so again rather than download again.
            self.set_status(
                UpdateStatus::Ready {
                    version: ready.version.to_string(),
                },
                asked,
            );
            return;
        }
        self.set_status(UpdateStatus::Checking, asked);
        let ua = install::user_agent(&self.cfg.current.to_string());
        let result =
            install::fetch(&self.cfg.feed_url, &ua).and_then(|bytes| feed::verify(&bytes, &key));
        {
            let mut state = lock(&self.state);
            state.last_check_ms = Some(now_ms());
            let _ = state.save(&self.cfg.data_dir);
        }
        let latest = match result {
            Ok(latest) => latest,
            Err(why) => {
                self.set_status(UpdateStatus::Failed(why), asked);
                return;
            }
        };
        let offered = lock(&self.state).worth_offering(latest.version, self.cfg.current);
        let Some(artifact) = latest.artifact(platform).filter(|_| offered) else {
            self.set_status(UpdateStatus::UpToDate, asked);
            return;
        };
        let version = latest.version.to_string();
        let team = match self.install_problem() {
            Ok(team) => team,
            Err(why) => {
                self.set_status(UpdateStatus::Manual { version, why }, asked);
                return;
            }
        };
        self.set_status(
            UpdateStatus::Downloading {
                version: version.clone(),
            },
            asked,
        );
        match self.fetch_release(platform, &latest.version, artifact, team.as_deref(), &ua) {
            Ok(staged) => {
                *lock(&self.ready) = Some(Ready {
                    version: latest.version,
                    staged,
                });
                self.set_status(UpdateStatus::Ready { version }, asked);
            }
            Err(why) => self.set_status(UpdateStatus::Failed(why), asked),
        }
    }

    /// Download, check and unpack a release; the folder or bundle to put in
    /// place.
    fn fetch_release(
        &self,
        platform: Platform,
        version: &Version,
        artifact: &feed::Artifact,
        team: Option<&str>,
        ua: &str,
    ) -> Result<PathBuf, String> {
        let dir = self.updates_dir().join(version.to_string());
        // A fresh start for this version: an earlier attempt may have stopped
        // halfway. The folder is the updater's own.
        if dir.exists() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        let archive = install::download(artifact, &dir, &platform.archive_name(version), ua)?;
        let unpacked = dir.join("unpacked");
        install::unpack(&archive, &unpacked)?;
        let _ = std::fs::remove_file(&archive);
        match platform {
            Platform::MacAarch64 | Platform::MacX86_64 => {
                let app = unpacked.join("Connecto.app");
                install::check_mac_bundle(&app, team.unwrap_or_default())?;
                Ok(app)
            }
            Platform::WindowsX86_64 => {
                let folder = unpacked.join("Connecto");
                install::check_windows_folder(&folder)?;
                Ok(folder)
            }
        }
    }

    fn hand_over(&self, ready: &Ready) -> Result<(), String> {
        let dir = self.updates_dir();
        let marker = state::healthy_marker(&self.cfg.data_dir, &ready.version);
        let log = dir.join("hand-over.log");
        let script = if cfg!(windows) {
            let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
            let home = std::env::var_os("USERPROFILE")
                .map(PathBuf::from)
                .unwrap_or_default();
            let target = install::windows_install_dir(local.as_deref(), &home);
            let previous = target.with_file_name("Connecto.previous");
            helper::windows_script(&WindowsHandOver {
                pid: std::process::id(),
                target: &target,
                staged: &ready.staged,
                previous: &previous,
                fallback: &self.cfg.exe,
                marker: &marker,
                log: &log,
                wait_secs: HAND_OVER_WAIT_SECS,
            })
        } else {
            let target = install::bundle_of(&self.cfg.exe)
                .ok_or("Connecto is not running from an app bundle")?;
            let previous = dir.join("previous").join("Connecto.app");
            helper::mac_script(&MacHandOver {
                pid: std::process::id(),
                target: &target,
                staged: &ready.staged,
                previous: &previous,
                marker: &marker,
                log: &log,
                wait_secs: HAND_OVER_WAIT_SECS,
            })
        };
        Pending {
            from: self.cfg.current,
            to: ready.version,
        }
        .save(&self.cfg.data_dir)
        .map_err(|err| format!("the update could not be recorded: {err}"))?;
        if let Err(why) = install::start_hand_over(&script, &dir) {
            Pending::clear(&self.cfg.data_dir);
            return Err(why);
        }
        Ok(())
    }
}

impl UpdateControl for Updater {
    fn view(&self) -> Arc<UpdateView> {
        self.view.load_full()
    }

    fn check_now(&self) {
        if let Some(tx) = lock(&self.tx).as_ref() {
            let _ = tx.send(true);
        }
    }

    fn set_auto_check(&self, on: bool) {
        {
            let mut state = lock(&self.state);
            state.auto_check = on;
            let _ = state.save(&self.cfg.data_dir);
        }
        let mut view = (**self.view.load()).clone();
        view.auto_check = on;
        self.view.store(Arc::new(view));
        (self.cfg.wake)();
    }

    fn install_and_restart(&self) -> Result<(), String> {
        let ready = lock(&self.ready);
        let ready = ready.as_ref().ok_or("no update is ready to install")?;
        self.hand_over(ready)
    }

    fn dismiss_notice(&self) {
        let mut view = (**self.view.load()).clone();
        view.notice = None;
        self.view.store(Arc::new(view));
    }
}

/// The `data_dir` this updater uses, for tests.
#[cfg(test)]
impl Updater {
    fn data_dir(&self) -> &std::path::Path {
        &self.cfg.data_dir
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn updater(key: Option<VerifyingKey>, feed_url: &str) -> Arc<Updater> {
        let dir = std::env::temp_dir().join(format!(
            "connecto-updater-{}-{}",
            std::process::id(),
            now_ms()
        ));
        Updater::start(Config {
            data_dir: dir,
            current: "0.1.8".parse().unwrap(),
            feed_url: feed_url.to_owned(),
            key,
            exe: PathBuf::from("/nowhere/connecto-app"),
            wake: Arc::new(|| {}),
            notice: Some("hello".to_owned()),
        })
    }

    fn wait_for(updater: &Updater, done: impl Fn(&UpdateStatus) -> bool) -> UpdateStatus {
        for _ in 0..200 {
            let status = updater.view().status.clone();
            if done(&status) {
                return status;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        updater.view().status.clone()
    }

    #[test]
    fn a_build_without_a_key_says_updates_are_off_and_fetches_nothing() {
        let updater = updater(None, "https://example.invalid/update.json");
        updater.check_now();
        let status = wait_for(&updater, |s| matches!(s, UpdateStatus::Unavailable(_)));
        assert!(matches!(status, UpdateStatus::Unavailable(_)), "{status:?}");
        assert!(updater.view().asked);
        assert!(updater.install_and_restart().is_err(), "nothing is ready");
    }

    #[test]
    fn the_automatic_choice_is_saved_and_the_notice_can_be_dismissed() {
        let updater = updater(None, "https://example.invalid/update.json");
        assert!(updater.view().auto_check);
        assert_eq!(updater.view().notice.as_deref(), Some("hello"));
        updater.set_auto_check(false);
        assert!(!updater.view().auto_check);
        assert!(!State::load(updater.data_dir()).auto_check);
        updater.dismiss_notice();
        assert_eq!(updater.view().notice, None);
    }
}
