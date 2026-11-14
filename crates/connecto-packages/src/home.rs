//! Connecto Home: the folder holding `automations/` and `packages/`, and the
//! move of an existing Automations folder into it.

use std::fs;
use std::path::{Path, PathBuf};

pub const AUTOMATIONS: &str = "automations";
pub const PACKAGES: &str = "packages";
const AUTOMATION_FILE: &str = "automation.json";

#[must_use]
pub fn automations_dir(home: &Path) -> PathBuf {
    home.join(AUTOMATIONS)
}

#[must_use]
pub fn packages_dir(home: &Path) -> PathBuf {
    home.join(PACKAGES)
}

/// Check a folder the user chose as Connecto Home, and give its real path. It
/// must exist, be a folder and be writable, and may not be the data folder, the
/// user's home folder, one that holds either, or a tool's own state.
pub fn validate(path: &Path, data_root: &Path, user_home: &Path) -> Result<PathBuf, String> {
    let real = fs::canonicalize(path).map_err(|_| "that folder does not exist".to_owned())?;
    if !real.is_dir() {
        return Err("that is not a folder".to_owned());
    }
    let resolve = crate::platform::resolve;
    let (data, home) = (resolve(data_root), resolve(user_home));
    if real == data || real.starts_with(&data) {
        return Err("that is inside Connecto's data folder".to_owned());
    }
    if real == home || home.starts_with(&real) || data.starts_with(&real) {
        return Err("choose a folder of its own, not your home folder".to_owned());
    }
    // `Library` on macOS, `AppData` on Windows.
    if crate::platform::HOME_STATE
        .iter()
        .any(|state| real.starts_with(home.join(state)))
    {
        return Err("that folder belongs to a tool's or the system's own state".to_owned());
    }
    if real.to_str().is_none() {
        return Err("the folder's path is not valid UTF-8".to_owned());
    }
    let probe = real.join(format!(".connecto-write-test-{}", std::process::id()));
    fs::write(&probe, b"").map_err(|_| "that folder is not writable".to_owned())?;
    let _ = fs::remove_file(&probe);
    Ok(real)
}

/// Make `automations/` and `packages/` in the home.
pub fn ensure_layout(home: &Path) -> Result<(), String> {
    for dir in [automations_dir(home), packages_dir(home)] {
        fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    Ok(())
}

/// The automation folders in an old Automations folder: every subfolder with
/// an `automation.json`.
fn automation_folders(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            e.file_type().is_ok_and(|t| t.is_dir()) && e.path().join(AUTOMATION_FILE).is_file()
        })
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .collect();
    names.sort();
    names
}

/// Move the automations of `old` into `target` (`<home>/automations`).
///
/// Safe by construction: nothing is moved unless every folder can be, so a name
/// that already exists in `target`, a folder inside the other, or a move across
/// volumes refuses the whole move and leaves both folders as they were. A move
/// that fails half way is put back. Returns the names that moved, which is
/// exactly what [`undo_migrate`] may put back.
pub fn migrate(old: &Path, target: &Path) -> Result<Vec<String>, String> {
    let real = crate::platform::resolve;
    let (old, target) = (real(old), real(target));
    if old == target {
        return Ok(Vec::new());
    }
    if target.starts_with(&old) || old.starts_with(&target) {
        return Err(format!(
            "the old automations folder ({}) and the new one overlap: choose a Connecto Home outside it",
            old.display()
        ));
    }
    let names = automation_folders(&old);
    if let Some(clash) = names.iter().find(|n| target.join(n).exists()) {
        return Err(format!(
            "{clash} exists in both the old automations folder and {}: rename or remove one, nothing was moved",
            target.display()
        ));
    }
    fs::create_dir_all(&target).map_err(|e| format!("{}: {e}", target.display()))?;
    let mut done: Vec<&String> = Vec::new();
    for name in &names {
        if let Err(e) = fs::rename(old.join(name), target.join(name)) {
            for back in done.iter().rev() {
                let _ = fs::rename(target.join(back), old.join(back));
            }
            let why = if e.raw_os_error() == Some(crate::platform::CROSS_DEVICE) {
                "the folders are on different volumes: choose a Connecto Home on the same volume"
                    .to_owned()
            } else {
                e.to_string()
            };
            return Err(format!("could not move {name}: {why}; nothing was moved"));
        }
        done.push(name);
    }
    Ok(names)
}

/// Put back the automations a [`migrate`] moved: only those names, and only
/// where nothing has taken their place. What was already in `target` is never
/// touched.
pub fn undo_migrate(old: &Path, target: &Path, moved: &[String]) {
    for name in moved {
        if !old.join(name).exists() {
            let _ = fs::rename(target.join(name), old.join(name));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn base(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mg-home-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        fs::canonicalize(&d).unwrap()
    }

    fn automation(root: &Path, slug: &str) {
        fs::create_dir_all(root.join(slug)).unwrap();
        fs::write(root.join(slug).join(AUTOMATION_FILE), slug).unwrap();
        fs::write(root.join(slug).join(".session.json"), "s").unwrap();
    }

    #[test]
    fn the_home_must_be_a_writable_folder_of_its_own() {
        let b = base("validate");
        let (data, user) = (b.join("data"), b.join("user"));
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(user.join("Connecto")).unwrap();
        fs::create_dir_all(user.join(".claude/x")).unwrap();
        assert!(validate(&user.join("Connecto"), &data, &user).is_ok());
        assert!(validate(&b.join("nope"), &data, &user).is_err());
        assert!(validate(&data, &data, &user).is_err());
        assert!(validate(&user, &data, &user).is_err());
        assert!(validate(&b, &data, &user).is_err(), "holds both");
        assert!(validate(&user.join(".claude/x"), &data, &user).is_err());
        fs::write(b.join("file"), "x").unwrap();
        assert!(validate(&b.join("file"), &data, &user).is_err());
        // `Library` on macOS, `AppData` on Windows.
        let system = user.join(crate::platform::HOME_STATE[3]).join("x");
        fs::create_dir_all(&system).unwrap();
        assert!(validate(&system, &data, &user).is_err());
    }

    /// Windows only: there a real path has the `\\?\` prefix and a plain one
    /// never matches it, so a missing folder resolves under its real parent.
    /// Unix keeps the path as given (on macOS a plain `/var/...` data folder
    /// that does not exist would not match `/private/var/...` either).
    #[cfg(windows)]
    #[test]
    fn a_data_folder_not_made_yet_is_still_kept_out() {
        let b = base("validate-missing");
        let (holder, user) = (b.join("holder"), b.join("user"));
        fs::create_dir_all(&holder).unwrap();
        fs::create_dir_all(&user).unwrap();
        // Given as a plain path, not the real one (`\\?\C:\...` on Windows).
        let plain =
            std::env::temp_dir().join(format!("mg-home-validate-missing-{}", std::process::id()));
        let data = plain.join("holder").join("data");
        assert!(validate(&holder, &data, &user).is_err());
        assert!(validate(&plain.join("holder"), &data, &plain.join("user")).is_err());
    }

    #[test]
    fn automations_move_into_the_new_home_with_their_files() {
        let b = base("move");
        let (old, target) = (b.join("old-auto"), b.join("home/automations"));
        fs::create_dir_all(&old).unwrap();
        automation(&old, "say-yes");
        automation(&old, "say-no");
        fs::write(old.join("CLAUDE.md"), "guide").unwrap();
        fs::create_dir_all(old.join("not-an-automation")).unwrap();
        assert_eq!(migrate(&old, &target).unwrap(), ["say-no", "say-yes"]);
        assert!(target.join("say-yes/automation.json").is_file());
        assert!(target.join("say-no/.session.json").is_file());
        assert!(!old.join("say-yes").exists());
        assert!(old.join("not-an-automation").is_dir(), "other things stay");
        assert!(old.join("CLAUDE.md").is_file());
        // Same folder: nothing to do.
        assert!(migrate(&target, &target).unwrap().is_empty());
    }

    #[test]
    fn a_conflict_refuses_the_whole_move_and_touches_nothing() {
        let b = base("conflict");
        let (old, target) = (b.join("old"), b.join("home/automations"));
        automation(&old, "a");
        automation(&old, "b");
        automation(&target, "b");
        let err = migrate(&old, &target).unwrap_err();
        assert!(err.contains("exists in both") && err.contains('b'), "{err}");
        assert!(old.join("a/automation.json").is_file(), "a was not moved");
        assert!(old.join("b/automation.json").is_file());
        assert_eq!(
            fs::read_to_string(target.join("b/automation.json")).unwrap(),
            "b"
        );
        assert!(!target.join("a").exists());
    }

    #[test]
    fn nested_folders_are_refused_and_a_move_can_be_undone() {
        let b = base("nested");
        let old = b.join("old");
        automation(&old, "a");
        assert!(migrate(&old, &old.join("home/automations")).is_err());
        assert!(migrate(&old, &b).is_err(), "the new one holds the old");
        let target = b.join("home/automations");
        automation(&target, "already-here");
        let moved = migrate(&old, &target).unwrap();
        assert_eq!(moved, ["a"]);
        undo_migrate(&old, &target, &moved);
        assert!(old.join("a/automation.json").is_file());
        assert!(!target.join("a").exists());
        assert!(
            target.join("already-here/automation.json").is_file(),
            "what was in the destination before stays there"
        );
        assert!(!old.join("already-here").exists());
    }
}
