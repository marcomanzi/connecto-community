//! Which folders may be a project. The home folder and Connecto's own state
//! are refused, because an agent with no sandbox could otherwise be pointed at
//! them (ADR 0013).

use std::fs;
use std::path::{Path, PathBuf};

/// Why a path was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PathError {
    #[error("path does not exist or cannot be resolved: {0}")]
    Unresolvable(PathBuf),
    #[error("path is not a directory: {0}")]
    NotADirectory(PathBuf),
    #[error("path is not valid UTF-8: {0}")]
    NotUtf8(PathBuf),
    #[error("this folder cannot be a project: {0}")]
    ForbiddenProject(PathBuf),
}

/// Folders a project may never be, or sit inside.
#[derive(Debug, Clone)]
pub struct ForbiddenRoots {
    pub home: PathBuf,
    pub data_root: PathBuf,
    /// A folder (with everything in it) that is a project even where the rules
    /// above would refuse it. See [`allow_root`].
    pub allowed: Option<PathBuf>,
}

/// The registered Automations folders, one per data folder: a workspace
/// registers (or clears) only its own, so workspaces never see each other's.
static ALLOWED: std::sync::RwLock<Vec<(PathBuf, PathBuf)>> = std::sync::RwLock::new(Vec::new());

/// Register the Automations folder of the workspace whose data folder is
/// `data_root` as allowed to be a project, or clear it with `None`. The
/// workspace does this when it starts (from its saved settings, so a settings
/// file without a folder clears an older registration) and when the user
/// changes it.
pub fn allow_root(data_root: &Path, root: Option<PathBuf>) {
    let mut table = ALLOWED
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    table.retain(|(data, _)| data != data_root);
    if let Some(root) = root {
        table.push((data_root.to_owned(), root));
    }
}

/// The folder registered for `data_root` with [`allow_root`].
#[must_use]
pub fn allowed_root(data_root: &Path) -> Option<PathBuf> {
    ALLOWED
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .find(|(data, _)| data == data_root)
        .map(|(_, root)| root.clone())
}

/// Resolve a user-chosen project folder, or refuse it.
///
/// Refuses `/` (on Windows, the root of any drive), the home folder and its
/// ancestors, and the connecto data root
/// and CLI state folders along with anything inside them. The result is the
/// real path, so a symlink cannot smuggle in a forbidden folder.
pub fn validate_project(path: &Path, roots: &ForbiddenRoots) -> Result<PathBuf, PathError> {
    let real = fs::canonicalize(path).map_err(|_| PathError::Unresolvable(path.to_owned()))?;
    if !real.is_dir() {
        return Err(PathError::NotADirectory(real));
    }
    if real.to_str().is_none() {
        return Err(PathError::NotUtf8(real));
    }
    allowed_place(real, roots)
}

/// Check a folder that does not exist yet (a clone's destination) by the
/// same rules, before anything is written there: its parent must exist, and
/// the folder it would be (the parent's real path and the name) may not be
/// forbidden. The real path it would have.
pub fn validate_new_project(path: &Path, roots: &ForbiddenRoots) -> Result<PathBuf, PathError> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(PathError::Unresolvable(path.to_owned()));
    };
    let parent = fs::canonicalize(parent).map_err(|_| PathError::Unresolvable(path.to_owned()))?;
    if !parent.is_dir() {
        return Err(PathError::NotADirectory(parent));
    }
    let real = parent.join(name);
    if real.to_str().is_none() {
        return Err(PathError::NotUtf8(real));
    }
    if fs::symlink_metadata(&real).is_ok() {
        // Something is there: the rules for an existing folder apply.
        return validate_project(&real, roots);
    }
    allowed_place(real, roots)
}

/// Whether two path components are one name on a case- and
/// normalization-insensitive volume (APFS, NTFS): decomposed, then compared
/// with full Unicode case folding (`ſ` is `s`, `ß` is `ss`).
fn same_name(a: &std::ffi::OsStr, b: &std::ffi::OsStr) -> bool {
    use unicode_normalization::UnicodeNormalization;
    let a: String = a.to_string_lossy().nfd().collect();
    let b: String = b.to_string_lossy().nfd().collect();
    unicase::eq(a.as_str(), b.as_str())
}

/// Whether `path` is `prefix` or inside it, comparing each component as a
/// case- and normalization-insensitive volume would. Used for a protected
/// folder that is not there (on such a volume `~/.ClAuDe`, made, would be
/// `~/.claude`); on a case-sensitive volume it only refuses more.
#[must_use]
pub fn within_folded(path: &Path, prefix: &Path) -> bool {
    let mut parts = path.components();
    prefix.components().all(|want| {
        parts
            .next()
            .is_some_and(|part| same_name(part.as_os_str(), want.as_os_str()))
    })
}

/// A directory's identity on disk (device and inode), through links: the
/// folder the name leads to. `None` where it is not there, or off Unix.
fn identity(path: &Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(path).ok().map(|meta| (meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Whether `path` is the protected folder `root` or inside it. A root that
/// is there is known by its identity on disk: `path` or one of its
/// ancestors is that very directory, however it is spelled (the volume's own
/// name rules decide, not Connecto's). A root that is not there is compared
/// by name with [`within_folded`]; both apply, so this only refuses more.
#[must_use]
pub fn within_protected(path: &Path, root: &Path) -> bool {
    if let Some(want) = identity(root)
        && path.ancestors().any(|step| identity(step) == Some(want))
    {
        return true;
    }
    within_folded(path, root)
}

/// The rules on a real path: not a root, home or above, Connecto's data or
/// the CLIs' and system's state, unless inside the allowed folder.
fn allowed_place(real: PathBuf, roots: &ForbiddenRoots) -> Result<PathBuf, PathError> {
    let resolve = |root: &Path| fs::canonicalize(root).unwrap_or_else(|_| root.to_owned());
    let home = resolve(&roots.home);
    // A root is refused even when home is elsewhere: on Windows, `D:\` when
    // home is on `C:`.
    if real.parent().is_none() || within_protected(&home, &real) {
        return Err(PathError::ForbiddenProject(real));
    }
    // The one exception: the user's Automations folder and what is inside it
    // (a root already vetted by the automations engine).
    if let Some(allowed) = &roots.allowed {
        let allowed = resolve(allowed);
        if real.starts_with(&allowed) {
            return Ok(real);
        }
    }
    // The CLIs' state and the system's (`Library` on macOS, `AppData` on
    // Windows).
    let inside_forbidden = std::iter::once(resolve(&roots.data_root))
        .chain(
            crate::platform::HOME_STATE
                .iter()
                .map(|name| home.join(name)),
        )
        .any(|root| within_protected(&real, &root));
    if inside_forbidden {
        return Err(PathError::ForbiddenProject(real));
    }
    Ok(real)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    use super::*;

    /// The system's state folder in the home folder.
    #[cfg(unix)]
    const SYSTEM_STATE: &str = "home/Library";
    #[cfg(windows)]
    const SYSTEM_STATE: &str = "home/AppData";

    struct Tree {
        root: PathBuf,
    }

    impl Tree {
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("connecto-path-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            for dir in [
                "home/work/project",
                "home/.claude/x",
                SYSTEM_STATE,
                "data/cli",
            ] {
                fs::create_dir_all(root.join(dir)).unwrap();
            }
            let root = fs::canonicalize(root).unwrap();
            Self { root }
        }

        fn roots(&self) -> ForbiddenRoots {
            ForbiddenRoots {
                home: self.root.join("home"),
                data_root: self.root.join("data"),
                allowed: None,
            }
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn forbidden(result: Result<PathBuf, PathError>) -> bool {
        matches!(result, Err(PathError::ForbiddenProject(_)))
    }

    #[test]
    fn an_ordinary_project_is_accepted_as_its_real_path() {
        let tree = Tree::new("ok");
        let project = tree.root.join("home/work/project");
        assert_eq!(validate_project(&project, &tree.roots()), Ok(project));
    }

    #[test]
    fn the_root_home_and_ancestors_of_home_are_refused() {
        let tree = Tree::new("home");
        let roots = tree.roots();
        assert!(forbidden(validate_project(Path::new("/"), &roots)));
        assert!(forbidden(validate_project(&roots.home, &roots)));
        assert!(forbidden(validate_project(&tree.root, &roots)));
    }

    /// On Windows `/` is the root of the current drive, which need not be
    /// home's: any drive root is refused all the same.
    #[test]
    fn the_root_of_any_drive_is_refused() {
        let tree = Tree::new("drives");
        let roots = tree.roots();
        let here = std::env::current_dir().unwrap();
        for path in [here.as_path(), tree.root.as_path()] {
            let root = path.ancestors().last().unwrap();
            assert!(forbidden(validate_project(root, &roots)), "{root:?}");
        }
    }

    #[test]
    fn cli_state_folders_and_their_contents_are_refused() {
        let tree = Tree::new("state");
        let roots = tree.roots();
        for path in ["home/.claude", "home/.claude/x", SYSTEM_STATE] {
            assert!(forbidden(validate_project(&tree.root.join(path), &roots)));
        }
    }

    #[test]
    fn the_data_root_and_its_contents_are_refused() {
        let tree = Tree::new("data");
        let roots = tree.roots();
        assert!(forbidden(validate_project(&roots.data_root, &roots)));
        assert!(forbidden(validate_project(
            &roots.data_root.join("cli"),
            &roots
        )));
    }

    #[test]
    fn the_automations_folder_and_its_contents_are_allowed_only_when_registered() {
        let tree = Tree::new("auto");
        let mut roots = tree.roots();
        let above = roots.data_root.join("automations");
        fs::create_dir_all(above.join("approve")).unwrap();
        assert!(forbidden(validate_project(&above, &roots)));
        roots.allowed = Some(above.clone());
        let above = fs::canonicalize(&above).unwrap();
        assert_eq!(validate_project(&above, &roots), Ok(above.clone()));
        let inner = fs::canonicalize(above.join("approve")).unwrap();
        assert_eq!(validate_project(&inner, &roots), Ok(inner));
        // Only that folder: its siblings stay refused, and so does a link out.
        assert!(forbidden(validate_project(
            &roots.data_root.join("cli"),
            &roots
        )));
        #[cfg(unix)]
        {
            let link = above.join("escape");
            symlink(tree.root.join("home/.claude"), &link).unwrap();
            assert!(forbidden(validate_project(&link, &roots)));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_forbidden_folder_is_refused() {
        let tree = Tree::new("link");
        let link = tree.root.join("home/work/innocent");
        symlink(tree.root.join("home/.claude"), &link).unwrap();
        assert!(forbidden(validate_project(&link, &tree.roots())));
    }

    #[test]
    fn a_missing_folder_is_refused() {
        let tree = Tree::new("missing");
        let project = tree.root.join("home/nope");
        assert!(matches!(
            validate_project(&project, &tree.roots()),
            Err(PathError::Unresolvable(_))
        ));
    }

    /// A folder that does not exist yet is checked as the folder it would
    /// be: a clone named `.codex` in the home folder is refused before
    /// anything is written, even though home itself may hold projects.
    #[test]
    fn a_new_folder_is_checked_before_it_exists() {
        let tree = Tree::new("new-folder");
        let roots = tree.roots();
        let home = tree.root.join("home");
        for refused in [".codex", ".gemini", ".claude"] {
            assert_eq!(
                validate_new_project(&home.join(refused), &roots),
                Err(PathError::ForbiddenProject(home.join(refused))),
                "{refused}"
            );
        }
        assert!(validate_new_project(&tree.root.join("data/new"), &roots).is_err());
        assert_eq!(
            validate_new_project(&home.join("work/new-app"), &roots),
            Ok(home.join("work/new-app"))
        );
        assert_eq!(
            validate_new_project(&home.join("new-app"), &roots),
            Ok(home.join("new-app"))
        );
        // The parent must exist.
        assert!(validate_new_project(&home.join("nope/app"), &roots).is_err());
        let _ = fs::remove_dir_all(&tree.root);
    }

    /// A protected folder that is not there yet, spelled in another case or
    /// Unicode form, is the same folder on a case-insensitive volume: refused.
    #[test]
    fn protected_names_are_compared_without_case_or_normalization() {
        let tree = Tree::new("folded");
        let roots = tree.roots();
        let home = tree.root.join("home");
        // Absent, in mixed case (only `.claude` exists in the tree).
        for spelled in [".CoDeX", ".GEMINI", ".ClAuDe", "LIBRARY", "library"] {
            let path = home.join(spelled);
            assert!(
                matches!(
                    validate_new_project(&path, &roots),
                    Err(PathError::ForbiddenProject(_))
                ),
                "{spelled}"
            );
        }
        // Composed and decomposed forms are one name.
        assert!(within_folded(
            Path::new("/home/caf\u{e9}/x"),
            Path::new("/home/cafe\u{301}")
        ));
        assert!(!within_folded(
            Path::new("/home/cafe"),
            Path::new("/home/caf\u{e9}")
        ));
        assert!(within_folded(Path::new("/A/b/c"), Path::new("/a/B")));
        assert!(!within_folded(Path::new("/a"), Path::new("/a/b")));
        let _ = fs::remove_dir_all(&tree.root);
    }

    /// Full case folding: the long s (`ſ`) is `s` to APFS and NTFS, though
    /// lowercase conversion leaves it as it is.
    #[test]
    fn names_are_compared_with_full_case_folding() {
        assert_eq!(
            "\u{17f}sh".to_lowercase(),
            "\u{17f}sh",
            "lowercase misses it"
        );
        assert!(within_folded(
            Path::new("/h/.\u{17f}sh/k"),
            Path::new("/h/.ssh")
        ));
        assert!(within_folded(
            Path::new("/h/STRASSE"),
            Path::new("/h/stra\u{df}e")
        ));
        let tree = Tree::new("fold");
        let roots = tree.roots();
        let home = tree.root.join("home");
        // `.gemini` is not in the tree.
        let path = home.join(".GEMINI");
        assert!(validate_new_project(&path, &roots).is_err());
        let long_s = home.join(".\u{17f}sh-not-protected");
        assert!(validate_new_project(&long_s, &roots).is_ok());
        let _ = fs::remove_dir_all(&tree.root);
    }

    /// An existing protected folder is known by its identity: a path that
    /// reaches it by another name (here a link to it, as another spelling
    /// would on a case-insensitive volume) is inside it.
    #[cfg(unix)]
    #[test]
    fn an_existing_protected_folder_is_known_by_its_identity() {
        let tree = Tree::new("identity");
        let roots = tree.roots();
        let other = tree.root.join("elsewhere");
        fs::create_dir_all(&other).unwrap();
        // `home/.claude` exists in the tree; reach it as `elsewhere/state`.
        std::os::unix::fs::symlink(tree.root.join("home/.claude"), other.join("state")).unwrap();
        assert!(within_protected(
            &other.join("state/x"),
            &tree.root.join("home/.claude")
        ));
        assert!(!within_folded(
            &other.join("state/x"),
            &tree.root.join("home/.claude")
        ));
        assert!(!within_protected(&other, &tree.root.join("home/.claude")));
        // The reviewer's case: `ſample` reaches `sample` on APFS (by
        // identity there; by folded name on a case-sensitive volume).
        let sample = tree.root.join("sample");
        fs::create_dir_all(&sample).unwrap();
        assert!(within_protected(&tree.root.join("\u{17f}ample/x"), &sample));
        let _ = roots;
        let _ = fs::remove_dir_all(&tree.root);
    }
}
