//! What the updater keeps between launches, and what a launch after an update
//! means. Everything is in `<data folder>/updates/`.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::version::Version;

/// How often an automatic check runs. The feed is about 1 KB and is asked for
/// with its last `ETag`, so an unchanged feed costs a `304` and no body.
pub const CHECK_EVERY_MS: u64 = 60 * 1000;

/// The least time between two restarts the updater makes by itself, whatever
/// the versions: a feed that keeps naming new versions cannot restart the app
/// over and over.
pub const AUTO_RESTART_GAP_MS: u64 = 10 * 60 * 1000;

/// How long a new version has to stay up before it counts as started. The
/// hand-over waits for this, and rolls back without it.
pub const HEALTHY_AFTER_SECS: u64 = 10;

/// How long the hand-over waits for the new version to say it started.
pub const HAND_OVER_WAIT_SECS: u32 = 120;

/// The updater's folder in the data folder.
#[must_use]
pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("updates")
}

/// The file a started version writes, which the hand-over waits for.
#[must_use]
pub fn healthy_marker(data_dir: &Path, version: &Version) -> PathBuf {
    dir(data_dir).join(format!("healthy-{version}"))
}

fn state_file(data_dir: &Path) -> PathBuf {
    dir(data_dir).join("state.json")
}

fn pending_file(data_dir: &Path) -> PathBuf {
    dir(data_dir).join("pending.json")
}

/// The saved choices and history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// Check at launch and every minute. On by default.
    pub auto_check: bool,
    /// When the last check ran, in ms since the epoch.
    pub last_check_ms: Option<u64>,
    /// A version that was installed and did not start: not offered again.
    pub failed: Option<Version>,
    /// Install a ready update by itself, as soon as nothing blocks a restart,
    /// and carry on the agent turns the restart cut. On by default.
    pub auto_install: bool,
    /// The versions the updater installed by itself, the latest last: one a
    /// release at most, so all are kept. None is installed by itself again (by
    /// hand it still can be), so versions that do not take cannot restart the
    /// app in a loop, even taking turns.
    pub auto_tried: Vec<Version>,
    /// When the updater last restarted the app by itself, in ms since the epoch.
    pub last_auto_restart_ms: Option<u64>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            auto_check: true,
            last_check_ms: None,
            failed: None,
            auto_install: true,
            auto_tried: Vec::new(),
            last_auto_restart_ms: None,
        }
    }
}

impl State {
    /// The saved state; the defaults when there is none or it is unreadable.
    #[must_use]
    pub fn load(data_dir: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(state_file(data_dir)) else {
            return Self::default();
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            return Self::default();
        };
        let version = |name: &str| {
            value
                .get(name)
                .and_then(Value::as_str)
                .and_then(|text| text.parse().ok())
        };
        Self {
            auto_check: value
                .get("auto_check")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            last_check_ms: value.get("last_check_ms").and_then(Value::as_u64),
            failed: version("failed"),
            auto_install: value
                .get("auto_install")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            auto_tried: value
                .get("auto_tried")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(|v| v.as_str()?.parse().ok())
                        .collect()
                })
                .unwrap_or_default(),
            last_auto_restart_ms: value.get("last_auto_restart_ms").and_then(Value::as_u64),
        }
    }

    /// # Errors
    /// When the file cannot be written.
    pub fn save(&self, data_dir: &Path) -> std::io::Result<()> {
        let value = json!({
            "auto_check": self.auto_check,
            "last_check_ms": self.last_check_ms,
            "failed": self.failed.map(|v| v.to_string()),
            "auto_install": self.auto_install,
            "auto_tried": self.auto_tried.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "last_auto_restart_ms": self.last_auto_restart_ms,
        });
        write_atomic(&state_file(data_dir), &value.to_string())
    }

    /// Whether an automatic check is due now.
    #[must_use]
    pub fn check_due(&self, now_ms: u64) -> bool {
        self.auto_check
            && self.last_check_ms.is_none_or(|last| {
                // A clock set back counts as due, rather than never.
                now_ms < last || now_ms - last >= CHECK_EVERY_MS
            })
    }

    /// Why `version`, ready to install, may not be installed by the updater
    /// itself now; `None` when it may. The setting, a version it already
    /// installed by itself once, and the gap since its last restart.
    #[must_use]
    pub fn auto_install_refusal(&self, version: Version, now_ms: u64) -> Option<&'static str> {
        if !self.auto_install {
            return Some("installing updates automatically is off");
        }
        if self.auto_tried.contains(&version) {
            return Some("this version was installed automatically once already");
        }
        // A clock set back counts as recent: waiting is the safe side.
        let recent = self
            .last_auto_restart_ms
            .is_some_and(|last| now_ms < last.saturating_add(AUTO_RESTART_GAP_MS));
        recent.then_some("Connecto restarted for an update a few minutes ago")
    }

    /// The updater is about to install `version` by itself.
    pub fn note_auto_attempt(&mut self, version: Version, now_ms: u64) {
        self.auto_tried.retain(|tried| *tried != version);
        self.auto_tried.push(version);
        self.last_auto_restart_ms = Some(now_ms);
    }

    /// Whether `latest` should be offered to a build running `current`.
    #[must_use]
    pub fn worth_offering(&self, latest: Version, current: Version) -> bool {
        latest > current && self.failed != Some(latest)
    }
}

/// Claim the automatic install of `version` for this process, across every
/// copy of the app on this data folder: the first to make the claim file
/// gets it, once. `Ok(false)` when it was claimed already.
///
/// # Errors
/// When the claim cannot be written.
pub fn claim_auto_attempt(data_dir: &Path, version: Version) -> std::io::Result<bool> {
    let folder = dir(data_dir);
    std::fs::create_dir_all(&folder)?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(folder.join(format!("auto-{version}.claimed")))
    {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(err) => Err(err),
    }
}

/// An update handed over to the helper and not yet seen to start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pending {
    pub from: Version,
    pub to: Version,
}

impl Pending {
    /// # Errors
    /// When the file cannot be written.
    pub fn save(&self, data_dir: &Path) -> std::io::Result<()> {
        let value = json!({"from": self.from.to_string(), "to": self.to.to_string()});
        write_atomic(&pending_file(data_dir), &value.to_string())
    }

    #[must_use]
    pub fn load(data_dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(pending_file(data_dir)).ok()?;
        let value: Value = serde_json::from_str(&text).ok()?;
        let field = |name: &str| value.get(name)?.as_str()?.parse::<Version>().ok();
        Some(Self {
            from: field("from")?,
            to: field("to")?,
        })
    }

    /// Forget it. Not being there is fine.
    pub fn clear(data_dir: &Path) {
        let _ = std::fs::remove_file(pending_file(data_dir));
    }
}

/// What this launch is, given an update that may be under way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launch {
    /// No update under way.
    Normal,
    /// The new version, started by the hand-over: it says so once it has been
    /// up for [`HEALTHY_AFTER_SECS`].
    Updated { from: Version, to: Version },
    /// The previous version again: the new one did not start and the hand-over
    /// put this one back.
    RolledBack { failed: Version },
    /// A version the update was not about (installed by hand meanwhile).
    Unrelated,
}

/// Decide what this launch is.
#[must_use]
pub fn on_launch(pending: Option<Pending>, running: Version) -> Launch {
    match pending {
        None => Launch::Normal,
        Some(p) if running == p.to => Launch::Updated {
            from: p.from,
            to: p.to,
        },
        Some(p) if running == p.from => Launch::RolledBack { failed: p.to },
        Some(_) => Launch::Unrelated,
    }
}

/// Apply what a launch means to the saved state, and return a notice for the
/// person, if there is one. For [`Launch::Updated`] the caller writes the
/// healthy marker once the app has been up a while; the pending record is
/// cleared then, not here.
pub fn settle_launch(data_dir: &Path, launch: Launch) -> Option<String> {
    match launch {
        Launch::Normal | Launch::Updated { .. } => None,
        Launch::RolledBack { failed } => {
            let mut state = State::load(data_dir);
            state.failed = Some(failed);
            let _ = state.save(data_dir);
            Pending::clear(data_dir);
            Some(format!(
                "Connecto {failed} did not start, so the version you had was put back. It will not be offered again; a later version will."
            ))
        }
        Launch::Unrelated => {
            Pending::clear(data_dir);
            None
        }
    }
}

/// The new version has been up long enough: tell the hand-over, and forget
/// the update.
pub fn mark_healthy(data_dir: &Path, version: &Version) {
    let _ = write_atomic(&healthy_marker(data_dir, version), "ok\n");
    Pending::clear(data_dir);
}

pub(crate) fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no folder"))?;
    std::fs::create_dir_all(dir)?;
    let temp = dir.join(format!(
        ".{}.{}",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id()
    ));
    std::fs::write(&temp, text)?;
    std::fs::rename(&temp, path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        text.parse().unwrap()
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "connecto-update-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_updater_installs_by_itself_once_per_version_and_not_twice_in_a_row() {
        let mut state = State::default();
        assert!(state.auto_install, "on by default");
        assert_eq!(state.auto_install_refusal(v("0.1.9"), 1_000), None);
        state.note_auto_attempt(v("0.1.9"), 1_000);
        let gap = 1_000 + AUTO_RESTART_GAP_MS;
        assert!(
            state.auto_install_refusal(v("0.1.9"), gap).is_some(),
            "the same version: never again by itself"
        );
        assert!(
            state.auto_install_refusal(v("0.1.10"), gap - 1).is_some(),
            "too soon after the last restart"
        );
        assert_eq!(state.auto_install_refusal(v("0.1.10"), gap), None);
        assert!(
            state.auto_install_refusal(v("0.1.10"), 10).is_some(),
            "a clock set back waits"
        );
        // Two versions taking turns: neither is installed by itself again.
        state.note_auto_attempt(v("0.1.10"), gap);
        let later = 1_000 + 10 * AUTO_RESTART_GAP_MS;
        assert!(state.auto_install_refusal(v("0.1.9"), later).is_some());
        assert!(state.auto_install_refusal(v("0.1.10"), later).is_some());
        assert_eq!(state.auto_install_refusal(v("0.1.11"), later), None);
        state.auto_install = false;
        assert!(state.auto_install_refusal(v("0.1.11"), later).is_some());
    }

    #[test]
    fn an_automatic_install_is_claimed_once_across_processes() {
        let dir = temp("claim");
        assert!(claim_auto_attempt(&dir, v("0.1.9")).unwrap());
        assert!(
            !claim_auto_attempt(&dir, v("0.1.9")).unwrap(),
            "a second copy does not get it"
        );
        assert!(claim_auto_attempt(&dir, v("0.1.10")).unwrap());
        // Not writable: an error, not "claimed already".
        let file = dir.join("a-file");
        std::fs::write(&file, "").unwrap();
        assert!(claim_auto_attempt(&file, v("0.1.9")).is_err());
    }

    #[test]
    fn checks_are_due_at_first_then_every_minute_and_never_when_off() {
        let mut state = State::default();
        assert!(state.check_due(1_000));
        state.last_check_ms = Some(1_000);
        assert!(!state.check_due(1_000 + CHECK_EVERY_MS - 1));
        assert!(state.check_due(1_000 + CHECK_EVERY_MS));
        assert!(state.check_due(10), "a clock set back");
        state.auto_check = false;
        assert!(!state.check_due(1_000 + 2 * CHECK_EVERY_MS));
    }

    #[test]
    fn only_a_newer_version_that_did_not_fail_is_offered() {
        let mut state = State::default();
        assert!(state.worth_offering(v("0.1.9"), v("0.1.8")));
        assert!(!state.worth_offering(v("0.1.8"), v("0.1.8")));
        assert!(
            !state.worth_offering(v("0.1.7"), v("0.1.8")),
            "never a downgrade"
        );
        state.failed = Some(v("0.1.9"));
        assert!(!state.worth_offering(v("0.1.9"), v("0.1.8")));
        assert!(
            state.worth_offering(v("0.1.10"), v("0.1.8")),
            "a later one is"
        );
    }

    #[test]
    fn state_survives_a_round_trip_and_defaults_to_checking() {
        let dir = temp("state");
        assert_eq!(State::load(&dir), State::default());
        assert!(State::load(&dir).auto_check);
        let state = State {
            auto_check: false,
            last_check_ms: Some(42),
            failed: Some(v("0.2.0")),
            auto_install: false,
            auto_tried: vec![v("0.1.9"), v("0.1.10")],
            last_auto_restart_ms: Some(7),
        };
        state.save(&dir).unwrap();
        assert_eq!(State::load(&dir), state);
        std::fs::write(state_file(&dir), "garbage").unwrap();
        assert_eq!(State::load(&dir), State::default());
    }

    #[test]
    fn a_launch_is_read_from_the_pending_update() {
        let p = Pending {
            from: v("0.1.8"),
            to: v("0.1.9"),
        };
        assert_eq!(on_launch(None, v("0.1.8")), Launch::Normal);
        assert_eq!(
            on_launch(Some(p), v("0.1.9")),
            Launch::Updated {
                from: v("0.1.8"),
                to: v("0.1.9")
            }
        );
        assert_eq!(
            on_launch(Some(p), v("0.1.8")),
            Launch::RolledBack { failed: v("0.1.9") }
        );
        assert_eq!(on_launch(Some(p), v("0.2.0")), Launch::Unrelated);
    }

    #[test]
    fn a_rollback_is_remembered_and_the_failed_version_not_offered_again() {
        let dir = temp("rollback");
        let p = Pending {
            from: v("0.1.8"),
            to: v("0.1.9"),
        };
        p.save(&dir).unwrap();
        assert_eq!(Pending::load(&dir), Some(p));
        let launch = on_launch(Pending::load(&dir), v("0.1.8"));
        let notice = settle_launch(&dir, launch).unwrap();
        assert!(notice.contains("0.1.9"), "{notice}");
        assert_eq!(Pending::load(&dir), None, "cleared");
        let state = State::load(&dir);
        assert_eq!(state.failed, Some(v("0.1.9")));
        assert!(!state.worth_offering(v("0.1.9"), v("0.1.8")));
    }

    #[test]
    fn a_healthy_new_version_leaves_the_marker_and_forgets_the_update() {
        let dir = temp("healthy");
        let p = Pending {
            from: v("0.1.8"),
            to: v("0.1.9"),
        };
        p.save(&dir).unwrap();
        let launch = on_launch(Pending::load(&dir), v("0.1.9"));
        assert_eq!(settle_launch(&dir, launch), None);
        assert!(
            Pending::load(&dir).is_some(),
            "kept until it has been up a while"
        );
        mark_healthy(&dir, &v("0.1.9"));
        assert!(healthy_marker(&dir, &v("0.1.9")).is_file());
        assert_eq!(Pending::load(&dir), None);
    }
}
