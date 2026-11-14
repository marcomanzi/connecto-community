//! The durable side of staging a paired machine's record (ADR 0022).
//!
//! Pairing writes a machine's record before the commit is sent, so that a
//! failure of the save cannot happen after the other side has kept its own. A
//! staged record must survive a crash, so every step is written to disk first
//! and recorded in `staged.json` in the remote folder:
//!
//! 1. the entry, status `staged`: the record's previous lines and host, so it
//!    can be put back;
//! 2. the new key, in the Keychain under `pair.<id>.staged` (the key this
//!    machine dials with is not touched until the pairing is kept);
//! 3. the new key line in `authorized_keys`, and 4. the new host entry.
//!
//! The staged key line carries a mark in its comment, `(staged <date>)`, until
//! the commit: a kept record rewrites it to `(paired <date>)`. A staged key is
//! never let in: [`unusable_fingerprints`] lists the keys of `staged` and
//! `sent` entries and every marked key line, and the host leaves them out of
//! its sessions. `sent` means this side has sent its final word, so the other
//! side may keep the pairing; a crash then is the one case that cannot be
//! resolved locally, and the record is rolled back (documented in ADR 0022).
//!
//! On a re-pairing the machine's previous key line stays valid until the commit,
//! which removes it in the same write that unmarks the new one. A rollback
//! restores the previous lines, so the old key is never out of service for the
//! pairing.
//!
//! On start, [`recover`] finishes every `kept` entry and rolls back every other
//! one, so no unconfirmed record survives a restart. If `staged.json` is damaged
//! or missing while a marked key line exists, [`recover`] instead removes every
//! key that cannot be shown to belong to a finished pairing (see [`sweep`]).

use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use connecto_secrets::STORE_NAME;
use serde_json::{Value, json};
use ssh_key::{HashAlg, PublicKey};

use crate::auth::{key_lines, without_key};
use crate::secrets::{SecretStore, pair_key_name};
use crate::transport::EndpointId;
use crate::trust::{KnownHost, KnownHosts, hex, unhex, write_private};

/// The prefix of the comment on a key a mutual pairing made: the machine's id.
pub const LINK_PREFIX: &str = "connecto-id=";

/// The mark of a staged key line's comment, before the commit.
const STAGED_MARK: &str = " (staged ";
/// The comment mark of a committed key line of a mutual pairing (as the older
/// flow wrote it, with no id).
const PAIRED_MARK: &str = " (paired ";

/// The comment of the key line a machine's staging writes.
#[must_use]
pub fn staged_comment(id: &EndpointId, date: &str) -> String {
    format!("{LINK_PREFIX}{} (staged {date})", hex(id))
}

/// Whether a key comment is that of a key still staged.
#[must_use]
pub fn is_staged_comment(comment: &str) -> bool {
    comment.starts_with(LINK_PREFIX) && comment.contains(STAGED_MARK)
}

/// The fingerprints of the staged key lines in `text`.
#[must_use]
pub fn marked_fingerprints(text: &str) -> Vec<String> {
    key_lines(text)
        .into_iter()
        .filter(|k| is_staged_comment(&k.comment))
        .map(|k| k.fingerprint)
        .collect()
}

/// A key of the older flow: no endpoint id, and written only once a pairing
/// was complete, since that flow had no staging.
fn is_legacy_paired(comment: &str) -> bool {
    !comment.starts_with(LINK_PREFIX) && comment.contains(PAIRED_MARK)
}

/// The text of a file; a missing file is empty, and any other failure an error.
fn read_optional(path: &Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("{} cannot be read: {e}", path.display())),
    }
}

/// What [`recover`] did about a damaged record file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovery {
    /// The machines whose pairing was not finished, by name where known (else
    /// by the first eight characters of the id). They must be paired again.
    pub machines: Vec<String>,
    /// The files kept for checking: the damaged record file, and copies of the
    /// key list and host list as they were before they changed.
    pub kept: Vec<PathBuf>,
}

/// Where a staged record stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Written; the commit has not been sent. Rolled back on a restart.
    Staged,
    /// This side has sent its final word. Rolled back on a restart: the other
    /// side may have kept it, which only the commit exchange could settle.
    Sent,
    /// Both sides have kept the pairing. Finished on a restart.
    Kept,
}

/// One staged record: what is needed to put back what it replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: EndpointId,
    /// The SHA256 fingerprint of the key the other machine dials with.
    pub fingerprint: String,
    pub status: Status,
    /// The machine's `authorized_keys` lines before staging (empty if none).
    pub prev_lines: Vec<String>,
    /// The machine's `known_hosts` line before staging, if it had one.
    pub prev_known: Option<String>,
}

/// The id a key's comment carries, if a mutual pairing made the key.
#[must_use]
pub fn link_id(comment: &str) -> Option<String> {
    comment
        .strip_prefix(LINK_PREFIX)?
        .split(' ')
        .next()
        .filter(|id| id.len() == 64)
        .map(str::to_owned)
}

/// The fingerprints of the keys that carry `id` as their link.
#[must_use]
pub fn fingerprints_for(text: &str, id: &str) -> Vec<String> {
    key_lines(text)
        .into_iter()
        .filter(|k| link_id(&k.comment).as_deref() == Some(id))
        .map(|k| k.fingerprint)
        .collect()
}

/// The lines of `text` that are keys of the machine `id`, as they are written.
#[must_use]
pub fn raw_lines_for(text: &str, id: &str) -> Vec<String> {
    text.lines()
        .filter(|raw| {
            let line = raw.split_once('#').map_or(*raw, |(l, _)| l).trim();
            PublicKey::from_openssh(line)
                .ok()
                .and_then(|k| link_id(k.comment()))
                .as_deref()
                == Some(id)
        })
        .map(str::to_owned)
        .collect()
}

/// The Keychain name of a machine's staged key.
#[must_use]
pub fn staged_secret_name(id: &EndpointId) -> String {
    format!("{}.staged", pair_key_name(id))
}

fn file(dir: &Path) -> std::path::PathBuf {
    dir.join("staged.json")
}

fn status_text(status: Status) -> &'static str {
    match status {
        Status::Staged => "staged",
        Status::Sent => "sent",
        Status::Kept => "kept",
    }
}

/// Every staged record. A missing file has none; a damaged one is an error,
/// and nothing is let in while it is damaged.
///
/// # Errors
/// When the file exists and cannot be read or parsed.
pub fn load(dir: &Path) -> Result<Vec<Entry>, String> {
    let text = match std::fs::read_to_string(file(dir)) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("the staged records cannot be read: {e}")),
    };
    let damaged = || "the staged records are damaged".to_owned();
    let v: Value = serde_json::from_str(&text).map_err(|_| damaged())?;
    let items = v
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(damaged)?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .and_then(unhex)
            .ok_or_else(damaged)?;
        let fingerprint = item
            .get("fingerprint")
            .and_then(Value::as_str)
            .ok_or_else(damaged)?
            .to_owned();
        let status = match item.get("status").and_then(Value::as_str) {
            Some("staged") => Status::Staged,
            Some("sent") => Status::Sent,
            Some("kept") => Status::Kept,
            _ => return Err(damaged()),
        };
        let prev_lines = item
            .get("prev_lines")
            .and_then(Value::as_array)
            .ok_or_else(damaged)?
            .iter()
            .map(|l| l.as_str().map(str::to_owned).ok_or_else(damaged))
            .collect::<Result<Vec<_>, _>>()?;
        let prev_known = match item.get("prev_known") {
            None | Some(Value::Null) => None,
            Some(Value::String(line)) => Some(line.clone()),
            Some(_) => return Err(damaged()),
        };
        out.push(Entry {
            id,
            fingerprint,
            status,
            prev_lines,
            prev_known,
        });
    }
    Ok(out)
}

/// Write the whole list. An empty list removes the file, so a machine with no
/// staged record has no such file.
fn save(dir: &Path, entries: &[Entry]) -> Result<(), String> {
    if entries.is_empty() {
        return match std::fs::remove_file(file(dir)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        };
    }
    let items: Vec<Value> = entries
        .iter()
        .map(|e| {
            json!({
                "id": hex(&e.id),
                "fingerprint": e.fingerprint,
                "status": status_text(e.status),
                "prev_lines": e.prev_lines,
                "prev_known": e.prev_known,
            })
        })
        .collect();
    let mut text = serde_json::to_string_pretty(&json!({ "version": 1, "entries": items }))
        .map_err(|e| e.to_string())?;
    text.push('\n');
    write_private(&file(dir), text.as_bytes()).map_err(|e| e.to_string())
}

/// Record a staged entry for `entry.id`, replacing any older entry for it.
///
/// # Errors
/// When the file cannot be written.
pub fn add(dir: &Path, entry: Entry) -> Result<(), String> {
    let mut entries = load(dir)?;
    entries.retain(|e| e.id != entry.id);
    entries.push(entry);
    save(dir, &entries)
}

/// Move a machine's entry to `status`.
///
/// # Errors
/// When there is no entry for `id`, or the file cannot be written.
pub fn set_status(dir: &Path, id: &EndpointId, status: Status) -> Result<(), String> {
    let mut entries = load(dir)?;
    let entry = entries
        .iter_mut()
        .find(|e| &e.id == id)
        .ok_or_else(|| "no staged record for this machine".to_owned())?;
    entry.status = status;
    save(dir, &entries)
}

fn drop_entry(dir: &Path, id: &EndpointId) -> Result<(), String> {
    let mut entries = load(dir)?;
    entries.retain(|e| &e.id != id);
    save(dir, &entries)
}

/// Put back `authorized_keys` as it was: this machine's new key lines removed,
/// and its previous lines restored.
fn restore_auth(dir: &Path, entry: &Entry) -> Result<(), String> {
    let path = dir.join("authorized_keys");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let id = hex(&entry.id);
    let mut auth = without_key(&text, &entry.fingerprint);
    for fp in fingerprints_for(&text, &id) {
        auth = without_key(&auth, &fp);
    }
    for line in &entry.prev_lines {
        if !auth.is_empty() && !auth.ends_with('\n') {
            auth.push('\n');
        }
        auth.push_str(line);
        auth.push('\n');
    }
    if auth != text {
        write_private(&path, auth.as_bytes()).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Put back `known_hosts` as it was: this machine's entry replaced by its
/// previous one, or removed if it had none.
fn restore_known(dir: &Path, entry: &Entry) -> Result<(), String> {
    let path = dir.join("known_hosts");
    let mut known = KnownHosts::load(&path)?;
    let before = known.clone();
    if let Some(name) = known.by_id(&entry.id).map(|h: &KnownHost| h.name.clone()) {
        known.remove(&name).map_err(|e| e.to_string())?;
    }
    if let Some(line) = &entry.prev_known {
        for host in KnownHosts::parse(line)?.list() {
            known.add(host.clone()).map_err(|e| e.to_string())?;
        }
    }
    if known != before {
        known.save(&path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Undo what staging did for `id`, and forget its entry. Safe to run again,
/// and safe after a crash at any step.
///
/// # Errors
/// When a file or the Keychain cannot be changed; the entry is then kept so
/// that the next start tries again.
pub fn rollback(dir: &Path, store: &dyn SecretStore, id: &EndpointId) -> Result<(), String> {
    if let Some(entry) = load(dir)?.into_iter().find(|e| &e.id == id) {
        restore_auth(dir, &entry)?;
        restore_known(dir, &entry)?;
    }
    store
        .clear(&staged_secret_name(id))
        .map_err(|e| format!("{STORE_NAME} could not be changed: {e}"))?;
    drop_entry(dir, id)
}

/// Make a kept record final: the staged key becomes the machine's key, the
/// previous key line is removed and the staged one committed, and the entry
/// goes. Safe to run again, and after a crash at any step: the Keychain moves
/// first, so a marked line always has its key.
///
/// # Errors
/// When the Keychain or the file cannot be changed.
pub fn keep(dir: &Path, store: &dyn SecretStore, id: &EndpointId) -> Result<(), String> {
    let staged = staged_secret_name(id);
    if let Some(key) = store
        .get(&staged)
        .map_err(|e| format!("{STORE_NAME} could not be read: {e}"))?
    {
        store
            .set(&pair_key_name(id), &key)
            .map_err(|e| format!("{STORE_NAME} could not be changed: {e}"))?;
    }
    store
        .clear(&staged)
        .map_err(|e| format!("{STORE_NAME} could not be changed: {e}"))?;
    if let Some(entry) = load(dir)?.into_iter().find(|e| &e.id == id) {
        commit_auth(dir, &entry)?;
    }
    drop_entry(dir, id)
}

/// Make a kept machine's key lines final, in one write: its staged line loses
/// its mark, and the lines it replaces are removed. If the staged line is not
/// in the file, the file is left as it is.
fn commit_auth(dir: &Path, entry: &Entry) -> Result<(), String> {
    let path = dir.join("authorized_keys");
    let text = read_optional(&path)?;
    let id = hex(&entry.id);
    let mut out = String::new();
    let mut committed = false;
    for raw in text.lines() {
        let line = raw.split_once('#').map_or(raw, |(l, _)| l).trim();
        let mine = PublicKey::from_openssh(line)
            .ok()
            .filter(|k| link_id(k.comment()).as_deref() == Some(id.as_str()));
        match mine {
            Some(key) if key.fingerprint(HashAlg::Sha256).to_string() == entry.fingerprint => {
                committed = true;
                out.push_str(&raw.replacen(STAGED_MARK, PAIRED_MARK, 1));
                out.push('\n');
            }
            // The previous key of this machine: replaced by the kept one.
            Some(_) => {}
            None => {
                out.push_str(raw);
                out.push('\n');
            }
        }
    }
    if committed && out != text {
        write_private(&path, out.as_bytes())
            .map_err(|e| format!("the key list cannot be changed: {e}"))?;
    }
    Ok(())
}

/// Finish what a restart found: kept entries are kept, every other entry is
/// rolled back. Run before anything reads the records, and only when the
/// records are readable (see [`recover`]).
///
/// # Errors
/// The first failure; the rest are still tried, and the failed entries stay.
fn recover_entries(dir: &Path, store: &dyn SecretStore) -> Result<(), String> {
    let mut first_error = None;
    for entry in load(dir)? {
        let done = match entry.status {
            Status::Kept => keep(dir, store, &entry.id),
            Status::Staged | Status::Sent => rollback(dir, store, &entry.id),
        };
        if let Err(e) = done {
            first_error.get_or_insert(e);
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// The time in milliseconds, for the names of files set aside.
fn stamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
        .to_string()
}

/// Owner-only, for a file whose mode may not be (a set-aside record).
#[cfg(unix)]
fn set_private(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("{} cannot be made owner-only: {e}", path.display()))
}

/// No mode on Windows: the file keeps the ACL of the user's own data folder,
/// which is already private to the user (as `trust::write_private` does).
#[cfg(not(unix))]
fn set_private(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// A copy of `path` as it was before it changed, owner-only, for checking.
fn keep_copy(path: &Path, text: &str, stamp: &str) -> Result<PathBuf, String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let copy = path.with_file_name(format!("{name}.{stamp}.damaged"));
    write_private(&copy, text.as_bytes())
        .map_err(|e| format!("{} cannot be copied: {e}", path.display()))?;
    Ok(copy)
}

/// Start-up recovery of the pairing records. Run before anything reads them.
///
/// Normally: every `kept` entry is finished and every other one is rolled back.
///
/// When `staged.json` cannot be read or parsed, or is missing while a marked
/// key line remains in `authorized_keys`, the records cannot say what is
/// pending, so nothing in them is trusted. [`sweep`] then removes every key
/// that cannot be shown to belong to a finished pairing, and only after that is
/// the damaged file set aside (renamed, kept, owner-only). A failure at any
/// step leaves the damaged file in place, so the next start runs the sweep again.
///
/// # Errors
/// When a file or the Keychain cannot be read or changed. Nothing is let in on
/// the way: the damaged records keep every key out until a start succeeds.
pub fn recover(dir: &Path, store: &dyn SecretStore) -> Result<Option<Recovery>, String> {
    let present = file(dir).try_exists().unwrap_or(true);
    let damaged = match load(dir) {
        Err(_) => true,
        Ok(_) if present => false,
        Ok(_) => !marked_fingerprints(&read_optional(&dir.join("authorized_keys"))?).is_empty(),
    };
    if !damaged {
        recover_entries(dir, store)?;
        return Ok(None);
    }
    let stamp = stamp();
    let text = std::fs::read(file(dir))
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let mut recovery = sweep(dir, store, &text, &stamp)?;
    if present {
        let set_aside = dir.join(format!("staged.json.{stamp}.damaged"));
        std::fs::rename(file(dir), &set_aside)
            .map_err(|e| format!("the damaged staged records could not be set aside: {e}"))?;
        set_private(&set_aside)?;
        recovery.kept.push(set_aside);
    }
    Ok(Some(recovery))
}

/// The trust state from the surviving files alone, for [`recover`].
///
/// A machine is finished when its `authorized_keys` has a committed key line
/// for it (no staged mark), its `known_hosts` has its host, and the Keychain
/// has the key it dials with (`pair.<id>`). Of the key lines, only those of
/// finished machines and of the older flow are kept; comments and blank lines
/// stay. Every other key line goes, including hand-added keys and lines with
/// options (which are never honoured). For a machine that is not finished, its
/// host, its staged key and its dial key are removed. For a finished one, a
/// staged key left by a re-pairing is removed, and its previous key stays.
///
/// Machines are found in the key lines, the host list, and any 64-hex id in
/// the damaged text. The key list and host list are copied before they change.
///
/// # Errors
/// When a file or the Keychain cannot be read or changed.
fn sweep(
    dir: &Path,
    store: &dyn SecretStore,
    damaged: &str,
    stamp: &str,
) -> Result<Recovery, String> {
    let auth_path = dir.join("authorized_keys");
    let known_path = dir.join("known_hosts");
    let auth = read_optional(&auth_path)?;
    let known_text = read_optional(&known_path)?;
    let known = KnownHosts::parse(&known_text)
        .map_err(|e| format!("the paired machines cannot be read: {e}"))?;
    let mut kept = Vec::new();

    let lines = key_lines(&auth);
    let mut ids = BTreeSet::new();
    for key in &lines {
        if let Some(id) = link_id(&key.comment).and_then(|s| unhex(&s)) {
            ids.insert(id);
        }
    }
    for host in known.list() {
        ids.insert(host.id);
    }
    for token in damaged
        .split(|c: char| !c.is_ascii_hexdigit())
        .filter(|t| t.len() == 64)
    {
        if let Some(id) = unhex(token) {
            ids.insert(id);
        }
    }

    let mut finished = BTreeSet::new();
    for id in &ids {
        let hex_id = hex(id);
        let has_line = lines.iter().any(|k| {
            !is_staged_comment(&k.comment)
                && link_id(&k.comment).as_deref() == Some(hex_id.as_str())
        });
        if has_line
            && known.by_id(id).is_some()
            && store
                .get(&pair_key_name(id))
                .map_err(|e| format!("{STORE_NAME} could not be read: {e}"))?
                .is_some()
        {
            finished.insert(*id);
        }
    }

    let mut out = String::new();
    for raw in auth.lines() {
        let line = raw.split_once('#').map_or(raw, |(l, _)| l).trim();
        if line.is_empty() {
            out.push_str(raw);
            out.push('\n');
            continue;
        }
        // A line that is not a plain key is not a key: it goes.
        let Ok(key) = PublicKey::from_openssh(line) else {
            continue;
        };
        let keep = match link_id(key.comment()).and_then(|s| unhex(&s)) {
            Some(id) => finished.contains(&id) && !is_staged_comment(key.comment()),
            None => is_legacy_paired(key.comment()),
        };
        if keep {
            out.push_str(raw);
            out.push('\n');
        }
    }
    if out != auth {
        if !auth.is_empty() {
            kept.push(keep_copy(&auth_path, &auth, stamp)?);
        }
        write_private(&auth_path, out.as_bytes())
            .map_err(|e| format!("the key list cannot be changed: {e}"))?;
    }

    let mut machines = Vec::new();
    let mut known_after = known.clone();
    for id in &ids {
        let hex_id = hex(id);
        let unfinished_host = known.by_id(id).map(|h| h.name.clone());
        if finished.contains(id) {
            continue;
        }
        if let Some(name) = unfinished_host {
            known_after
                .remove(&name)
                .map_err(|e| format!("the paired machines cannot be changed: {e}"))?;
            machines.push(name);
        } else if lines
            .iter()
            .any(|k| link_id(&k.comment).as_deref() == Some(hex_id.as_str()))
        {
            machines.push(format!(
                "Machine {}",
                hex_id.chars().take(8).collect::<String>()
            ));
        }
    }
    if known_after != known {
        if !known_text.is_empty() {
            kept.push(keep_copy(&known_path, &known_text, stamp)?);
        }
        known_after
            .save(&known_path)
            .map_err(|e| format!("the paired machines cannot be changed: {e}"))?;
    }

    for id in &ids {
        store
            .clear(&staged_secret_name(id))
            .map_err(|e| format!("{STORE_NAME} could not be changed: {e}"))?;
        if !finished.contains(id) {
            store
                .clear(&pair_key_name(id))
                .map_err(|e| format!("{STORE_NAME} could not be changed: {e}"))?;
        }
    }
    Ok(Recovery { machines, kept })
}

/// The fingerprints of keys that may not open a session yet: those of
/// `staged` and `sent` entries, and every marked (staged) key line.
///
/// # Errors
/// When the staged records or the key list cannot be read; the caller then
/// lets in no key.
pub fn unusable_fingerprints(dir: &Path) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = load(dir)?
        .into_iter()
        .filter(|e| e.status != Status::Kept)
        .map(|e| e.fingerprint)
        .collect();
    for fp in marked_fingerprints(&read_optional(&dir.join("authorized_keys"))?) {
        if !out.contains(&fp) {
            out.push(fp);
        }
    }
    Ok(out)
}

/// Write `authorized_keys` and `known_hosts`, through the same atomic write
/// every record file uses.
///
/// # Errors
/// When the file cannot be written.
pub fn write_auth(dir: &Path, text: &str) -> Result<(), String> {
    write_private(&dir.join("authorized_keys"), text.as_bytes()).map_err(|e| e.to_string())
}

/// See [`write_auth`].
///
/// # Errors
/// When the file cannot be written.
pub fn write_known(dir: &Path, known: &KnownHosts) -> Result<(), String> {
    known
        .save(&dir.join("known_hosts"))
        .map_err(|e| e.to_string())
}

/// The line `known_hosts` writes for one host, to put it back later.
///
/// # Errors
/// When the host cannot be stored.
pub fn host_line(host: &KnownHost) -> Result<String, String> {
    let mut one = KnownHosts::default();
    one.add(host.clone()).map_err(|e| e.to_string())?;
    one.to_text()
        .lines()
        .find(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .ok_or_else(|| "a machine has no line to keep".to_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::secrets::MemorySecrets;

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("mg-staging-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A machine's new key: its authorized line (carrying `id`, staged) and fingerprint.
    fn machine_key(id: &EndpointId) -> (String, String, String) {
        let key = ssh_key::PrivateKey::random(
            &mut ssh_key::rand_core::OsRng,
            ssh_key::Algorithm::Ed25519,
        )
        .unwrap();
        let mut public = key.public_key().clone();
        public.set_comment(staged_comment(id, "2026-11-12"));
        let pem = key.to_openssh(ssh_key::LineEnding::LF).unwrap().to_string();
        (
            public.to_openssh().unwrap(),
            public.fingerprint(ssh_key::HashAlg::Sha256).to_string(),
            pem,
        )
    }

    fn host(id: &EndpointId, name: &str) -> KnownHost {
        KnownHost {
            name: name.to_owned(),
            id: *id,
            relay: Some("https://relay.example".into()),
            addrs: vec!["10.0.0.1:4000".into()],
            paired_at: "2026-11-12".into(),
        }
    }

    /// The four durable steps of staging, as the manager runs them, stopped
    /// after `steps` of them (a crash after that step).
    fn simulate_stage(
        dir: &std::path::Path,
        store: &MemorySecrets,
        id: &EndpointId,
        name: &str,
        steps: usize,
    ) -> (String, String) {
        let (line, fp, pem) = machine_key(id);
        let auth_before = std::fs::read_to_string(dir.join("authorized_keys")).unwrap_or_default();
        let known_before = KnownHosts::load(&dir.join("known_hosts")).unwrap();
        let prev_known = known_before.by_id(id).map(|h| host_line(h).unwrap());
        let mut known = known_before.clone();
        if let Some(prev) = known_before.by_id(id).map(|h| h.name.clone()) {
            known.remove(&prev).unwrap();
        }
        known.add(host(id, name)).unwrap();
        // The previous lines of the machine stay until the commit.
        let mut auth = auth_before.clone();
        if !auth.is_empty() && !auth.ends_with('\n') {
            auth.push('\n');
        }
        auth.push_str(&line);
        auth.push('\n');
        let entry = Entry {
            id: *id,
            fingerprint: fp.clone(),
            status: Status::Staged,
            prev_lines: raw_lines_for(&auth_before, &hex(id)),
            prev_known,
        };
        if steps >= 1 {
            add(dir, entry).unwrap();
        }
        if steps >= 2 {
            store.set(&staged_secret_name(id), pem.as_bytes()).unwrap();
        }
        if steps >= 3 {
            write_auth(dir, &auth).unwrap();
        }
        if steps >= 4 {
            write_known(dir, &known).unwrap();
        }
        (fp, pem)
    }

    fn auth_has(dir: &std::path::Path, id: &EndpointId) -> bool {
        !fingerprints_for(
            &std::fs::read_to_string(dir.join("authorized_keys")).unwrap_or_default(),
            &hex(id),
        )
        .is_empty()
    }

    fn known_has(dir: &std::path::Path, id: &EndpointId) -> Option<String> {
        KnownHosts::load(&dir.join("known_hosts"))
            .unwrap()
            .by_id(id)
            .map(|h| h.name.clone())
    }

    #[test]
    fn a_crash_at_any_staging_step_is_rolled_back_on_restart() {
        let id = [7_u8; 32];
        for steps in 0..=4 {
            let d = dir(&format!("crash-{steps}"));
            let store = MemorySecrets::default();
            simulate_stage(&d, &store, &id, "Studio", steps);
            // The restart: nothing unconfirmed survives it.
            recover(&d, &store).unwrap();
            assert!(!auth_has(&d, &id), "steps {steps}: a key line survived");
            assert!(
                known_has(&d, &id).is_none(),
                "steps {steps}: a host entry survived"
            );
            assert!(
                store.get(&staged_secret_name(&id)).unwrap().is_none(),
                "steps {steps}"
            );
            assert!(
                load(&d).unwrap().is_empty(),
                "steps {steps}: an entry survived"
            );
            assert!(
                unusable_fingerprints(&d).unwrap().is_empty(),
                "steps {steps}"
            );
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn a_staged_key_is_unusable_until_it_is_kept_and_a_sent_one_is_rolled_back() {
        let id = [8_u8; 32];
        let d = dir("unusable");
        let store = MemorySecrets::default();
        let (fp, _pem) = simulate_stage(&d, &store, &id, "Studio", 4);
        assert_eq!(unusable_fingerprints(&d).unwrap(), vec![fp.clone()]);
        // Sent: this side has given its final word, and is rolled back if it crashes.
        set_status(&d, &id, Status::Sent).unwrap();
        assert_eq!(unusable_fingerprints(&d).unwrap(), vec![fp]);
        recover(&d, &store).unwrap();
        assert!(!auth_has(&d, &id));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_crash_after_keeping_is_finished_on_restart() {
        let id = [9_u8; 32];
        let d = dir("kept");
        let store = MemorySecrets::default();
        let (fp, pem) = simulate_stage(&d, &store, &id, "Studio", 4);
        set_status(&d, &id, Status::Kept).unwrap();
        // Crash before the Keychain move and the entry removal.
        recover(&d, &store).unwrap();
        assert!(auth_has(&d, &id), "the kept key line stays");
        assert_eq!(known_has(&d, &id).as_deref(), Some("Studio"));
        assert_eq!(
            store.get(&pair_key_name(&id)).unwrap().unwrap(),
            pem.as_bytes(),
            "the staged key became the machine's key"
        );
        assert!(store.get(&staged_secret_name(&id)).unwrap().is_none());
        assert!(load(&d).unwrap().is_empty());
        assert!(unusable_fingerprints(&d).unwrap().is_empty());
        // Running it again changes nothing.
        recover(&d, &store).unwrap();
        assert!(auth_has(&d, &id));
        let _ = fp;
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_restart_during_a_replacement_puts_the_previous_record_back() {
        let id = [10_u8; 32];
        let d = dir("replace");
        let store = MemorySecrets::default();
        // The first pairing is kept.
        let (first_fp, first_pem) = simulate_stage(&d, &store, &id, "Studio", 4);
        set_status(&d, &id, Status::Kept).unwrap();
        recover(&d, &store).unwrap();
        // A second pairing with the same machine stages, then the app dies.
        simulate_stage(&d, &store, &id, "Studio renamed", 4);
        recover(&d, &store).unwrap();
        let auth = std::fs::read_to_string(d.join("authorized_keys")).unwrap();
        assert_eq!(
            fingerprints_for(&auth, &hex(&id)),
            vec![first_fp],
            "the first key is back, alone"
        );
        assert_eq!(known_has(&d, &id).as_deref(), Some("Studio"));
        assert_eq!(
            store.get(&pair_key_name(&id)).unwrap().unwrap(),
            first_pem.as_bytes()
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_damaged_staged_file_is_an_error_that_lets_no_key_in() {
        let d = dir("damaged");
        std::fs::write(d.join("staged.json"), "{ not json").unwrap();
        assert!(load(&d).is_err());
        assert!(unusable_fingerprints(&d).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A finished pairing, as a restart leaves it: the fingerprint of its key.
    fn finish(d: &std::path::Path, store: &MemorySecrets, id: &EndpointId, name: &str) -> String {
        let (fp, _) = simulate_stage(d, store, id, name, 4);
        set_status(d, id, Status::Kept).unwrap();
        recover(d, store).unwrap();
        fp
    }

    /// The fingerprints a session may use: the key lines less the unusable ones.
    fn usable(d: &std::path::Path) -> Vec<String> {
        let auth = std::fs::read_to_string(d.join("authorized_keys")).unwrap_or_default();
        let bad = unusable_fingerprints(d).unwrap();
        key_lines(&auth)
            .into_iter()
            .map(|k| k.fingerprint)
            .filter(|fp| !bad.contains(fp))
            .collect()
    }

    /// What must hold after a damaged file was recovered: the finished machine
    /// is kept, the unfinished one is gone, and the damaged file is kept intact.
    fn check_recovered(
        d: &std::path::Path,
        store: &MemorySecrets,
        rec: &Recovery,
        finished: (&EndpointId, &str),
        unfinished: &EndpointId,
        original: &[u8],
        tag: &str,
    ) {
        assert_eq!(rec.machines, vec!["Studio".to_owned()], "{tag}");
        assert!(!d.join("staged.json").exists(), "{tag}: set aside");
        let set_aside = rec
            .kept
            .iter()
            .find(|p| p.to_string_lossy().contains("staged.json."))
            .unwrap_or_else(|| panic!("{tag}: the damaged file is kept"));
        assert_eq!(std::fs::read(set_aside).unwrap(), original, "{tag}: intact");
        // Windows has no mode: the data folder's ACL keeps it private.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(set_aside).unwrap().permissions().mode() & 0o777,
                0o600,
                "{tag}"
            );
        }
        let auth = std::fs::read_to_string(d.join("authorized_keys")).unwrap();
        assert_eq!(fingerprints_for(&auth, &hex(finished.0)).len(), 1, "{tag}");
        assert!(
            fingerprints_for(&auth, &hex(unfinished)).is_empty(),
            "{tag}"
        );
        assert_eq!(
            known_has(d, finished.0).as_deref(),
            Some(finished.1),
            "{tag}"
        );
        assert!(known_has(d, unfinished).is_none(), "{tag}");
        assert!(
            store.get(&pair_key_name(finished.0)).unwrap().is_some(),
            "{tag}"
        );
        assert!(
            store.get(&pair_key_name(unfinished)).unwrap().is_none(),
            "{tag}"
        );
        assert!(
            store
                .get(&staged_secret_name(unfinished))
                .unwrap()
                .is_none(),
            "{tag}"
        );
        assert_eq!(
            usable(d),
            fingerprints_for(&auth, &hex(finished.0)),
            "{tag}"
        );
    }

    #[test]
    fn a_damaged_staged_file_drops_unfinished_keys_and_keeps_finished_machines() {
        let laptop = [21_u8; 32];
        let studio = [22_u8; 32];
        let damage: [(&str, &[u8]); 4] = [
            ("truncated", b"{\"version\": 1, \"entries\": [{\"id\": \"ab"),
            ("wrong types", b"{\"version\": 1, \"entries\": \"nope\"}"),
            ("empty", b""),
            ("not text", &[0xff, 0xfe, 0x00, 0x01]),
        ];
        for (tag, bytes) in damage {
            let d = dir(&format!("damage-{}", tag.replace(' ', "-")));
            let store = MemorySecrets::default();
            let laptop_fp = finish(&d, &store, &laptop, "Laptop");
            simulate_stage(&d, &store, &studio, "Studio", 4);
            std::fs::write(d.join("staged.json"), bytes).unwrap();
            assert!(load(&d).is_err(), "{tag}");

            let rec = recover(&d, &store).unwrap().expect("recovered");
            check_recovered(&d, &store, &rec, (&laptop, "Laptop"), &studio, bytes, tag);
            assert_eq!(usable(&d), vec![laptop_fp.clone()], "{tag}");
            // A new pairing is possible again.
            let (fp, _) = simulate_stage(&d, &store, &studio, "Studio", 4);
            assert!(!usable(&d).contains(&fp), "{tag}: staged, not yet usable");
            set_status(&d, &studio, Status::Kept).unwrap();
            recover(&d, &store).unwrap();
            assert!(usable(&d).contains(&fp), "{tag}: kept, usable");
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    // A file made unreadable by its Unix mode: Unix only.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_staged_file_is_set_aside_like_a_damaged_one() {
        use std::os::unix::fs::PermissionsExt;
        let d = dir("unreadable");
        let store = MemorySecrets::default();
        let laptop = [23_u8; 32];
        let studio = [24_u8; 32];
        finish(&d, &store, &laptop, "Laptop");
        simulate_stage(&d, &store, &studio, "Studio", 4);
        let path = d.join("staged.json");
        let bytes = std::fs::read(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&path).is_ok() {
            // Readable whatever its mode (running as root): nothing to test.
            let _ = std::fs::remove_dir_all(&d);
            return;
        }
        assert!(load(&d).is_err());
        let rec = recover(&d, &store).unwrap().expect("recovered");
        check_recovered(
            &d,
            &store,
            &rec,
            (&laptop, "Laptop"),
            &studio,
            &bytes,
            "unreadable",
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_hand_deleted_staged_file_does_not_make_a_staged_key_trusted() {
        let d = dir("hand-deleted");
        let store = MemorySecrets::default();
        let laptop = [25_u8; 32];
        let studio = [26_u8; 32];
        let laptop_fp = finish(&d, &store, &laptop, "Laptop");
        let (studio_fp, _) = simulate_stage(&d, &store, &studio, "Studio", 4);
        std::fs::remove_file(d.join("staged.json")).unwrap();
        // Even before the restart, the marked line is out of the sessions.
        assert_eq!(usable(&d), vec![laptop_fp.clone()]);
        let rec = recover(&d, &store).unwrap().expect("recovered");
        assert_eq!(rec.machines, vec!["Studio".to_owned()]);
        assert_eq!(usable(&d), vec![laptop_fp]);
        assert!(!usable(&d).contains(&studio_fp));
        let auth = std::fs::read_to_string(d.join("authorized_keys")).unwrap();
        assert!(fingerprints_for(&auth, &hex(&studio)).is_empty());
        assert!(store.get(&staged_secret_name(&studio)).unwrap().is_none());
        assert!(store.get(&pair_key_name(&studio)).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_missing_staged_file_with_no_staged_key_changes_nothing() {
        let d = dir("no-staged");
        let store = MemorySecrets::default();
        let laptop = [27_u8; 32];
        let laptop_fp = finish(&d, &store, &laptop, "Laptop");
        // A key added by hand, and no staged record anywhere.
        let manual = ssh_key::PrivateKey::random(
            &mut ssh_key::rand_core::OsRng,
            ssh_key::Algorithm::Ed25519,
        )
        .unwrap();
        let mut public = manual.public_key().clone();
        public.set_comment("my laptop");
        let manual_fp = public.fingerprint(ssh_key::HashAlg::Sha256).to_string();
        let mut auth = std::fs::read_to_string(d.join("authorized_keys")).unwrap();
        auth.push_str(&format!("{}\n", public.to_openssh().unwrap()));
        std::fs::write(d.join("authorized_keys"), auth).unwrap();
        std::fs::remove_file(d.join("staged.json")).ok();
        assert!(recover(&d, &store).unwrap().is_none());
        assert_eq!(usable(&d), vec![laptop_fp, manual_fp]);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A store that cannot be read or written: the recovery must then change nothing.
    struct BrokenStore;

    impl SecretStore for BrokenStore {
        fn get(&self, _: &str) -> Result<Option<Vec<u8>>, String> {
            Err("locked".to_owned())
        }
        fn set(&self, _: &str, _: &[u8]) -> Result<(), String> {
            Err("locked".to_owned())
        }
        fn clear(&self, _: &str) -> Result<(), String> {
            Err("locked".to_owned())
        }
    }

    #[test]
    fn a_recovery_that_cannot_use_the_keychain_changes_nothing_and_lets_nothing_in() {
        let d = dir("broken-keychain");
        let store = MemorySecrets::default();
        let laptop = [28_u8; 32];
        let studio = [29_u8; 32];
        finish(&d, &store, &laptop, "Laptop");
        simulate_stage(&d, &store, &studio, "Studio", 4);
        std::fs::write(d.join("staged.json"), "{ damaged").unwrap();
        let auth_before = std::fs::read_to_string(d.join("authorized_keys")).unwrap();
        assert!(recover(&d, &BrokenStore).is_err());
        assert_eq!(
            std::fs::read_to_string(d.join("authorized_keys")).unwrap(),
            auth_before
        );
        assert_eq!(
            std::fs::read_to_string(d.join("staged.json")).unwrap(),
            "{ damaged"
        );
        assert!(unusable_fingerprints(&d).is_err());
        // The next start, with the Keychain back, finishes the recovery.
        assert!(recover(&d, &store).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_re_pairing_keeps_the_old_key_until_the_commit_and_restores_it_on_rollback() {
        let id = [30_u8; 32];

        // Kept: the commit replaces the old key in one write.
        let d = dir("repair-keep");
        let store = MemorySecrets::default();
        let old_fp = finish(&d, &store, &id, "Studio");
        let (new_fp, new_pem) = simulate_stage(&d, &store, &id, "Studio", 4);
        assert_eq!(
            usable(&d),
            vec![old_fp.clone()],
            "the old key works while staged"
        );
        set_status(&d, &id, Status::Kept).unwrap();
        keep(&d, &store, &id).unwrap();
        let auth = std::fs::read_to_string(d.join("authorized_keys")).unwrap();
        assert_eq!(fingerprints_for(&auth, &hex(&id)), vec![new_fp.clone()]);
        assert!(!auth.contains("(staged"), "the new line is committed");
        assert!(auth.contains("(paired 2026-11-12)"));
        assert_eq!(
            store.get(&pair_key_name(&id)).unwrap().unwrap(),
            new_pem.as_bytes()
        );
        assert!(store.get(&staged_secret_name(&id)).unwrap().is_none());
        assert_eq!(usable(&d), vec![new_fp]);
        let _ = std::fs::remove_dir_all(&d);

        // Rolled back: the old key is the only key, and the old dial key is back.
        let d = dir("repair-rollback");
        let store = MemorySecrets::default();
        let old_fp = finish(&d, &store, &id, "Studio");
        let old_pem = store.get(&pair_key_name(&id)).unwrap().unwrap();
        simulate_stage(&d, &store, &id, "Studio", 4);
        recover(&d, &store).unwrap();
        let auth = std::fs::read_to_string(d.join("authorized_keys")).unwrap();
        assert_eq!(fingerprints_for(&auth, &hex(&id)), vec![old_fp.clone()]);
        assert_eq!(usable(&d), vec![old_fp]);
        assert_eq!(store.get(&pair_key_name(&id)).unwrap().unwrap(), old_pem);
        let _ = std::fs::remove_dir_all(&d);
    }
}
