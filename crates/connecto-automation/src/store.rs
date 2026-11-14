//! Automations on disk: `<data>/automations/<slug>/automation.json` plus the
//! agent's instructions file.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use connecto_hook::automation::{Agent, Definition, FILE_NAME};
use serde_json::Value;

use crate::guide;

pub const HISTORY_FILE: &str = "history.jsonl";

#[must_use]
pub fn automations_dir(data_root: &Path) -> PathBuf {
    data_root.join("automations")
}

/// What the loader found in one folder.
#[derive(Debug, Clone)]
pub struct Entry {
    pub slug: String,
    pub dir: PathBuf,
    pub def: Result<Definition, Vec<String>>,
}

/// Which tool session an automation's authoring terminal ran, kept beside
/// `automation.json` (never inside it) in `.session.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub tool: connecto_core::ToolKind,
    /// The terminal's id when it was recorded.
    pub terminal: String,
    pub session: String,
    /// The guide version the session was started or resumed under; `None` for
    /// a record from before this was kept.
    pub guide: Option<u32>,
}

const SESSION_FILE: &str = ".session.json";

/// A tool session id: letters, digits, `-` and `_` (it goes on a command line).
fn valid_session_id(id: &str) -> bool {
    // Letters, digits, `-` and `_`, starting with a letter or digit: never an
    // option such as `--last`.
    id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[must_use]
pub fn read_session(dir: &Path) -> Option<SessionRecord> {
    let value: Value =
        serde_json::from_str(&fs::read_to_string(dir.join(SESSION_FILE)).ok()?).ok()?;
    let text = |f: &str| value.get(f).and_then(Value::as_str).map(str::to_owned);
    let tool = match text("tool")?.as_str() {
        "claude" => connecto_core::ToolKind::Claude,
        "codex" => connecto_core::ToolKind::Codex,
        _ => return None,
    };
    let session = text("session").filter(|id| valid_session_id(id))?;
    Some(SessionRecord {
        tool,
        terminal: text("terminal").unwrap_or_default(),
        session,
        guide: value
            .get("guide")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok()),
    })
}

pub fn write_session(dir: &Path, record: &SessionRecord) -> Result<(), String> {
    if !valid_session_id(&record.session) {
        return Err("not a session id".to_owned());
    }
    let body = serde_json::json!({
        "version": 1,
        "tool": record.tool.name(),
        "terminal": record.terminal,
        "session": record.session,
        "guide": record.guide,
    });
    let tmp = dir.join(".session.json.tmp");
    fs::write(&tmp, body.to_string()).map_err(|e| e.to_string())?;
    fs::rename(&tmp, dir.join(SESSION_FILE)).map_err(|e| e.to_string())
}

/// Check a folder the user chose as the Automations folder, and give its real
/// path. It must exist, be a folder and be writable, and may not be the data
/// folder, the home folder, one that holds either, or the CLIs' own state.
pub fn validate_root(path: &Path, data_root: &Path, home: &Path) -> Result<PathBuf, String> {
    let real = fs::canonicalize(path).map_err(|_| "that folder does not exist".to_owned())?;
    if !real.is_dir() {
        return Err("that is not a folder".to_owned());
    }
    let resolve = |p: &Path| fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
    let (data, home) = (resolve(data_root), resolve(home));
    if real == data {
        return Err("that is Connecto's data folder".to_owned());
    }
    if real == home || home.starts_with(&real) || data.starts_with(&real) {
        return Err("choose a folder of its own, not your home folder".to_owned());
    }
    if [".claude", ".codex", ".local"]
        .iter()
        .any(|state| real.starts_with(home.join(state)))
    {
        return Err("that folder belongs to a tool's own state".to_owned());
    }
    // Writable: the probe is created and removed at once.
    let probe = real.join(format!(".connecto-write-test-{}", std::process::id()));
    fs::write(&probe, b"").map_err(|_| "that folder is not writable".to_owned())?;
    let _ = fs::remove_file(&probe);
    Ok(real)
}

/// Bring one guide file up to date (see [`guide::apply`]). True if it was written.
pub fn ensure_guide(path: &Path, body: &str) -> std::io::Result<bool> {
    let existing = fs::read_to_string(path).ok();
    match guide::apply(existing.as_deref(), body) {
        Some(text) => {
            // The file may hold the user's own text: never leave half of it.
            let mut name = std::ffi::OsString::from(".");
            name.push(path.file_name().unwrap_or_default());
            name.push(".tmp");
            let tmp = path.with_file_name(name);
            fs::write(&tmp, text)?;
            if let Err(err) = fs::rename(&tmp, path) {
                let _ = fs::remove_file(&tmp);
                return Err(err);
            }
            Ok(true)
        }
        None => Ok(false),
    }
}

/// The folder-level guide, for both tools.
pub fn ensure_root_guides(root: &Path) -> std::io::Result<()> {
    for name in ["CLAUDE.md", "AGENTS.md"] {
        ensure_guide(&root.join(name), &guide::root_text())?;
    }
    Ok(())
}

/// One automation's own guide, for its agent's tool.
pub fn ensure_slug_guide(root: &Path, slug: &str, name: &str, agent: Agent) -> std::io::Result<()> {
    ensure_guide(
        &root.join(slug).join(guide::file_name(agent)),
        &guide::text(name),
    )?;
    Ok(())
}

/// A slug is a safe folder name: lowercase letters, digits and `-`. The
/// rules for it and for a name are shared with packages.
pub use connecto_hook::name::{check_name, valid_slug};

#[must_use]
pub fn slugify(name: &str) -> String {
    connecto_hook::name::slugify(name, "automation")
}

/// Every folder with an `automation.json`, by slug.
#[must_use]
pub fn scan(root: &Path) -> Vec<Entry> {
    let mut out = Vec::new();
    let Ok(read) = fs::read_dir(root) else {
        return out;
    };
    for item in read.flatten() {
        let dir = item.path();
        let Some(slug) = dir.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            continue;
        };
        if !valid_slug(&slug) || !dir.is_dir() {
            continue;
        }
        let file = dir.join(FILE_NAME);
        let def = match fs::read_to_string(&file) {
            Ok(text) => Definition::parse_lenient(&text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => Err(vec![format!("cannot read {FILE_NAME}: {err}")]),
        };
        out.push(Entry { slug, dir, def });
    }
    out.sort_by(|a, b| a.slug.cmp(&b.slug));
    out
}

/// Cheap change detector: every definition file's modification time and size.
#[must_use]
pub fn signature(root: &Path) -> Vec<(String, Option<SystemTime>, u64)> {
    let mut out = Vec::new();
    if let Ok(read) = fs::read_dir(root) {
        for item in read.flatten() {
            let file = item.path().join(FILE_NAME);
            let meta = fs::metadata(&file).ok();
            out.push((
                item.file_name().to_string_lossy().into_owned(),
                meta.as_ref().and_then(|m| m.modified().ok()),
                meta.map_or(0, |m| m.len()),
            ));
        }
    }
    out.sort();
    out
}

/// Make the folder, the template and the agent's instructions. Returns the slug.
pub fn create(root: &Path, name: &str, agent: Agent) -> Result<String, String> {
    let name = check_name(name)?;
    fs::create_dir_all(root).map_err(|e| e.to_string())?;
    let base = slugify(name);
    let mut slug = base.clone();
    let mut n = 2;
    // `create_dir` fails if it exists, so two creators cannot share a folder.
    let dir = loop {
        let dir = root.join(&slug);
        match fs::create_dir(&dir) {
            Ok(()) => break dir,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                slug = format!("{base}-{n}");
                n += 1;
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    let write = || -> std::io::Result<()> {
        let def = Definition::template(name, agent);
        let mut json = serde_json::to_string_pretty(&def.to_json())?;
        json.push('\n');
        fs::write(dir.join(FILE_NAME), json)?;
        ensure_guide(&dir.join(guide::file_name(agent)), &guide::text(name)).map(|_| ())
    };
    write().map_err(|e| {
        let _ = fs::remove_dir_all(&dir);
        e.to_string()
    })?;
    Ok(slug)
}

/// Change fields of the raw JSON file (atomic replace).
pub fn update(dir: &Path, change: impl FnOnce(&mut Value)) -> Result<(), String> {
    let file = dir.join(FILE_NAME);
    let text = fs::read_to_string(&file).map_err(|e| e.to_string())?;
    let mut value: Value =
        serde_json::from_str(&text).map_err(|e| format!("not valid JSON: {e}"))?;
    if !value.is_object() {
        return Err("the file is not a JSON object".to_owned());
    }
    change(&mut value);
    let mut out = serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?;
    out.push('\n');
    let tmp = dir.join(".automation.json.tmp");
    fs::write(&tmp, out).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &file).map_err(|e| e.to_string())
}

pub fn delete(root: &Path, slug: &str) -> Result<(), String> {
    if !valid_slug(slug) {
        return Err("not an automation".to_owned());
    }
    let dir = root.join(slug);
    // Never follow a link out of the automations folder.
    let meta = fs::symlink_metadata(&dir).map_err(|e| e.to_string())?;
    if meta.file_type().is_symlink() {
        fs::remove_file(&dir).map_err(|e| e.to_string())
    } else {
        fs::remove_dir_all(&dir).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn root(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("connecto-store-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn the_automations_folder_must_be_a_writable_folder_of_its_own() {
        let base = root("validate");
        let (data, home, chosen) = (base.join("data"), base.join("home"), base.join("home/auto"));
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&chosen).unwrap();
        let ok = validate_root(&chosen, &data, &home).unwrap();
        assert_eq!(ok, fs::canonicalize(&chosen).unwrap());
        assert!(validate_root(&base.join("nope"), &data, &home).is_err());
        assert!(
            validate_root(&data, &data, &home).is_err(),
            "the data folder"
        );
        assert!(validate_root(&home, &data, &home).is_err(), "home");
        assert!(validate_root(&base, &data, &home).is_err(), "holds both");
        fs::create_dir_all(home.join(".claude/x")).unwrap();
        assert!(validate_root(&home.join(".claude/x"), &data, &home).is_err());
        // A file, and a folder that cannot be written.
        fs::write(base.join("file"), "x").unwrap();
        assert!(validate_root(&base.join("file"), &data, &home).is_err());
        // Unix only: on Windows the read-only attribute of a folder does not
        // stop files being made in it, and an ACL that does needs a Windows API.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let locked = base.join("home/locked");
            fs::create_dir_all(&locked).unwrap();
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
            let readonly = validate_root(&locked, &data, &home);
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
            assert!(readonly.is_err());
        }
        // Both tools get the guide; a user's own file keeps its text.
        ensure_root_guides(&chosen).unwrap();
        assert!(
            fs::read_to_string(chosen.join("AGENTS.md"))
                .unwrap()
                .contains("automations folder")
        );
        fs::write(chosen.join("CLAUDE.md"), "mine").unwrap();
        ensure_root_guides(&chosen).unwrap();
        let kept = fs::read_to_string(chosen.join("CLAUDE.md")).unwrap();
        assert!(kept.starts_with("mine\n\n") && kept.contains("connecto-automations:begin"));
        // Written through a temporary file that does not stay behind.
        assert!(!chosen.join(".CLAUDE.md.tmp").exists());
        // A guide that cannot be replaced is left whole, and the failure is told.
        let blocked = chosen.join("blocked");
        fs::create_dir_all(blocked.join("CLAUDE.md")).unwrap();
        assert!(ensure_guide(&blocked.join("CLAUDE.md"), "body").is_err());
        assert!(blocked.join("CLAUDE.md").is_dir() && !blocked.join(".CLAUDE.md.tmp").exists());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn slugs_are_safe() {
        assert_eq!(
            slugify("Approve  \"Edit\" prompts!"),
            "approve-edit-prompts"
        );
        assert_eq!(slugify("../../etc"), "etc");
        assert_eq!(slugify("???"), "automation");
        assert!(valid_slug("a-1") && !valid_slug("../x") && !valid_slug("A") && !valid_slug(""));
    }

    #[test]
    fn create_writes_template_and_guide_and_avoids_collisions() {
        let r = root("create");
        let a = create(&r, "Say Yes", Agent::Claude).unwrap();
        let b = create(&r, "Say Yes", Agent::Codex).unwrap();
        assert_eq!((a.as_str(), b.as_str()), ("say-yes", "say-yes-2"));
        assert!(r.join("say-yes/CLAUDE.md").is_file());
        assert!(r.join("say-yes-2/AGENTS.md").is_file());
        let entries = scan(&r);
        assert_eq!(entries.len(), 2);
        let def = entries[0].def.as_ref().unwrap();
        assert!(!def.enabled && def.has_todo());
        assert!(create(&r, "  ", Agent::Claude).is_err());
        let _ = fs::remove_dir_all(r);
    }

    #[test]
    fn update_edits_the_file_and_a_broken_one_is_listed_with_errors() {
        let r = root("update");
        let slug = create(&r, "X", Agent::Claude).unwrap();
        let dir = r.join(&slug);
        update(&dir, |v| v["enabled"] = Value::Bool(true)).unwrap();
        assert!(scan(&r)[0].def.as_ref().unwrap().enabled);
        let before = signature(&r);
        fs::write(dir.join(FILE_NAME), "{ nope").unwrap();
        assert_ne!(before, signature(&r));
        assert!(scan(&r)[0].def.is_err());
        assert!(update(&dir, |_| {}).is_err());
        // A folder without the file is not an automation.
        fs::create_dir_all(r.join("empty")).unwrap();
        assert_eq!(scan(&r).len(), 1);
        delete(&r, &slug).unwrap();
        assert!(scan(&r).is_empty());
        assert!(delete(&r, "../x").is_err());
        let _ = fs::remove_dir_all(r);
    }
}
