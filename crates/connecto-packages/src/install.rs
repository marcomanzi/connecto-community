//! Installing packages into a project, and taking them out again.
//!
//! The files are copied into the project's `.claude/`, `.codex/` and
//! `.agents/`. `<project>/.connecto/packages.lock` records every installed file
//! with its hash and what was merged into `.claude/settings.json`, so an update
//! removes the files a package dropped, leaves the files the user edited alone
//! (and says so), and unmerges exactly what it merged.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use connecto_core::ProjectTools;
use serde_json::{Map, Value, json};

use crate::fsx;
use crate::package::{BUILTIN_ID, Package, Platform, hash_bytes, wanted};

pub const LOCK_DIR: &str = ".connecto";
pub const LOCK_FILE: &str = "packages.lock";
const SETTINGS: &str = ".claude/settings.json";
const LOCK_VERSION: u64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedPackage {
    pub id: String,
    pub version: u32,
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LockedFile {
    /// SHA-256 of what Connecto wrote.
    hash: String,
    package: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddedKind {
    /// A key that was absent: the whole value was added.
    Value,
    /// One element appended to an array the user already had.
    Item,
}

/// One thing merged into `settings.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Added {
    path: Vec<String>,
    kind: AddedKind,
    value: Value,
}

/// What is installed, as the lockfile says.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Lock {
    pub packages: Vec<LockedPackage>,
    files: BTreeMap<String, LockedFile>,
    settings: Vec<Added>,
    /// `.claude/settings.json` did not exist before the first merge.
    settings_created: bool,
}

impl Lock {
    fn is_empty(&self) -> bool {
        self.packages.is_empty() && self.files.is_empty() && self.settings.is_empty()
    }

    fn to_text(&self) -> String {
        let packages: Vec<Value> = self
            .packages
            .iter()
            .map(|p| json!({"id": p.id, "version": p.version, "hash": p.hash}))
            .collect();
        let files: Map<String, Value> = self
            .files
            .iter()
            .map(|(path, f)| (path.clone(), json!({"hash": f.hash, "package": f.package})))
            .collect();
        let added: Vec<Value> = self
            .settings
            .iter()
            .map(|a| {
                json!({
                    "path": a.path,
                    "kind": match a.kind { AddedKind::Value => "value", AddedKind::Item => "item" },
                    "value": a.value,
                })
            })
            .collect();
        let value = json!({
            "version": LOCK_VERSION,
            "packages": packages,
            "files": files,
            "settings": { "created": self.settings_created, "added": added },
        });
        let mut text = serde_json::to_string_pretty(&value).unwrap_or_default();
        text.push('\n');
        text
    }

    fn parse(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        if value.get("version")?.as_u64()? != LOCK_VERSION {
            return None;
        }
        let packages = value
            .get("packages")?
            .as_array()?
            .iter()
            .map(|p| {
                Some(LockedPackage {
                    id: p.get("id")?.as_str()?.to_owned(),
                    version: u32::try_from(p.get("version")?.as_u64()?).ok()?,
                    hash: p.get("hash")?.as_str()?.to_owned(),
                })
            })
            .collect::<Option<Vec<_>>>()?;
        let files = value
            .get("files")?
            .as_object()?
            .iter()
            .map(|(path, f)| {
                Some((
                    path.clone(),
                    LockedFile {
                        hash: f.get("hash")?.as_str()?.to_owned(),
                        package: f.get("package")?.as_str()?.to_owned(),
                    },
                ))
            })
            .collect::<Option<BTreeMap<_, _>>>()?;
        let settings = value.get("settings")?;
        let added = settings
            .get("added")?
            .as_array()?
            .iter()
            .map(|a| {
                Some(Added {
                    path: a
                        .get("path")?
                        .as_array()?
                        .iter()
                        .map(|s| s.as_str().map(str::to_owned))
                        .collect::<Option<Vec<_>>>()?,
                    kind: match a.get("kind")?.as_str()? {
                        "value" => AddedKind::Value,
                        "item" => AddedKind::Item,
                        _ => return None,
                    },
                    value: a.get("value")?.clone(),
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            packages,
            files,
            settings: added,
            settings_created: settings.get("created")?.as_bool()?,
        })
    }
}

fn lock_path(project: &Path) -> PathBuf {
    project.join(LOCK_DIR).join(LOCK_FILE)
}

const LOCK_REL: &str = ".connecto/packages.lock";

/// The project's lockfile; empty when there is none. A lockfile that names a
/// path outside the tool folders is refused whole, before anything is touched.
pub fn read_lock(project: &Path) -> Result<Lock, String> {
    if fsx::has_link(project, LOCK_DIR) {
        return Err(format!("{LOCK_DIR} is a link: it is not used"));
    }
    let lock = match fs::read_to_string(lock_path(project)) {
        Ok(text) => Lock::parse(&text).ok_or_else(|| {
            format!("{LOCK_DIR}/{LOCK_FILE} is damaged: fix or delete it, then try again")
        })?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Lock::default()),
        Err(e) => return Err(format!("{LOCK_DIR}/{LOCK_FILE}: {e}")),
    };
    if let Some(bad) = lock.files.keys().find(|p| !fsx::tool_path(p)) {
        return Err(format!(
            "{LOCK_DIR}/{LOCK_FILE} names {bad:?}, which is not inside .claude, .codex or .agents: nothing was changed"
        ));
    }
    Ok(lock)
}

/// The writes of one install, undoable: every change first records what the
/// path held (its content, or its absence), and a failure undoes the journal
/// from the last change back to the first, so a file turned into a folder, or a
/// folder into a file, comes back as it was.
/// What a path held: its content and mode, or `None` if it did not exist.
type Kept = Option<(Vec<u8>, u32)>;

struct Txn<'a> {
    project: &'a Path,
    journal: Vec<(String, Kept)>,
}

impl<'a> Txn<'a> {
    const fn new(project: &'a Path) -> Self {
        Self {
            project,
            journal: Vec::new(),
        }
    }

    /// Record what `rel` holds now; returns its mode if it is a file.
    fn record(&mut self, rel: &str) -> Result<Option<u32>, String> {
        let file = self.project.join(rel);
        let kept = match fs::symlink_metadata(&file) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!("{rel} is a link"));
            }
            Ok(meta) if meta.is_file() => {
                let bytes = fs::read(&file).map_err(|e| format!("{rel}: {e}"))?;
                Some((bytes, crate::platform::mode(&meta)))
            }
            Ok(_) => return Err(format!("{rel} is a folder")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("{rel}: {e}")),
        };
        let mode = kept.as_ref().map(|(_, mode)| *mode);
        self.journal.push((rel.to_owned(), kept));
        Ok(mode)
    }

    /// Write one of Connecto's own files (settings, the lockfile), keeping the
    /// mode of the file it replaces.
    fn write(&mut self, rel: &str, bytes: &[u8]) -> Result<(), String> {
        let mode = self.record(rel)?;
        fsx::write_atomic(self.project, rel, bytes, mode).map_err(|e| format!("{rel}: {e}"))
    }

    /// Write a package's file, executable or not as the package has it.
    fn write_file(&mut self, rel: &str, bytes: &[u8], exec: bool) -> Result<(), String> {
        let mode = file_mode(self.record(rel)?, exec);
        fsx::write_atomic(self.project, rel, bytes, Some(mode)).map_err(|e| format!("{rel}: {e}"))
    }

    fn remove(&mut self, rel: &str) -> Result<(), String> {
        self.record(rel)?;
        fs::remove_file(self.project.join(rel)).map_err(|e| format!("{rel}: {e}"))?;
        prune_dirs(self.project, rel);
        Ok(())
    }

    /// Undo every change, newest first. Returns what could not be restored.
    fn rollback(&mut self) -> Vec<String> {
        let mut failed = Vec::new();
        while let Some((rel, kept)) = self.journal.pop() {
            let done = match kept {
                Some((bytes, mode)) => fsx::write_atomic(self.project, &rel, &bytes, Some(mode)),
                None => match fs::remove_file(self.project.join(&rel)) {
                    Ok(()) => {
                        prune_dirs(self.project, &rel);
                        Ok(())
                    }
                    // The write never landed, but its new folders may have.
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        prune_dirs(self.project, &rel);
                        Ok(())
                    }
                    Err(e) => Err(e),
                },
            };
            if let Err(e) = done {
                failed.push(format!("{rel}: {e}"));
            }
        }
        failed
    }
}

/// The mode a package file gets: that of the file it replaces (or the usual
/// one), with the executable bits following the package.
const fn file_mode(existing: Option<u32>, exec: bool) -> u32 {
    let base = match existing {
        Some(mode) => mode,
        None => 0o644,
    };
    if exec {
        // Whoever may read it may run it.
        base | ((base & 0o444) >> 2)
    } else {
        base & !0o111
    }
}

/// Whether the file at `path` can be run by its owner; `None` if it is not
/// there, or on Windows, where files have no such bit.
fn is_exec(path: &Path) -> Option<bool> {
    fs::symlink_metadata(path)
        .ok()
        .and_then(|meta| crate::platform::owner_exec(&meta))
}

/// The part of the path above `rel` that exists and is not a folder, if any.
fn file_above(project: &Path, rel: &str) -> Option<String> {
    let mut at = Path::new(rel).parent();
    let mut found = None;
    while let Some(dir) = at {
        if dir.as_os_str().is_empty() {
            break;
        }
        if fs::symlink_metadata(project.join(dir)).is_ok_and(|m| !m.is_dir()) {
            found = Some(dir.to_string_lossy().into_owned());
        }
        at = dir.parent();
    }
    found
}

/// What an install did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Outcome {
    /// Files or settings changed in the project.
    pub changed: bool,
    pub written: Vec<String>,
    pub removed: Vec<String>,
    /// Connecto's own files this install wrote or removed: the lockfile, and
    /// `.claude/settings.json` when it was merged.
    pub bookkeeping: Vec<String>,
    /// Left alone because the user changed them.
    pub skipped_modified: Vec<String>,
    pub warnings: Vec<String>,
    /// What is installed now.
    pub installed: Vec<LockedPackage>,
}

/// One file to install: its content, the package it is from, and whether it
/// is executable.
type Desired<'a> = (&'a [u8], &'a str, bool);

/// Overlay the packages' files: a later package replaces an earlier one's file.
fn desired_files(packages: &[Arc<Package>], tools: ProjectTools) -> BTreeMap<&str, Desired<'_>> {
    let mut map = BTreeMap::new();
    for package in packages {
        for (path, bytes) in &package.files {
            if path != SETTINGS && wanted(path, tools) {
                map.insert(
                    path.as_str(),
                    (
                        bytes.as_slice(),
                        package.id.as_str(),
                        package.exec.contains(path),
                    ),
                );
            }
        }
    }
    map
}

/// Every package's settings fragment, merged into one object (arrays join,
/// objects merge, a later scalar wins).
fn fragments(packages: &[Arc<Package>], warnings: &mut Vec<String>) -> Map<String, Value> {
    fn join(into: &mut Map<String, Value>, from: &Map<String, Value>) {
        for (key, value) in from {
            match (into.get_mut(key), value) {
                (Some(Value::Object(a)), Value::Object(b)) => join(a, b),
                (Some(Value::Array(a)), Value::Array(b)) => {
                    for item in b {
                        if !a.contains(item) {
                            a.push(item.clone());
                        }
                    }
                }
                _ => {
                    into.insert(key.clone(), value.clone());
                }
            }
        }
    }
    let mut all = Map::new();
    for package in packages {
        let Some(bytes) = package.files.get(SETTINGS) else {
            continue;
        };
        match serde_json::from_slice::<Value>(bytes) {
            Ok(Value::Object(object)) => join(&mut all, &object),
            _ => warnings.push(format!(
                "{}: .claude/settings.json is not a JSON object",
                package.id
            )),
        }
    }
    all
}

fn object_at<'a>(
    root: &'a mut Map<String, Value>,
    path: &[String],
) -> Option<&'a mut Map<String, Value>> {
    let mut cur = root;
    for key in path {
        cur = cur.get_mut(key)?.as_object_mut()?;
    }
    Some(cur)
}

/// Remove the empty objects left above a removed key.
fn prune_empty(root: &mut Map<String, Value>, parent: &[String]) {
    let mut path = parent.to_vec();
    while let Some(last) = path.pop() {
        let Some(holder) = object_at(root, &path) else {
            return;
        };
        if holder
            .get(&last)
            .and_then(Value::as_object)
            .is_some_and(Map::is_empty)
        {
            holder.remove(&last);
        } else {
            return;
        }
    }
}

/// Take back what an earlier install merged, leaf by leaf, where it is still
/// as it was. Whatever else the user put beside it stays.
fn unmerge(root: &mut Map<String, Value>, added: &[Added]) {
    for item in added.iter().rev() {
        let Some((last, parent)) = item.path.split_last() else {
            continue;
        };
        match item.kind {
            AddedKind::Value => {
                let Some(holder) = object_at(root, parent) else {
                    continue;
                };
                match (holder.get_mut(last), &item.value) {
                    (Some(now), recorded) if now == recorded => {
                        holder.remove(last);
                    }
                    // A list we added whole: take our items out of it.
                    (Some(Value::Array(now)), Value::Array(ours)) => {
                        now.retain(|v| !ours.contains(v));
                        if now.is_empty() {
                            holder.remove(last);
                        }
                    }
                    _ => continue,
                }
                prune_empty(root, parent);
            }
            AddedKind::Item => {
                if let Some(holder) = object_at(root, parent)
                    && let Some(Value::Array(list)) = holder.get_mut(last)
                    && let Some(at) = list.iter().position(|v| *v == item.value)
                {
                    list.remove(at);
                }
            }
        }
    }
}

/// Add `value` under `key`, which is absent, recording each leaf separately so
/// it can be taken back one by one.
fn insert_new(
    holder: &mut Map<String, Value>,
    key: &str,
    value: &Value,
    path: &mut Vec<String>,
    added: &mut Vec<Added>,
) {
    match value {
        Value::Object(object) if !object.is_empty() => {
            holder.insert(key.to_owned(), Value::Object(Map::new()));
            if let Some(Value::Object(inner)) = holder.get_mut(key) {
                for (k, v) in object {
                    path.push(k.clone());
                    insert_new(inner, k, v, path, added);
                    path.pop();
                }
            }
        }
        leaf => {
            holder.insert(key.to_owned(), leaf.clone());
            added.push(Added {
                path: path.clone(),
                kind: AddedKind::Value,
                value: leaf.clone(),
            });
        }
    }
}

/// Merge `fragment` into `root`, recording what was added. Whatever the user
/// set stays as it is.
fn merge(
    root: &mut Map<String, Value>,
    fragment: &Map<String, Value>,
    path: &mut Vec<String>,
    added: &mut Vec<Added>,
    warnings: &mut Vec<String>,
) {
    for (key, value) in fragment {
        path.push(key.clone());
        match root.get_mut(key) {
            None => insert_new(root, key, value, path, added),
            Some(Value::Object(mine)) if value.is_object() => {
                if let Some(theirs) = value.as_object() {
                    merge(mine, theirs, path, added, warnings);
                }
            }
            Some(Value::Array(mine)) if value.is_array() => {
                for item in value.as_array().into_iter().flatten() {
                    if !mine.contains(item) {
                        mine.push(item.clone());
                        added.push(Added {
                            path: path.clone(),
                            kind: AddedKind::Item,
                            value: item.clone(),
                        });
                    }
                }
            }
            Some(existing) if existing == value => {}
            Some(_) => warnings.push(format!(
                "settings.json: kept your `{}`; a package sets another value",
                path.join(".")
            )),
        }
        path.pop();
    }
}

/// The settings step. Returns whether the file changed.
fn apply_settings(
    txn: &mut Txn<'_>,
    lock: &mut Lock,
    fragment: &Map<String, Value>,
    wanted_settings: bool,
    warnings: &mut Vec<String>,
) -> Result<bool, String> {
    let project = txn.project;
    let file = project.join(SETTINGS);
    if lock.settings.is_empty() && (fragment.is_empty() || !wanted_settings) {
        return Ok(false);
    }
    if fsx::has_link(project, SETTINGS) {
        warnings.push("settings.json is a link: left alone".to_owned());
        return Ok(false);
    }
    let existing = match fs::read_to_string(&file) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("{SETTINGS}: {e}")),
    };
    let mut root = match &existing {
        Some(text) => match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(object)) => object,
            _ => {
                warnings.push(
                    ".claude/settings.json is not a JSON object: package settings not merged"
                        .to_owned(),
                );
                return Ok(false);
            }
        },
        None => Map::new(),
    };
    let before = root.clone();
    unmerge(&mut root, &lock.settings);
    // Only Connecto's own keys were in the file: the file is Connecto's.
    let ours_only = existing.is_none() || (lock.settings_created && root.is_empty());
    lock.settings.clear();
    if wanted_settings {
        let mut added = Vec::new();
        merge(&mut root, fragment, &mut Vec::new(), &mut added, warnings);
        lock.settings = added;
    }
    lock.settings_created = ours_only && !lock.settings.is_empty();
    if root.is_empty() && existing.is_some() && ours_only {
        txn.remove(SETTINGS)?;
        return Ok(true);
    }
    // The user's own file stays, even when it is empty now: write it back so
    // what we took out is really out.
    if (root.is_empty() && existing.is_none()) || (existing.is_some() && root == before) {
        return Ok(false);
    }
    let mut text = serde_json::to_string_pretty(&Value::Object(root)).map_err(|e| e.to_string())?;
    text.push('\n');
    txn.write(SETTINGS, text.as_bytes())?;
    Ok(true)
}

/// Remove the empty folders above a removed file, up to the project.
fn prune_dirs(project: &Path, rel: &str) {
    let mut at = Path::new(rel).parent().map(Path::to_owned);
    while let Some(dir) = at {
        if dir.as_os_str().is_empty() || fs::remove_dir(project.join(&dir)).is_err() {
            break;
        }
        at = dir.parent().map(Path::to_owned);
    }
}

/// Make the project hold exactly `packages` (already in install order, bases
/// first) for these tools. An empty list takes everything out.
///
/// Nothing is touched until the lockfile and every package path have been
/// checked. If a step fails, every file already written or removed is put back
/// (and so is the lockfile), so a failure never leaves a half-applied install.
pub fn apply(
    project: &Path,
    packages: &[Arc<Package>],
    tools: ProjectTools,
) -> Result<Outcome, String> {
    apply_for(project, packages, tools, Platform::CURRENT)
}

/// [`apply`] as `platform` does it.
fn apply_for(
    project: &Path,
    packages: &[Arc<Package>],
    tools: ProjectTools,
    platform: Platform,
) -> Result<Outcome, String> {
    let lock = read_lock(project)?;
    for package in packages {
        if let Some(bad) = package.files.keys().find(|p| !fsx::tool_path(p)) {
            return Err(format!(
                "package {} has the path {bad:?}, which is not inside .claude, .codex or .agents: nothing was changed",
                package.id
            ));
        }
    }
    let mut txn = Txn::new(project);
    match apply_inner(project, packages, tools, platform, lock, &mut txn) {
        Ok(out) => Ok(out),
        Err(why) => {
            let failed = txn.rollback();
            Err(if failed.is_empty() {
                format!("{why}; nothing was changed")
            } else {
                format!("{why}; could not restore: {}", failed.join(", "))
            })
        }
    }
}

fn apply_inner(
    project: &Path,
    packages: &[Arc<Package>],
    tools: ProjectTools,
    platform: Platform,
    mut lock: Lock,
    txn: &mut Txn<'_>,
) -> Result<Outcome, String> {
    let mut out = Outcome::default();
    let desired = desired_files(packages, tools);
    let fragment = fragments(packages, &mut out.warnings);
    let wants_claude = connecto_core::ProjectTools::allows(tools, connecto_core::ToolKind::Claude);
    let has_builtin = packages.iter().any(|p| p.builtin && p.id == BUILTIN_ID);

    // 1. Files the packages no longer have. A built-in file that only another
    // platform installs is not one of them while the built-in package is still
    // here for the same tools: it stays, with its lockfile entry, so a project
    // used from a Mac and a PC is not changed back and forth.
    let dropped: Vec<String> = lock
        .files
        .iter()
        .filter(|(path, locked)| {
            !desired.contains_key(path.as_str())
                && !(has_builtin
                    && locked.package == BUILTIN_ID
                    && platform.foreign(path)
                    && wanted(path, tools))
        })
        .map(|(path, _)| path.clone())
        .collect();
    for path in dropped {
        let Some(locked) = lock.files.get(&path).cloned() else {
            continue;
        };
        if fsx::has_link(project, &path) {
            out.warnings
                .push(format!("{path}: not removed (unsafe path)"));
            lock.files.remove(&path);
            continue;
        }
        match fs::read(project.join(&path)) {
            Ok(bytes) if hash_bytes(&bytes) == locked.hash => {
                txn.remove(&path)?;
                out.removed.push(path.clone());
            }
            Ok(_) => out.skipped_modified.push(path.clone()),
            Err(_) => {}
        }
        lock.files.remove(&path);
    }

    // 2. Files the packages have.
    let mut blocked: BTreeMap<String, usize> = BTreeMap::new();
    for (path, (bytes, package, exec)) in &desired {
        let new_hash = hash_bytes(bytes);
        if fsx::has_link(project, path) {
            out.warnings
                .push(format!("{path}: not installed (unsafe path)"));
            continue;
        }
        // A file the user kept (their own, or one of ours they edited) where
        // the package now needs a folder: it stays, and what would go under it
        // is left out, instead of the whole install failing on it.
        if let Some(file) = file_above(project, path) {
            *blocked.entry(file).or_default() += 1;
            lock.files.remove(*path);
            continue;
        }
        let current = fs::read(project.join(path)).ok();
        let locked = lock.files.get(*path).cloned();
        // The content is ours but the mode is not what the package has (a
        // script installed before modes were kept, or a chmod in the package).
        let mode_differs = is_exec(&project.join(path)).is_some_and(|now| now != *exec);
        let write = match (&current, &locked) {
            (None, _) => true,
            (Some(now), Some(l)) if hash_bytes(now) == l.hash => l.hash != new_hash || mode_differs,
            // Already the same: adopt it.
            (Some(now), _) if hash_bytes(now) == new_hash => mode_differs,
            (Some(_), _) => {
                out.skipped_modified.push((*path).to_owned());
                continue;
            }
        };
        if write {
            txn.write_file(path, bytes, *exec)?;
            out.written.push((*path).to_owned());
        }
        lock.files.insert(
            (*path).to_owned(),
            LockedFile {
                hash: new_hash,
                package: (*package).to_owned(),
            },
        );
    }

    for (file, count) in blocked {
        out.warnings.push(format!(
            "{file}: this file is yours (or you changed it) and a package now needs a folder \
             there, so {count} file(s) under {file}/ were not installed; move or delete it"
        ));
        if !out.skipped_modified.contains(&file) {
            out.skipped_modified.push(file);
        }
    }

    // 3. settings.json.
    let settings_changed =
        apply_settings(txn, &mut lock, &fragment, wants_claude, &mut out.warnings)?;

    lock.packages = packages
        .iter()
        .map(|p| LockedPackage {
            id: p.id.clone(),
            version: p.manifest.version,
            hash: p.hash.clone(),
        })
        .collect();
    // 4. The lockfile, last: until it is written the old records stand.
    if lock.is_empty() {
        if lock_path(project).exists() {
            txn.remove(LOCK_REL)?;
        }
    } else {
        txn.write(LOCK_REL, lock.to_text().as_bytes())?;
    }
    out.bookkeeping.push(LOCK_REL.to_owned());
    if settings_changed {
        out.bookkeeping.push(SETTINGS.to_owned());
    }
    out.changed = !out.written.is_empty() || !out.removed.is_empty() || settings_changed;
    out.installed = lock.packages;
    Ok(out)
}

/// Take out every package.
pub fn remove_all(project: &Path) -> Result<Outcome, String> {
    apply(project, &[], ProjectTools::Both)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use connecto_hook::package::Manifest;

    fn project(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mg-install-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn package(id: &str, version: u32, files: &[(&str, &str)]) -> Arc<Package> {
        let files: BTreeMap<String, Vec<u8>> = files
            .iter()
            .map(|(p, t)| ((*p).to_owned(), t.as_bytes().to_vec()))
            .collect();
        let manifest = Manifest {
            name: id.to_owned(),
            description: String::new(),
            version,
            extends: None,
        };
        let exec = BTreeSet::new();
        let hash = crate::package::content_hash(&files, &exec, None);
        Arc::new(Package {
            id: id.to_owned(),
            manifest,
            files,
            exec,
            hash,
            builtin: false,
        })
    }

    /// The same package with these files executable.
    fn with_exec(package: &Arc<Package>, exec: &[&str]) -> Arc<Package> {
        let mut package = Package::clone(package);
        package.exec = exec.iter().map(|p| (*p).to_owned()).collect();
        package.hash = crate::package::content_hash(&package.files, &package.exec, None);
        Arc::new(package)
    }

    /// A project used from a Mac and from a PC (shared through git, say): the
    /// PC adds the PowerShell twins, and from then on neither platform changes
    /// the project, whichever installs first or last.
    #[test]
    fn a_project_shared_by_a_mac_and_a_pc_settles_after_one_install_on_each() {
        use crate::package::{Variant, WINDOWS_ONLY, builtin_for};
        for (variant, tools) in [
            (Variant::Claude, ProjectTools::Claude),
            (Variant::Both, ProjectTools::Both),
        ] {
            let mac = Arc::new(builtin_for(variant, Platform::Unix).unwrap());
            let pc = Arc::new(builtin_for(variant, Platform::Windows).unwrap());
            let mut settled: Option<Vec<u8>> = None;
            for (name, first, second) in [
                (
                    "mac-first",
                    (Platform::Unix, &mac),
                    (Platform::Windows, &pc),
                ),
                ("pc-first", (Platform::Windows, &pc), (Platform::Unix, &mac)),
            ] {
                let dir = project(&format!("shared-{variant:?}-{name}"));
                let on = |(platform, package): (Platform, &Arc<Package>)| {
                    apply_for(&dir, std::slice::from_ref(package), tools, platform).unwrap()
                };
                let lock = || fs::read(dir.join(LOCK_REL)).unwrap();
                on(first);
                let out = on(second);
                assert!(
                    out.removed.is_empty() && out.skipped_modified.is_empty(),
                    "{out:?}"
                );
                let twins: Vec<_> = WINDOWS_ONLY.iter().map(|t| (*t).to_owned()).collect();
                if second.0 == Platform::Windows {
                    assert_eq!(out.written, twins, "the PC adds the twins and nothing else");
                } else {
                    assert!(!out.changed, "the Mac keeps the twins: {out:?}");
                }
                for twin in WINDOWS_ONLY {
                    assert!(dir.join(twin).is_file(), "{twin}");
                }
                let after = lock();
                // Each again, in turn: nothing moves, not even the lockfile.
                for side in [first, second, first, second] {
                    let out = on(side);
                    assert!(!out.changed && out.skipped_modified.is_empty(), "{out:?}");
                    assert_eq!(lock(), after);
                }
                // Both orders end in the same lockfile.
                if let Some(other) = &settled {
                    assert_eq!(&after, other);
                }
                settled = Some(after);
                // Once the built-in package is gone, the Mac takes the twins out too.
                apply_for(&dir, &[], tools, Platform::Unix).unwrap();
                for twin in WINDOWS_ONLY {
                    assert!(!dir.join(twin).exists(), "{twin}");
                }
                assert!(!dir.join(LOCK_REL).exists());
                let _ = fs::remove_dir_all(&dir);
            }
        }
    }

    /// The Mac keeps the twins only for the built-in package and only for the
    /// tools that take them; a package of the user's with a file of that name
    /// is installed and removed like any other.
    #[test]
    fn only_the_builtin_twins_for_claude_are_kept_on_a_mac() {
        use crate::package::{STATUSLINE_PS1, Variant, builtin_for};
        let dir = project("twins-codex");
        let pc = Arc::new(builtin_for(Variant::Both, Platform::Windows).unwrap());
        apply_for(&dir, &[pc], ProjectTools::Both, Platform::Windows).unwrap();
        assert!(dir.join(STATUSLINE_PS1).is_file());
        // The project is switched to Codex on the Mac: Claude's files go,
        // twins included.
        let codex = Arc::new(builtin_for(Variant::Codex, Platform::Unix).unwrap());
        let out = apply_for(&dir, &[codex], ProjectTools::Codex, Platform::Unix).unwrap();
        assert!(out.removed.contains(&STATUSLINE_PS1.to_owned()), "{out:?}");
        assert!(!dir.join(STATUSLINE_PS1).exists());
        let _ = fs::remove_dir_all(&dir);

        let dir = project("twins-user");
        let mine = package("mine", 1, &[(STATUSLINE_PS1, "mine")]);
        apply_for(&dir, &[mine], ProjectTools::Both, Platform::Unix).unwrap();
        assert_eq!(read(&dir, STATUSLINE_PS1), "mine");
        let mac = Arc::new(builtin_for(Variant::Both, Platform::Unix).unwrap());
        let out = apply_for(&dir, &[mac], ProjectTools::Both, Platform::Unix).unwrap();
        assert!(out.removed.contains(&STATUSLINE_PS1.to_owned()), "{out:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    fn mode(project: &Path, rel: &str) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(project.join(rel))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }

    /// Windows files have no mode: a script installs like any file, and a
    /// change of the executable flag alone rewrites nothing.
    #[cfg(windows)]
    #[test]
    fn a_script_installs_and_the_executable_flag_alone_rewrites_nothing() {
        let dir = project("exec");
        let files = [
            (".claude/hooks/run.sh", "#!/bin/sh\n"),
            (".claude/a.md", "a"),
        ];
        let scripted = with_exec(&package("p", 1, &files), &[".claude/hooks/run.sh"]);
        let out = apply(&dir, std::slice::from_ref(&scripted), ProjectTools::Both).unwrap();
        assert_eq!(out.written.len(), 2);
        assert_eq!(read(&dir, ".claude/hooks/run.sh"), "#!/bin/sh\n");
        let out = apply(&dir, &[scripted], ProjectTools::Both).unwrap();
        assert!(!out.changed, "{out:?}");
        let out = apply(&dir, &[package("p", 2, &files)], ProjectTools::Both).unwrap();
        assert!(!out.changed && out.skipped_modified.is_empty(), "{out:?}");
        assert_eq!(out.installed[0].version, 2);
    }

    #[cfg(unix)]
    #[test]
    fn a_script_is_installed_executable_and_a_mode_change_is_an_update() {
        let dir = project("exec");
        let files = [
            (".claude/hooks/run.sh", "#!/bin/sh\n"),
            (".claude/a.md", "a"),
        ];
        let scripted = with_exec(&package("p", 1, &files), &[".claude/hooks/run.sh"]);
        let out = apply(&dir, std::slice::from_ref(&scripted), ProjectTools::Both).unwrap();
        assert_eq!(out.written.len(), 2);
        assert_eq!(mode(&dir, ".claude/hooks/run.sh") & 0o111, 0o111);
        assert_eq!(mode(&dir, ".claude/a.md") & 0o111, 0);
        // The same again changes nothing.
        let out = apply(&dir, &[scripted], ProjectTools::Both).unwrap();
        assert!(!out.changed, "{out:?}");
        // The package drops the bit: same bytes, yet the file is updated and
        // not taken for one the user changed.
        let out = apply(&dir, &[package("p", 2, &files)], ProjectTools::Both).unwrap();
        assert_eq!(out.written, [".claude/hooks/run.sh"]);
        assert!(out.skipped_modified.is_empty());
        assert_eq!(mode(&dir, ".claude/hooks/run.sh") & 0o111, 0);
        assert_eq!(read(&dir, ".claude/hooks/run.sh"), "#!/bin/sh\n");
        // And back, as for a script installed before modes were kept.
        let scripted = with_exec(&package("p", 3, &files), &[".claude/hooks/run.sh"]);
        let out = apply(&dir, &[scripted], ProjectTools::Both).unwrap();
        assert_eq!(out.written, [".claude/hooks/run.sh"]);
        assert_eq!(mode(&dir, ".claude/hooks/run.sh") & 0o111, 0o111);
    }

    #[cfg(unix)]
    #[test]
    fn a_script_the_user_edited_keeps_its_content_and_its_mode() {
        let dir = project("exec-edited");
        let files = [(".claude/run.sh", "one")];
        apply(&dir, &[package("p", 1, &files)], ProjectTools::Both).unwrap();
        fs::write(dir.join(".claude/run.sh"), "mine").unwrap();
        let scripted = with_exec(&package("p", 2, &files), &[".claude/run.sh"]);
        let out = apply(&dir, &[scripted], ProjectTools::Both).unwrap();
        assert_eq!(out.skipped_modified, [".claude/run.sh"]);
        assert_eq!(mode(&dir, ".claude/run.sh") & 0o111, 0);
        assert_eq!(read(&dir, ".claude/run.sh"), "mine");
    }

    #[cfg(unix)]
    #[test]
    fn a_rollback_puts_back_the_mode_of_a_file_it_rewrote() {
        let dir = project("exec-rollback");
        let files = [(".claude/run.sh", "one")];
        apply(&dir, &[package("p", 1, &files)], ProjectTools::Both).unwrap();
        fs::create_dir_all(dir.join(".claude/z")).unwrap();
        fs::write(dir.join(".claude/z/keep"), "k").unwrap();
        let v2 = with_exec(
            &package("p", 2, &[(".claude/run.sh", "one"), (".claude/z", "f")]),
            &[".claude/run.sh"],
        );
        assert!(apply(&dir, &[v2], ProjectTools::Both).is_err());
        assert_eq!(mode(&dir, ".claude/run.sh") & 0o111, 0);
    }

    #[test]
    fn an_edited_file_where_a_folder_is_now_needed_is_skipped_not_fatal() {
        let dir = project("file-above");
        let v1 = package("p", 1, &[(".claude/a", "one"), (".claude/c.md", "c1")]);
        apply(&dir, &[v1], ProjectTools::Both).unwrap();
        fs::write(dir.join(".claude/a"), "mine").unwrap();
        let v2 = || {
            package(
                "p",
                2,
                &[
                    (".claude/a/b", "b"),
                    (".claude/a/deep/c", "c"),
                    (".claude/c.md", "c2"),
                ],
            )
        };
        // Twice: the second time the lockfile no longer knows `.claude/a`.
        for round in 0..2 {
            let out = apply(&dir, &[v2()], ProjectTools::Both).unwrap();
            assert_eq!(out.skipped_modified, [".claude/a"], "round {round}");
            let warning = out.warnings.iter().find(|w| w.starts_with(".claude/a:"));
            assert!(
                warning.is_some_and(|w| w.contains("2 file(s)") && w.contains("move or delete")),
                "round {round}: {:?}",
                out.warnings
            );
            assert_eq!(read(&dir, ".claude/a"), "mine");
            assert_eq!(read(&dir, ".claude/c.md"), "c2", "the rest is installed");
            assert_eq!(out.installed[0].version, 2);
        }
        // Once the file is out of the way, the files under it arrive.
        fs::remove_file(dir.join(".claude/a")).unwrap();
        let out = apply(&dir, &[v2()], ProjectTools::Both).unwrap();
        assert!(out.skipped_modified.is_empty() && out.warnings.is_empty());
        assert_eq!(read(&dir, ".claude/a/b"), "b");
        assert_eq!(read(&dir, ".claude/a/deep/c"), "c");
        // And they go again with the package, leaving nothing behind.
        remove_all(&dir).unwrap();
        assert!(!dir.join(".claude").exists());
    }

    fn read(project: &Path, rel: &str) -> String {
        fs::read_to_string(project.join(rel)).unwrap()
    }

    #[test]
    fn install_update_and_removal_of_dropped_files() {
        let dir = project("update");
        let v1 = package(
            "p",
            1,
            &[
                (".claude/skills/a/SKILL.md", "a1"),
                (".claude/commands/old.md", "old"),
                (".codex/agents/x.toml", "x"),
            ],
        );
        let out = apply(&dir, &[v1], ProjectTools::Both).unwrap();
        assert!(out.changed);
        assert_eq!(out.written.len(), 3);
        assert_eq!(read(&dir, ".claude/skills/a/SKILL.md"), "a1");
        assert!(lock_path(&dir).is_file());
        // The same again changes nothing.
        let v1 = package(
            "p",
            1,
            &[
                (".claude/skills/a/SKILL.md", "a1"),
                (".claude/commands/old.md", "old"),
                (".codex/agents/x.toml", "x"),
            ],
        );
        assert!(!apply(&dir, &[v1], ProjectTools::Both).unwrap().changed);
        // Version 2 changes one file and drops two.
        let v2 = package("p", 2, &[(".claude/skills/a/SKILL.md", "a2")]);
        let out = apply(&dir, &[v2], ProjectTools::Both).unwrap();
        assert_eq!(out.written, [".claude/skills/a/SKILL.md"]);
        assert_eq!(out.removed.len(), 2);
        assert_eq!(read(&dir, ".claude/skills/a/SKILL.md"), "a2");
        assert!(
            !dir.join(".claude/commands").exists(),
            "empty folder pruned"
        );
        assert!(!dir.join(".codex").exists());
        assert_eq!(out.installed[0].version, 2);
        // No package: everything goes, including the lockfile.
        let out = remove_all(&dir).unwrap();
        assert_eq!(out.removed, [".claude/skills/a/SKILL.md"]);
        assert!(!dir.join(".connecto").exists());
        assert!(!dir.join(".claude").exists());
    }

    #[test]
    fn a_file_the_user_edited_is_skipped_and_reported() {
        let dir = project("modified");
        let v1 = package("p", 1, &[(".claude/a.md", "one"), (".claude/b.md", "b")]);
        apply(&dir, &[v1], ProjectTools::Both).unwrap();
        fs::write(dir.join(".claude/a.md"), "mine").unwrap();
        let v2 = package("p", 2, &[(".claude/a.md", "two")]);
        let out = apply(&dir, &[v2], ProjectTools::Both).unwrap();
        assert_eq!(out.skipped_modified, [".claude/a.md"]);
        assert_eq!(read(&dir, ".claude/a.md"), "mine");
        assert_eq!(out.removed, [".claude/b.md"]);
        // Still reported on the next check, still untouched.
        let v2 = package("p", 2, &[(".claude/a.md", "two")]);
        let out = apply(&dir, &[v2], ProjectTools::Both).unwrap();
        assert_eq!(out.skipped_modified, [".claude/a.md"]);
        // A dropped file the user edited stays too.
        let out = apply(&dir, &[], ProjectTools::Both).unwrap();
        assert_eq!(out.skipped_modified, [".claude/a.md"]);
        assert_eq!(read(&dir, ".claude/a.md"), "mine");
        // A file that was there before and differs is not Connecto's to replace.
        fs::write(dir.join(".claude/c.md"), "theirs").unwrap();
        let v3 = package("p", 3, &[(".claude/c.md", "ours")]);
        let out = apply(&dir, &[v3], ProjectTools::Both).unwrap();
        assert_eq!(out.skipped_modified, [".claude/c.md"]);
        assert_eq!(read(&dir, ".claude/c.md"), "theirs");
    }

    #[test]
    fn a_later_package_overlays_an_earlier_one() {
        let dir = project("overlay");
        let base = package(
            "base",
            1,
            &[(".claude/a.md", "base"), (".claude/b.md", "b")],
        );
        let top = package("top", 1, &[(".claude/a.md", "top")]);
        apply(&dir, &[base, top], ProjectTools::Both).unwrap();
        assert_eq!(read(&dir, ".claude/a.md"), "top");
        assert_eq!(read(&dir, ".claude/b.md"), "b");
        let lock = read_lock(&dir).unwrap();
        assert_eq!(lock.packages.len(), 2);
        assert_eq!(lock.files[".claude/a.md"].package, "top");
    }

    #[test]
    fn only_the_tool_folders_that_match_the_tools_setting_are_installed() {
        let files = [
            (".claude/a.md", "c"),
            (".codex/b.toml", "x"),
            (".agents/s/SKILL.md", "s"),
        ];
        let dir = project("tools");
        apply(&dir, &[package("p", 1, &files)], ProjectTools::Claude).unwrap();
        assert!(dir.join(".claude/a.md").is_file());
        assert!(!dir.join(".codex").exists() && !dir.join(".agents").exists());
        // Switching to Codex removes the Claude files and adds the others.
        let out = apply(&dir, &[package("p", 1, &files)], ProjectTools::Codex).unwrap();
        assert!(!dir.join(".claude").exists());
        assert_eq!(out.written.len(), 2);
        assert_eq!(out.removed, [".claude/a.md"]);
    }

    #[test]
    fn settings_are_merged_without_replacing_the_users_and_unmerged_exactly() {
        let dir = project("settings");
        fs::create_dir_all(dir.join(".claude")).unwrap();
        fs::write(
            dir.join(".claude/settings.json"),
            r#"{"statusLine":{"type":"command","command":"mine"},"enabledPlugins":{"mine@x":true},"permissions":{"allow":["Bash(ls)"]}}"#,
        )
        .unwrap();
        let fragment = r#"{
            "enabledPlugins": {"codex@openai": true},
            "extraKnownMarketplaces": {"openai": {"source": "github"}},
            "statusLine": {"type": "command", "command": "theirs"},
            "permissions": {"allow": ["Bash(ls)", "Bash(pwd)"]}
        }"#;
        let p = package("p", 1, &[(".claude/settings.json", fragment)]);
        let out = apply(&dir, std::slice::from_ref(&p), ProjectTools::Both).unwrap();
        assert!(out.changed);
        assert!(
            out.warnings.iter().any(|w| w.contains("statusLine")),
            "{:?}",
            out.warnings
        );
        let now: Value = serde_json::from_str(&read(&dir, ".claude/settings.json")).unwrap();
        assert_eq!(now["statusLine"]["command"], "mine", "never replaced");
        assert_eq!(now["enabledPlugins"]["mine@x"], true);
        assert_eq!(now["enabledPlugins"]["codex@openai"], true);
        assert!(now["extraKnownMarketplaces"]["openai"].is_object());
        assert_eq!(
            now["permissions"]["allow"],
            json!(["Bash(ls)", "Bash(pwd)"])
        );
        // Again: nothing changes, nothing is recorded twice.
        assert!(!apply(&dir, &[p], ProjectTools::Both).unwrap().changed);
        // The user adds a key of their own meanwhile.
        let mut value = now.clone();
        value["env"] = json!({"A": "1"});
        fs::write(dir.join(".claude/settings.json"), value.to_string()).unwrap();
        // Out again: exactly what was added goes, the rest stays.
        let out = apply(&dir, &[], ProjectTools::Both).unwrap();
        assert!(out.changed);
        let back: Value = serde_json::from_str(&read(&dir, ".claude/settings.json")).unwrap();
        assert_eq!(
            back,
            json!({
                "statusLine": {"type": "command", "command": "mine"},
                "enabledPlugins": {"mine@x": true},
                "permissions": {"allow": ["Bash(ls)"]},
                "env": {"A": "1"},
            })
        );
        assert!(!dir.join(".connecto").exists());
    }

    #[test]
    fn a_settings_file_that_connecto_created_goes_with_the_package() {
        let dir = project("created");
        let p = package(
            "p",
            1,
            &[(
                ".claude/settings.json",
                r#"{"enabledPlugins":{"a@b":true}}"#,
            )],
        );
        apply(&dir, &[p], ProjectTools::Both).unwrap();
        assert!(dir.join(".claude/settings.json").is_file());
        let p2 = package("p", 2, &[(".claude/a.md", "a")]);
        apply(&dir, &[p2], ProjectTools::Both).unwrap();
        assert!(!dir.join(".claude/settings.json").exists());
        assert!(dir.join(".claude/a.md").is_file());
        // A Codex-only project gets no settings.json.
        let dir = project("created-codex");
        let p = package(
            "p",
            1,
            &[
                (".claude/settings.json", r#"{"statusLine":{}}"#),
                (".codex/a.toml", "x"),
            ],
        );
        apply(&dir, &[p], ProjectTools::Codex).unwrap();
        assert!(!dir.join(".claude").exists());
    }

    #[test]
    fn a_broken_settings_file_is_reported_not_overwritten() {
        let dir = project("broken");
        fs::create_dir_all(dir.join(".claude")).unwrap();
        fs::write(dir.join(".claude/settings.json"), "{ nope").unwrap();
        let p = package(
            "p",
            1,
            &[
                (".claude/settings.json", r#"{"a":1}"#),
                (".claude/x.md", "x"),
            ],
        );
        let out = apply(&dir, &[p], ProjectTools::Both).unwrap();
        assert!(out.warnings.iter().any(|w| w.contains("not a JSON object")));
        assert_eq!(read(&dir, ".claude/settings.json"), "{ nope");
        assert!(dir.join(".claude/x.md").is_file());
    }

    #[test]
    fn a_damaged_lockfile_stops_the_install_and_links_are_not_followed() {
        let dir = project("lock");
        fs::create_dir_all(dir.join(".connecto")).unwrap();
        fs::write(dir.join(".connecto/packages.lock"), "garbage").unwrap();
        let p = package("p", 1, &[(".claude/a.md", "a")]);
        assert!(apply(&dir, std::slice::from_ref(&p), ProjectTools::Both).is_err());
        assert!(!dir.join(".claude").exists());
        fs::remove_file(dir.join(".connecto/packages.lock")).unwrap();
        let outside = project("outside");
        crate::platform::link_dir(&outside, &dir.join(".claude")).unwrap();
        let out = apply(&dir, &[p], ProjectTools::Both).unwrap();
        assert!(out.warnings.iter().any(|w| w.contains("unsafe")));
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
    }

    fn lock_with(dir: &Path, file_key: &str) {
        fs::create_dir_all(dir.join(".connecto")).unwrap();
        let hash = hash_bytes(b"victim");
        let text = json!({
            "version": 1,
            "packages": [],
            "files": { file_key: {"hash": hash, "package": "p"} },
            "settings": {"created": false, "added": []},
        });
        fs::write(dir.join(".connecto/packages.lock"), text.to_string()).unwrap();
    }

    #[test]
    fn a_lockfile_path_outside_the_tool_folders_deletes_nothing() {
        let victim = project("traversal").join("victim");
        let absolute = victim.display().to_string();
        let mut keys = vec![
            "../victim".to_owned(),
            "/tmp/victim".to_owned(),
            ".claude/../../victim".to_owned(),
            "other/x".to_owned(),
            ".connecto/packages.lock".to_owned(),
            // Escapes on Windows, odd names elsewhere: refused everywhere.
            r"..\victim".to_owned(),
            r"\\localhost\C$\victim".to_owned(),
            absolute.clone(),
        ];
        if cfg!(windows) {
            keys.extend([
                r".claude\..\..\victim".to_owned(),
                r".claude/..\..\victim".to_owned(),
                ".claude/.. /.. /victim".to_owned(),
                format!(r"\\?\{absolute}"),
                format!(".claude/{absolute}"),
            ]);
        }
        for key in &keys {
            let base = project("traversal");
            let dir = base.join("project");
            fs::create_dir_all(&dir).unwrap();
            fs::write(base.join("victim"), "victim").unwrap();
            lock_with(&dir, key);
            let err = apply(&dir, &[], ProjectTools::Both).unwrap_err();
            assert!(err.contains("not inside"), "{key}: {err}");
            assert_eq!(
                fs::read_to_string(base.join("victim")).unwrap(),
                "victim",
                "{key}"
            );
            assert!(
                dir.join(".connecto/packages.lock").is_file(),
                "{key}: lock untouched"
            );
        }
    }

    #[test]
    fn a_package_path_outside_the_tool_folders_is_refused_before_anything_is_written() {
        let dir = project("badsource");
        let good = package("p", 1, &[(".claude/a.md", "a")]);
        let bad = package("q", 1, &[("../escape.md", "x")]);
        let err = apply(&dir, &[good, bad], ProjectTools::Both).unwrap_err();
        assert!(err.contains("not inside"), "{err}");
        assert!(!dir.join(".claude").exists() && !dir.join(".connecto").exists());
        assert!(!dir.parent().unwrap().join("escape.md").exists());
    }

    #[cfg(windows)]
    #[test]
    fn a_package_path_that_escapes_on_windows_is_refused_before_anything_is_written() {
        let dir = project("badsource-windows");
        let outside = dir
            .parent()
            .unwrap()
            .join("escape.md")
            .display()
            .to_string();
        for bad in [
            r"..\escape.md".to_owned(),
            r".claude\..\..\escape.md".to_owned(),
            r".claude/..\..\escape.md".to_owned(),
            ".claude/.. /.. /escape.md".to_owned(),
            outside.clone(),
            format!(".claude/{outside}"),
            r"\\localhost\C$\escape.md".to_owned(),
            ".claude/settings.json:stream".to_owned(),
            ".claude/nul".to_owned(),
        ] {
            let good = package("p", 1, &[(".claude/a.md", "a")]);
            let bad_package = package("q", 1, &[(bad.as_str(), "x")]);
            let err = apply(&dir, &[good, bad_package], ProjectTools::Both).unwrap_err();
            assert!(err.contains("not inside"), "{bad}: {err}");
            assert!(!dir.join(".claude").exists() && !dir.join(".connecto").exists());
            assert!(!Path::new(&outside).exists(), "{bad}");
        }
    }

    #[test]
    fn a_linked_connecto_folder_or_parent_is_refused_and_nothing_follows_it() {
        let base = project("linked");
        let (dir, outside) = (base.join("project"), base.join("outside"));
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&outside).unwrap();
        crate::platform::link_dir(&outside, &dir.join(".connecto")).unwrap();
        let p = package("p", 1, &[(".claude/a.md", "a")]);
        assert!(apply(&dir, std::slice::from_ref(&p), ProjectTools::Both).is_err());
        assert!(
            fs::read_dir(&outside).unwrap().next().is_none(),
            "nothing written through the link"
        );
        assert!(!dir.join(".claude/a.md").exists(), "and rolled back");
        // A planted link where a staging file would go is not followed either
        // (a file symlink needs a privilege on Windows).
        #[cfg(unix)]
        {
            fs::remove_file(dir.join(".connecto")).unwrap();
            fs::create_dir_all(dir.join(".claude")).unwrap();
            fs::write(outside.join("victim"), "v").unwrap();
            std::os::unix::fs::symlink(
                outside.join("victim"),
                dir.join(".claude/.a.md.connecto-tmp"),
            )
            .unwrap();
            apply(&dir, &[p], ProjectTools::Both).unwrap();
            assert_eq!(fs::read_to_string(outside.join("victim")).unwrap(), "v");
        }
    }

    #[test]
    fn a_failure_half_way_puts_back_files_and_lock_records() {
        let dir = project("rollback");
        let v1 = package(
            "p",
            1,
            &[
                (".claude/a.md", "one"),
                (".claude/gone.md", "gone"),
                (".claude/settings.json", r#"{"x":1}"#),
            ],
        );
        apply(&dir, &[v1], ProjectTools::Both).unwrap();
        let lock_before = fs::read_to_string(dir.join(".connecto/packages.lock")).unwrap();
        // `.claude/z` is a folder, so writing the file there fails after a.md
        // was rewritten and gone.md removed.
        fs::create_dir_all(dir.join(".claude/z")).unwrap();
        fs::write(dir.join(".claude/z/keep"), "keep").unwrap();
        let v2 = package(
            "p",
            2,
            &[
                (".claude/a.md", "two"),
                (".claude/z", "file"),
                (".claude/settings.json", r#"{"y":2}"#),
            ],
        );
        let err = apply(&dir, &[v2], ProjectTools::Both).unwrap_err();
        assert!(err.contains("nothing was changed"), "{err}");
        assert_eq!(read(&dir, ".claude/a.md"), "one");
        assert_eq!(read(&dir, ".claude/gone.md"), "gone");
        assert_eq!(read(&dir, ".claude/z/keep"), "keep");
        assert_eq!(
            serde_json::from_str::<Value>(&read(&dir, ".claude/settings.json")).unwrap(),
            json!({"x": 1})
        );
        assert_eq!(read(&dir, ".connecto/packages.lock"), lock_before);
        // Retrying with the obstacle gone works.
        fs::remove_dir_all(dir.join(".claude/z")).unwrap();
        let v2 = package("p", 2, &[(".claude/a.md", "two")]);
        assert!(apply(&dir, &[v2], ProjectTools::Both).unwrap().changed);
    }

    #[test]
    fn a_failed_first_install_leaves_no_files_or_folders() {
        let dir = project("rollback-first");
        fs::create_dir_all(dir.join(".claude/z/inner")).unwrap();
        let p = package(
            "p",
            1,
            &[
                (".claude/a.md", "a"),
                (".codex/b.toml", "b"),
                (".claude/z", "f"),
            ],
        );
        assert!(apply(&dir, &[p], ProjectTools::Both).is_err());
        assert!(!dir.join(".claude/a.md").exists());
        assert!(!dir.join(".codex").exists() && !dir.join(".connecto").exists());
    }

    #[test]
    fn settings_leaves_come_out_one_by_one_when_the_user_added_siblings() {
        let dir = project("leaves");
        let p = package(
            "p",
            1,
            &[(
                ".claude/settings.json",
                r#"{"enabledPlugins":{"a@x":true,"b@x":true},"extraKnownMarketplaces":{"m":{"source":{"repo":"r"}}},"list":["one"]}"#,
            )],
        );
        apply(&dir, &[p], ProjectTools::Both).unwrap();
        let mut value: Value = serde_json::from_str(&read(&dir, ".claude/settings.json")).unwrap();
        value["enabledPlugins"]["mine@x"] = json!(true);
        value["extraKnownMarketplaces"]["m"]["source"]["extra"] = json!(1);
        value["list"] = json!(["one", "mine"]);
        fs::write(dir.join(".claude/settings.json"), value.to_string()).unwrap();
        let out = apply(&dir, &[], ProjectTools::Both).unwrap();
        assert!(out.changed);
        let back: Value = serde_json::from_str(&read(&dir, ".claude/settings.json")).unwrap();
        assert_eq!(
            back,
            json!({
                "enabledPlugins": {"mine@x": true},
                "extraKnownMarketplaces": {"m": {"source": {"extra": 1}}},
                "list": ["mine"],
            })
        );
        assert!(!dir.join(".connecto").exists());
    }

    #[test]
    fn rollback_undoes_in_reverse_so_a_file_and_a_folder_swap_back() {
        // File to folder: `a` is replaced by `a/b`, then a later file fails.
        let dir = project("swap-file-folder");
        let v1 = package("p", 1, &[(".claude/a", "file a")]);
        apply(&dir, &[v1], ProjectTools::Both).unwrap();
        let lock_before = read(&dir, ".connecto/packages.lock");
        fs::create_dir_all(dir.join(".claude/z")).unwrap();
        fs::write(dir.join(".claude/z/keep"), "k").unwrap();
        let v2 = package("p", 2, &[(".claude/a/b", "b"), (".claude/z", "blocked")]);
        let err = apply(&dir, &[v2], ProjectTools::Both).unwrap_err();
        assert!(err.contains("nothing was changed"), "{err}");
        assert_eq!(read(&dir, ".claude/a"), "file a");
        assert_eq!(read(&dir, ".connecto/packages.lock"), lock_before);
        // Folder to file: `a/b` is replaced by the file `a`, then a later file fails.
        let dir = project("swap-folder-file");
        let v1 = package("p", 1, &[(".claude/a/b", "file b")]);
        apply(&dir, &[v1], ProjectTools::Both).unwrap();
        let lock_before = read(&dir, ".connecto/packages.lock");
        fs::create_dir_all(dir.join(".claude/z")).unwrap();
        fs::write(dir.join(".claude/z/keep"), "k").unwrap();
        let v2 = package(
            "p",
            2,
            &[(".claude/a", "now a file"), (".claude/z", "blocked")],
        );
        assert!(apply(&dir, &[v2], ProjectTools::Both).is_err());
        assert_eq!(read(&dir, ".claude/a/b"), "file b");
        assert_eq!(read(&dir, ".connecto/packages.lock"), lock_before);
    }

    #[test]
    fn a_users_empty_settings_file_is_written_back_when_the_package_goes() {
        let dir = project("empty-settings");
        fs::create_dir_all(dir.join(".claude")).unwrap();
        fs::write(dir.join(".claude/settings.json"), "{}").unwrap();
        let p = package(
            "p",
            1,
            &[(
                ".claude/settings.json",
                r#"{"enabledPlugins":{"a@b":true}}"#,
            )],
        );
        apply(&dir, &[p], ProjectTools::Both).unwrap();
        assert!(read(&dir, ".claude/settings.json").contains("a@b"));
        let out = apply(&dir, &[], ProjectTools::Both).unwrap();
        assert!(out.changed);
        let back: Value = serde_json::from_str(&read(&dir, ".claude/settings.json")).unwrap();
        assert_eq!(back, json!({}), "the plugin is no longer enabled");
    }
}
