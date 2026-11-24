//! Quoting a word for the shell a tool runs its hook commands with.
//!
//! macOS: `sh -c`. Windows: Codex runs hook commands as
//! `pwsh.exe -NoProfile -Command "<cmd>"` (or Windows PowerShell), checked on
//! Codex 0.167.2 (`docs/windows-port.md` 4.8.3). Claude's Windows hooks need no
//! quoting: they use the exec form.

/// Why a word cannot be quoted safely. Nothing is ever built from such a word.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{value:?} cannot be quoted for {shell}: {reason}")]
pub struct QuoteError {
    pub shell: &'static str,
    pub value: String,
    pub reason: &'static str,
}

/// Quote a word for `sh`, which runs hook commands. The data root usually has
/// a space in it.
pub(crate) fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// The characters PowerShell reads as a single quote: the ASCII one and the
/// four typographic ones (as its own `EscapeSingleQuotedStringContent` does).
pub(crate) fn is_powershell_single_quote(ch: char) -> bool {
    matches!(ch, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}')
}

/// Quote a word as a PowerShell single-quoted string: everything in it is
/// literal (`$`, backtick, `&`, `;`, `\`), and a single quote is written twice.
///
/// Refused: control characters (a line break would end the command) and `"`
/// (double quotes were lost on the way through the Windows command line into
/// `-Command`; no Windows path holds one).
pub(crate) fn powershell_quote(word: &str) -> Result<String, QuoteError> {
    let refuse = |reason| QuoteError {
        shell: "PowerShell",
        value: word.to_owned(),
        reason,
    };
    let mut out = String::with_capacity(word.len() + 2);
    out.push('\'');
    for ch in word.chars() {
        if ch.is_control() {
            return Err(refuse("it holds a control character"));
        }
        if ch == '"' {
            return Err(refuse("it holds a double quote"));
        }
        if is_powershell_single_quote(ch) {
            out.push(ch);
        }
        out.push(ch);
    }
    out.push('\'');
    Ok(out)
}

/// Quote a program path for `cmd /c`, in double quotes. Antigravity runs its
/// hook commands that way on Windows, exactly as written.
///
/// Refused: control characters, `"` (it would end the quotes) and `%` (`cmd`
/// expands `%NAME%` even inside them). No Windows path holds a `"`, and a
/// Connecto data folder with a `%` in it gets no Antigravity hooks.
pub(crate) fn cmd_quote(word: &str) -> Result<String, QuoteError> {
    let refuse = |reason| QuoteError {
        shell: "cmd",
        value: word.to_owned(),
        reason,
    };
    if word.chars().any(char::is_control) {
        return Err(refuse("it holds a control character"));
    }
    if word.contains('"') {
        return Err(refuse("it holds a double quote"));
    }
    if word.contains('%') {
        return Err(refuse("it holds a percent sign"));
    }
    Ok(format!("\"{word}\""))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn program_paths_are_quoted_for_cmd_and_what_it_would_expand_is_refused() {
        assert_eq!(
            cmd_quote(r"C:\Users\Jane O'Neil\connecto-hook.exe").unwrap(),
            r#""C:\Users\Jane O'Neil\connecto-hook.exe""#
        );
        for bad in [r"C:\a%PATH%\x", "C:\\a\nb", r#"C:\a"b"#] {
            assert_eq!(cmd_quote(bad).unwrap_err().shell, "cmd", "{bad:?}");
        }
    }

    #[test]
    fn words_are_quoted_for_sh_whatever_they_hold() {
        assert_eq!(shell_quote("/App Support/it's"), r"'/App Support/it'\''s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("$(rm -rf /) `x`"), "'$(rm -rf /) `x`'");
    }

    #[test]
    fn words_are_quoted_for_powershell_whatever_they_hold() {
        let q = |word: &str| powershell_quote(word).unwrap();
        assert_eq!(
            q(r"C:\Users\Jane Doe\AppData\Roaming\connecto\cli\bin\connecto-hook.exe"),
            r"'C:\Users\Jane Doe\AppData\Roaming\connecto\cli\bin\connecto-hook.exe'"
        );
        assert_eq!(q(r"C:\it's\x.exe"), r"'C:\it''s\x.exe'");
        assert_eq!(q("''"), "''''''");
        assert_eq!(q(r"C:\$env:PATH\$(calc)\x"), r"'C:\$env:PATH\$(calc)\x'");
        assert_eq!(q(r"C:\a`b`\x"), r"'C:\a`b`\x'");
        assert_eq!(q(r"C:\a & b; c | d\x"), r"'C:\a & b; c | d\x'");
        assert_eq!(q(r"C:\Users\Zoë\日本\x.exe"), r"'C:\Users\Zoë\日本\x.exe'");
        // A trailing backslash stays literal and does not escape the quote.
        assert_eq!(q(r"C:\dir\"), r"'C:\dir\'");
        assert_eq!(q(""), "''");
        // PowerShell also ends a single-quoted string at a typographic quote.
        assert_eq!(q("C:\\it\u{2019}s"), "'C:\\it\u{2019}\u{2019}s'");
        assert_eq!(
            q("\u{2018}\u{201A}\u{201B}"),
            "'\u{2018}\u{2018}\u{201A}\u{201A}\u{201B}\u{201B}'"
        );
    }

    #[test]
    fn what_powershell_cannot_take_literally_is_refused() {
        for bad in [
            "C:\\a\nb",
            "C:\\a\rb",
            "C:\\a\0b",
            "C:\\a\u{1b}b",
            r#"C:\a"b"#,
        ] {
            let err = powershell_quote(bad).unwrap_err();
            assert_eq!(err.shell, "PowerShell", "{bad:?}");
        }
    }
}
