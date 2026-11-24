//! The projects one Connecto knows, for the bridge's cross-project commands
//! (ADR 0035).
//!
//! The workspace fills the directory: every project in the sidebar, with a
//! link to its bridge while it is open, and the recent ones, closed. A relay
//! that has joined the directory can then resolve `--project <name|path>` to
//! another project's bridge. Nothing here holds a project open: a link is
//! weak, and a project whose bridge has stopped counts as closed.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use crate::bridge::server::Ctx;

/// A link to an open project's bridge. Weak: it never keeps a project alive.
#[derive(Clone)]
pub struct BridgeLink(pub(crate) Weak<Ctx>);

impl std::fmt::Debug for BridgeLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BridgeLink")
    }
}

/// One project the directory knows.
#[derive(Debug, Clone)]
pub struct ProjectEntry {
    /// The project's id (its data-folder id).
    pub id: String,
    /// The name the sidebar shows.
    pub name: String,
    /// The project folder.
    pub path: PathBuf,
    /// Its bridge while it is open; `None` for a closed project.
    pub link: Option<BridgeLink>,
}

impl ProjectEntry {
    /// The open project's bridge, if it still runs.
    pub(crate) fn ctx(&self) -> Option<Arc<Ctx>> {
        let ctx = self.link.as_ref()?.0.upgrade()?;
        (!ctx.stopping()).then_some(ctx)
    }
}

/// Why `--project` names no usable project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unresolved {
    Unknown,
    /// Several projects have that name: the path tells them apart.
    Ambiguous(Vec<String>),
}

/// The projects this Connecto knows.
#[derive(Default)]
pub struct ProjectDirectory {
    entries: Mutex<Vec<ProjectEntry>>,
}

impl std::fmt::Debug for ProjectDirectory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectDirectory").finish_non_exhaustive()
    }
}

impl ProjectDirectory {
    /// Replace what the directory knows. The workspace calls it whenever its
    /// projects change.
    pub fn set(&self, entries: Vec<ProjectEntry>) {
        *self.entries.lock().unwrap_or_else(PoisonError::into_inner) = entries;
    }

    /// Everything the directory knows, in the sidebar's order.
    #[must_use]
    pub fn entries(&self) -> Vec<ProjectEntry> {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The entry with this id.
    pub(crate) fn by_id(&self, id: &str) -> Option<ProjectEntry> {
        self.entries().into_iter().find(|entry| entry.id == id)
    }

    /// The project `query` names: its exact name, its folder, or its id. A
    /// name several projects share is refused, with their paths.
    pub(crate) fn resolve(&self, query: &str) -> Result<ProjectEntry, Unresolved> {
        let entries = self.entries();
        let query = query.trim();
        if query.is_empty() {
            return Err(Unresolved::Unknown);
        }
        if let Some(found) = entries.iter().find(|entry| entry.id == query) {
            return Ok(found.clone());
        }
        // Only what reads as a path is looked up as one: a bare name is never
        // taken relative to the app's own folder, where it could name one of
        // two projects of that name without a word.
        let looks_like_path = query.contains('/')
            || query.contains(std::path::MAIN_SEPARATOR)
            || query.starts_with('~');
        if looks_like_path {
            let as_path = expand_home(query);
            // `connecto` sends a path relative to its own folder made
            // absolute; one still relative would be read from the app's
            // folder, which may name another project.
            if as_path.is_relative() {
                return Err(Unresolved::Unknown);
            }
            let canonical = std::fs::canonicalize(&as_path).ok();
            if let Some(found) = entries.iter().find(|entry| {
                entry.path == as_path
                    || canonical.as_deref().is_some_and(|path| {
                        std::fs::canonicalize(&entry.path).is_ok_and(|own| own == path)
                    })
            }) {
                return Ok(found.clone());
            }
        }
        let named = |exact: bool| -> Vec<&ProjectEntry> {
            entries
                .iter()
                .filter(|entry| {
                    let folder = entry
                        .path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if exact {
                        entry.name == query || folder == query
                    } else {
                        entry.name.eq_ignore_ascii_case(query) || folder.eq_ignore_ascii_case(query)
                    }
                })
                .collect()
        };
        let mut found = named(true);
        if found.is_empty() {
            found = named(false);
        }
        match found.as_slice() {
            [] => Err(Unresolved::Unknown),
            [one] => Ok((*one).clone()),
            many => Err(Unresolved::Ambiguous(
                many.iter()
                    .map(|entry| entry.path.display().to_string())
                    .collect(),
            )),
        }
    }
}

fn expand_home(text: &str) -> PathBuf {
    match text.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map_or_else(|| PathBuf::from(text), |home| Path::new(&home).join(rest)),
        None => PathBuf::from(text),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn entry(id: &str, name: &str, path: &str) -> ProjectEntry {
        ProjectEntry {
            id: id.into(),
            name: name.into(),
            path: PathBuf::from(path),
            link: None,
        }
    }

    #[test]
    fn a_project_is_found_by_name_folder_path_or_id_and_a_shared_name_is_refused() {
        let directory = ProjectDirectory::default();
        directory.set(vec![
            entry("a1", "connecto", "/w/connecto"),
            entry("b2", "Harbor", "/w/harbor_app"),
            entry("c3", "app", "/x/app"),
            entry("d4", "app", "/y/app"),
        ]);
        let id = |query: &str| directory.resolve(query).map(|entry| entry.id);
        assert_eq!(id("connecto"), Ok("a1".into()));
        assert_eq!(id("harbor_app"), Ok("b2".into()));
        assert_eq!(id("harbor"), Ok("b2".into()));
        assert_eq!(id("/w/harbor_app"), Ok("b2".into()));
        assert_eq!(id("b2"), Ok("b2".into()));
        assert_eq!(id("/x/app"), Ok("c3".into()));
        assert_eq!(id("nope"), Err(Unresolved::Unknown));
        assert_eq!(id(" "), Err(Unresolved::Unknown));
        assert!(matches!(id("app"), Err(Unresolved::Ambiguous(paths)) if paths.len() == 2));
    }

    /// Two projects named `src`, one of them the folder `src` under the
    /// folder this process runs in: the bare name is still ambiguous, not
    /// taken as a path relative to that folder.
    #[test]
    fn a_bare_name_is_never_resolved_against_the_current_folder() {
        let here = std::env::current_dir().unwrap().join("src");
        assert!(here.is_dir(), "the test runs in the crate's folder");
        let other = std::env::temp_dir().join(format!("mg-projects-{}/src", std::process::id()));
        std::fs::create_dir_all(&other).unwrap();
        let directory = ProjectDirectory::default();
        directory.set(vec![
            entry("near", "src", here.to_str().unwrap()),
            entry("far", "src", other.to_str().unwrap()),
        ]);
        assert!(matches!(
            directory.resolve("src"),
            Err(Unresolved::Ambiguous(paths)) if paths.len() == 2
        ));
        // Given as an absolute path, each is found.
        assert_eq!(
            directory
                .resolve(other.to_str().unwrap())
                .map(|entry| entry.id),
            Ok("far".into())
        );
        assert_eq!(
            directory
                .resolve(here.to_str().unwrap())
                .map(|entry| entry.id),
            Ok("near".into())
        );
        // A relative path is never read from the app's own folder: the
        // command that sent it ran somewhere else (`connecto` makes it
        // absolute before sending).
        assert_eq!(
            directory.resolve("./src").map(|entry| entry.id),
            Err(Unresolved::Unknown)
        );
        let _ = std::fs::remove_dir_all(other.parent().unwrap());
    }
}
