//! A package in memory: its manifest, its installable files and a hash of
//! them. Read from a folder (`<home>/packages/<id>/`) or from the built-in
//! table.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use connecto_core::ProjectTools;
use connecto_hook::package::{FILE_NAME, Manifest, TOOL_DIRS};
use sha2::{Digest, Sha256};

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/builtin_files.rs"));
}

/// Id of the built-in package.
pub const BUILTIN_ID: &str = connecto_core::BUILTIN_PACKAGE_ID;

/// The PowerShell twin of the built-in package's `statusline.sh`, as a path in
/// the project: same folder, same base name. Installed on Windows only, where
/// Connecto's own Claude settings point the status line at it.
pub const STATUSLINE_PS1: &str = ".claude/scripts/statusline.ps1";
/// The PowerShell twin of the built-in package's `subagent-statusline.sh`.
pub const SUBAGENT_STATUSLINE_PS1: &str = ".claude/scripts/subagent-statusline.ps1";
/// The built-in package's files that only Windows installs. Windows installs
/// the `.sh` originals too, so a project shared with a Mac (through git, say)
/// holds the same files whichever platform installed it last.
pub const WINDOWS_ONLY: [&str; 2] = [STATUSLINE_PS1, SUBAGENT_STATUSLINE_PS1];

/// The platform a built-in package is made for. Its files and its hash are the
/// same on both, except for [`WINDOWS_ONLY`], which only Windows installs and
/// which no hash covers, so the lockfile of a shared project reads the same
/// on a Mac and on a PC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Platform {
    Unix,
    Windows,
}

impl Platform {
    /// The platform this build runs on.
    pub(crate) const CURRENT: Self = if cfg!(windows) {
        Self::Windows
    } else {
        Self::Unix
    };

    /// Whether the built-in package installs `path` here.
    fn installs(self, path: &str) -> bool {
        self == Self::Windows || !WINDOWS_ONLY.contains(&path)
    }

    /// Whether `path` is a built-in file that only another platform
    /// installs: the installer keeps such a file, and its lockfile entry, in a
    /// project that still has the built-in package.
    pub(crate) fn foreign(self, path: &str) -> bool {
        !self.installs(path)
    }
}
/// What one package may hold, so a stray folder cannot stall the manager.
const MAX_FILES: usize = 2000;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_DEPTH: usize = 12;
/// How many entries of one package the change detector looks at. A package
/// with more files than `MAX_FILES` does not load, so more would be wasted.
const MAX_SEEN: usize = 2 * MAX_FILES;
/// How many entries the change detector looks at in all.
const MAX_SEEN_TOTAL: usize = 10 * MAX_SEEN;

/// The three forms of the built-in package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Variant {
    /// Reviewers and critics run as Claude terminals.
    Claude,
    /// Everything runs in Codex.
    Codex,
    /// Each side is reviewed by the other tool.
    Both,
}

impl Variant {
    /// The form that fits a project's Tools setting.
    #[must_use]
    pub const fn for_tools(tools: ProjectTools) -> Self {
        match tools {
            ProjectTools::Claude => Self::Claude,
            ProjectTools::Codex => Self::Codex,
            ProjectTools::Both => Self::Both,
        }
    }

    const fn folder(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Both => "both",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub id: String,
    pub manifest: Manifest,
    /// Installable files by path relative to the project (`.claude/...`).
    pub files: BTreeMap<String, Vec<u8>>,
    /// The files that are installed executable (a hook or status script).
    pub exec: BTreeSet<String>,
    pub hash: String,
    pub builtin: bool,
}

impl Package {
    #[must_use]
    pub fn has_claude(&self) -> bool {
        self.files.keys().any(|p| is_claude(p))
    }

    #[must_use]
    pub fn has_codex(&self) -> bool {
        self.files.keys().any(|p| !is_claude(p))
    }
}

/// Whether a project-relative path belongs to Claude (else to Codex).
#[must_use]
pub fn is_claude(path: &str) -> bool {
    path.starts_with(".claude/")
}

/// Whether a project with these tools takes the file.
#[must_use]
pub fn wanted(path: &str, tools: ProjectTools) -> bool {
    use connecto_core::ToolKind;
    tools.allows(if is_claude(path) {
        ToolKind::Claude
    } else {
        ToolKind::Codex
    })
}

/// SHA-256 of a file's bytes, as lowercase hex.
#[must_use]
pub fn hash_bytes(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// What a version bump follows: the files, which of them are executable, and
/// what the package extends. A file that is not executable adds nothing for
/// its mode, so a package without scripts keeps the hash it had.
#[must_use]
pub fn content_hash(
    files: &BTreeMap<String, Vec<u8>>,
    exec: &BTreeSet<String>,
    extends: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"extends\0");
    hasher.update(extends.unwrap_or("").as_bytes());
    for (path, bytes) in files {
        hasher.update(b"\0file\0");
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        hasher.update(hash_bytes(bytes).as_bytes());
        if exec.contains(path) {
            hasher.update(b"\0exec");
        }
    }
    hex(&hasher.finalize())
}

/// Files that are never installed.
fn skipped(path: &str) -> bool {
    path.ends_with(".DS_Store") || path == ".claude/settings.local.json"
}

/// The installable files of a package folder and which are executable.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Files {
    pub files: BTreeMap<String, Vec<u8>>,
    pub exec: BTreeSet<String>,
}

fn walk(
    base: &Path,
    rel: &str,
    depth: usize,
    out: &mut Files,
    bytes: &mut usize,
) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err(format!("{rel}: folders are nested too deeply"));
    }
    let entries = fs::read_dir(base).map_err(|e| format!("{rel}: {e}"))?;
    for entry in entries {
        // An entry that cannot be read is an error, not a missing file: a
        // package loaded without it would take that file out of projects.
        let entry = entry.map_err(|e| format!("{rel}: {e}"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(format!("{rel}: a file name is not valid UTF-8"));
        };
        let child = format!("{rel}/{name}");
        let kind = entry.file_type().map_err(|e| format!("{child}: {e}"))?;
        // A link could point anywhere on the machine: not followed.
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            walk(&entry.path(), &child, depth + 1, out, bytes)?;
        } else if kind.is_file() && !skipped(&child) {
            let meta = entry.metadata().map_err(|e| format!("{child}: {e}"))?;
            let data = fs::read(entry.path()).map_err(|e| format!("{child}: {e}"))?;
            *bytes += data.len();
            if out.files.len() >= MAX_FILES || *bytes > MAX_BYTES {
                return Err("the package is too large".to_owned());
            }
            if crate::platform::marked_exec(&meta, &data) {
                out.exec.insert(child.clone());
            }
            out.files.insert(child, data);
        }
    }
    Ok(())
}

/// `dir/tool_dir` if it is a real folder (a link could point anywhere).
fn tool_folder(dir: &Path, tool_dir: &str) -> Option<PathBuf> {
    let base = dir.join(tool_dir);
    fs::symlink_metadata(&base)
        .is_ok_and(|m| m.is_dir())
        .then_some(base)
}

/// The installable files of a package folder.
pub fn read_files(dir: &Path) -> Result<Files, String> {
    let mut out = Files::default();
    let mut bytes = 0;
    for tool_dir in TOOL_DIRS {
        if let Some(base) = tool_folder(dir, tool_dir) {
            walk(&base, tool_dir, 0, &mut out, &mut bytes)?;
        }
    }
    Ok(out)
}

/// One file or folder as the change detector sees it.
pub type Seen = (PathBuf, Option<SystemTime>, u64);

/// The part of `signature` for one tool folder: the entries `walk` reads.
fn see(dir: &Path, rel: &str, depth: usize, left: &mut usize, out: &mut Vec<Seen>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(read) = fs::read_dir(dir) else { return };
    for entry in read.flatten() {
        if *left == 0 {
            return;
        }
        let path = entry.path();
        let child = format!("{rel}/{}", entry.file_name().to_string_lossy());
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            // Its time changes when a file in it comes or goes.
            *left -= 1;
            out.push((path.clone(), meta.modified().ok(), 0));
            see(&path, &child, depth + 1, left, out);
        } else if meta.is_file() && !skipped(&child) {
            *left -= 1;
            // The mode is part of the hash, so a chmod must show here too
            // (Windows has none: there the content decides, and its time shows).
            let exec = u64::from(crate::platform::any_exec(&meta));
            out.push((path, meta.modified().ok(), (meta.len() << 1) | exec));
        }
    }
}

/// A cheap change detector for `<home>/packages`: the times and sizes of what
/// a load reads and nothing else (each package's manifest and tool folders,
/// with the rules of `walk`), up to a fixed number of entries. Whatever else
/// lies there (a `.git`, a `node_modules`) is never walked.
#[must_use]
pub fn signature(packages: &Path) -> Vec<Seen> {
    let mut out = Vec::new();
    let Ok(read) = fs::read_dir(packages) else {
        return out;
    };
    let mut folders: Vec<PathBuf> = read
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    folders.sort();
    let mut total = MAX_SEEN_TOTAL;
    for folder in folders {
        if total == 0 {
            break;
        }
        let budget = MAX_SEEN.min(total);
        let mut left = budget;
        let manifest = folder.join(FILE_NAME);
        let (time, size) =
            fs::symlink_metadata(&manifest).map_or((None, 0), |m| (m.modified().ok(), m.len()));
        out.push((manifest, time, size));
        for tool_dir in TOOL_DIRS {
            if let Some(base) = tool_folder(&folder, tool_dir) {
                see(&base, tool_dir, 0, &mut left, &mut out);
            }
        }
        // The folder itself counts, so an empty one is still seen.
        out.push((folder, None, 0));
        total -= budget - left;
    }
    out.sort();
    out
}

/// Read `<packages>/<id>`.
pub fn load_dir(packages: &Path, id: &str) -> Result<Package, String> {
    let dir = packages.join(id);
    // A linked manifest could point anywhere (and be rewritten there).
    if fs::symlink_metadata(dir.join(FILE_NAME)).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(format!("{FILE_NAME} is a link: it is not used"));
    }
    let text = fs::read_to_string(dir.join(FILE_NAME))
        .map_err(|e| format!("cannot read {FILE_NAME}: {e}"))?;
    let manifest = Manifest::parse(&text).map_err(|errors| errors.join("; "))?;
    let Files { files, exec } = read_files(&dir)?;
    let hash = content_hash(&files, &exec, manifest.extends.as_deref());
    Ok(Package {
        id: id.to_owned(),
        manifest,
        files,
        exec,
        hash,
        builtin: false,
    })
}

/// The built-in package in the form that fits `variant`, for this platform.
pub fn builtin(variant: Variant) -> Result<Package, String> {
    builtin_for(variant, Platform::CURRENT)
}

/// The built-in package in the form that fits `variant`, as `platform`
/// installs it. Every platform embeds every file, so both forms can be checked
/// anywhere.
pub(crate) fn builtin_for(variant: Variant, platform: Platform) -> Result<Package, String> {
    let folder = variant.folder();
    let mut files = BTreeMap::new();
    let mut exec = BTreeSet::new();
    let mut manifest = None;
    for (name, path, executable, data) in embedded::FILES {
        if *name != folder || !platform.installs(path) {
            continue;
        }
        if *path == FILE_NAME {
            let text = std::str::from_utf8(data).map_err(|e| e.to_string())?;
            manifest = Some(Manifest::parse(text).map_err(|e| e.join("; "))?);
        } else if !skipped(path) {
            files.insert((*path).to_owned(), data.to_vec());
            if *executable {
                exec.insert((*path).to_owned());
            }
        }
    }
    let manifest = manifest.ok_or("the built-in package is damaged")?;
    // The files every platform installs: the same hash on a Mac and a PC.
    let mut common = files.clone();
    common.retain(|path, _| !WINDOWS_ONLY.contains(&path.as_str()));
    let hash = content_hash(&common, &exec, manifest.extends.as_deref());
    Ok(Package {
        id: BUILTIN_ID.to_owned(),
        manifest,
        files,
        exec,
        hash,
        builtin: true,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_builtin_package_has_three_variants_with_the_right_content() {
        let both = builtin(Variant::Both).unwrap();
        let claude = builtin(Variant::Claude).unwrap();
        let codex = builtin(Variant::Codex).unwrap();
        assert!(both.has_claude() && both.has_codex());
        assert!(claude.has_claude() && !claude.has_codex());
        assert!(!codex.has_claude() && codex.has_codex());
        assert!(
            claude
                .files
                .contains_key(".claude/skills/dev-team/SKILL.md")
        );
        assert!(codex.files.contains_key(".agents/skills/dev-team/SKILL.md"));
        assert_ne!(claude.hash, both.hash);
        assert!(!both.files.keys().any(|p| p.starts_with("tools/")));
        assert!(!both.files.contains_key("setup_teams.sh"));
    }

    /// Guards against a repeat of `.claude/` being left out of the repository by
    /// an ignore rule: the embedded variants must hold their skills.
    #[test]
    fn every_variant_embeds_the_skills_it_should() {
        let has = |p: &Package, prefix: &str| p.files.keys().any(|f| f.starts_with(prefix));
        for (variant, claude, codex) in [
            (Variant::Both, true, true),
            (Variant::Claude, true, false),
            (Variant::Codex, false, true),
        ] {
            let package = builtin(variant).unwrap();
            assert_eq!(
                has(&package, ".claude/skills/"),
                claude,
                "{variant:?}: .claude/skills (is `.claude/` ignored by git?)"
            );
            assert_eq!(
                has(&package, ".agents/skills/"),
                codex,
                "{variant:?}: .agents/skills"
            );
            assert_eq!(
                has(&package, ".codex/agents/"),
                codex,
                "{variant:?}: .codex/agents"
            );
        }
    }

    #[test]
    fn reviewers_use_the_same_cli_in_the_single_tool_variants() {
        let text = |p: &Package, path: &str| {
            String::from_utf8(p.files.get(path).cloned().unwrap()).unwrap()
        };
        let claude = builtin(Variant::Claude).unwrap();
        for skill in ["dev-team", "brainstorm-team"] {
            let body = text(&claude, &format!(".claude/skills/{skill}/SKILL.md"));
            assert!(
                body.contains("connecto add claude --model claude-opus-5-5"),
                "{skill}"
            );
            assert!(!body.to_lowercase().contains("codex"), "{skill}");
        }
        let settings = text(&claude, ".claude/settings.json");
        assert!(!settings.contains("codex"));
        assert!(settings.contains("statusLine"));
        let codex = builtin(Variant::Codex).unwrap();
        for skill in ["dev-team", "brainstorm-team"] {
            let body = text(&codex, &format!(".agents/skills/{skill}/SKILL.md"));
            assert!(
                body.contains("connecto add codex --model gpt-6.1-sol"),
                "{skill}"
            );
            assert!(!body.contains("claude-opus"), "{skill}");
            assert!(!body.contains("connecto add claude"), "{skill}");
        }
        let both = builtin(Variant::Both).unwrap();
        let body = text(&both, ".claude/skills/dev-team/SKILL.md");
        assert!(body.contains("connecto add codex --model gpt-6.1-sol"));
        let body = text(&both, ".agents/skills/dev-team/SKILL.md");
        assert!(body.contains("connecto add claude --model claude-opus-5-5"));
    }

    #[test]
    fn the_vendored_variants_pass_the_package_check() {
        for variant in ["both", "claude", "codex"] {
            let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("builtin")
                .join(variant);
            let (ok, report) = connecto_hook::package::check_dir(&dir);
            assert!(ok, "{variant}: {report}");
        }
    }

    /// What each platform installs of the built-in package. macOS (any Unix)
    /// installs exactly what it did before the Windows twins were added: 83
    /// embedded files (80 installable and 3 manifests), 4 of them executable,
    /// and the hash of the same files. Windows installs those and the 4 twins,
    /// under the same hash.
    #[test]
    fn each_platform_installs_its_own_builtin_files_under_one_hash() {
        let table: Vec<String> = embedded::FILES
            .iter()
            .map(|(variant, path, _, _)| format!("{variant}/{path}"))
            .collect();
        assert_eq!(table.len(), 87, "every file is embedded on every platform");
        let listed = |platform: Platform| {
            let mut names = Vec::new();
            let mut exec = Vec::new();
            for variant in [Variant::Both, Variant::Claude, Variant::Codex] {
                let package = builtin_for(variant, platform).unwrap();
                for path in package.files.keys() {
                    names.push(format!("{}/{path}", variant.folder()));
                }
                for path in &package.exec {
                    exec.push(format!("{}/{path}", variant.folder()));
                }
            }
            (names, exec)
        };
        let (unix, unix_exec) = listed(Platform::Unix);
        let (windows, windows_exec) = listed(Platform::Windows);
        assert_eq!(unix.len() + 3, 83);
        assert_eq!(windows.len() + 3, 87);
        let scripts = [
            "both/.claude/scripts/statusline.sh",
            "both/.claude/scripts/subagent-statusline.sh",
            "claude/.claude/scripts/statusline.sh",
            "claude/.claude/scripts/subagent-statusline.sh",
        ];
        assert_eq!(unix_exec, scripts);
        assert_eq!(windows_exec, scripts, "a twin is no executable");
        // The names, pinned: a file that comes or goes on macOS shows here.
        let digest = hash_bytes(unix.join("\n").as_bytes());
        assert_eq!(
            digest,
            "1078ebbf0c80ac1842f68d221a1239be549a392da2f409a6d8f39f30ec08d78f",
            "the macOS file list changed:\n{}",
            unix.join("\n")
        );
        let mut extra: Vec<&String> = windows.iter().filter(|n| !unix.contains(n)).collect();
        extra.sort();
        assert_eq!(
            extra,
            [
                "both/.claude/scripts/statusline.ps1",
                "both/.claude/scripts/subagent-statusline.ps1",
                "claude/.claude/scripts/statusline.ps1",
                "claude/.claude/scripts/subagent-statusline.ps1",
            ]
        );
        assert!(unix.iter().all(|n| windows.contains(n)));
        for variant in [Variant::Both, Variant::Claude, Variant::Codex] {
            let mac = builtin_for(variant, Platform::Unix).unwrap();
            let pc = builtin_for(variant, Platform::Windows).unwrap();
            assert_eq!(mac.hash, pc.hash, "{variant:?}"); // The hash is that of the whole Unix form, as before the twins.
            assert_eq!(mac.hash, content_hash(&mac.files, &mac.exec, None));
            for twin in WINDOWS_ONLY {
                assert!(!mac.files.contains_key(twin), "{variant:?}");
                assert_eq!(
                    pc.files.contains_key(twin),
                    variant != Variant::Codex,
                    "{variant:?}"
                );
            }
        }
        assert_eq!(
            builtin(Variant::Both).unwrap(),
            builtin_for(Variant::Both, Platform::CURRENT).unwrap()
        );
    }

    /// The twins sit next to their `.sh` originals under the same base name;
    /// `connecto-adapter` names the same files (`statusline.rs`).
    #[test]
    fn the_twins_are_named_after_the_originals_next_to_them() {
        assert_eq!(STATUSLINE_PS1, ".claude/scripts/statusline.ps1");
        assert_eq!(
            SUBAGENT_STATUSLINE_PS1,
            ".claude/scripts/subagent-statusline.ps1"
        );
        let pc = builtin_for(Variant::Claude, Platform::Windows).unwrap();
        for twin in WINDOWS_ONLY {
            let original = twin.replace(".ps1", ".sh");
            assert!(pc.files.contains_key(&original), "{original}");
            assert!(pc.files.contains_key(twin), "{twin}");
        }
        assert!(Platform::Unix.foreign(STATUSLINE_PS1));
        assert!(!Platform::Unix.foreign(".claude/scripts/statusline.sh"));
        assert!(!Platform::Windows.foreign(STATUSLINE_PS1));
        assert!(!Platform::Windows.foreign(".claude/scripts/statusline.sh"));
    }

    #[test]
    fn variants_follow_the_tools_setting() {
        assert_eq!(Variant::for_tools(ProjectTools::Claude), Variant::Claude);
        assert_eq!(Variant::for_tools(ProjectTools::Codex), Variant::Codex);
        assert_eq!(Variant::for_tools(ProjectTools::Both), Variant::Both);
    }

    #[test]
    fn the_hash_follows_the_files_and_the_base_only() {
        let mut files = BTreeMap::new();
        files.insert(".claude/a".to_owned(), b"1".to_vec());
        let none = BTreeSet::new();
        let one = content_hash(&files, &none, None);
        assert_eq!(one, content_hash(&files, &none, None));
        assert_ne!(one, content_hash(&files, &none, Some("base")));
        // A chmod is a change of content.
        let exec: BTreeSet<String> = [".claude/a".to_owned()].into();
        assert_ne!(one, content_hash(&files, &exec, None));
        files.insert(".claude/a".to_owned(), b"2".to_vec());
        assert_ne!(one, content_hash(&files, &none, None));
    }

    #[cfg(unix)]
    #[test]
    fn the_executable_bit_is_read_from_folders_and_kept_in_the_builtin_package() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("mg-pkgexec-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("team");
        fs::create_dir_all(dir.join(".claude/scripts")).unwrap();
        fs::write(dir.join("package.json"), r#"{"name":"Team"}"#).unwrap();
        fs::write(dir.join(".claude/scripts/run.sh"), "#!/bin/sh\n").unwrap();
        fs::write(dir.join(".claude/a.md"), "a").unwrap();
        let plain = load_dir(&root, "team").unwrap();
        assert!(plain.exec.is_empty());
        fs::set_permissions(
            dir.join(".claude/scripts/run.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let package = load_dir(&root, "team").unwrap();
        let exec: Vec<_> = package.exec.iter().cloned().collect();
        assert_eq!(exec, [".claude/scripts/run.sh"]);
        assert_ne!(plain.hash, package.hash, "a chmod changes the hash");
        let _ = fs::remove_dir_all(&root);
        for variant in [Variant::Both, Variant::Claude] {
            let builtin = builtin(variant).unwrap();
            assert!(
                builtin.exec.contains(".claude/scripts/statusline.sh"),
                "{variant:?}"
            );
            assert!(!builtin.exec.contains(".claude/settings.json"));
        }
    }

    /// Windows has no executable bit: a file that starts with `#!` is a
    /// script, in a package folder as in the built-in package.
    #[cfg(windows)]
    #[test]
    fn a_script_is_told_by_its_first_line_and_kept_in_the_builtin_package() {
        let root = std::env::temp_dir().join(format!("mg-pkgexec-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("team");
        fs::create_dir_all(dir.join(".claude/scripts")).unwrap();
        fs::write(dir.join("package.json"), r#"{"name":"Team"}"#).unwrap();
        fs::write(dir.join(".claude/scripts/run.sh"), "echo\n").unwrap();
        fs::write(dir.join(".claude/a.md"), "#!a").unwrap();
        let plain = load_dir(&root, "team").unwrap();
        assert_eq!(
            plain.exec.iter().cloned().collect::<Vec<_>>(),
            [".claude/a.md"]
        );
        fs::write(dir.join(".claude/scripts/run.sh"), "#!/bin/sh\necho\n").unwrap();
        let package = load_dir(&root, "team").unwrap();
        assert!(package.exec.contains(".claude/scripts/run.sh"));
        assert_ne!(plain.hash, package.hash);
        let _ = fs::remove_dir_all(&root);
        for variant in [Variant::Both, Variant::Claude] {
            let builtin = builtin(variant).unwrap();
            for script in [
                ".claude/scripts/statusline.sh",
                ".claude/scripts/subagent-statusline.sh",
            ] {
                assert!(builtin.exec.contains(script), "{variant:?} {script}");
            }
            assert!(!builtin.exec.contains(".claude/settings.json"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_folder_inside_a_package_is_a_load_error_not_missing_files() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("mg-pkgdeny-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("team");
        fs::create_dir_all(dir.join(".claude/skills/s")).unwrap();
        fs::write(dir.join("package.json"), r#"{"name":"Team"}"#).unwrap();
        fs::write(dir.join(".claude/a.md"), "a").unwrap();
        fs::write(dir.join(".claude/skills/s/SKILL.md"), "s").unwrap();
        assert_eq!(load_dir(&root, "team").unwrap().files.len(), 2);
        let locked = dir.join(".claude/skills");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let denied = fs::read_dir(&locked).is_err();
        let loaded = load_dir(&root, "team");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        // Root reads any folder, so there the load simply works.
        if denied {
            let why = loaded.unwrap_err();
            assert!(why.contains(".claude/skills"), "{why}");
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_signature_follows_what_a_load_reads_and_nothing_beside_it() {
        let root = std::env::temp_dir().join(format!("mg-pkgsig-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("team");
        fs::create_dir_all(dir.join(".claude/skills/s")).unwrap();
        fs::write(dir.join("package.json"), r#"{"name":"Team"}"#).unwrap();
        fs::write(dir.join(".claude/skills/s/SKILL.md"), "s").unwrap();
        let before = signature(&root);
        assert!(before.iter().any(|(p, ..)| p.ends_with("SKILL.md")));
        assert!(before.iter().any(|(p, ..)| p.ends_with("package.json")));
        // Not read by a load: never walked, however large.
        fs::create_dir_all(dir.join("node_modules/x/y")).unwrap();
        fs::write(dir.join("node_modules/x/y/index.js"), "x").unwrap();
        fs::create_dir_all(root.join(".git/objects")).unwrap();
        fs::write(root.join(".git/objects/aa"), "x").unwrap();
        fs::write(dir.join("CLAUDE.md"), "guide").unwrap();
        fs::write(dir.join("ORIGIN.md"), "from a repo").unwrap();
        fs::write(dir.join(".claude/.DS_Store"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc", dir.join(".agents")).unwrap();
        #[cfg(windows)]
        crate::platform::link_dir(&std::env::temp_dir(), &dir.join(".agents")).unwrap();
        let after = signature(&root);
        let inside = |s: &[Seen], part: &str| {
            s.iter()
                .any(|(p, ..)| p.components().any(|c| c.as_os_str() == part))
        };
        for part in [
            "node_modules",
            "objects",
            ".agents",
            "CLAUDE.md",
            "ORIGIN.md",
            ".DS_Store",
        ] {
            assert!(!inside(&after, part), "{part}");
        }
        // What a load reads does show: content, a chmod, a new folder.
        fs::write(dir.join(".claude/skills/s/SKILL.md"), "longer").unwrap();
        let edited = signature(&root);
        assert_ne!(after, edited);
        // Windows has no mode to change.
        #[cfg(unix)]
        let edited = {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                dir.join(".claude/skills/s/SKILL.md"),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            let chmod = signature(&root);
            assert_ne!(edited, chmod);
            chmod
        };
        fs::create_dir_all(root.join("empty")).unwrap();
        assert_ne!(edited, signature(&root));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_signature_stops_at_a_fixed_number_of_entries() {
        let root = std::env::temp_dir().join(format!("mg-pkgsigcap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("big/.claude/many");
        fs::create_dir_all(&dir).unwrap();
        for n in 0..MAX_SEEN + 50 {
            fs::write(dir.join(format!("f{n}")), "").unwrap();
        }
        // The manifest and the folder itself, then no more than the cap.
        assert_eq!(signature(&root).len(), MAX_SEEN + 2);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_folder_loads_its_tool_dirs_only_and_skips_links_and_local_settings() {
        let root = std::env::temp_dir().join(format!("mg-pkgload-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("team");
        fs::create_dir_all(dir.join(".claude/skills/s")).unwrap();
        fs::create_dir_all(dir.join(".agents")).unwrap();
        fs::write(dir.join("package.json"), r#"{"name":"Team","version":2}"#).unwrap();
        fs::write(dir.join("CLAUDE.md"), "guide").unwrap();
        fs::write(dir.join("ORIGIN.md"), "from a repo").unwrap();
        fs::write(dir.join(".claude/skills/s/SKILL.md"), "s").unwrap();
        fs::write(dir.join(".claude/settings.local.json"), "{}").unwrap();
        fs::write(dir.join(".agents/x"), "x").unwrap();
        // A file symlink needs a privilege on Windows; a junction does not.
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/hosts", dir.join(".claude/link")).unwrap();
        #[cfg(windows)]
        crate::platform::link_dir(&std::env::temp_dir(), &dir.join(".claude/link")).unwrap();
        let package = load_dir(&root, "team").unwrap();
        assert_eq!(package.manifest.version, 2);
        let names: Vec<_> = package.files.keys().cloned().collect();
        assert_eq!(names, [".agents/x", ".claude/skills/s/SKILL.md"]);
        assert!(load_dir(&root, "missing").is_err());
        // A linked manifest is refused.
        #[cfg(unix)]
        {
            fs::write(root.join("real.json"), r#"{"name":"X"}"#).unwrap();
            fs::remove_file(dir.join("package.json")).unwrap();
            std::os::unix::fs::symlink(root.join("real.json"), dir.join("package.json")).unwrap();
            assert!(load_dir(&root, "team").unwrap_err().contains("link"));
        }
        let _ = fs::remove_dir_all(&root);
    }
}
