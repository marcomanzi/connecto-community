//! The initial build's inventory and coverage plan (spec §5.9): what the
//! project offers, what is read, what is left out and why. Everything comes
//! from the [`Host`], read-only; nothing here writes the project.
//!
//! - **Sources.** Project documentation and instructions, manifests, the
//!   component structure (a listing the host makes), relevant code, test
//!   configuration, git state (branch, HEAD, local changes including
//!   untracked files, the uncommitted diff), the commits of the history
//!   window and the past Connecto conversations of the window.
//! - **Left out, never read:** the user's exclusions (paths or globs),
//!   dependency and build folders, lock files, binaries (by extension here,
//!   by content when read), files that may hold credentials (`.env`, keys),
//!   files larger than [`MAX_FILE_BYTES`].
//! - **Selection.** Sources are ranked in tiers (instructions and README
//!   first, code last; locally changed and entry-point code before the rest)
//!   and taken while the total stays within `max_input_chars`; the others
//!   are listed as over the limit, with [`CoverageGap::InputLimit`].
//! - **Gaps** say what the build could not see: no git, no conversations,
//!   history older than the window, the limits, the user's exclusions.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use connecto_core::memory::{BootstrapOptions, CoverageGap, SourceClass, SourceCoverage};

use crate::host::{
    Commit, GitHistory, GitSnapshot, Host, PastConversation, PastConversations, ProjectFiles,
    TurnRole,
};

/// Files listed, at most.
pub const MAX_FILES: usize = 20_000;
/// Largest file read, in bytes: a larger one is left out (generated or data).
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Largest uncommitted diff read, in bytes.
pub const MAX_DIFF_BYTES: u64 = 2 * 1024 * 1024;
/// Commits read from the window, at most.
pub const MAX_COMMITS: usize = 500;
/// Lines of the structure listing, at most.
const MAX_STRUCTURE_LINES: usize = 600;
const DAY_MS: u64 = 86_400_000;

/// Folders of downloaded dependencies and build outputs, never read.
pub const IGNORED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    "vendor",
    ".venv",
    "venv",
    "__pycache__",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".gradle",
    "coverage",
    "Pods",
    ".dart_tool",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".cache",
    "obj",
    ".terraform",
    "DerivedData",
    ".build",
    "bower_components",
    "jspm_packages",
    ".parcel-cache",
    ".turbo",
];

/// Generated lock files, never read.
const LOCK_FILES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "poetry.lock",
    "Gemfile.lock",
    "composer.lock",
    "go.sum",
    "uv.lock",
    "Pipfile.lock",
    "bun.lockb",
    "flake.lock",
    "Podfile.lock",
    "packages.lock.json",
];

const BINARY_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "icns", "webp", "svg", "tif", "tiff", "pdf", "zip",
    "gz", "tgz", "bz2", "xz", "zst", "7z", "rar", "jar", "war", "class", "so", "dylib", "dll",
    "exe", "o", "a", "lib", "wasm", "ttf", "otf", "woff", "woff2", "eot", "mp3", "mp4", "mov",
    "wav", "ogg", "flac", "avi", "mkv", "webm", "psd", "sqlite", "sqlite3", "db", "bin", "dat",
    "pyc", "pyo", "node", "ipa", "apk", "aab", "dmg", "iso", "pack", "idx", "keystore", "jks",
    "xcf", "blend", "glb", "fbx", "parquet", "npy", "pt", "onnx", "map",
];

/// Names that may hold credentials: never read, whatever redaction would do.
fn sensitive(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == ".env"
        || lower.starts_with(".env.")
        || lower.ends_with(".env")
        || [
            ".pem", ".key", ".p12", ".pfx", ".crt", ".cer", ".der", ".asc", ".gpg", ".kdbx",
        ]
        .iter()
        .any(|ext| lower.ends_with(ext))
        || lower.starts_with("id_rsa")
        || lower.starts_with("id_ed25519")
        || lower.starts_with("id_ecdsa")
        || lower.starts_with("id_dsa")
        || matches!(
            lower.as_str(),
            ".npmrc" | ".netrc" | ".pypirc" | ".pgpass" | ".htpasswd" | ".git-credentials"
        )
        || lower.starts_with("credentials")
        || lower.starts_with("secrets.")
}

const CODE_EXTENSIONS: &[&str] = &[
    "rs", "py", "js", "jsx", "ts", "tsx", "mjs", "cjs", "go", "java", "kt", "kts", "swift", "m",
    "mm", "c", "h", "cc", "cpp", "cxx", "hpp", "hh", "cs", "fs", "fsx", "rb", "php", "scala", "ex",
    "exs", "erl", "hrl", "hs", "ml", "mli", "clj", "cljs", "dart", "lua", "sh", "bash", "zsh",
    "fish", "ps1", "sql", "vue", "svelte", "css", "scss", "sass", "less", "html", "proto",
    "graphql", "gql", "zig", "nim", "r", "jl", "toml", "yaml", "yml", "json", "xml", "ini", "cfg",
    "conf", "nix", "tf", "hcl", "gradle", "groovy", "pl", "pm", "vim", "el", "elm", "purs", "re",
    "res", "sol", "cmake", "mk", "bzl", "star", "txt",
];

const DOC_EXTENSIONS: &[&str] = &["md", "mdx", "markdown", "rst", "adoc", "asciidoc", "org"];

/// Files that instruct agents or contributors: the project's own rules.
const INSTRUCTION_FILES: &[&str] = &[
    "agents.md",
    "claude.md",
    "contributing.md",
    "conventions.md",
    ".cursorrules",
    ".windsurfrules",
    "copilot-instructions.md",
    "gemini.md",
];

const MANIFEST_FILES: &[&str] = &[
    "cargo.toml",
    "package.json",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "pipfile",
    "go.mod",
    "gemfile",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "build.sbt",
    "composer.json",
    "mix.exs",
    "package.swift",
    "cmakelists.txt",
    "makefile",
    "justfile",
    "dockerfile",
    "docker-compose.yml",
    "docker-compose.yaml",
    "compose.yml",
    "compose.yaml",
    "deno.json",
    "tsconfig.json",
    "rust-toolchain",
    "rust-toolchain.toml",
    ".tool-versions",
    "flake.nix",
    "default.nix",
    "meson.build",
    "build.bazel",
    "workspace",
    "module.bazel",
    "pubspec.yaml",
    "project.clj",
    "deps.edn",
    "stack.yaml",
    "dune-project",
    "vcpkg.json",
    "conanfile.txt",
];

const TEST_CONFIG_PREFIXES: &[&str] = &[
    "jest.config.",
    "vitest.config.",
    "karma.conf.",
    "playwright.config.",
    "cypress.config.",
    ".mocharc",
    "phpunit.xml",
    "nextest.toml",
    "codecov.",
    ".coveragerc",
    "pytest.ini",
    "tox.ini",
    "conftest.py",
    "noxfile.py",
    "jenkinsfile",
    ".gitlab-ci.",
    "azure-pipelines.",
    ".travis.",
    "bitbucket-pipelines.",
];

/// How a file is treated, from its path alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// A source of this class, with its rank (lower is read first).
    Source(SourceClass, u8),
    /// Never read, for this reason.
    Skip(Exclusion),
}

/// Why a source is not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Exclusion {
    /// Matches one of the user's exclusions.
    User,
    /// A dependency or build folder, or a lock file.
    Ignored,
    Binary,
    /// May hold credentials.
    Sensitive,
    TooLarge,
    /// Could not be read (gone, a link out of the project, not text).
    Unreadable,
    /// Its kind of source was not chosen.
    NotChosen,
    /// Git was refused for the repository (see [`CoverageGap::GitNotUsable`]).
    GitRefused,
    /// A nested repository or submodule: never entered.
    Nested,
}

impl Exclusion {
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::User => "excluded:user",
            Self::Ignored => "excluded:ignored",
            Self::Binary => "excluded:binary",
            Self::Sensitive => "excluded:sensitive",
            Self::TooLarge => "excluded:too_large",
            Self::Unreadable => "excluded:unreadable",
            Self::NotChosen => "excluded:not_chosen",
            Self::GitRefused => "excluded:git_refused",
            Self::Nested => "excluded:nested_repository",
        }
    }
}

fn extension(name: &str) -> Option<String> {
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty()).then(|| ext.to_ascii_lowercase())
}

/// Whether a path is a recorded decision (an ADR, an RFC, a decision log) or
/// one of the project's instruction files: the documents whose word can
/// make a decision or convention accepted (spec §5.9).
#[must_use]
pub fn decision_record(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    if INSTRUCTION_FILES.contains(&name) {
        return true;
    }
    let in_folder = lower.split('/').rev().skip(1).any(|dir| {
        matches!(
            dir,
            "adr" | "adrs" | "decisions" | "decision-records" | "rfcs" | "rfc"
        )
    });
    let named = name.starts_with("adr-")
        || name.starts_with("adr_")
        || name.starts_with("decision-")
        || name.starts_with("rfc-");
    (in_folder || named) && extension(name).is_some_and(|e| DOC_EXTENSIONS.contains(&e.as_str()))
}

/// How a file is treated, from its path (relative, `/`-separated) and size.
#[must_use]
pub fn classify(path: &str, bytes: u64) -> FileKind {
    let parts: Vec<&str> = path.split('/').collect();
    let name = parts.last().copied().unwrap_or(path);
    let lower = name.to_ascii_lowercase();
    let dirs = &parts[..parts.len().saturating_sub(1)];
    if dirs.iter().any(|d| IGNORED_DIRS.contains(d)) || LOCK_FILES.contains(&name) {
        return FileKind::Skip(Exclusion::Ignored);
    }
    if sensitive(name) {
        return FileKind::Skip(Exclusion::Sensitive);
    }
    let ext = extension(&lower);
    if ext
        .as_deref()
        .is_some_and(|e| BINARY_EXTENSIONS.contains(&e))
        || lower.ends_with(".min.js")
        || lower.ends_with(".min.css")
    {
        return FileKind::Skip(Exclusion::Binary);
    }
    if bytes > MAX_FILE_BYTES {
        return FileKind::Skip(Exclusion::TooLarge);
    }
    let depth = dirs.len();
    let lower_path = path.to_ascii_lowercase();
    let in_ci = lower_path.starts_with(".github/workflows/")
        || lower_path.starts_with(".circleci/")
        || lower_path.starts_with(".buildkite/");
    if in_ci || TEST_CONFIG_PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return FileKind::Source(SourceClass::TestConfig, 1);
    }
    if MANIFEST_FILES.contains(&lower.as_str())
        || lower.starts_with("requirements") && lower.ends_with(".txt")
        || lower.ends_with(".gemspec")
        || lower.ends_with(".csproj")
        || lower.ends_with(".fsproj")
        || lower.ends_with(".sln")
        || lower.ends_with(".cabal")
        || lower.ends_with(".opam")
    {
        return FileKind::Source(SourceClass::Manifests, if depth <= 1 { 1 } else { 2 });
    }
    if INSTRUCTION_FILES.contains(&lower.as_str()) {
        return FileKind::Source(SourceClass::Documentation, 0);
    }
    let is_doc = ext.as_deref().is_some_and(|e| DOC_EXTENSIONS.contains(&e))
        || (lower == "readme" || lower.starts_with("readme."));
    if is_doc {
        let rank = if lower.starts_with("readme") && depth == 0 {
            0
        } else if decision_record(path) {
            1
        } else if lower.starts_with("changelog") || lower.starts_with("history.") {
            4
        } else if matches!(
            lower.as_str(),
            "license.md" | "code_of_conduct.md" | "security.md"
        ) {
            5
        } else {
            3
        };
        return FileKind::Source(SourceClass::Documentation, rank);
    }
    if lower.starts_with("license") || lower.starts_with("copying") || lower.starts_with("notice") {
        return FileKind::Source(SourceClass::Documentation, 5);
    }
    if ext.as_deref().is_some_and(|e| CODE_EXTENSIONS.contains(&e)) {
        let entry = [
            "main.",
            "lib.",
            "index.",
            "app.",
            "mod.",
            "server.",
            "cli.",
            "__init__.",
            "__main__.",
        ]
        .iter()
        .any(|p| lower.starts_with(p));
        return FileKind::Source(SourceClass::Code, if entry && depth <= 3 { 4 } else { 5 });
    }
    FileKind::Skip(Exclusion::NotChosen)
}

// ---- the user's exclusions

/// Whether `text` matches the glob `pattern`: `*` any run within one path
/// segment, `**` any run across segments, `?` one character other than `/`.
fn glob(pattern: &[char], text: &[char]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some(('*', rest)) if rest.first() == Some(&'*') => {
            let mut rest = &rest[1..];
            // `**/` also matches nothing at all.
            if rest.first() == Some(&'/') && glob(&rest[1..], text) {
                return true;
            }
            if rest.first() == Some(&'/') {
                rest = &rest[1..];
            }
            (0..=text.len()).any(|i| glob(rest, &text[i..]))
        }
        Some(('*', rest)) => (0..=text.len())
            .take_while(|i| *i == 0 || text[i - 1] != '/')
            .any(|i| glob(rest, &text[i..])),
        Some(('?', rest)) => text
            .split_first()
            .is_some_and(|(c, t)| *c != '/' && glob(rest, t)),
        Some((p, rest)) => text
            .split_first()
            .is_some_and(|(c, t)| c == p && glob(rest, t)),
    }
}

/// Whether `path` falls under one of the user's exclusions: a path (a file,
/// or a folder and everything in it) or a glob. A glob without `/` applies
/// to any file or folder name (`*.log`, `secret*`); a glob with `/` to the
/// path from the project root (`docs/private/**`).
#[must_use]
pub fn excluded_by(patterns: &[String], path: &str) -> bool {
    patterns.iter().any(|raw| {
        let p = raw.trim().trim_start_matches("./").trim_end_matches('/');
        if p.is_empty() {
            return false;
        }
        if !p.contains(['*', '?']) {
            return path == p || path.starts_with(&format!("{p}/"));
        }
        let pat: Vec<char> = p.chars().collect();
        let parts: Vec<&str> = path.split('/').collect();
        if p.contains('/') {
            // The path or any of its folders.
            (1..=parts.len()).any(|n| {
                let prefix: Vec<char> = parts[..n].join("/").chars().collect();
                glob(&pat, &prefix)
            })
        } else {
            parts.iter().any(|part| {
                let part: Vec<char> = part.chars().collect();
                glob(&pat, &part)
            })
        }
    })
}

// ---- the inventory

/// What a source is, so the build can read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum What {
    File {
        path: String,
    },
    /// The listing of the project's folders and files.
    Structure,
    /// Branch, HEAD and changed paths.
    GitState,
    /// The uncommitted diff.
    GitDiff,
    /// A commit of `Inventory::history`, by index.
    Commit(usize),
    /// A conversation of `Inventory::conversations`, by index.
    Conversation(usize),
}

/// Where a source stands in a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    Selected,
    Excluded(Exclusion),
    /// Left for lack of input budget.
    OverLimit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// Unique in the manifest: `file:<path>`, `structure`, `git:state`,
    /// `git:diff`, `commit:<hash>`, `conversation:<id>`.
    pub key: String,
    pub class: SourceClass,
    pub what: What,
    /// Characters, estimated from bytes for files until they are read.
    pub chars: u64,
    pub rank: u8,
    pub pick: Pick,
}

/// What the project offers, and what a build with given options reads.
#[derive(Debug, Clone, Default)]
pub struct Inventory {
    pub sources: Vec<Source>,
    pub gaps: Vec<CoverageGap>,
    pub git: Option<GitSnapshot>,
    pub history: Vec<Commit>,
    pub conversations: Vec<PastConversation>,
    pub structure: String,
    pub git_state: String,
    pub diff: Option<String>,
    /// Why git was not used, when a repository was found but refused.
    pub git_refused: Option<String>,
}

fn chars_of(text: &str) -> u64 {
    text.chars().count() as u64
}

/// The text of the structure source: folders with their file counts, then
/// the files of the top levels.
fn structure_text(files: &ProjectFiles) -> String {
    let mut dirs: BTreeMap<String, usize> = BTreeMap::new();
    for f in &files.files {
        let parts: Vec<&str> = f.path.split('/').collect();
        for depth in 1..parts.len().min(4) {
            *dirs.entry(parts[..depth].join("/")).or_default() += 1;
        }
    }
    let mut lines: Vec<String> = vec![format!(
        "{} files listed{}",
        files.files.len(),
        if files.truncated {
            " (more not listed)"
        } else {
            ""
        }
    )];
    lines.extend(dirs.iter().map(|(d, n)| format!("{d}/ ({n} files)")));
    lines.extend(
        files
            .files
            .iter()
            .filter(|f| f.path.matches('/').count() <= 1)
            .map(|f| f.path.clone()),
    );
    lines.truncate(MAX_STRUCTURE_LINES);
    lines.join("\n")
}

/// The text of the git state source.
#[must_use]
pub fn git_state_text(snap: &GitSnapshot) -> String {
    let mut out = format!(
        "branch: {}\nhead: {}\n",
        snap.branch.as_deref().unwrap_or("(detached, or withheld)"),
        snap.head
            .as_deref()
            .unwrap_or("(no commit yet, or withheld)")
    );
    if snap.changed.is_empty() {
        out.push_str("no local changes\n");
    } else {
        out.push_str("local changes (modified, added, deleted or untracked):\n");
        for f in &snap.changed {
            out.push_str(&format!(
                "  {}{}\n",
                f.path,
                if f.deleted { " (deleted)" } else { "" }
            ));
        }
    }
    out
}

/// What git shows of a commit: its id, time and the paths it changed.
#[must_use]
pub fn commit_facts(c: &Commit) -> String {
    let mut out = format!("commit {} at unix time {}s\n", c.hash, c.time_ms / 1000);
    if c.files.is_empty() {
        out.push_str("no file listed\n");
    } else {
        out.push_str("changed:\n");
        for f in &c.files {
            out.push_str(&format!("  {f}\n"));
        }
    }
    out
}

/// What the author of a commit wrote: its subject and body.
#[must_use]
pub fn commit_message(c: &Commit) -> String {
    let mut out = c.subject.clone();
    if !c.body.trim().is_empty() {
        out.push_str("\n\n");
        out.push_str(c.body.trim_end());
    }
    out.push('\n');
    out
}

/// The text of one commit as a source.
#[must_use]
pub fn commit_text(c: &Commit) -> String {
    let mut out = format!("commit {}\n{}\n", c.hash, c.subject);
    if !c.body.trim().is_empty() {
        out.push('\n');
        out.push_str(c.body.trim_end());
        out.push('\n');
    }
    if !c.files.is_empty() {
        out.push_str("files:\n");
        for f in &c.files {
            out.push_str(&format!("  {f}\n"));
        }
    }
    out
}

/// What of a path may reach a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// Its content and its name.
    Content,
    /// Its name only (in a listing): a dependency, a build output, a binary,
    /// a kind of file that is not read.
    Name(Exclusion),
    /// Nothing: excluded by the user, a file that may hold credentials, or a
    /// name that is not a plain project path or holds a known secret.
    Nothing(Exclusion),
}

/// The one decision, for every path a build meets: a file to read, a diff
/// hunk, a status or listing line, a commit's file list. `bytes` is the
/// file's size as listed: content may reach a packet only when it is known
/// and within [`MAX_FILE_BYTES`] (an unknown size is not allowed, never
/// taken as zero); a name-only use passes `None`.
#[must_use]
pub fn reach(path: &str, bytes: Option<u64>, exclude: &[String], secrets: &[String]) -> Reach {
    if !crate::ingest::safe_path(path, secrets) {
        return Reach::Nothing(Exclusion::Sensitive);
    }
    if excluded_by(exclude, path) {
        return Reach::Nothing(Exclusion::User);
    }
    match classify(path, 0) {
        FileKind::Skip(Exclusion::Sensitive) => Reach::Nothing(Exclusion::Sensitive),
        FileKind::Skip(why) => Reach::Name(why),
        FileKind::Source(..) => match bytes {
            None => Reach::Name(Exclusion::Unreadable),
            Some(n) if n > MAX_FILE_BYTES => Reach::Name(Exclusion::TooLarge),
            Some(_) => Reach::Content,
        },
    }
}

/// The hunks of a unified diff whose file is in `allowed`, header and all;
/// any other hunk, or one whose header is not exactly
/// `diff --git a/<path> b/<path>` for an allowed path (a quoted or renamed
/// one), is left out.
#[must_use]
pub fn diff_of(diff: &str, allowed: &BTreeSet<String>) -> String {
    let mut out = String::new();
    let mut keep = false;
    for line in diff.split_inclusive('\n') {
        if let Some(header) = line.strip_prefix("diff --git ") {
            let header = header.trim_end_matches(['\n', '\r']);
            keep = allowed.iter().any(|p| header == format!("a/{p} b/{p}"));
        }
        if keep {
            out.push_str(line);
        }
    }
    out
}

/// A key for the manifest: the path when it is plain, otherwise a digest.
fn file_key(path: &str, secrets: &[String]) -> String {
    if crate::ingest::safe_path(path, secrets) {
        format!("file:{path}")
    } else {
        format!("file:#{}", &crate::bootstrap_packet::hash(path)[..16])
    }
}

/// What the host can see of the project, with the options' choices made.
/// Reads listings, git and the conversation index; reads no file content.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn collect(
    host: &dyn Host,
    project_id: &str,
    project: &Path,
    options: &BootstrapOptions,
    now_ms: u64,
) -> Inventory {
    let options = options.clone().clamped();
    let secrets = host.known_secrets();
    let chosen: BTreeSet<SourceClass> = options.sources.iter().copied().collect();
    let since = now_ms.saturating_sub(u64::from(options.history_days).saturating_mul(DAY_MS));
    let mut inv = Inventory::default();
    let mut gaps: BTreeSet<CoverageGap> = BTreeSet::new();
    let reads = host.reads_files();
    if !reads {
        gaps.insert(CoverageGap::FilesNotRead);
    }
    // Names only (status, listings, commit file lists).
    let named_ok = |path: &str| {
        !matches!(
            reach(path, None, &options.exclude, &secrets),
            Reach::Nothing(_)
        )
    };
    let add = |inv: &mut Inventory, key: String, class: SourceClass, what, chars, rank| {
        let pick = if chosen.contains(&class) {
            Pick::Selected
        } else {
            Pick::Excluded(Exclusion::NotChosen)
        };
        inv.sources.push(Source {
            key,
            class,
            what,
            chars,
            rank,
            pick,
        });
    };

    // Files.
    let listed = host.project_files(project, MAX_FILES);
    let files = listed.clone().unwrap_or_default();
    if files.truncated {
        gaps.insert(CoverageGap::InputLimit);
    }
    // The git state with every field that could carry a secret checked, and
    // only the paths that may be named.
    let git = host.git_snapshot(project).map(|g| {
        let (mut g, _) = crate::ingest::sanitize_snapshot(g, &secrets);
        g.changed.retain(|f| named_ok(&f.path));
        g
    });
    if git.is_some() && listed.is_none() {
        // A repository whose files could not be listed (too many to list).
        gaps.insert(CoverageGap::InputLimit);
    }
    let changed: BTreeSet<&str> = git
        .as_ref()
        .map(|g| g.changed.iter().map(|f| f.path.as_str()).collect())
        .unwrap_or_default();
    for folder in &files.nested {
        gaps.insert(CoverageGap::NestedRepositories);
        inv.sources.push(Source {
            key: format!(
                "nested:{}",
                file_key(folder, &secrets).trim_start_matches("file:")
            ),
            class: SourceClass::Code,
            what: What::File {
                path: folder.clone(),
            },
            chars: 0,
            rank: 9,
            pick: Pick::Excluded(Exclusion::Nested),
        });
    }
    let mut named = ProjectFiles {
        files: Vec::new(),
        truncated: files.truncated,
        nested: Vec::new(),
    };
    for f in &files.files {
        if !connecto_core::memory::is_project_relative(&f.path) {
            continue;
        }
        let key = file_key(&f.path, &secrets);
        let what = What::File {
            path: f.path.clone(),
        };
        let left_out = |why| Source {
            key: key.clone(),
            class: match classify(&f.path, 0) {
                FileKind::Source(c, _) => c,
                FileKind::Skip(_) => SourceClass::Code,
            },
            what: what.clone(),
            chars: f.bytes,
            rank: 9,
            pick: Pick::Excluded(why),
        };
        match reach(&f.path, Some(f.bytes), &options.exclude, &secrets) {
            Reach::Nothing(why) => {
                if why == Exclusion::User {
                    gaps.insert(CoverageGap::UserExclusions);
                }
                inv.sources.push(left_out(why));
                continue;
            }
            Reach::Name(why) => {
                named.files.push(f.clone());
                inv.sources.push(left_out(why));
                continue;
            }
            Reach::Content if !reads => {
                named.files.push(f.clone());
                inv.sources.push(left_out(Exclusion::Unreadable));
                continue;
            }
            Reach::Content => named.files.push(f.clone()),
        }
        match classify(&f.path, f.bytes) {
            FileKind::Source(class, rank) => {
                // Locally changed code is read before the rest of the code.
                let rank = if class == SourceClass::Code && changed.contains(f.path.as_str()) {
                    3
                } else {
                    rank
                };
                add(&mut inv, key, class, what, f.bytes, rank);
            }
            FileKind::Skip(why) => inv.sources.push(left_out(why)),
        }
    }
    if !named.files.is_empty() {
        inv.structure = structure_text(&named);
        let chars = chars_of(&inv.structure);
        add(
            &mut inv,
            "structure".to_owned(),
            SourceClass::Structure,
            What::Structure,
            chars,
            0,
        );
    }

    // Git.
    match &git {
        None => match host.git_refusal(project) {
            Some(reason) => {
                gaps.insert(CoverageGap::GitNotUsable);
                inv.git_refused = Some(reason);
                inv.sources.push(Source {
                    key: "git".to_owned(),
                    class: SourceClass::GitState,
                    what: What::GitState,
                    chars: 0,
                    rank: 9,
                    pick: Pick::Excluded(Exclusion::GitRefused),
                });
            }
            None => {
                gaps.insert(CoverageGap::NotAGitRepository);
            }
        },
        Some(snap) => {
            inv.git_state = git_state_text(snap);
            let chars = chars_of(&inv.git_state);
            add(
                &mut inv,
                "git:state".to_owned(),
                SourceClass::GitState,
                What::GitState,
                chars,
                0,
            );
            // The diff of the changed files whose content may be read, and
            // only theirs: asked of git by path, then checked hunk by hunk.
            // A changed file's size is its listed size; a file not listed
            // (deleted, or past the listing) has none, so no content.
            let sizes: BTreeMap<&str, u64> = files
                .files
                .iter()
                .map(|f| (f.path.as_str(), f.bytes))
                .collect();
            let readable: BTreeSet<String> = snap
                .changed
                .iter()
                .filter(|_| reads)
                .filter(|f| {
                    reach(
                        &f.path,
                        sizes.get(f.path.as_str()).copied(),
                        &options.exclude,
                        &secrets,
                    ) == Reach::Content
                })
                .map(|f| f.path.clone())
                .collect();
            if chosen.contains(&SourceClass::GitState) && !readable.is_empty() {
                let paths: Vec<String> = readable.iter().cloned().collect();
                match host.git_diff(project, &paths, MAX_FILE_BYTES, MAX_DIFF_BYTES) {
                    Some(diff) => {
                        let diff = diff_of(&diff, &readable);
                        if !diff.trim().is_empty() {
                            let chars = chars_of(&diff);
                            inv.diff = Some(diff);
                            add(
                                &mut inv,
                                "git:diff".to_owned(),
                                SourceClass::GitState,
                                What::GitDiff,
                                chars,
                                3,
                            );
                        }
                    }
                    None if snap.head.is_some() => {
                        // Too large to read whole: never cut.
                        gaps.insert(CoverageGap::InputLimit);
                        inv.sources.push(Source {
                            key: "git:diff".to_owned(),
                            class: SourceClass::GitState,
                            what: What::GitDiff,
                            chars: MAX_DIFF_BYTES,
                            rank: 9,
                            pick: Pick::Excluded(Exclusion::TooLarge),
                        });
                    }
                    None => {}
                }
            }
            if chosen.contains(&SourceClass::GitHistory) {
                let history: GitHistory = match host.git_history(project, since, MAX_COMMITS) {
                    Some(h) => h,
                    None => {
                        // Git could not give it (its output over the cap).
                        gaps.insert(CoverageGap::InputLimit);
                        GitHistory::default()
                    }
                };
                if history.older {
                    gaps.insert(CoverageGap::HistoryWindow);
                }
                let commits: Vec<Commit> = history
                    .commits
                    .into_iter()
                    .filter_map(|mut c| {
                        c.hash = crate::ingest::safe_head(&c.hash)?;
                        c.files.retain(|p| named_ok(p));
                        Some(c)
                    })
                    .collect();
                for (i, c) in commits.iter().enumerate() {
                    let chars = chars_of(&commit_text(c));
                    add(
                        &mut inv,
                        format!("commit:{}", c.hash),
                        SourceClass::GitHistory,
                        What::Commit(i),
                        chars,
                        2,
                    );
                }
                inv.history = commits;
            }
        }
    }

    // A git command that failed after the repository was bound (listing,
    // status, history, diff) is declared, never taken as "nothing there".
    if git.is_some() && host.git_refusal(project).is_some() {
        gaps.insert(CoverageGap::GitNotUsable);
    }
    if git.is_some() && !host.git_limits(project).is_empty() {
        gaps.insert(CoverageGap::GlobalGitSettings);
    }

    // Conversations.
    if chosen.contains(&SourceClass::Conversations) {
        let past: PastConversations = host
            .past_conversations(project_id, since)
            .unwrap_or_default();
        if past.older {
            gaps.insert(CoverageGap::HistoryWindow);
        }
        if past.truncated {
            gaps.insert(CoverageGap::InputLimit);
        }
        let conversations: Vec<PastConversation> = past
            .conversations
            .into_iter()
            .filter(|c| crate::ingest::safe_id(&c.id, &secrets).is_some())
            .collect();
        if conversations.is_empty() {
            gaps.insert(CoverageGap::NoConversations);
        }
        let roles = |role: TurnRole| {
            conversations
                .iter()
                .flat_map(|c| &c.turns)
                .any(|t| t.role == role)
        };
        if roles(TurnRole::Unknown) {
            gaps.insert(CoverageGap::RolesUnknown);
        }
        if roles(TurnRole::User) {
            gaps.insert(CoverageGap::PastUserProvenance);
        }
        for (i, c) in conversations.iter().enumerate() {
            let chars: u64 = c.turns.iter().map(|t| chars_of(&t.text)).sum();
            add(
                &mut inv,
                format!("conversation:{}", c.id),
                SourceClass::Conversations,
                What::Conversation(i),
                chars,
                2,
            );
        }
        inv.conversations = conversations;
    }

    inv.git = git;
    // Within the budget, in rank order; ties: smaller first, then by key.
    let mut order: Vec<usize> = (0..inv.sources.len())
        .filter(|i| inv.sources[*i].pick == Pick::Selected)
        .collect();
    order.sort_by(|a, b| {
        let (a, b) = (&inv.sources[*a], &inv.sources[*b]);
        (a.rank, a.chars, &a.key).cmp(&(b.rank, b.chars, &b.key))
    });
    let mut total = 0u64;
    for i in order {
        let chars = inv.sources[i].chars;
        if total.saturating_add(chars) <= options.max_input_chars {
            total += chars;
        } else {
            inv.sources[i].pick = Pick::OverLimit;
            gaps.insert(CoverageGap::InputLimit);
        }
    }
    inv.gaps = gaps.into_iter().collect();
    inv
}

impl Inventory {
    /// Characters the selected sources hold.
    #[must_use]
    pub fn selected_chars(&self) -> u64 {
        self.sources
            .iter()
            .filter(|s| s.pick == Pick::Selected)
            .map(|s| s.chars)
            .sum()
    }

    /// Coverage before the build starts: `remaining` is what would be read,
    /// `excluded` what would not (left out, or over the input limit).
    #[must_use]
    pub fn plan_coverage(&self) -> Vec<SourceCoverage> {
        coverage(self.sources.iter().map(|s| {
            let bucket = match s.pick {
                Pick::Selected => Bucket::Remaining,
                Pick::Excluded(_) | Pick::OverLimit => Bucket::Excluded,
            };
            (s.class, bucket)
        }))
    }
}

/// Where a source counts in [`SourceCoverage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    Analysed,
    Excluded,
    Remaining,
}

/// Coverage per class, in [`SourceClass::ALL`] order, every class listed.
pub fn coverage(items: impl Iterator<Item = (SourceClass, Bucket)>) -> Vec<SourceCoverage> {
    let mut out: Vec<SourceCoverage> = SourceClass::ALL
        .iter()
        .map(|class| SourceCoverage {
            class: *class,
            analysed: 0,
            excluded: 0,
            remaining: 0,
        })
        .collect();
    for (class, bucket) in items {
        if let Some(c) = out.iter_mut().find(|c| c.class == class) {
            let slot = match bucket {
                Bucket::Analysed => &mut c.analysed,
                Bucket::Excluded => &mut c.excluded,
                Bucket::Remaining => &mut c.remaining,
            };
            *slot = slot.saturating_add(1);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_are_classified_by_path() {
        let source = |p: &str| match classify(p, 10) {
            FileKind::Source(c, _) => Some(c),
            FileKind::Skip(_) => None,
        };
        assert_eq!(source("README.md"), Some(SourceClass::Documentation));
        assert_eq!(source("AGENTS.md"), Some(SourceClass::Documentation));
        assert_eq!(
            source("docs/adr/0001-pty.md"),
            Some(SourceClass::Documentation)
        );
        assert_eq!(source("Cargo.toml"), Some(SourceClass::Manifests));
        assert_eq!(source("crates/x/Cargo.toml"), Some(SourceClass::Manifests));
        assert_eq!(
            source(".github/workflows/ci.yml"),
            Some(SourceClass::TestConfig)
        );
        assert_eq!(source("jest.config.ts"), Some(SourceClass::TestConfig));
        assert_eq!(source("src/lib.rs"), Some(SourceClass::Code));
        assert_eq!(
            classify("node_modules/x/index.js", 1),
            FileKind::Skip(Exclusion::Ignored)
        );
        assert_eq!(
            classify("target/debug/a.rs", 1),
            FileKind::Skip(Exclusion::Ignored)
        );
        assert_eq!(
            classify("Cargo.lock", 1),
            FileKind::Skip(Exclusion::Ignored)
        );
        assert_eq!(classify("logo.png", 1), FileKind::Skip(Exclusion::Binary));
        assert_eq!(classify(".env", 1), FileKind::Skip(Exclusion::Sensitive));
        assert_eq!(
            classify("config/.env.production", 1),
            FileKind::Skip(Exclusion::Sensitive)
        );
        assert_eq!(
            classify("certs/server.key", 1),
            FileKind::Skip(Exclusion::Sensitive)
        );
        assert_eq!(
            classify("src/big.rs", MAX_FILE_BYTES + 1),
            FileKind::Skip(Exclusion::TooLarge)
        );
    }

    #[test]
    fn decision_records_are_recognised() {
        assert!(decision_record("docs/adr/0002-threads.md"));
        assert!(decision_record("ADR-7.md"));
        assert!(decision_record("docs/decisions/use-sqlite.md"));
        assert!(decision_record("AGENTS.md"));
        assert!(decision_record("sub/CLAUDE.md"));
        assert!(!decision_record("README.md"));
        assert!(!decision_record("docs/adr/script.rs"));
        assert!(!decision_record("src/adr.rs"));
    }

    #[test]
    fn exclusions_take_paths_and_globs() {
        let ex = |p: &[&str], path: &str| {
            excluded_by(&p.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(), path)
        };
        assert!(ex(&["docs/private"], "docs/private/a.md"));
        assert!(ex(&["docs/private/"], "docs/private"));
        assert!(!ex(&["docs/private"], "docs/privateer.md"));
        assert!(ex(&["*.log"], "a/b/c.log"));
        assert!(!ex(&["*.log"], "a/b/c.logs"));
        assert!(ex(&["secret*"], "x/secret-plan/notes.md"));
        assert!(ex(&["docs/**/draft-*.md"], "docs/a/b/draft-1.md"));
        assert!(ex(&["docs/**/draft-*.md"], "docs/draft-1.md"));
        assert!(!ex(&["docs/*.md"], "docs/a/b.md"));
        assert!(ex(&["docs/*"], "docs/a/b.md"));
        assert!(ex(&["./internal"], "internal/x.rs"));
        assert!(ex(&["src/?.rs"], "src/a.rs"));
        assert!(!ex(&["src/?.rs"], "src/ab.rs"));
        assert!(!ex(&["", "  "], "a"));
    }
}
