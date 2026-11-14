//! What the updater keeps between launches, and what a launch after an update
//! means. Everything is in `<data folder>/updates/`.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::version::Version;

/// How often an automatic check runs.
pub const CHECK_EVERY_MS: u64 = 24 * 60 * 60 * 1000;

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
    /// Check at launch and daily. On by default.
    pub auto_check: bool,
    /// When the last check ran, in ms since the epoch.
    pub last_check_ms: Option<u64>,
    /// A version that was installed and did not start: not offered again.
    pub failed: Option<Version>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            auto_check: true,
            last_check_ms: None,
            failed: None,
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
        Self {
            auto_check: value
                .get("auto_check")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            last_check_ms: value.get("last_check_ms").and_then(Value::as_u64),
            failed: value
                .get("failed")
                .and_then(Value::as_str)
                .and_then(|text| text.parse().ok()),
        }
    }

    /// # Errors
    /// When the file cannot be written.
    pub fn save(&self, data_dir: &Path) -> std::io::Result<()> {
        let value = json!({
            "auto_check": self.auto_check,
            "last_check_ms": self.last_check_ms,
            "failed": self.failed.map(|v| v.to_string()),
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

    /// Whether `latest` should be offered to a build running `current`.
    #[must_use]
    pub fn worth_offering(&self, latest: Version, current: Version) -> bool {
        latest > current && self.failed != Some(latest)
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
    fn checks_are_due_at_first_then_daily_and_never_when_off() {
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
