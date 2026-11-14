//! Windows side of [`crate::signature`]: Authenticode.
//!
//! The check is Windows PowerShell's `Get-AuthenticodeSignature` (Windows'
//! own `WinVerifyTrust`, which cannot be called here without `unsafe`). One
//! PowerShell process checks every file of a release, because each start
//! costs about two seconds:
//!
//! - the program is `%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe`,
//!   by absolute path, with `-NoProfile -NonInteractive -EncodedCommand`
//!   (the script is a constant; `-Command` text loses its quotes on the way
//!   through the Windows command line);
//! - the paths go in the environment variable [`PATHS_VAR`], one per line,
//!   and the script reads each with `-LiteralPath`: no path is ever part of
//!   the script, and none is interpreted (`[`, `]`, `$`, `'`, `;`, spaces
//!   and non-ASCII letters reach the check as they are). A line break cannot
//!   be part of a Windows file name (control characters are not allowed in
//!   one), so it cannot split a path in two; a path with one is refused
//!   before anything runs;
//! - the environment is empty except `SystemRoot` (without it PowerShell
//!   does not start) and the paths; no console window opens;
//! - the answer is one JSON line per path, in order, naming the path it
//!   checked. Anything else (a missing, extra or unreadable line, another
//!   path, a failed or slow PowerShell) refuses every file.
//!
//! A file passes when its status is exactly `Valid` (checked first: a
//! changed file still names its signer, with `HashMismatch`), and then, when
//! a publisher is asked for, the signer certificate's subject has exactly one
//! common name (CN) and one organisation (O), equal to that publisher's.
//! Certificates are never pinned: Anthropic's has already been renewed, and
//! OpenAI's live for a day.

use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::Failure;
use crate::platform::{CLAUDE_SIGNER, CODEX_SIGNER, MICROSOFT_RUNTIME_SIGNER};

/// A signer, as its certificate's subject names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Publisher {
    /// The subject's common name (CN).
    common_name: &'static str,
    /// The subject's organisation (O).
    organization: &'static str,
}

/// The publishers a `team` may name, each with the exact subject its
/// certificates have (seen on Windows on 2026-11-12). A `team` not listed
/// here is refused.
const PUBLISHERS: [(&str, Publisher); 3] = [
    (
        CLAUDE_SIGNER,
        Publisher {
            common_name: "Anthropic, PBC",
            organization: "Anthropic, PBC",
        },
    ),
    (
        CODEX_SIGNER,
        Publisher {
            common_name: "OpenAI OpCo, LLC",
            organization: "OpenAI OpCo, LLC",
        },
    ),
    // The Visual C++ runtime that Codex ships (`vcruntime140.dll`) is signed
    // by Microsoft's Windows certificate.
    (
        MICROSOFT_RUNTIME_SIGNER,
        Publisher {
            common_name: "Microsoft Windows",
            organization: "Microsoft Corporation",
        },
    ),
];

/// The environment variable that carries the paths to the script.
const PATHS_VAR: &str = "CONNECTO_VERIFY_PATHS";
/// Between two paths in [`PATHS_VAR`]; never part of a Windows file name.
const SEPARATOR: char = '\n';
/// How long one PowerShell run may take. About five seconds are needed for
/// Codex's 32 programs and libraries (half a gigabyte).
const TIME_LIMIT: Duration = Duration::from_secs(120);
/// The most of [`PATHS_VAR`] one run gets, in UTF-16 units: an environment
/// variable may hold 32 767; more paths go to further runs.
const BATCH_UNITS: usize = 24_000;
/// More output than this is not believed (about 400 bytes per file).
const MAX_OUTPUT: u64 = 4 * 1024 * 1024;
/// `CREATE_NO_WINDOW`: PowerShell gets no console window, so none flashes up.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The script. It knows nothing of the paths but the variable's name.
const SCRIPT: &str = r#"$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false
$paths = $env:CONNECTO_VERIFY_PATHS -split "`n"
for ($i = 0; $i -lt $paths.Count; $i++) {
  $p = $paths[$i]
  try {
    $s = Get-AuthenticodeSignature -LiteralPath $p
    $subject = $null
    if ($s.SignerCertificate) { $subject = $s.SignerCertificate.Subject }
    [pscustomobject]@{ i = $i; path = [string]$s.Path; status = [string]$s.Status; type = [string]$s.SignatureType; subject = $subject } | ConvertTo-Json -Compress
  } catch {
    [pscustomobject]@{ i = $i; path = $p; status = 'Error'; error = $_.Exception.GetType().Name } | ConvertTo-Json -Compress
  }
}
"#;

/// Check every file: `Ok` only when each passes. A file that Windows judged
/// (unsigned, changed, another signer, a path that can never be handed over)
/// is [`Failure::Invalid`]; a check that did not finish (PowerShell missing,
/// slow, failed or garbled, or a file Windows could not read) is
/// [`Failure::Incomplete`], as on macOS.
pub(super) fn check_all(files: &[(&Path, Option<&str>)]) -> Result<(), Failure> {
    check(files)?.into_iter().collect()
}

/// [`check_all`], with every failure as text.
#[cfg(test)]
fn verify_all(files: &[(&Path, Option<&str>)]) -> Result<(), String> {
    check_all(files).map_err(|failure| failure.to_string())
}

/// One answer per file, in order; `Err` when the check itself failed.
fn check(files: &[(&Path, Option<&str>)]) -> Result<Vec<Result<(), Failure>>, Failure> {
    // With one kind of separator: PowerShell reports `bin\codex.exe` for
    // `bin/codex.exe` (the store joins its relative names with `/`), and its
    // answer must name exactly the path that was sent.
    let normal: Vec<PathBuf> = files
        .iter()
        .map(|(file, _)| file.components().collect())
        .collect();
    let paths = normal
        .iter()
        .map(|file| sendable(file))
        .collect::<Result<Vec<_>, _>>()
        .map_err(Failure::Invalid)?;
    let program = powershell().map_err(Failure::Incomplete)?;
    let mut records = Vec::with_capacity(paths.len());
    for batch in batches(&paths) {
        records.extend(run(&program, SCRIPT, batch, TIME_LIMIT).map_err(Failure::Incomplete)?);
    }
    Ok(files
        .iter()
        .zip(records)
        .map(|((file, team), record)| {
            judge(file, *team, &record).map_err(|reason| {
                if record.status == "Error" {
                    Failure::Incomplete(reason)
                } else {
                    Failure::Invalid(reason)
                }
            })
        })
        .collect())
}

/// The path as it is handed to PowerShell, or why it cannot be.
fn sendable(file: &Path) -> Result<&str, String> {
    let shown = file.display();
    let text = file.to_str().ok_or_else(|| {
        format!("{shown}: its path is not valid Unicode, so its signature cannot be checked")
    })?;
    if !file.is_absolute() || text.chars().any(char::is_control) {
        return Err(format!(
            "{shown}: its path cannot be handed to the signature check"
        ));
    }
    Ok(text)
}

/// The paths split into runs that each fit in one environment variable.
fn batches<'a>(paths: &'a [&'a str]) -> Vec<&'a [&'a str]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut units = 0;
    for (index, path) in paths.iter().enumerate() {
        let size = path.encode_utf16().count() + 1;
        if index > start && units + size > BATCH_UNITS {
            out.extend(paths.get(start..index));
            start = index;
            units = 0;
        }
        units += size;
    }
    if start < paths.len() {
        out.extend(paths.get(start..));
    }
    out
}

/// Windows PowerShell 5.1, by absolute path under `SystemRoot`.
fn powershell() -> Result<PathBuf, String> {
    let root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .filter(|root| root.is_absolute())
        .ok_or(
            "SystemRoot is not set, so Windows PowerShell cannot be found to check signatures",
        )?;
    Ok(root
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe"))
}

/// What PowerShell reported for one file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Record {
    status: String,
    /// `Authenticode` (in the file) or `Catalog` (in a Windows catalog).
    kind: String,
    /// The signer certificate's subject, as .NET writes it.
    subject: Option<String>,
    /// The kind of error, for status `Error`.
    error: Option<String>,
}

/// `-EncodedCommand`'s argument: the script as base64 of UTF-16LE.
fn encoded(script: &str) -> String {
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64(&bytes)
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let byte = |n: usize| u32::from(chunk.get(n).copied().unwrap_or(0));
        let triple = (byte(0) << 16) | (byte(1) << 8) | byte(2);
        for position in 0..4 {
            if position <= chunk.len() {
                let index = (triple >> (18 - 6 * position)) & 0x3f;
                let digit = ALPHABET.get(index as usize).copied().unwrap_or(b'=');
                out.push(char::from(digit));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Run `script` on `paths` with `program` and read its answer, within `limit`.
fn run(
    program: &Path,
    script: &str,
    paths: &[&str],
    limit: Duration,
) -> Result<Vec<Record>, String> {
    let system_root = program
        .ancestors()
        .nth(4)
        .ok_or("Windows PowerShell's path is not under SystemRoot")?;
    let mut list = String::new();
    for (index, path) in paths.iter().enumerate() {
        if index > 0 {
            list.push(SEPARATOR);
        }
        list.push_str(path);
    }
    let deadline = Instant::now() + limit;
    let mut child = Command::new(program)
        .args(["-NoProfile", "-NonInteractive", "-EncodedCommand"])
        .arg(encoded(script))
        .env_clear()
        .env("SystemRoot", system_root)
        .env(PATHS_VAR, &list)
        .current_dir(system_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|err| {
            format!("could not start Windows PowerShell to check signatures ({err}), so nothing can be verified")
        })?;
    let output = read_within(&mut child, deadline, limit);
    let status =
        output.and_then(|output| wait_within(&mut child, deadline).map(|code| (output, code)));
    let (output, status) = match status {
        Ok(done) => done,
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(err);
        }
    };
    if !status.success() {
        return Err(format!(
            "the signature check failed (Windows PowerShell ended with {status})"
        ));
    }
    parse_output(paths, &output)
}

/// All of the child's output, unless it does not end before `deadline`.
fn read_within(child: &mut Child, deadline: Instant, limit: Duration) -> Result<Vec<u8>, String> {
    let mut pipe = child
        .stdout
        .take()
        .ok_or("the signature check gave no output")?;
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("authenticode-read".to_owned())
        .spawn(move || {
            let mut output = Vec::new();
            let read = (&mut pipe).take(MAX_OUTPUT + 1).read_to_end(&mut output);
            let _ = tx.send(read.map(|_| output));
        })
        .map_err(|err| format!("the signature check could not start: {err}"))?;
    let wait = deadline.saturating_duration_since(Instant::now());
    match rx.recv_timeout(wait) {
        Ok(Ok(output)) if output.len() as u64 > MAX_OUTPUT => {
            Err("the signature check said too much".to_owned())
        }
        Ok(Ok(output)) => Ok(output),
        Ok(Err(err)) => Err(format!(
            "the signature check's output could not be read: {err}"
        )),
        Err(_) => Err(format!(
            "the signature check took longer than {} seconds, so nothing is trusted",
            limit.as_secs()
        )),
    }
}

/// The child's exit status, unless it does not end before `deadline`.
fn wait_within(child: &mut Child, deadline: Instant) -> Result<std::process::ExitStatus, String> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                return Err("the signature check did not end in time".to_owned());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(err) => return Err(format!("the signature check could not be followed: {err}")),
        }
    }
}

/// One record per path, in order, each naming the path it is about.
fn parse_output(paths: &[&str], output: &[u8]) -> Result<Vec<Record>, String> {
    const GARBLED: &str = "the signature check's answer could not be read, so nothing is trusted";
    let text = std::str::from_utf8(output).map_err(|_| GARBLED.to_owned())?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .filter(|line| !line.is_empty())
        .collect();
    if lines.len() != paths.len() {
        return Err(format!(
            "the signature check answered for {} files instead of {}, so nothing is trusted",
            lines.len(),
            paths.len()
        ));
    }
    lines
        .iter()
        .zip(paths)
        .enumerate()
        .map(|(index, (line, path))| {
            let value: serde_json::Value =
                serde_json::from_str(line).map_err(|_| GARBLED.to_owned())?;
            let field = |name: &str| -> Result<Option<String>, String> {
                match value.get(name) {
                    None | Some(serde_json::Value::Null) => Ok(None),
                    Some(serde_json::Value::String(text)) => Ok(Some(text.clone())),
                    Some(_) => Err(GARBLED.to_owned()),
                }
            };
            if value.get("i").and_then(serde_json::Value::as_u64) != Some(index as u64) {
                return Err(GARBLED.to_owned());
            }
            if field("path")?.as_deref() != Some(*path) {
                return Err(format!(
                    "the signature check answered for another file than {path}, so nothing is trusted"
                ));
            }
            Ok(Record {
                status: field("status")?.ok_or(GARBLED)?,
                kind: field("type")?.unwrap_or_default(),
                subject: field("subject")?,
                error: field("error")?,
            })
        })
        .collect()
}

/// A word from PowerShell fit to be shown: letters and digits only.
fn shown(word: &str) -> &str {
    if !word.is_empty() && word.len() <= 64 && word.bytes().all(|b| b.is_ascii_alphanumeric()) {
        word
    } else {
        "an unknown status"
    }
}

/// Whether one file passes: `Valid` first, then the publisher.
fn judge(file: &Path, team: Option<&str>, record: &Record) -> Result<(), String> {
    let name = file.display();
    if record.status != "Valid" {
        let status = shown(&record.status);
        return Err(match (status, record.error.as_deref()) {
            ("NotSigned", _) => format!("{name} is not signed (Windows reports NotSigned)"),
            ("HashMismatch", _) => {
                format!("{name} was changed after it was signed (Windows reports HashMismatch)")
            }
            ("Error", Some(error)) => {
                format!("{name} could not be checked ({})", shown(error))
            }
            _ => format!("{name} does not have a valid signature (Windows reports {status})"),
        });
    }
    let Some(team) = team else {
        return Ok(());
    };
    let Some(expected) = PUBLISHERS
        .iter()
        .find_map(|(key, publisher)| (*key == team).then_some(*publisher))
    else {
        return Err(format!("{name}: no publisher is known for {team}"));
    };
    let subject = record
        .subject
        .as_deref()
        .ok_or_else(|| format!("{name}: its signature names no signer"))?;
    let attributes =
        parse_name(subject).ok_or_else(|| format!("{name}: its signer's name cannot be read"))?;
    let only = |key: &str| -> Option<&str> {
        let mut found = attributes
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str());
        let first = found.next()?;
        found.next().is_none().then_some(first)
    };
    let (common_name, organization) = (only("CN"), only("O"));
    if common_name == Some(expected.common_name) && organization == Some(expected.organization) {
        return Ok(());
    }
    Err(format!(
        "{name} is signed by {:?} ({:?}), not by {} ({}), with {} signature",
        common_name.unwrap_or("?"),
        organization.unwrap_or("?"),
        expected.common_name,
        expected.organization,
        if record.kind == "Catalog" {
            "a catalog"
        } else {
            "an embedded"
        }
    ))
}

/// The attributes of an X.500 name as .NET writes it, in order:
/// `CN="Anthropic, PBC", O="Anthropic, PBC", L=San Francisco, C=US`. They
/// are separated by `,` (or `;`, or `+` inside one multi-valued part); a
/// value in double quotes may hold those and writes `"` as `""`. `None` for
/// anything not of that form, so nothing is guessed.
fn parse_name(text: &str) -> Option<Vec<(String, String)>> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    loop {
        while chars.next_if_eq(&' ').is_some() {}
        let mut key = String::new();
        while let Some(c) = chars.next_if(|c| *c != '=') {
            if matches!(c, ',' | ';' | '+' | '"') {
                return None;
            }
            key.push(c);
        }
        chars.next()?;
        let key = key.trim_end();
        if key.is_empty() {
            return None;
        }
        while chars.next_if_eq(&' ').is_some() {}
        let mut value = String::new();
        if chars.next_if_eq(&'"').is_some() {
            loop {
                match chars.next()? {
                    '"' if chars.next_if_eq(&'"').is_some() => value.push('"'),
                    '"' => break,
                    c => value.push(c),
                }
            }
            while chars.next_if_eq(&' ').is_some() {}
        } else {
            while let Some(c) = chars.next_if(|c| !matches!(c, ',' | ';' | '+')) {
                if c == '"' {
                    return None;
                }
                value.push(c);
            }
            value.truncate(value.trim_end().len());
        }
        out.push((key.to_owned(), value));
        match chars.next() {
            None => return Some(out),
            Some(',' | ';' | '+') => {}
            Some(_) => return None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::fs;

    use super::*;

    const ANTHROPIC_SUBJECT: &str = r#"CN="Anthropic, PBC", O="Anthropic, PBC", L=San Francisco, S=California, C=US, SERIALNUMBER=4860621, OID.2.5.4.15=Private Organization, OID.1.3.6.1.4.1.311.60.2.1.2=Delaware, OID.1.3.6.1.4.1.311.60.2.1.3=US"#;
    const OPENAI_SUBJECT: &str =
        r#"CN="OpenAI OpCo, LLC", O="OpenAI OpCo, LLC", L=San Francisco, S=California, C=US"#;
    const MICROSOFT_SUBJECT: &str =
        "CN=Microsoft Windows, O=Microsoft Corporation, L=Redmond, S=Washington, C=US";

    fn record(status: &str, subject: Option<&str>) -> Record {
        Record {
            status: status.to_owned(),
            kind: "Authenticode".to_owned(),
            subject: subject.map(str::to_owned),
            error: None,
        }
    }

    #[test]
    fn names_with_quoted_commas_are_read_attribute_by_attribute() {
        let parsed = parse_name(ANTHROPIC_SUBJECT).unwrap();
        assert_eq!(parsed[0], ("CN".to_owned(), "Anthropic, PBC".to_owned()));
        assert_eq!(parsed[1], ("O".to_owned(), "Anthropic, PBC".to_owned()));
        assert_eq!(parsed[2], ("L".to_owned(), "San Francisco".to_owned()));
        assert_eq!(parsed.len(), 9);
        assert_eq!(
            parsed[6],
            ("OID.2.5.4.15".to_owned(), "Private Organization".to_owned())
        );
        let microsoft = parse_name(MICROSOFT_SUBJECT).unwrap();
        assert_eq!(microsoft[0].1, "Microsoft Windows");
        assert_eq!(microsoft[1].1, "Microsoft Corporation");
        // A doubled quote is one quote; `+` and `;` separate too.
        assert_eq!(
            parse_name(r#"CN="a ""b"", c" + O=d; L=e"#).unwrap(),
            [
                ("CN".to_owned(), r#"a "b", c"#.to_owned()),
                ("O".to_owned(), "d".to_owned()),
                ("L".to_owned(), "e".to_owned()),
            ]
        );
        for bad in [
            "",
            "CN",
            "CN=a,",
            "=a",
            r#"CN="open"#,
            r#"CN="a"b"#,
            r#"CN=a"b"#,
            "CN=a, , O=b",
        ] {
            assert_eq!(parse_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn only_valid_files_of_the_pinned_publisher_pass() {
        let file = Path::new(r"C:\store\claude.exe");
        assert_eq!(
            judge(
                file,
                Some(CLAUDE_SIGNER),
                &record("Valid", Some(ANTHROPIC_SUBJECT))
            ),
            Ok(())
        );
        assert_eq!(
            judge(
                file,
                Some(CODEX_SIGNER),
                &record("Valid", Some(OPENAI_SUBJECT))
            ),
            Ok(())
        );
        assert_eq!(
            judge(
                file,
                Some(MICROSOFT_RUNTIME_SIGNER),
                &record("Valid", Some(MICROSOFT_SUBJECT))
            ),
            Ok(())
        );
        // No publisher asked for: a valid signature is enough.
        assert_eq!(judge(file, None, &record("Valid", None)), Ok(()));

        // The status comes first: a changed file still names its signer.
        let err = judge(
            file,
            Some(CLAUDE_SIGNER),
            &record("HashMismatch", Some(ANTHROPIC_SUBJECT)),
        )
        .unwrap_err();
        assert!(
            err.contains("HashMismatch") && err.contains("claude.exe"),
            "{err}"
        );
        for status in [
            "NotSigned",
            "NotTrusted",
            "UnknownError",
            "Incompatible",
            "valid",
            "",
            "Valid ",
        ] {
            assert!(
                judge(file, None, &record(status, Some(ANTHROPIC_SUBJECT))).is_err(),
                "{status}"
            );
        }
        // A status that is not a plain word is not repeated.
        let err = judge(file, None, &record("<script>\nx", None)).unwrap_err();
        assert!(err.contains("an unknown status"), "{err}");
        let mut error = record("Error", None);
        error.error = Some("FileNotFoundException".to_owned());
        let err = judge(file, None, &error).unwrap_err();
        assert!(err.contains("FileNotFoundException"), "{err}");

        // Another publisher, or a name that only looks like the right one.
        for subject in [
            OPENAI_SUBJECT,
            MICROSOFT_SUBJECT,
            r#"CN="Anthropic, PBC", O=Evil"#,
            r#"CN=Evil, O="Anthropic, PBC""#,
            r#"CN="Anthropic, PBC Evil", O="Anthropic, PBC""#,
            r#"CN="anthropic, pbc", O="anthropic, pbc""#,
            r#"CN=Anthropic, O=PBC"#,
            r#"CN="Anthropic, PBC", O="Anthropic, PBC", O=Evil"#,
            r#"CN="Anthropic, PBC", CN=Evil, O="Anthropic, PBC""#,
            r#"OU="Anthropic, PBC", O="Anthropic, PBC""#,
            r#"CN="Anthropic, PBC"#,
            "garbled",
        ] {
            assert!(
                judge(file, Some(CLAUDE_SIGNER), &record("Valid", Some(subject))).is_err(),
                "{subject}"
            );
        }
        assert!(judge(file, Some(CLAUDE_SIGNER), &record("Valid", None)).is_err());
        // Microsoft is accepted only where Microsoft is asked for.
        assert!(
            judge(
                file,
                Some(CODEX_SIGNER),
                &record("Valid", Some(MICROSOFT_SUBJECT))
            )
            .is_err()
        );
        // A publisher nobody pinned is refused, whatever the certificate.
        let err = judge(
            file,
            Some("Q6L2SF6YDW"),
            &record("Valid", Some(ANTHROPIC_SUBJECT)),
        )
        .unwrap_err();
        assert!(err.contains("no publisher is known"), "{err}");
    }

    fn line(i: usize, path: &str, status: &str) -> String {
        serde_json::json!({"i": i, "path": path, "status": status, "type": "Authenticode", "subject": OPENAI_SUBJECT})
            .to_string()
    }

    #[test]
    fn the_answer_must_be_one_line_per_path_in_order_naming_that_path() {
        let paths = [r"C:\a b\x.exe", r"C:\é'[1];$x\y.dll"];
        let good = format!(
            "{}\r\n{}\r\n",
            line(0, paths[0], "Valid"),
            line(1, paths[1], "NotSigned")
        );
        let records = parse_output(&paths, good.as_bytes()).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].status, "NotSigned");
        assert_eq!(records[0].subject.as_deref(), Some(OPENAI_SUBJECT));
        // A byte order mark and blank lines do not matter.
        let marked = format!("\u{feff}\r\n{good}\r\n");
        assert!(parse_output(&paths, marked.as_bytes()).is_ok());

        let bad = [
            // One missing, one extra.
            format!("{}\r\n", line(0, paths[0], "Valid")),
            format!("{good}{}\r\n", line(2, paths[1], "Valid")),
            // Out of order, wrong index, another path.
            format!(
                "{}\r\n{}\r\n",
                line(1, paths[1], "Valid"),
                line(0, paths[0], "Valid")
            ),
            format!(
                "{}\r\n{}\r\n",
                line(0, paths[0], "Valid"),
                line(0, paths[1], "Valid")
            ),
            format!(
                "{}\r\n{}\r\n",
                line(0, paths[0], "Valid"),
                line(1, r"C:\other.dll", "Valid")
            ),
            // Garbled lines and fields.
            format!("{}\r\nnot json\r\n", line(0, paths[0], "Valid")),
            format!(
                "{}\r\nWARNING: x\r\n{}\r\n",
                line(0, paths[0], "Valid"),
                line(1, paths[1], "Valid")
            ),
            format!(
                "{}\r\n{}\r\n",
                line(0, paths[0], "Valid"),
                r#"{"i":1,"path":"C:\\é'[1];$x\\y.dll","status":5}"#
            ),
            format!(
                "{}\r\n{}\r\n",
                line(0, paths[0], "Valid"),
                r#"{"i":1,"status":"Valid"}"#
            ),
            String::new(),
        ];
        for output in &bad {
            assert!(parse_output(&paths, output.as_bytes()).is_err(), "{output}");
        }
        assert!(parse_output(&paths, b"\xff\xfe").is_err());
    }

    #[test]
    fn paths_that_cannot_be_handed_over_are_refused_before_anything_runs() {
        assert!(sendable(Path::new(r"C:\a\b.exe")).is_ok());
        assert!(sendable(Path::new(r"\\?\C:\a b\é.exe")).is_ok());
        for bad in [
            "relative.exe",
            "C:\\a\nb.exe",
            "C:\\a\rb.exe",
            "C:\\a\u{0}b",
        ] {
            assert!(sendable(Path::new(bad)).is_err(), "{bad:?}");
        }
        assert!(check(&[(Path::new("relative.exe"), None)]).is_err());
    }

    #[test]
    fn long_lists_are_split_into_runs_that_fit_one_variable() {
        let long = format!(r"C:\{}", "x".repeat(10_000));
        let paths = [long.as_str(), long.as_str(), long.as_str(), r"C:\y"];
        let runs = batches(&paths);
        assert_eq!(runs.iter().map(|run| run.len()).collect::<Vec<_>>(), [2, 2]);
        assert_eq!(batches(&[]).len(), 0);
        let short = [r"C:\a"; 100];
        assert_eq!(batches(&short).len(), 1);
    }

    #[test]
    fn base64_is_standard() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), expected);
        }
        // "a" as UTF-16LE.
        assert_eq!(encoded("a"), "YQA=");
    }

    #[test]
    fn a_missing_powershell_or_one_that_hangs_refuses_everything() {
        let missing =
            Path::new(r"C:\Windows\System32\WindowsPowerShell\v1.0\no-such-powershell.exe");
        let err = run(missing, SCRIPT, &[r"C:\x.exe"], TIME_LIMIT).unwrap_err();
        assert!(err.contains("could not start Windows PowerShell"), "{err}");

        let program = powershell().unwrap();
        let started = Instant::now();
        let err = run(
            &program,
            "Start-Sleep -Seconds 30",
            &[r"C:\x.exe"],
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert!(err.contains("longer than"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(15));
        // A script that answers for nothing, or fails.
        assert!(run(&program, "exit 0", &[r"C:\x.exe"], TIME_LIMIT).is_err());
        assert!(run(&program, "exit 3", &[r"C:\x.exe"], TIME_LIMIT).is_err());
    }

    /// A file that Windows signed in its catalog, small and always there.
    fn system_program() -> PathBuf {
        powershell()
            .unwrap()
            .ancestors()
            .nth(3)
            .unwrap()
            .join("where.exe")
    }

    /// A copy of `source` with one byte flipped in its middle.
    fn tampered(source: &Path, target: &Path) {
        let mut bytes = fs::read(source).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0xff;
        fs::write(target, bytes).unwrap();
    }

    /// One PowerShell run over real files: each answer is the right one, and
    /// a path with spaces, quotes, brackets, `;`, `$` and non-ASCII letters
    /// gets through unchanged.
    #[test]
    fn real_signatures_are_checked_in_one_run() {
        let dir =
            std::env::temp_dir().join(format!("connecto-authenticode-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let odd = dir.join("a b'c;d[1]$e é 文");
        fs::create_dir_all(&odd).unwrap();
        let microsoft = odd.join("where copy.exe");
        fs::copy(system_program(), &microsoft).unwrap();
        let text = dir.join("text.exe");
        fs::write(&text, "not a program").unwrap();
        let missing = dir.join("missing.exe");
        // This test's own program is not signed.
        let unsigned = std::env::current_exe().unwrap();

        // The store joins `bin/codex.exe`: a `/` in the path is checked too.
        let slashed = dir.join("a b'c;d[1]$e é 文/where copy.exe");
        let mut cases: Vec<(PathBuf, Option<&str>, Result<(), &str>)> = vec![
            (slashed, Some(MICROSOFT_RUNTIME_SIGNER), Ok(())),
            (microsoft.clone(), Some(MICROSOFT_RUNTIME_SIGNER), Ok(())),
            (microsoft.clone(), None, Ok(())),
            (
                microsoft.clone(),
                Some(CLAUDE_SIGNER),
                Err("Microsoft Windows"),
            ),
            (microsoft, Some(CODEX_SIGNER), Err("Microsoft Windows")),
            (unsigned, Some(CLAUDE_SIGNER), Err("NotSigned")),
            (text, None, Err("valid signature")),
            (missing, None, Err("could not be checked")),
        ];
        // A file with its signature inside, changed by one byte: Codex's
        // smallest library, else Claude Code itself (both when installed).
        let home = crate::platform::home_dir().unwrap();
        let codex_lib = home.join(
            r"AppData\Roaming\npm\node_modules\@openai\codex\node_modules\@openai\codex-win32-x64\vendor\x86_64-pc-windows-msvc\codex-resources\voice\bin\z.dll",
        );
        let claude = home.join(r".local\bin\claude.exe");
        if codex_lib.is_file() {
            let copy = dir.join("z.dll");
            fs::copy(&codex_lib, &copy).unwrap();
            cases.push((copy, Some(CODEX_SIGNER), Ok(())));
            let changed = dir.join("z-changed.dll");
            tampered(&codex_lib, &changed);
            cases.push((changed, Some(CODEX_SIGNER), Err("HashMismatch")));
        }
        if claude.is_file() {
            cases.push((claude.clone(), Some(CLAUDE_SIGNER), Ok(())));
            cases.push((claude.clone(), Some(CODEX_SIGNER), Err("Anthropic, PBC")));
            if !codex_lib.is_file() {
                let changed = dir.join("claude-changed.exe");
                tampered(&claude, &changed);
                cases.push((changed, Some(CLAUDE_SIGNER), Err("HashMismatch")));
            }
        }

        let files: Vec<(&Path, Option<&str>)> = cases
            .iter()
            .map(|(path, team, _)| (path.as_path(), *team))
            .collect();
        let started = Instant::now();
        let answers = check(&files).unwrap();
        eprintln!("{} files checked in {:?}", files.len(), started.elapsed());
        assert_eq!(answers.len(), cases.len());
        for ((path, team, expected), answer) in cases.iter().zip(&answers) {
            match (expected, answer) {
                (Ok(()), Ok(())) => {}
                (Err(part), Err(err)) => {
                    // A file Windows could not read is an unfinished check;
                    // every other refusal is a definite one.
                    let unfinished = matches!(err, Failure::Incomplete(_));
                    assert_eq!(unfinished, *part == "could not be checked", "{err:?}");
                    let err = err.to_string();
                    assert!(err.contains(part), "{}: {err}", path.display());
                }
                _ => panic!("{} as {team:?}: {answer:?}", path.display()),
            }
        }
        // The whole list fails when one file does.
        assert!(verify_all(&files).is_err());
        assert!(verify_all(&files[..2]).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }
}
