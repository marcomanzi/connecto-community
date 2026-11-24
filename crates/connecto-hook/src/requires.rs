//! The tools a package needs on the machine (`"requires"` in `package.json`):
//! how Connecto finds each one, and how the user gets it (a command Connecto
//! runs in a visible shell terminal once the user confirms it, or steps to
//! follow by hand). See ADR 0037.
//!
//! What a package may ask Connecto to run is kept small on purpose, because a
//! package can come from someone else (an imported file):
//! - detection only looks for files (a program on `PATH`, or a fixed path) and
//!   may run the program it found with one version flag;
//! - an install command is never run without the user confirming that exact
//!   command;
//! - a tool whose install means accepting a licence or EULA, or signing in, is
//!   `"licence": true` and has manual steps only: Connecto never accepts terms
//!   on the user's behalf.

use std::cmp::Ordering;

use serde_json::{Map, Value, json};

/// At most this many tools per package.
pub const MAX_REQUIREMENTS: usize = 16;
const MAX_TEXT: usize = 600;
const MAX_LIST: usize = 12;
/// The only arguments detection may pass to the program it found.
/// Each starts with `-`, so no program can read one as a file to open or a
/// script to run.
pub const VERSION_FLAGS: [&str; 4] = ["--version", "-version", "-v", "-V"];

/// An operating system, as `package.json` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Os {
    Macos,
    Windows,
    Linux,
}

impl Os {
    pub const ALL: [Self; 3] = [Self::Macos, Self::Windows, Self::Linux];

    /// The system this build runs on (any other Unix counts as Linux).
    pub const CURRENT: Self = if cfg!(target_os = "macos") {
        Self::Macos
    } else if cfg!(windows) {
        Self::Windows
    } else {
        Self::Linux
    };

    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Macos => "macos",
            Self::Windows => "windows",
            Self::Linux => "linux",
        }
    }
}

/// One value per operating system.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PerOs<T> {
    pub macos: T,
    pub windows: T,
    pub linux: T,
}

impl<T> PerOs<T> {
    #[must_use]
    pub const fn get(&self, os: Os) -> &T {
        match os {
            Os::Macos => &self.macos,
            Os::Windows => &self.windows,
            Os::Linux => &self.linux,
        }
    }

    fn get_mut(&mut self, os: Os) -> &mut T {
        match os {
            Os::Macos => &mut self.macos,
            Os::Windows => &mut self.windows,
            Os::Linux => &mut self.linux,
        }
    }
}

/// One tool a package needs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Requirement {
    /// Unique in the package: lowercase letters, digits and `-`.
    pub id: String,
    /// What the dialog calls it ("Godot 4.7 or later").
    pub name: String,
    /// Why the package needs it, in a sentence.
    pub why: String,
    /// Program names looked up on `PATH` (and the usual install folders).
    pub programs: Vec<String>,
    /// Fixed places it may be, per system: absolute, or under `~/` (on
    /// Windows also `%VARIABLE%\...`).
    pub paths: PerOs<Vec<String>>,
    /// The flag that makes the program print its version, if a minimum is
    /// wanted.
    pub version_flag: Option<String>,
    /// The oldest version that will do (`4.7`).
    pub min_version: Option<String>,
    /// The first version that no longer does (`5`: any 4.x), if any.
    pub below_version: Option<String>,
    /// The command that installs it, per system. Never for a `licence` tool.
    pub install: PerOs<Option<String>>,
    /// What to do by hand: shown when there is no install command for this
    /// system, and beside it otherwise.
    pub manual: String,
    /// Where to read more or download it (`https://`).
    pub url: Option<String>,
    /// Getting it means accepting a licence or EULA, or signing in: manual
    /// steps only.
    pub licence: bool,
}

impl Requirement {
    /// The install command for `os`, if Connecto may offer to run one.
    #[must_use]
    pub fn install_on(&self, os: Os) -> Option<&str> {
        if self.licence {
            return None;
        }
        self.install.get(os).as_deref()
    }

    /// Whether only some versions will do: then a version must be read and
    /// in range for the tool to count as there.
    #[must_use]
    pub const fn wants_a_version(&self) -> bool {
        self.min_version.is_some() || self.below_version.is_some()
    }

    /// Whether `version` is in range.
    #[must_use]
    pub fn version_fits(&self, version: &str) -> bool {
        self.min_version
            .as_deref()
            .is_none_or(|min| compare_versions(version, min).is_ge())
            && self
                .below_version
                .as_deref()
                .is_none_or(|below| compare_versions(version, below).is_lt())
    }

    /// The versions that will do, in words: `4.7 or later, before 5`.
    #[must_use]
    pub fn wanted_versions(&self) -> String {
        match (&self.min_version, &self.below_version) {
            (Some(min), Some(below)) => format!("{min} or later, before {below}"),
            (Some(min), None) => format!("{min} or later"),
            (None, Some(below)) => format!("before {below}"),
            (None, None) => "any version".to_owned(),
        }
    }

    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut detect = Map::new();
        if !self.programs.is_empty() {
            detect.insert("programs".into(), json!(self.programs));
        }
        let mut paths = Map::new();
        for os in Os::ALL {
            if !self.paths.get(os).is_empty() {
                paths.insert(os.word().into(), json!(self.paths.get(os)));
            }
        }
        if !paths.is_empty() {
            detect.insert("paths".into(), Value::Object(paths));
        }
        if let Some(flag) = &self.version_flag {
            let mut version = Map::new();
            version.insert("flag".into(), json!(flag));
            if let Some(min) = &self.min_version {
                version.insert("min".into(), json!(min));
            }
            if let Some(below) = &self.below_version {
                version.insert("below".into(), json!(below));
            }
            detect.insert("version".into(), Value::Object(version));
        }
        let mut object = Map::new();
        object.insert("id".into(), json!(self.id));
        object.insert("name".into(), json!(self.name));
        if !self.why.is_empty() {
            object.insert("why".into(), json!(self.why));
        }
        object.insert("detect".into(), Value::Object(detect));
        let mut install = Map::new();
        for os in Os::ALL {
            if let Some(command) = self.install.get(os) {
                install.insert(os.word().into(), json!(command));
            }
        }
        if !install.is_empty() {
            object.insert("install".into(), Value::Object(install));
        }
        if !self.manual.is_empty() {
            object.insert("manual".into(), json!(self.manual));
        }
        if let Some(url) = &self.url {
            object.insert("url".into(), json!(url));
        }
        if self.licence {
            object.insert("licence".into(), json!(true));
        }
        Value::Object(object)
    }
}

/// `"requires"`: every problem found, as `requires[i]...: why`.
pub fn parse_list(value: &Value, errors: &mut Vec<String>) -> Vec<Requirement> {
    let Some(items) = value.as_array() else {
        errors.push("\"requires\" must be a list".to_owned());
        return Vec::new();
    };
    if items.len() > MAX_REQUIREMENTS {
        errors.push(format!(
            "\"requires\" lists more than {MAX_REQUIREMENTS} tools"
        ));
        return Vec::new();
    }
    let mut out: Vec<Requirement> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let mut mine = Vec::new();
        let parsed = parse_one(item, &mut mine);
        if let Some(req) = &parsed
            && out.iter().any(|r| r.id == req.id)
        {
            mine.push(format!("the id {:?} is used twice", req.id));
        }
        if mine.is_empty() {
            out.extend(parsed);
        } else {
            errors.extend(mine.into_iter().map(|e| format!("requires[{index}]: {e}")));
        }
    }
    out
}

fn text(object: &Map<String, Value>, key: &str, errors: &mut Vec<String>) -> Option<String> {
    match object.get(key) {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.chars().count() <= MAX_TEXT => Some(s.trim().to_owned()),
        Some(_) => {
            errors.push(format!(
                "{key:?} must be text of at most {MAX_TEXT} characters"
            ));
            None
        }
    }
}

fn list(value: &Value, key: &str, errors: &mut Vec<String>) -> Vec<String> {
    let items = value.as_array().filter(|a| a.len() <= MAX_LIST);
    let strings: Option<Vec<String>> = items.and_then(|a| {
        a.iter()
            .map(|v| v.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
    });
    strings.unwrap_or_else(|| {
        errors.push(format!(
            "{key:?} must be a list of at most {MAX_LIST} texts"
        ));
        Vec::new()
    })
}

fn known_keys(object: &Map<String, Value>, keys: &[&str], at: &str, errors: &mut Vec<String>) {
    for key in object.keys() {
        if !keys.contains(&key.as_str()) {
            errors.push(format!("unknown field {at}{key:?}"));
        }
    }
}

/// A program name: a plain file name, never a path.
fn plain_program(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.starts_with('.')
}

/// A fixed path for `os`: absolute or under the home folder, with no `..`.
#[must_use]
pub fn fixed_path(path: &str, os: Os) -> bool {
    let rooted = match os {
        Os::Windows => {
            let bytes = path.as_bytes();
            (bytes.len() > 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && bytes[2] == b'\\')
                || (path.starts_with('%')
                    && path[1..].split_once("%\\").is_some_and(|(var, _)| {
                        !var.is_empty()
                            && var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    }))
                || path.starts_with("~\\")
        }
        Os::Macos | Os::Linux => path.starts_with('/') || path.starts_with("~/"),
    };
    rooted
        && path.len() <= 260
        && !path.split(['/', '\\']).any(|part| part == "..")
        && !path.chars().any(char::is_control)
}

/// `4.7`, `4.7.2`: digits and dots.
#[must_use]
pub fn plain_version(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 20
        && text
            .split('.')
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

fn parse_one(item: &Value, errors: &mut Vec<String>) -> Option<Requirement> {
    let Some(object) = item.as_object() else {
        errors.push("must be an object".to_owned());
        return None;
    };
    known_keys(
        object,
        &[
            "id", "name", "why", "detect", "install", "manual", "url", "licence",
        ],
        "",
        errors,
    );
    let mut req = Requirement::default();
    match object.get("id").and_then(Value::as_str) {
        Some(id) if crate::package::valid_id(id) && id.len() <= 64 => id.clone_into(&mut req.id),
        _ => errors.push("\"id\" is required (lowercase letters, digits, `-`)".to_owned()),
    }
    match text(object, "name", errors) {
        Some(name) if !name.is_empty() => req.name = name,
        _ => errors.push("\"name\" is required (text)".to_owned()),
    }
    req.why = text(object, "why", errors).unwrap_or_default();
    req.manual = text(object, "manual", errors).unwrap_or_default();
    req.url = text(object, "url", errors).filter(|u| !u.is_empty());
    if let Some(url) = &req.url
        && (!url.starts_with("https://") || url.chars().any(char::is_whitespace))
    {
        errors.push("\"url\" must be an https:// link".to_owned());
    }
    req.licence = match object.get("licence") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            errors.push("\"licence\" must be true or false".to_owned());
            false
        }
    };
    parse_detect(object.get("detect"), &mut req, errors);
    if let Some(install) = object.get("install") {
        parse_install(install, &mut req, errors);
    }
    let any_install = Os::ALL.iter().any(|os| req.install.get(*os).is_some());
    if req.licence && any_install {
        errors.push(
            "a tool that needs a licence, EULA or sign-in has manual steps only: remove \"install\""
                .to_owned(),
        );
    }
    let every_os_covered = Os::ALL.iter().all(|os| req.install.get(*os).is_some());
    if req.manual.is_empty() && !every_os_covered {
        errors.push("\"manual\" is required when some system has no install command".to_owned());
    }
    Some(req)
}

fn parse_detect(detect: Option<&Value>, req: &mut Requirement, errors: &mut Vec<String>) {
    let Some(detect) = detect.and_then(Value::as_object) else {
        errors.push("\"detect\" is required (an object)".to_owned());
        return;
    };
    known_keys(detect, &["programs", "paths", "version"], "detect.", errors);
    if let Some(programs) = detect.get("programs") {
        req.programs = list(programs, "detect.programs", errors);
        for name in &req.programs {
            if !plain_program(name) {
                errors.push(format!(
                    "detect.programs: {name:?} must be a program name, not a path"
                ));
            }
        }
    }
    if let Some(paths) = detect.get("paths") {
        match paths.as_object() {
            Some(by_os) => {
                known_keys(
                    by_os,
                    &["macos", "windows", "linux"],
                    "detect.paths.",
                    errors,
                );
                for os in Os::ALL {
                    if let Some(value) = by_os.get(os.word()) {
                        let found = list(value, &format!("detect.paths.{}", os.word()), errors);
                        for path in &found {
                            if !fixed_path(path, os) {
                                errors.push(format!(
                                    "detect.paths.{}: {path:?} must be absolute or start with ~",
                                    os.word()
                                ));
                            }
                        }
                        *req.paths.get_mut(os) = found;
                    }
                }
            }
            None => errors.push("\"detect.paths\" must be an object by system".to_owned()),
        }
    }
    if req.programs.is_empty() && Os::ALL.iter().all(|os| req.paths.get(*os).is_empty()) {
        errors.push("\"detect\" needs at least one program or path".to_owned());
    }
    if let Some(version) = detect.get("version") {
        let Some(version) = version.as_object() else {
            errors.push("\"detect.version\" must be an object".to_owned());
            return;
        };
        known_keys(
            version,
            &["flag", "min", "below"],
            "detect.version.",
            errors,
        );
        match version.get("flag").and_then(Value::as_str) {
            Some(flag) if VERSION_FLAGS.contains(&flag) => req.version_flag = Some(flag.to_owned()),
            _ => errors.push(format!(
                "\"detect.version.flag\" must be one of {}",
                VERSION_FLAGS.join(", ")
            )),
        }
        match version.get("min") {
            None | Some(Value::Null) => {}
            Some(Value::String(min)) if plain_version(min) => req.min_version = Some(min.clone()),
            Some(_) => errors.push("\"detect.version.min\" must be like 4.7 or 4.7.2".to_owned()),
        }
        match version.get("below") {
            None | Some(Value::Null) => {}
            Some(Value::String(below)) if plain_version(below) => {
                req.below_version = Some(below.clone());
            }
            Some(_) => errors.push("\"detect.version.below\" must be like 5 or 4.8".to_owned()),
        }
        if let (Some(min), Some(below)) = (&req.min_version, &req.below_version)
            && compare_versions(min, below).is_ge()
        {
            errors.push("\"detect.version.below\" must be above \"min\"".to_owned());
        }
    }
}

fn parse_install(install: &Value, req: &mut Requirement, errors: &mut Vec<String>) {
    let Some(by_os) = install.as_object() else {
        errors.push("\"install\" must be an object by system".to_owned());
        return;
    };
    known_keys(by_os, &["macos", "windows", "linux"], "install.", errors);
    for os in Os::ALL {
        match by_os.get(os.word()) {
            None | Some(Value::Null) => {}
            Some(Value::String(command))
                if !command.trim().is_empty()
                    && command.chars().count() <= MAX_TEXT
                    && !command
                        .chars()
                        .any(|c| c.is_control() || hidden_direction(c)) =>
            {
                *req.install.get_mut(os) = Some(command.trim().to_owned());
            }
            Some(_) => errors.push(format!(
                "install.{}: must be one command line of at most {MAX_TEXT} characters",
                os.word()
            )),
        }
    }
}

/// A character that changes the direction text is shown in, so a command
/// could read differently from what runs.
const fn hidden_direction(c: char) -> bool {
    matches!(
        c,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

/// The first version number in a program's output (`Godot 4.7.2.stable` →
/// `4.7.2`): at least two parts. FFmpeg's banner is read its own way
/// ([`ffmpeg_version`]).
#[must_use]
pub fn version_in(output: &str) -> Option<String> {
    if let Some(version) = ffmpeg_version(output) {
        return version;
    }
    let bytes = output.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        if bytes[start].is_ascii_digit() && starts_a_number(bytes, start) {
            let mut end = start;
            while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b'.') {
                end += 1;
            }
            let found = output[start..end].trim_end_matches('.');
            if found.contains('.') && plain_version(found) {
                return Some(found.to_owned());
            }
            start = end;
        }
        start += 1;
    }
    None
}

/// Whether a number may start at `at`: after nothing, a non-letter, or a lone
/// `v` (`v22.11.0`).
fn starts_a_number(bytes: &[u8], at: usize) -> bool {
    let free = |i: usize| i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
    free(at) || (matches!(bytes[at - 1], b'v' | b'V') && free(at - 1))
}

/// FFmpeg's banner (`ffmpeg version 7.1-full_build ...`): the version is the
/// word after `ffmpeg version` and nothing further on, where the compiler's
/// version is. A release tag may start with `n` (`n7.1`, as Arch builds it);
/// a snapshot (`N-117000-g...`, or a date) has no release number, so no
/// version is read. `None` when the output does not start with FFmpeg's
/// banner.
fn ffmpeg_version(output: &str) -> Option<Option<String>> {
    let rest = output.trim_start().strip_prefix("ffmpeg version ")?;
    let word = rest.split_whitespace().next().unwrap_or("");
    let word = word.strip_prefix('n').unwrap_or(word);
    let end = word
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(word.len());
    let found = word[..end].trim_end_matches('.');
    Some((found.contains('.') && plain_version(found)).then(|| found.to_owned()))
}

/// Compares versions part by part (`4.10` is newer than `4.9`; a missing part
/// counts as 0).
#[must_use]
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let parts = |v: &str| -> Vec<u64> { v.split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    let (a, b) = (parts(a), parts(b));
    for i in 0..a.len().max(b.len()) {
        let order = a.get(i).unwrap_or(&0).cmp(b.get(i).unwrap_or(&0));
        if order != Ordering::Equal {
            return order;
        }
    }
    Ordering::Equal
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Vec<Requirement>, Vec<String>> {
        let mut errors = Vec::new();
        let value: Value = serde_json::from_str(text).unwrap();
        let out = parse_list(&value, &mut errors);
        if errors.is_empty() {
            Ok(out)
        } else {
            Err(errors)
        }
    }

    const GODOT: &str = r#"[{
        "id": "godot", "name": "Godot 4.7 or later", "why": "The engine.",
        "detect": {
            "programs": ["godot"],
            "paths": {"macos": ["/Applications/Godot.app/Contents/MacOS/Godot"],
                      "windows": ["%LOCALAPPDATA%\\Microsoft\\WinGet\\Links\\godot.exe"],
                      "linux": ["~/.local/share/flatpak/exports/bin/org.godotengine.Godot"]},
            "version": {"flag": "--version", "min": "4.7", "below": "5"}
        },
        "install": {"macos": "brew install --cask godot"},
        "manual": "Download it from the website.",
        "url": "https://godotengine.org/download/"
    }]"#;

    #[test]
    fn a_full_requirement_reads_and_writes_back() {
        let reqs = parse(GODOT).unwrap();
        let godot = &reqs[0];
        assert_eq!(
            godot.install_on(Os::Macos),
            Some("brew install --cask godot")
        );
        assert_eq!(godot.install_on(Os::Linux), None);
        assert_eq!(godot.min_version.as_deref(), Some("4.7"));
        assert_eq!(godot.wanted_versions(), "4.7 or later, before 5");
        assert!(godot.version_fits("4.7") && godot.version_fits("4.12.1"));
        assert!(!godot.version_fits("4.6.9") && !godot.version_fits("5.0"));
        let back = parse(&Value::Array(vec![godot.to_json()]).to_string()).unwrap();
        assert_eq!(back, reqs);
    }

    #[test]
    fn a_licence_tool_never_has_an_install_command() {
        let bad = r#"[{"id":"unity","name":"Unity","licence":true,
            "detect":{"programs":["unity"]},
            "install":{"macos":"unity license activate --accept-eula"},
            "manual":"Sign in and accept the licence yourself."}]"#;
        let errors = parse(bad).unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("manual steps only")),
            "{errors:?}"
        );
        let ok = r#"[{"id":"unity","name":"Unity","licence":true,
            "detect":{"programs":["unity"]},
            "manual":"Sign in and accept the licence yourself."}]"#;
        let reqs = parse(ok).unwrap();
        assert!(reqs[0].licence && reqs[0].install_on(Os::CURRENT).is_none());
    }

    #[test]
    fn detection_cannot_be_made_to_run_anything_else() {
        for bad in [
            // A path where a program name goes.
            r#"[{"id":"x","name":"X","manual":"m","detect":{"programs":["/bin/sh"]}}]"#,
            r#"[{"id":"x","name":"X","manual":"m","detect":{"programs":["..\\x"]}}]"#,
            // Arguments other than a version flag.
            r#"[{"id":"x","name":"X","manual":"m","detect":{"programs":["sh"],"version":{"flag":"-c"}}}]"#,
            r#"[{"id":"x","name":"X","manual":"m","detect":{"programs":["python3"],"version":{"flag":"version"}}}]"#,
            // A relative or climbing path.
            r#"[{"id":"x","name":"X","manual":"m","detect":{"paths":{"macos":["bin/x"]}}}]"#,
            r#"[{"id":"x","name":"X","manual":"m","detect":{"paths":{"linux":["/opt/../etc/x"]}}}]"#,
            // Nothing to look for.
            r#"[{"id":"x","name":"X","manual":"m","detect":{}}]"#,
            // A second line in an install command.
            r#"[{"id":"x","name":"X","manual":"m","detect":{"programs":["x"]},"install":{"linux":"a\nb"}}]"#,
            // A command that would show differently from what runs.
            "[{\"id\":\"x\",\"name\":\"X\",\"manual\":\"m\",\"detect\":{\"programs\":[\"x\"]},\"install\":{\"linux\":\"echo \u{202e}x\"}}]",
            // No way to get it on some system.
            r#"[{"id":"x","name":"X","detect":{"programs":["x"]},"install":{"linux":"apt install x"}}]"#,
            r#"[{"id":"x","name":"X","manual":"m","url":"http://x","detect":{"programs":["x"]}}]"#,
            r#"[{"id":"x","name":"X","manual":"m","detect":{"programs":["x"]},"extra":1}]"#,
            r#"[{"id":"x","name":"X","manual":"m","detect":{"programs":["x"]}},
                {"id":"x","name":"Y","manual":"m","detect":{"programs":["y"]}}]"#,
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn versions_are_read_from_output_and_compared_by_part() {
        assert_eq!(
            version_in("4.7.2.stable.official.abc"),
            Some("4.7.2".into())
        );
        assert_eq!(version_in("Blender 5.3.0\nbuild"), Some("5.3.0".into()));
        assert_eq!(version_in("v22.11.0"), Some("22.11.0".into()));
        assert_eq!(
            version_in("x86 build 1.2"),
            Some("1.2".into()),
            "86 is glued to x"
        );
        assert_eq!(version_in("node 22"), None);
        // What Game Dev's tools print (ADR 0037).
        assert_eq!(
            version_in("Blender 4.2.3 LTS\n\tbuild date: 2024-10-14\n\tbuild hash: abc"),
            Some("4.2.3".into())
        );
        assert_eq!(
            version_in(
                "ffmpeg version 9.0.2 Copyright (c) 2000-2026 the FFmpeg developers\n\
                 built with Apple clang version 21.0.0 (clang-2100.3.34.2)"
            ),
            Some("9.0.2".into())
        );
        assert_eq!(
            version_in("ffmpeg version 4.4.2-0ubuntu0.22.04.1 Copyright (c) 2000-2021"),
            Some("4.4.2".into())
        );
        assert_eq!(
            version_in("ffmpeg version 7.1-full_build-www.gyan.dev Copyright (c) 2000-2024"),
            Some("7.1".into())
        );
        assert_eq!(
            version_in(
                "ffmpeg version n7.1 Copyright (c) 2000-2024 the FFmpeg developers\n\
                 built with gcc 14.2.1 (GCC) 20240910\n\
                 libavutil      59. 39.100 / 59. 39.100"
            ),
            Some("7.1".into()),
            "Arch's n7.1, not the compiler's version"
        );
        // A snapshot has no release number: unreadable, never the compiler's.
        for snapshot in [
            "ffmpeg version N-104465-g08a501946f Copyright (c) 2000-2021 the FFmpeg developers\n\
             built with gcc 11.2.0 (Rev7, Built by MSYS2 project)",
            "ffmpeg version 2021-10-11-git-90a0da9f14-full_build-www.gyan.dev Copyright (c)\n\
             built with gcc 11.2.0",
        ] {
            assert_eq!(version_in(snapshot), None, "{snapshot}");
        }
        // The `n` is FFmpeg's alone.
        assert_eq!(
            version_in("tool n7.1 built with gcc 14.2.1"),
            Some("14.2.1".into())
        );
        assert_eq!(
            version_in("libx ffmpeg version 6.0, v2.1"),
            Some("6.0".into())
        );
        assert_eq!(compare_versions("4.10", "4.9"), Ordering::Greater);
        assert_eq!(compare_versions("4.7", "4.7.0"), Ordering::Equal);
        assert_eq!(compare_versions("4.6.9", "4.7"), Ordering::Less);
        assert!(fixed_path(
            "C:\\Program Files\\Godot\\godot.exe",
            Os::Windows
        ));
        assert!(fixed_path("%LOCALAPPDATA%\\x\\godot.exe", Os::Windows));
        assert!(!fixed_path("/usr/bin/godot", Os::Windows));
    }
}
