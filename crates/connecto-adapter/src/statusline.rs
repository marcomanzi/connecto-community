//! Claude's status lines on Windows: PowerShell twins of the `.sh` scripts,
//! so no `bash` or `jq` is needed.
//!
//! Connecto's builtin packages put a `statusLine` and a `subagentStatusLine`
//! into the project's `.claude/settings.json` that run `.sh` scripts with
//! `bash`/`sh` and `jq`. A Windows PC usually has neither `jq` nor, without Git
//! for Windows, `bash`. The project file stays as it is (it may be shared with a
//! Mac); instead, on Windows, the settings Connecto passes with `--settings`
//! name the PowerShell twins of the two scripts. The Claude Code docs (settings,
//! "Settings precedence", fetched 2026-11-12) say a key passed with
//! `--settings` "overrides the same key in your project and user settings
//! files", and `statusLine`/`subagentStatusLine` may be set in "Any file".
//!
//! This is done only while the project's own value is still the builtin one,
//! so a status line the user wrote is never replaced.
//!
//! A status line has no exec form: its `command` runs through Git Bash when
//! Git for Windows is installed, else through PowerShell (statusline docs,
//! "Windows configuration"). [`powershell_command`] is written so both shells
//! read it the same way.

use std::path::Path;

use serde_json::{Map, Value, json};

use crate::platform::comparable;
use crate::quote::{QuoteError, is_powershell_single_quote};

/// One status line setting and its builtin value, as in
/// `crates/connecto-packages/builtin/{claude,both}/.claude/settings.json`.
struct Line {
    key: &'static str,
    builtin: &'static str,
    /// The PowerShell twin, in the project's `.claude/scripts/`.
    script: &'static str,
}

const LINES: [Line; 2] = [
    Line {
        key: "statusLine",
        builtin: r#"bash "${CLAUDE_PROJECT_DIR:-.}/.claude/scripts/statusline.sh""#,
        script: "statusline.ps1",
    },
    Line {
        key: "subagentStatusLine",
        builtin: r#"sh "${CLAUDE_PROJECT_DIR:-.}/.claude/scripts/subagent-statusline.sh""#,
        script: "subagent-statusline.ps1",
    },
];

/// The `command` of `key` that the project's settings give Claude: the local
/// file's value if it sets the key, else the shared file's. (Above them only
/// `--settings` and managed settings; below them the user's file, which a
/// project value always outranks.)
fn project_command(project: &Path, key: &str) -> Option<String> {
    ["settings.local.json", "settings.json"]
        .iter()
        .find_map(|name| {
            let text = std::fs::read_to_string(project.join(".claude").join(name)).ok()?;
            let value = serde_json::from_str::<Value>(&text).ok()?;
            value.get(key).cloned()
        })
        .and_then(|setting| {
            setting
                .get("command")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
}

/// Windows PowerShell 5.1, which every Windows has, as a command word both Git
/// Bash and PowerShell run: an absolute path with forward slashes and no
/// quotes (a quoted first word is a string in PowerShell, and Git Bash eats
/// unquoted backslashes). Refused when `system_root` holds anything else than
/// letters, digits, `\`, `/`, `:`, `.`, `_` and `-`.
fn powershell_program(system_root: &str) -> Result<String, QuoteError> {
    let bytes = system_root.as_bytes();
    let drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    let plain = system_root.bytes().all(|b| {
        b.is_ascii_alphanumeric() || matches!(b, b'\\' | b'/' | b':' | b'.' | b'_' | b'-')
    });
    if !drive || !plain {
        return Err(QuoteError {
            shell: "Git Bash and PowerShell",
            value: system_root.to_owned(),
            reason: "the Windows folder is not a plain drive path",
        });
    }
    let root = system_root.trim_end_matches(['\\', '/']).replace('\\', "/");
    Ok(format!(
        "{root}/System32/WindowsPowerShell/v1.0/powershell.exe"
    ))
}

/// The status line command that runs `script` with Windows PowerShell:
///
/// `C:/WINDOWS/System32/WindowsPowerShell/v1.0/powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File 'C:\…\.claude\scripts\statusline.ps1'`
///
/// The script path is single-quoted, which both Git Bash and PowerShell read
/// literally; the two write a quote inside differently, so a path with a
/// single quote (or a typographic one, which PowerShell also reads as a quote),
/// a `"` or a control character is refused. `-ExecutionPolicy Bypass` applies
/// to this one process and changes no setting.
pub(crate) fn powershell_command(system_root: &str, script: &str) -> Result<String, QuoteError> {
    let program = powershell_program(system_root)?;
    let refuse = |reason| QuoteError {
        shell: "Git Bash and PowerShell",
        value: script.to_owned(),
        reason,
    };
    if script.chars().any(is_powershell_single_quote) {
        return Err(refuse("the two shells quote a single quote differently"));
    }
    if script.chars().any(|ch| ch == '"' || ch.is_control()) {
        return Err(refuse("it holds a double quote or a control character"));
    }
    Ok(format!(
        "{program} -NoProfile -NonInteractive -ExecutionPolicy Bypass -File '{script}'"
    ))
}

/// The status line settings for Claude's `--settings` on Windows: for each of
/// the two, the PowerShell twin, when the project still uses the builtin value
/// and the twin is installed. Empty when there is nothing to replace, or when
/// the command cannot be written safely (the project's value then stays).
pub(crate) fn windows_statuslines(project: &Path, system_root: Option<&str>) -> Map<String, Value> {
    let mut out = Map::new();
    let Some(system_root) = system_root else {
        return out;
    };
    for line in &LINES {
        if project_command(project, line.key).as_deref() != Some(line.builtin) {
            continue;
        }
        // Windows PowerShell may not take a verbatim (`\\?\`) path for `-File`.
        let script = comparable(&project.join(".claude").join("scripts").join(line.script));
        if !script.is_file() {
            continue;
        }
        if let Ok(command) = powershell_command(system_root, &script.to_string_lossy()) {
            out.insert(
                line.key.to_owned(),
                json!({ "type": "command", "command": command }),
            );
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_command_runs_windows_powershell_by_absolute_path_on_the_script() {
        assert_eq!(
            powershell_command(
                r"C:\WINDOWS",
                r"C:\Users\Jane Doe\my $proj & co; `x`\.claude\scripts\statusline.ps1"
            )
            .unwrap(),
            r"C:/WINDOWS/System32/WindowsPowerShell/v1.0/powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File 'C:\Users\Jane Doe\my $proj & co; `x`\.claude\scripts\statusline.ps1'"
        );
        assert_eq!(
            powershell_command(r"D:\Win\", r"D:\Zoë\日本\s.ps1").unwrap(),
            r"D:/Win/System32/WindowsPowerShell/v1.0/powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File 'D:\Zoë\日本\s.ps1'"
        );
    }

    #[test]
    fn what_both_shells_cannot_read_alike_is_refused() {
        for script in [
            r"C:\it's\s.ps1",
            "C:\\it\u{2019}s\\s.ps1",
            r#"C:\a"b\s.ps1"#,
            "C:\\a\nb\\s.ps1",
        ] {
            assert!(
                powershell_command(r"C:\WINDOWS", script).is_err(),
                "{script}"
            );
        }
        for root in ["", "WINDOWS", r"C:\Win dows", r"C:\$x", r"\\server\share"] {
            assert!(powershell_command(root, r"C:\s.ps1").is_err(), "{root}");
        }
    }

    fn project(files: &[(&str, &str)], scripts: bool) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mg-statusline-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".claude").join("scripts")).unwrap();
        for (name, body) in files {
            std::fs::write(dir.join(".claude").join(name), body).unwrap();
        }
        if scripts {
            for line in &LINES {
                std::fs::write(dir.join(".claude").join("scripts").join(line.script), "").unwrap();
            }
        }
        dir
    }

    fn builtin_settings() -> String {
        json!({
            "statusLine": { "type": "command", "command": LINES[0].builtin },
            "subagentStatusLine": { "type": "command", "command": LINES[1].builtin },
        })
        .to_string()
    }

    #[test]
    fn the_builtin_values_are_the_packaged_ones() {
        // Must match crates/connecto-packages/builtin/*/.claude/settings.json.
        assert_eq!(
            LINES[0].builtin,
            "bash \"${CLAUDE_PROJECT_DIR:-.}/.claude/scripts/statusline.sh\""
        );
        assert_eq!(
            LINES[1].builtin,
            "sh \"${CLAUDE_PROJECT_DIR:-.}/.claude/scripts/subagent-statusline.sh\""
        );
    }

    #[test]
    fn the_projects_effective_value_is_the_local_files_else_the_shared_files() {
        let dir = project(&[("settings.json", &builtin_settings())], false);
        assert_eq!(
            project_command(&dir, "statusLine").as_deref(),
            Some(LINES[0].builtin)
        );
        let dir = project(
            &[
                ("settings.json", &builtin_settings()),
                (
                    "settings.local.json",
                    r#"{"statusLine":{"type":"command","command":"mine"}}"#,
                ),
            ],
            false,
        );
        assert_eq!(project_command(&dir, "statusLine").as_deref(), Some("mine"));
        assert_eq!(
            project_command(&dir, "subagentStatusLine").as_deref(),
            Some(LINES[1].builtin)
        );
        assert_eq!(project_command(&dir, "nothing"), None);
        assert_eq!(
            project_command(Path::new("/nonexistent"), "statusLine"),
            None
        );
    }

    #[test]
    fn only_a_builtin_value_with_its_twin_installed_is_replaced() {
        // The builtin value without the twin, a value of the user's, no Windows
        // folder: nothing is replaced.
        let without_twin = project(&[("settings.json", &builtin_settings())], false);
        assert!(windows_statuslines(&without_twin, Some(r"C:\WINDOWS")).is_empty());
        let own = project(
            &[(
                "settings.json",
                r#"{"statusLine":{"type":"command","command":"my-line"}}"#,
            )],
            true,
        );
        assert!(windows_statuslines(&own, Some(r"C:\WINDOWS")).is_empty());
        let builtin = project(&[("settings.json", &builtin_settings())], true);
        assert!(windows_statuslines(&builtin, None).is_empty());
        // The builtin value with its twin: both are replaced.
        let found = windows_statuslines(&builtin, Some(r"C:\WINDOWS"));
        assert_eq!(found.len(), 2);
        for line in &LINES {
            let command = found[line.key]["command"].as_str().unwrap();
            assert!(
                command.starts_with(
                    "C:/WINDOWS/System32/WindowsPowerShell/v1.0/powershell.exe -NoProfile"
                ),
                "{command}"
            );
            let script = builtin.join(".claude").join("scripts").join(line.script);
            assert!(
                command.ends_with(&format!("-File '{}'", script.to_string_lossy())),
                "{command}"
            );
            assert_eq!(found[line.key]["type"], "command");
        }
    }
}
