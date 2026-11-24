//! The updater as the app runs it: a background thread that checks the feed
//! at launch and every minute (or when asked), downloads and checks a newer
//! release, and hands it over to the installer when the person says so, or,
//! with automatic installs on, by itself as soon as nothing blocks a restart
//! (ADR 0030).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
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
/// How often the thread looks whether a check is due.
const LOOK_EVERY: Duration = Duration::from_secs(60);
/// How often the automatic install looks again at what holds it back.
const AUTO_LOOK_EVERY: Duration = Duration::from_secs(5);

/// What the app's automatic install needs from the app: what blocks a restart
/// now (a pairing, a file transfer...), and the restart itself, which quits
/// the app once the hand-over has started.
pub struct AutoInstall {
    pub blocker: Box<dyn Fn() -> Option<String> + Send>,
    pub restart: Box<dyn FnOnce() + Send>,
}

/// What the automatic install does at one look.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AutoStep {
    /// Nothing to install, or automatic installs are off.
    Nothing,
    /// A ready version waits, and this holds it back.
    Hold(String),
    /// Install this version now.
    Go(Version),
}

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

/// Gives the check's claim back when the check ends, however it ends.
struct Checking<'a>(&'a AtomicBool);

impl Drop for Checking<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
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
    /// The last feed fetched and its `ETag`, for a conditional fetch.
    feed_cache: Mutex<Option<(String, Vec<u8>)>>,
    /// A check is under way (the feed, then any newer release's download):
    /// the automatic install waits for it, so it hands over the newest one.
    downloading: AtomicBool,
    /// The hand-over has started: the app is about to quit for the update.
    handed_over: AtomicBool,
    /// The app is quitting: no automatic hand-over starts any more.
    stopped: AtomicBool,
    /// Why the last automatic install of a version did not start, kept to
    /// say while that version waits for a hand-over by hand.
    auto_failure: Mutex<Option<(Version, String)>>,
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
            auto_install: state.auto_install,
            held: None,
            asked: false,
            notice: cfg.notice.clone(),
        };
        let updater = Arc::new(Self {
            cfg,
            view: ArcSwap::from_pointee(view),
            tx: Mutex::new(None),
            ready: Mutex::new(None),
            state: Mutex::new(state),
            feed_cache: Mutex::new(None),
            downloading: AtomicBool::new(false),
            handed_over: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            auto_failure: Mutex::new(None),
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

    /// Whether the hand-over has started, so the quit that follows is for the
    /// update: the app marks the agent turns it cuts, to carry them on.
    #[must_use]
    pub fn handed_over(&self) -> bool {
        self.handed_over.load(Ordering::SeqCst)
    }

    /// The app is quitting: from now on no automatic hand-over starts. One
    /// under way is finished first, so after this [`Updater::handed_over`]
    /// says for good whether the quit is for an update.
    pub fn stop_auto(&self) {
        let _ready = lock(&self.ready);
        self.stopped.store(true, Ordering::SeqCst);
    }

    /// Start the automatic install: every few seconds, a ready update is
    /// installed if automatic installs are on and nothing blocks a restart.
    /// At most once per version, and not twice within
    /// [`state::AUTO_RESTART_GAP_MS`], so it cannot restart in a loop.
    pub fn start_auto(self: &Arc<Self>, auto: AutoInstall) {
        let updater = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("update-auto".into())
            .spawn(move || {
                let AutoInstall { blocker, restart } = auto;
                loop {
                    std::thread::sleep(AUTO_LOOK_EVERY);
                    if updater.handed_over() || updater.stopped.load(Ordering::SeqCst) {
                        return;
                    }
                    match updater.auto_step(now_ms(), &*blocker) {
                        AutoStep::Nothing => updater.set_held(None),
                        AutoStep::Hold(why) => updater.set_held(Some(why)),
                        AutoStep::Go(version) => {
                            if updater.auto_install(version, now_ms(), &*blocker) {
                                restart();
                                return;
                            }
                        }
                    }
                }
            });
        if spawned.is_err() {
            self.set_held(Some("the automatic install could not start".to_owned()));
        }
    }

    fn auto_step(&self, now: u64, blocker: &dyn Fn() -> Option<String>) -> AutoStep {
        let Some(version) = lock(&self.ready).as_ref().map(|ready| ready.version) else {
            return AutoStep::Nothing;
        };
        {
            let state = lock(&self.state);
            if !state.auto_install {
                return AutoStep::Nothing;
            }
            if let Some(why) = state.auto_install_refusal(version, now) {
                if let Some((failed, why)) = lock(&self.auto_failure).as_ref()
                    && *failed == version
                {
                    return AutoStep::Hold(why.clone());
                }
                return AutoStep::Hold(format!("Not installed automatically: {why}."));
            }
        }
        if self.downloading.load(Ordering::SeqCst) {
            return AutoStep::Hold("Looking for a newer version.".to_owned());
        }
        match blocker() {
            Some(why) => AutoStep::Hold(why),
            None => AutoStep::Go(version),
        }
    }

    /// Decide again with the ready release locked, record the attempt, then
    /// hand `version` over. True when the app must now quit. The lock keeps a
    /// hand-over by hand, a newer download and the app's quit out meanwhile,
    /// and the record is kept whatever happens, so this version is never
    /// installed by itself again.
    fn auto_install(
        &self,
        version: Version,
        now: u64,
        blocker: &dyn Fn() -> Option<String>,
    ) -> bool {
        let ready = lock(&self.ready);
        if self.handed_over() || self.stopped.load(Ordering::SeqCst) {
            return false;
        }
        // What was decided may be out of date: a newer one is ready or on its
        // way, or automatic installs were turned off.
        if ready.as_ref().map(|ready| ready.version) != Some(version)
            || self.downloading.load(Ordering::SeqCst)
        {
            return false;
        }
        // Something may have begun since the look that chose to go.
        if let Some(why) = blocker() {
            drop(ready);
            self.set_held(Some(why));
            return false;
        }
        let saved = {
            let mut state = lock(&self.state);
            if state.auto_install_refusal(version, now).is_some() {
                return false;
            }
            // Across processes too (a second copy of the app on the same data
            // folder): whoever makes the claim file first goes, once.
            let claim = state::claim_auto_attempt(&self.cfg.data_dir, version);
            // Tried, in this run, whatever comes of it: not again by itself.
            state.note_auto_attempt(version, now);
            match claim {
                Ok(true) => state.save(&self.cfg.data_dir),
                Ok(false) => {
                    // Another copy of the app took it: never by this one.
                    let _ = state.save(&self.cfg.data_dir);
                    return false;
                }
                Err(err) => Err(err),
            }
        };
        if let Err(err) = saved {
            drop(ready);
            self.auto_failed(
                version,
                format!("Not installed automatically: the attempt could not be recorded ({err}). Use Update and restart."),
            );
            return false;
        }
        let result = match ready.as_ref() {
            Some(ready) => self.hand_over(ready),
            None => Err("nothing is ready".to_owned()),
        };
        drop(ready);
        match result {
            Ok(()) => true,
            Err(why) => {
                self.auto_failed(
                    version,
                    format!("The automatic install failed: {why}. Use Update and restart."),
                );
                false
            }
        }
    }

    fn auto_failed(&self, version: Version, why: String) {
        *lock(&self.auto_failure) = Some((version, why.clone()));
        self.set_held(Some(why));
    }

    /// Change the view. Several threads change it (the checks, the automatic
    /// install, the window): each change is made on the latest view, so none
    /// undoes another.
    fn change_view(&self, change: impl Fn(&mut UpdateView)) {
        self.view.rcu(|view| {
            let mut view = (**view).clone();
            change(&mut view);
            view
        });
        (self.cfg.wake)();
    }

    fn set_held(&self, held: Option<String>) {
        if self.view.load().held == held {
            return;
        }
        self.change_view(|view| view.held.clone_from(&held));
    }

    fn set_status(&self, status: UpdateStatus, asked: bool) {
        self.change_view(|view| {
            view.status = status.clone();
            view.asked = asked;
        });
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
        // Claimed with the ready release locked, so an automatic hand-over
        // either sees the check or has started already; given back on every
        // way out.
        {
            let _ready = lock(&self.ready);
            if self.handed_over() {
                return;
            }
            self.downloading.store(true, Ordering::SeqCst);
        }
        let _checking = Checking(&self.downloading);
        let ready_version = lock(&self.ready).as_ref().map(|ready| ready.version);
        // With a version ready, a check only looks for a newer one: the strip
        // keeps saying it is ready meanwhile.
        if ready_version.is_none() || asked {
            self.set_status(UpdateStatus::Checking, asked);
        }
        let ua = install::user_agent(&self.cfg.current.to_string());
        let result = self
            .fetch_feed(&ua)
            .and_then(|bytes| feed::verify(&bytes, &key));
        {
            let mut state = lock(&self.state);
            state.last_check_ms = Some(now_ms());
            let _ = state.save(&self.cfg.data_dir);
        }
        let still_ready = |asked| {
            ready_version.map(|version| {
                self.set_status(
                    UpdateStatus::Ready {
                        version: version.to_string(),
                    },
                    asked,
                );
            })
        };
        let latest = match result {
            Ok(latest) => latest,
            Err(why) => {
                if still_ready(asked).is_none() {
                    self.set_status(UpdateStatus::Failed(why), asked);
                }
                return;
            }
        };
        // Nothing newer than what is ready already: it stays the one.
        if ready_version.is_some_and(|ready| latest.version <= ready) {
            still_ready(asked);
            return;
        }
        let offered = lock(&self.state).worth_offering(latest.version, self.cfg.current);
        let Some(artifact) = latest.artifact(platform).filter(|_| offered) else {
            if still_ready(asked).is_none() {
                self.set_status(UpdateStatus::UpToDate, asked);
            }
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
        let fetched = self.fetch_release(platform, &latest.version, artifact, team.as_deref(), &ua);
        let outcome = match fetched {
            Ok(staged) => {
                let mut ready = lock(&self.ready);
                if self.handed_over() {
                    // A hand-over of the older one started meanwhile, by hand:
                    // it stands, and its files stay.
                    None
                } else {
                    let older = ready.replace(Ready {
                        version: latest.version,
                        staged,
                    });
                    if let Some(older) = older {
                        let _ = std::fs::remove_dir_all(
                            self.updates_dir().join(older.version.to_string()),
                        );
                    }
                    Some(Ok(()))
                }
            }
            Err(why) => Some(Err(why)),
        };
        match outcome {
            None => {}
            Some(Ok(())) => self.set_status(UpdateStatus::Ready { version }, asked),
            Some(Err(why)) if still_ready(asked).is_none() => {
                self.set_status(UpdateStatus::Failed(why), asked);
            }
            Some(Err(_)) => {}
        }
    }

    /// The feed's bytes: fetched again only when it changed since the last
    /// fetch (`If-None-Match`).
    fn fetch_feed(&self, ua: &str) -> Result<Vec<u8>, String> {
        let etag = lock(&self.feed_cache)
            .as_ref()
            .map(|(etag, _)| etag.clone());
        match install::fetch_conditional(&self.cfg.feed_url, ua, etag.as_deref())? {
            install::Fetched::Fresh { body, etag } => {
                *lock(&self.feed_cache) = etag.map(|etag| (etag, body.clone()));
                Ok(body)
            }
            install::Fetched::NotModified => lock(&self.feed_cache)
                .as_ref()
                .map(|(_, body)| body.clone())
                .ok_or_else(|| "the update check got an answer it did not ask for".to_owned()),
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
        self.handed_over.store(true, Ordering::SeqCst);
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
        self.change_view(|view| view.auto_check = on);
    }

    fn set_auto_install(&self, on: bool) {
        {
            let mut state = lock(&self.state);
            state.auto_install = on;
            let _ = state.save(&self.cfg.data_dir);
        }
        self.change_view(|view| {
            view.auto_install = on;
            if !on {
                view.held = None;
            }
        });
    }

    fn install_and_restart(&self) -> Result<(), String> {
        let ready = lock(&self.ready);
        if self.handed_over() {
            return Err("the update is already being installed".to_owned());
        }
        let ready = ready.as_ref().ok_or("no update is ready to install")?;
        self.hand_over(ready)
    }

    fn dismiss_notice(&self) {
        self.view.rcu(|view| {
            let mut view = (**view).clone();
            view.notice = None;
            view
        });
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
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "connecto-updater-{}-{}-{}",
            std::process::id(),
            now_ms(),
            NEXT.fetch_add(1, Ordering::SeqCst)
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

    fn make_ready(updater: &Updater, version: &str) {
        *lock(&updater.ready) = Some(Ready {
            version: version.parse().unwrap(),
            staged: PathBuf::from("/nowhere/Connecto.app"),
        });
    }

    #[test]
    fn a_ready_update_waits_while_something_blocks_and_goes_when_nothing_does() {
        let updater = updater(None, "https://example.invalid/update.json");
        let none = || None;
        let pairing = || Some("A pairing is under way.".to_owned());
        assert_eq!(
            updater.auto_step(1_000, &none),
            AutoStep::Nothing,
            "nothing ready"
        );
        make_ready(&updater, "0.1.9");
        assert_eq!(
            updater.auto_step(1_000, &pairing),
            AutoStep::Hold("A pairing is under way.".to_owned())
        );
        updater.downloading.store(true, Ordering::SeqCst);
        assert!(
            matches!(updater.auto_step(1_000, &none), AutoStep::Hold(_)),
            "a newer one is coming"
        );
        updater.downloading.store(false, Ordering::SeqCst);
        assert_eq!(
            updater.auto_step(1_000, &none),
            AutoStep::Go("0.1.9".parse().unwrap())
        );
        updater.set_auto_install(false);
        assert_eq!(updater.auto_step(1_000, &none), AutoStep::Nothing, "off");
        assert!(!State::load(updater.data_dir()).auto_install, "saved");
    }

    #[test]
    fn a_failed_automatic_install_is_not_tried_again_and_does_not_restart() {
        let updater = updater(None, "https://example.invalid/update.json");
        make_ready(&updater, "0.1.9");
        let version = "0.1.9".parse().unwrap();
        // This copy is not in an app bundle: the hand-over cannot start.
        assert!(!updater.auto_install(version, 5_000, &|| None));
        assert!(!updater.handed_over());
        assert!(updater.view().held.as_ref().unwrap().contains("failed"));
        let saved = State::load(updater.data_dir());
        assert_eq!(saved.auto_tried, vec![version], "recorded before the try");
        assert_eq!(saved.last_auto_restart_ms, Some(5_000));
        let later = 5_000 + crate::state::AUTO_RESTART_GAP_MS * 10;
        match updater.auto_step(later, &|| None) {
            AutoStep::Hold(why) => assert!(why.contains("failed"), "the failure stays said: {why}"),
            other => panic!("never by itself again: {other:?}"),
        }
        // A newer one is: after the gap.
        make_ready(&updater, "0.1.10");
        assert!(matches!(
            updater.auto_step(6_000, &|| None),
            AutoStep::Hold(_)
        ));
        assert!(matches!(
            updater.auto_step(later, &|| None),
            AutoStep::Go(_)
        ));
    }

    #[test]
    fn an_automatic_install_decided_on_stale_facts_is_dropped_without_a_record() {
        let updater = updater(None, "https://example.invalid/update.json");
        make_ready(&updater, "0.1.10");
        let older = "0.1.9".parse().unwrap();
        assert!(
            !updater.auto_install(older, 5_000, &|| None),
            "a newer one is ready"
        );
        updater.downloading.store(true, Ordering::SeqCst);
        assert!(
            !updater.auto_install("0.1.10".parse().unwrap(), 5_000, &|| None),
            "a newer one downloads"
        );
        updater.downloading.store(false, Ordering::SeqCst);
        updater.set_auto_install(false);
        assert!(
            !updater.auto_install("0.1.10".parse().unwrap(), 5_000, &|| None),
            "turned off"
        );
        updater.set_auto_install(true);
        let pairing = || Some("A pairing is under way.".to_owned());
        assert!(
            !updater.auto_install("0.1.10".parse().unwrap(), 5_000, &pairing),
            "something began since the look"
        );
        updater.stop_auto();
        assert!(
            !updater.auto_install("0.1.10".parse().unwrap(), 5_000, &|| None),
            "the app is quitting"
        );
        assert!(
            State::load(updater.data_dir()).auto_tried.is_empty(),
            "nothing recorded"
        );
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
