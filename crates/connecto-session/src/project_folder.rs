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
/// Refuses `/`, the home folder and its ancestors, and the connecto data root
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

    let resolve = |root: &Path| fs::canonicalize(root).unwrap_or_else(|_| root.to_owned());
    let home = resolve(&roots.home);
    if home.starts_with(&real) {
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
        .any(|root| real.starts_with(root));
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
}
