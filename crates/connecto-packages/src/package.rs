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
        if tools.len() > 1 {
            // Two or more tools: the teams choose among the available ones
            // (a tool the project does not allow cannot be started).
            Self::Both
        } else if tools.allows(connecto_core::ToolKind::Claude) {
            Self::Claude
        } else {
            // A project of Pi, Antigravity or OpenCode alone runs everything
            // in one tool, as a Codex one does, and reads the same skills.
            Self::Codex
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
    if is_claude(path) {
        tools.allows(ToolKind::Claude)
    } else if path.starts_with(".agents/") {
        // The skills folder is Codex's, Pi's, Antigravity's and OpenCode's
        // alike.
        tools.allows(ToolKind::Codex)
            || tools.allows(ToolKind::Pi)
            || tools.allows(ToolKind::Antigravity)
            || tools.allows(ToolKind::OpenCode)
    } else {
        tools.allows(ToolKind::Codex)
    }
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
    embedded_package(variant.folder(), BUILTIN_ID, platform)
}

/// The built-in package's always-on rule for Claude: end a task with a status
/// recap. Claude Code reads every `.claude/rules/*.md` like `CLAUDE.md`.
pub const STATUS_RULE: &str = ".claude/rules/connecto-status.md";

/// Id of the built-in Game Dev package (Godot), ADR 0037.
pub const GAME_DEV_ID: &str = connecto_core::GAME_DEV_PACKAGE_ID;

/// Whether `id` names a package built into Connecto (never a folder in
/// Connecto Home: such a folder is not read).
#[must_use]
pub fn is_builtin(id: &str) -> bool {
    connecto_hook::package::BUILTIN_IDS.contains(&id)
}

/// The built-in Game Dev package. One form for every agent: the installer
/// takes the tool folders each project's Tools setting allows.
pub fn game_dev() -> Result<Package, String> {
    embedded_package(GAME_DEV_ID, GAME_DEV_ID, Platform::CURRENT)
}

fn embedded_package(folder: &str, id: &str, platform: Platform) -> Result<Package, String> {
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
        id: id.to_owned(),
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
    use connecto_core::ToolKind;

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
            // Every role runs in a Connecto terminal: no agent definitions.
            assert!(!has(&package, ".codex/"), "{variant:?}: .codex");
            assert!(
                !has(&package, ".claude/agents/"),
                "{variant:?}: .claude/agents"
            );
            for team in ["daily-team", "dev-team", "brainstorm-team"] {
                if claude {
                    assert!(
                        package
                            .files
                            .contains_key(&format!(".claude/skills/{team}/SKILL.md")),
                        "{variant:?}: {team}"
                    );
                }
                if codex {
                    assert!(
                        package
                            .files
                            .contains_key(&format!(".agents/skills/{team}/SKILL.md")),
                        "{variant:?}: {team}"
                    );
                }
            }
        }
    }

    /// Every agent of a project with the built-in package ends a task with the
    /// four-point status recap: Claude from `.claude/rules`, the others from
    /// the launcher, which passes the same text compiled into Connecto.
    #[test]
    fn every_variant_carries_the_end_of_task_status_recap() {
        for (variant, claude) in [
            (Variant::Both, true),
            (Variant::Claude, true),
            (Variant::Codex, false),
        ] {
            let package = builtin(variant).unwrap();
            assert!(package.manifest.version >= 9, "{variant:?}: bump VERSION");
            assert_eq!(
                package.files.contains_key(STATUS_RULE),
                claude,
                "{variant:?}"
            );
            // Nothing the launcher would read: the others' text is Connecto's.
            assert!(
                !package
                    .files
                    .keys()
                    .any(|p| p.starts_with(".agents/instructions")),
                "{variant:?}"
            );
            if let Some(rule) = package.files.get(STATUS_RULE) {
                assert_eq!(
                    std::str::from_utf8(rule).unwrap(),
                    connecto_session::package_text::STATUS_RECAP,
                    "{variant:?}: one text for every agent"
                );
            }
        }
        let text = connecto_session::package_text::STATUS_RECAP;
        for wanted in [
            "**Done**",
            "**Committed/pushed**",
            "branch + hash",
            "committed locally",
            "pushed",
            "**Not done / blocked**",
            "**Waiting on user**",
            "yes/no",
        ] {
            assert!(text.contains(wanted), "{wanted}");
        }
    }

    /// The skill that is not a team: how to set up Agentic Works.
    const WORKS_SKILL: &str = "/skills/agentic-works/SKILL.md";

    /// The skill that is a single consultation, not a team (ADR 0045).
    const SECOND_OPINION: &str = "/skills/second-opinion/SKILL.md";

    /// The team skills of one variant, by path.
    fn skill_texts(package: &Package) -> Vec<(String, String)> {
        package
            .files
            .iter()
            .filter(|(path, _)| {
                path.ends_with("/SKILL.md")
                    && !path.ends_with(WORKS_SKILL)
                    && !path.ends_with(SECOND_OPINION)
            })
            .map(|(path, data)| (path.clone(), String::from_utf8(data.clone()).unwrap()))
            .collect()
    }

    /// The effort rubric every skill that starts terminals carries.
    const RUBRIC: [&str; 4] = [
        "- `low`: a trivial edit",
        "- `medium`: a normal feature or fix",
        "- `high`: work across modules or crates, concurrency, security, or a risk of losing data",
        "- `xhigh`: only a rare, very hard problem",
    ];

    /// Every variant's teams take their agents from `connecto agents
    /// --roles`, which knows the project's allowed CLIs and which are signed
    /// in, and choose an effort with the rubric (ADR 0045). No variant keeps
    /// a table of its own, nor the Argon models the roster replaced. A
    /// Claude-only project's teams name no other CLI.
    #[test]
    fn each_variant_takes_its_agents_from_the_roster() {
        for (variant, count) in [
            (Variant::Claude, 3),
            (Variant::Codex, 3),
            (Variant::Both, 6),
        ] {
            let package = builtin(variant).unwrap();
            let skills = skill_texts(&package);
            assert_eq!(skills.len(), count, "{variant:?}");
            for (path, body) in &skills {
                assert!(body.contains("connecto agents --roles"), "{path}");
                assert!(body.contains("(same CLI)"), "{path}");
                assert!(
                    body.contains("The reviewer gets the same effort as the coder."),
                    "{path}"
                );
                for line in RUBRIC {
                    assert!(body.contains(line), "{path}: {line}");
                }
                assert!(!body.contains("command -v"), "{path}");
                assert!(!body.contains("gemini-4-argon"), "{path}");
                assert!(!body.contains("aren't used by this team"), "{path}");
                assert!(body.contains(r#"`unknown command "agents"`"#), "{path}");
                assert!(body.contains("`(unverified)`"), "{path}");
                // The teams that only the user starts say so to Pi too.
                if !path.contains("daily-team") {
                    assert!(body.contains("disable-model-invocation: true"), "{path}");
                }
                if variant == Variant::Claude {
                    assert!(!body.to_lowercase().contains("codex"), "{path}");
                    assert!(!body.to_lowercase().contains("antigravity"), "{path}");
                }
            }
        }
        let claude = builtin(Variant::Claude).unwrap();
        let settings = String::from_utf8(claude.files[".claude/settings.json"].clone()).unwrap();
        assert!(!settings.contains("codex"));
        assert!(settings.contains("statusLine"));
    }

    /// The second opinion is in every variant, for each tool folder the
    /// variant has; only the user starts it; it takes its agent from the
    /// roster, labels a same-CLI opinion, and changes nothing (ADR 0045).
    #[test]
    fn every_variant_has_the_second_opinion_only_on_request() {
        for (variant, sides) in [
            (Variant::Both, &[".claude", ".agents"][..]),
            (Variant::Claude, &[".claude"][..]),
            (Variant::Codex, &[".agents"][..]),
        ] {
            let package = builtin(variant).unwrap();
            for side in sides {
                let path = format!("{side}{SECOND_OPINION}");
                let body = String::from_utf8(package.files[&path].clone()).unwrap();
                assert!(body.contains("connecto agents --roles"), "{path}");
                assert!(body.contains("`second-opinion` line"), "{path}");
                assert!(body.contains("Same CLI as mine"), "{path}");
                assert!(body.contains("Read only"), "{path}");
                for line in RUBRIC {
                    assert!(body.contains(line), "{path}: {line}");
                }
                // Second review: an older `connecto` command, which does not
                // know `agents`, is recognized too; then the opinion comes
                // from the asker's own CLI, which the project allows, and is
                // labelled so; and the label follows the terminal used.
                assert!(body.contains(r#"`unknown command "agents"`"#), "{path}");
                assert!(
                    body.contains("start a fresh session of your own CLI"),
                    "{path}"
                );
                // Third review: with an older Connecto, the other CLIs are
                // tried first, and the same CLI comes only after them; an
                // agent the user named is used when unverified too.
                assert!(
                    body.contains("first try the CLIs other than your own"),
                    "{path}"
                );
                // Fourth review: a CLI the user named comes first there too,
                // and is not swapped for another without asking.
                assert!(
                    body.contains("If the user named a CLI, try that one first"),
                    "{path}"
                );
                assert!(
                    body.contains("tell the user why and ask before trying another"),
                    "{path}"
                );
                assert!(
                    body.contains("Only when every other CLI has been refused or has failed"),
                    "{path}"
                );
                assert!(body.contains("`ready` or `unverified`"), "{path}");
                assert!(body.contains("Whenever that CLI is your own"), "{path}");
                assert!(body.contains("`(unverified)`"), "{path}");
                // Pi reads this field too, so it stays in `.agents` as well.
                assert!(body.contains("disable-model-invocation: true"), "{path}");
                if *side == ".agents" {
                    assert!(!body.contains("$ARGUMENTS"), "{path}");
                    let yaml = ".agents/skills/second-opinion/agents/openai.yaml";
                    let yaml = String::from_utf8(package.files[yaml].clone()).unwrap();
                    assert!(yaml.contains("allow_implicit_invocation: false"), "{yaml}");
                }
            }
        }
    }

    /// The rules every team keeps, in every form: roles in Connecto terminals
    /// briefed from the skill's role files, and at most 3 review rounds
    /// before the user chooses a compromise.
    #[test]
    fn every_team_runs_its_roles_in_terminals_with_three_rounds_at_most() {
        for variant in [Variant::Both, Variant::Claude, Variant::Codex] {
            let package = builtin(variant).unwrap();
            for (path, body) in skill_texts(&package) {
                assert!(
                    !body.contains("<!-- agents -->\n<!-- /agents -->"),
                    "{path}"
                );
                assert!(body.contains("## Choosing the agents"), "{path}");
                assert!(body.contains("at most 3"), "{path}");
                assert!(body.contains("**Options:** two or three"), "{path}");
                assert!(body.contains("Then wait for the user."), "{path}");
                assert!(!body.contains("SendMessage"), "{path}");
                assert!(!body.contains("subagent_type"), "{path}");
                // Every role file is named by its skill, and every one named
                // is there.
                let dir = format!("{}roles/", path.trim_end_matches("SKILL.md"));
                let roles: Vec<&str> = package
                    .files
                    .keys()
                    .filter_map(|file| file.strip_prefix(dir.as_str()))
                    .collect();
                assert!(!roles.is_empty(), "{path}");
                for role in &roles {
                    assert!(body.contains(role), "{path}: {role}");
                }
                for (at, _) in body.match_indices("/roles/") {
                    let name: String = body[at + 7..]
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.')
                        .collect();
                    if name.ends_with(".md") {
                        assert!(roles.contains(&name.as_str()), "{path}: {name}");
                    }
                }
                if path.starts_with(".agents/") {
                    assert!(!body.contains("$ARGUMENTS"), "{path}");
                    assert!(!body.contains("CLAUDE_SKILL_DIR"), "{path}");
                }
            }
        }
    }

    /// Every variant teaches its agents `connecto works`, by itself (no team
    /// roles), and lets the model pick it up when asked.
    #[test]
    fn every_variant_has_the_agentic_works_skill() {
        for (variant, sides) in [
            (Variant::Both, &[".claude", ".agents"][..]),
            (Variant::Claude, &[".claude"][..]),
            (Variant::Codex, &[".agents"][..]),
        ] {
            let package = builtin(variant).unwrap();
            for side in sides {
                let path = format!("{side}{WORKS_SKILL}");
                let body = String::from_utf8(package.files[&path].clone()).unwrap();
                for wanted in [
                    "connecto works link",
                    "connecto works publish",
                    "connecto works test",
                    "--agentic-works",
                ] {
                    assert!(body.contains(wanted), "{variant:?} {path}: {wanted}");
                }
                assert!(!body.contains("disable-model-invocation"), "{path}");
            }
            assert!(
                !package
                    .files
                    .keys()
                    .any(|f| f.contains("agentic-works/agents/")),
                "{variant:?}: implicit invocation stays allowed"
            );
        }
    }

    /// The daily team is how code changes are made by default, so the model
    /// may pick it up by itself on every side, and a team's own coder or
    /// reviewer (briefed by another agent) is told not to.
    #[test]
    fn every_variant_lets_the_model_start_the_daily_team() {
        for (variant, sides) in [
            (Variant::Both, &[".claude", ".agents"][..]),
            (Variant::Claude, &[".claude"][..]),
            (Variant::Codex, &[".agents"][..]),
        ] {
            let package = builtin(variant).unwrap();
            for side in sides {
                let path = format!("{side}/skills/daily-team/SKILL.md");
                let body = String::from_utf8(package.files[&path].clone()).unwrap();
                assert!(!body.contains("disable-model-invocation"), "{path}");
                assert!(body.contains("changes code or files"), "{path}");
                assert!(body.contains("[connecto from"), "{path}");
            }
            if let Some(policy) = package
                .files
                .get(".agents/skills/daily-team/agents/openai.yaml")
            {
                let policy = String::from_utf8(policy.clone()).unwrap();
                assert!(
                    policy.contains("allow_implicit_invocation: true"),
                    "{variant:?}: {policy}"
                );
            }
        }
    }

    #[test]
    fn game_dev_is_one_form_for_every_agent_and_needs_godot_blender_and_ffmpeg() {
        assert_eq!(
            connecto_hook::package::BUILTIN_IDS,
            [BUILTIN_ID, GAME_DEV_ID],
            "connecto-hook names the same built-in packages"
        );
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("builtin")
            .join(GAME_DEV_ID);
        let (ok, report) = connecto_hook::package::check_dir(&dir);
        assert!(ok, "{report}");
        let game = game_dev().unwrap();
        assert!(game.builtin && game.manifest.extends.is_none());
        let skills = ["game-design", "game-playtest", "godot-game"];
        for skill in skills {
            let claude = &game.files[&format!(".claude/skills/{skill}/SKILL.md")];
            let agents = &game.files[&format!(".agents/skills/{skill}/SKILL.md")];
            assert_eq!(claude, agents, "{skill}: run tools/vendor_builtin.py");
            assert!(
                game.files
                    .contains_key(&format!(".agents/skills/{skill}/agents/openai.yaml"))
            );
        }
        // The reference files a SKILL.md points to, next to it on both sides.
        let references = [
            "game-design/team.md",
            "game-playtest/bot.md",
            "godot-game/art-pipeline.md",
            "godot-game/gotchas.md",
            "godot-game/test-runner.md",
        ];
        for reference in references {
            let claude = &game.files[&format!(".claude/skills/{reference}")];
            let agents = &game.files[&format!(".agents/skills/{reference}")];
            assert_eq!(claude, agents, "{reference}: run tools/vendor_builtin.py");
        }
        assert_eq!(
            game.files.len(),
            skills.len() * 3 + references.len() * 2,
            "nothing else is installed"
        );
        // Version 2 shipped in 0.3.10 with Godot only.
        assert_eq!(game.manifest.version, 3);
        let ids: Vec<&str> = game
            .manifest
            .requires
            .iter()
            .map(|r| r.id.as_str())
            .collect();
        assert_eq!(
            ids,
            ["godot", "blender", "ffmpeg"],
            "the art is made by Blender scripts and reviewed on frames FFmpeg pulls"
        );
        let godot = &game.manifest.requires[0];
        assert_eq!(godot.min_version.as_deref(), Some("4.7"));
        // Godot 4 only: the skills are written for it.
        assert!(godot.version_fits("4.7") && godot.version_fits("4.9.1"));
        assert!(!godot.version_fits("4.6.2") && !godot.version_fits("5.0"));
        let blender = &game.manifest.requires[1];
        assert!(blender.version_fits("4.2") && blender.version_fits("5.3.0"));
        assert!(!blender.version_fits("4.1.1"));
        let ffmpeg = &game.manifest.requires[2];
        // The skills' frame commands were run on 4.4.2 (Ubuntu 22.04) and 9.0.2.
        assert!(ffmpeg.version_fits("4.4.2") && ffmpeg.version_fits("9.0.2"));
        assert!(!ffmpeg.version_fits("4.3.9"));
        for tool in &game.manifest.requires {
            assert!(
                !tool.licence,
                "{}: free software, no licence to accept",
                tool.id
            );
            for os in connecto_hook::requires::Os::ALL {
                assert!(tool.install_on(os).is_some(), "{} on {os:?}", tool.id);
            }
        }
        // winget's Blender is a machine-wide MSI in a folder named after the
        // version; its FFmpeg is a portable zip behind a Links shim.
        let windows = |r: &connecto_hook::requires::Requirement| {
            r.paths.get(connecto_hook::requires::Os::Windows).clone()
        };
        assert!(
            windows(blender)
                .iter()
                .any(|p| p == "%ProgramFiles%\\Blender Foundation\\Blender 5.3\\blender.exe")
        );
        assert!(
            windows(ffmpeg)
                .iter()
                .any(|p| p == "%ProgramFiles%\\WinGet\\Links\\ffmpeg.exe")
        );
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
    /// installs 83 embedded files (80 installable and 3 manifests), 4 of them
    /// executable, under the hash of those files. Windows installs those and
    /// the 4 twins, under the same hash.
    #[test]
    fn each_platform_installs_its_own_builtin_files_under_one_hash() {
        let table: Vec<String> = embedded::FILES
            .iter()
            .filter(|(variant, _, _, _)| *variant != GAME_DEV_ID)
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
            "030bb444f47db19eeaa5564e074da0182a74303c86049b2866ce58dd1a550c5f",
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
        assert_eq!(Variant::for_tools(ProjectTools::All), Variant::Both);
        assert_eq!(Variant::for_tools(ProjectTools::OpenCode), Variant::Codex);
        let pair = |a, b| ProjectTools::from_tools([a, b]).unwrap();
        assert_eq!(
            Variant::for_tools(pair(ToolKind::Codex, ToolKind::Pi)),
            Variant::Both
        );
        assert_eq!(
            Variant::for_tools(pair(ToolKind::Claude, ToolKind::OpenCode)),
            Variant::Both
        );
    }

    /// Codex and Pi chosen: the `.agents` and `.codex` folders, no `.claude`.
    #[test]
    fn a_codex_and_pi_project_takes_their_folders_only() {
        let tools = ProjectTools::from_tools([ToolKind::Codex, ToolKind::Pi]).unwrap();
        assert!(wanted(".agents/skills/daily-team/SKILL.md", tools));
        assert!(wanted(".codex/config.toml", tools));
        assert!(!wanted(".claude/skills/daily-team/SKILL.md", tools));
        let pi_and_claude = ProjectTools::from_tools([ToolKind::Pi, ToolKind::Claude]).unwrap();
        assert!(wanted(".agents/skills/x/SKILL.md", pi_and_claude));
        assert!(wanted(".claude/settings.json", pi_and_claude));
        assert!(!wanted(".codex/config.toml", pi_and_claude));
    }

    /// OpenCode reads its skills from `.agents/skills`: a project of OpenCode
    /// alone gets that folder, and no `.claude` or `.codex` one.
    #[test]
    fn an_opencode_project_takes_the_agents_folder_only() {
        assert!(wanted(
            ".agents/skills/daily-team/SKILL.md",
            ProjectTools::OpenCode
        ));
        assert!(!wanted(
            ".claude/skills/daily-team/SKILL.md",
            ProjectTools::OpenCode
        ));
        assert!(!wanted(".codex/config.toml", ProjectTools::OpenCode));
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
