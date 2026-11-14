//! Code signature checks.
//!
//! macOS: the system's `codesign`, and the Apple Team ID of the signer.
//! Windows: Authenticode, through Windows PowerShell's
//! `Get-AuthenticodeSignature`, and the signer certificate's publisher
//! (`signature/windows.rs`). Either way a file that cannot be checked is
//! refused.

#[cfg(unix)]
use std::ffi::OsStr;
use std::fmt;
#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::io::{self, Read};
use std::path::Path;
#[cfg(unix)]
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

#[cfg(windows)]
mod windows;

#[cfg(unix)]
const CODESIGN: &str = "/usr/bin/codesign";

/// How long one `codesign` run may take before it is stopped. An idle Mac
/// checks a 300 MB CLI in about a second; the margin is for a loaded one.
/// (Windows has its own limit for its one run over all files.)
#[cfg(unix)]
pub(crate) const CODESIGN_TIMEOUT: Duration = Duration::from_secs(60);

/// Tries for a check that could not be completed. A definite result is never
/// retried.
pub(crate) const CHECK_ATTEMPTS: u32 = 3;

/// The pause before the second try; it grows by this much for each later one.
pub(crate) const RETRY_PAUSE: Duration = Duration::from_millis(500);

/// How often a running `codesign` is looked at for the time limit.
#[cfg(unix)]
const POLL: Duration = Duration::from_millis(20);

/// Thin and universal Mach-O magic numbers, in both byte orders. `CAFEBABE` is
/// shared with Java class files; those are treated as Mach-O and fail the
/// signature check, which is the safe direction. (Windows looks for PE files
/// instead: `platform::is_native_binary`.)
#[cfg(unix)]
const MACHO_MAGIC: [[u8; 4]; 8] = [
    [0xfe, 0xed, 0xfa, 0xce],
    [0xce, 0xfa, 0xed, 0xfe],
    [0xfe, 0xed, 0xfa, 0xcf],
    [0xcf, 0xfa, 0xed, 0xfe],
    [0xca, 0xfe, 0xba, 0xbe],
    [0xbe, 0xba, 0xfe, 0xca],
    [0xca, 0xfe, 0xba, 0xbf],
    [0xbf, 0xba, 0xfe, 0xca],
];

/// Whether `file` is Mach-O, judged by its content and never by its name.
#[cfg(unix)]
pub fn is_macho(file: &Path) -> io::Result<bool> {
    let mut magic = [0_u8; 4];
    match File::open(file)?.read_exact(&mut magic) {
        Ok(()) => Ok(MACHO_MAGIC.contains(&magic)),
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(err) => Err(err),
    }
}

/// Why a signature check did not pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The file was checked and does not pass: unsigned, modified, or signed
    /// by another team.
    Invalid(String),
    /// The check did not finish: it timed out, could not run, was stopped, or
    /// gave no verdict. Says nothing about the file itself.
    Incomplete(String),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(reason) | Self::Incomplete(reason) => f.write_str(reason),
        }
    }
}

/// Verifies that a file is validly signed by a team.
///
/// `team` names the signer: on macOS the Apple Team ID, on Windows the
/// publisher (see [`crate::Tool::team_id`]).
pub trait Signature {
    /// Check the signature. `team` of `None` skips the team comparison.
    fn verify(&self, file: &Path, team: Option<&str>) -> Result<(), String>;

    /// The same check, telling a definite failure from one that could not be
    /// completed. Every `verify` failure is taken as definite.
    fn check(&self, file: &Path, team: Option<&str>) -> Result<(), Failure> {
        self.verify(file, team).map_err(Failure::Invalid)
    }

    /// Check several files, each with its own team; the first failure is the
    /// answer. By default one [`Signature::check`] after the other, in order.
    /// The Windows checker does them all in one go (each start of its
    /// checker costs about two seconds), so a wrapper around a checker
    /// should pass this call, and [`Signature::checks_together`], on too.
    fn check_all(&self, files: &[(&Path, Option<&str>)]) -> Result<(), Failure> {
        for (file, team) in files {
            self.check(file, *team)?;
        }
        Ok(())
    }

    /// Whether [`Signature::check_all`] checks its files together, in one run
    /// of the checker. Then a check that could not be completed is retried for
    /// all of them; otherwise (the default, and macOS) each file is checked,
    /// and retried, on its own.
    fn checks_together(&self) -> bool {
        false
    }
}

/// The system's signature check: `codesign` on macOS, Authenticode on Windows.
#[derive(Debug, Clone, Copy, Default)]
pub struct Codesign;

#[cfg(unix)]
fn team_identifier(details: &str) -> Option<&str> {
    details
        .lines()
        .find_map(|line| line.strip_prefix("TeamIdentifier="))
}

/// What one run of a tool printed on stderr, and how it ended.
#[cfg(unix)]
#[derive(Debug)]
struct Ran {
    status: ExitStatus,
    stderr: String,
}

/// Run `program` on `file` with `args`, stopping it if it runs past `timeout`.
/// A stopped child is killed and reaped before this returns.
#[cfg(unix)]
fn run_tool(
    program: &Path,
    args: &[&OsStr],
    file: &Path,
    timeout: Duration,
) -> Result<Ran, Failure> {
    let mut child = Command::new(program)
        .args(args)
        .arg(file)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| {
            Failure::Incomplete(format!("could not run {}: {err}", program.display()))
        })?;
    // Read on its own thread, so a full pipe can never hold the child up.
    let stderr = child.stderr.take();
    let reader = thread::spawn(move || {
        let mut text = Vec::new();
        if let Some(mut pipe) = stderr {
            let _ = pipe.read_to_end(&mut text);
        }
        text
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL),
            Ok(None) => {
                stop(&mut child);
                return Err(Failure::Incomplete(format!(
                    "{} did not finish within {} seconds for {} and was stopped",
                    program.display(),
                    timeout.as_secs(),
                    file.display()
                )));
            }
            Err(err) => {
                stop(&mut child);
                return Err(Failure::Incomplete(format!(
                    "could not wait for {}: {err}",
                    program.display()
                )));
            }
        }
    };
    let text = reader.join().unwrap_or_default();
    Ok(Ran {
        status,
        stderr: String::from_utf8_lossy(&text).into_owned(),
    })
}

#[cfg(unix)]
fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// The first line of a tool's stderr, short enough for a screen.
#[cfg(unix)]
fn first_line(stderr: &str) -> String {
    let line = stderr.lines().map(str::trim).find(|line| !line.is_empty());
    line.unwrap_or_default().chars().take(240).collect()
}

/// Whether codesign's own words say the signature is bad, as opposed to the
/// tool failing for some other reason.
#[cfg(unix)]
fn says_invalid(stderr: &str) -> bool {
    let text = stderr.to_ascii_lowercase();
    [
        "invalid signature",
        "not signed",
        "sealed resource",
        "modified",
        "requirement",
        "strict validation",
    ]
    .iter()
    .any(|phrase| text.contains(phrase))
}

/// Read codesign's `--verify` result. Only exit status zero passes; a non-zero
/// status with codesign's own words about the signature is a definite failure,
/// and any other non-zero status (a missing file, a busy resource) is one that
/// could not be completed.
#[cfg(unix)]
fn verdict(ran: &Ran, file: &Path) -> Result<(), Failure> {
    if ran.status.success() {
        return Ok(());
    }
    let Some(code) = ran.status.code() else {
        return Err(Failure::Incomplete(format!(
            "codesign was stopped ({}) while checking {}",
            ran.status,
            file.display()
        )));
    };
    let detail = first_line(&ran.stderr);
    if says_invalid(&ran.stderr) {
        Err(Failure::Invalid(format!(
            "invalid signature: {}: {detail}",
            file.display()
        )))
    } else {
        Err(Failure::Incomplete(format!(
            "codesign exited with status {code} while checking {}: {detail}",
            file.display()
        )))
    }
}

/// Authenticode: the file's status must be `Valid`, then the signer must be
/// the publisher `team` names (see `signature/windows.rs`). Every file is
/// checked in one run, so a retry repeats them all.
#[cfg(windows)]
impl Signature for Codesign {
    fn verify(&self, file: &Path, team: Option<&str>) -> Result<(), String> {
        self.check(file, team)
            .map_err(|failure| failure.to_string())
    }

    fn check(&self, file: &Path, team: Option<&str>) -> Result<(), Failure> {
        windows::check_all(&[(file, team)])
    }

    fn check_all(&self, files: &[(&Path, Option<&str>)]) -> Result<(), Failure> {
        windows::check_all(files)
    }

    fn checks_together(&self) -> bool {
        true
    }
}

#[cfg(unix)]
impl Signature for Codesign {
    fn verify(&self, file: &Path, team: Option<&str>) -> Result<(), String> {
        self.check(file, team)
            .map_err(|failure| failure.to_string())
    }

    fn check(&self, file: &Path, team: Option<&str>) -> Result<(), Failure> {
        let codesign = Path::new(CODESIGN);
        let ran = run_tool(
            codesign,
            &[OsStr::new("--verify"), OsStr::new("--strict")],
            file,
            CODESIGN_TIMEOUT,
        )?;
        verdict(&ran, file)?;
        let Some(expected) = team else {
            return Ok(());
        };
        // codesign prints the details on stderr.
        let ran = run_tool(codesign, &[OsStr::new("-dv")], file, CODESIGN_TIMEOUT)?;
        if !ran.status.success() {
            return Err(Failure::Incomplete(format!(
                "codesign could not read the details of {} ({}): {}",
                file.display(),
                ran.status,
                first_line(&ran.stderr)
            )));
        }
        match team_identifier(&ran.stderr) {
            Some(actual) if actual == expected => Ok(()),
            Some(actual) => Err(Failure::Invalid(format!(
                "wrong signer: expected team {expected}, found {actual}"
            ))),
            None => Err(Failure::Invalid(format!(
                "no team identifier: {}",
                file.display()
            ))),
        }
    }
}

/// Run `check` until it gives a definite answer. An incomplete check is tried
/// again after a growing pause, up to `attempts` times in all; a definite
/// failure is returned at once.
pub(crate) fn check_with_retries(
    signature: &impl Signature,
    file: &Path,
    team: Option<&str>,
    attempts: u32,
    pause: Duration,
) -> Result<(), Failure> {
    retrying(|| signature.check(file, team), attempts, pause)
}

/// [`check_with_retries`] for several files: each on its own, in order, or,
/// for a checker that checks them together ([`Signature::checks_together`]),
/// all of them in each try.
pub(crate) fn check_all_with_retries(
    signature: &impl Signature,
    files: &[(&Path, Option<&str>)],
    attempts: u32,
    pause: Duration,
) -> Result<(), Failure> {
    if signature.checks_together() {
        return retrying(|| signature.check_all(files), attempts, pause);
    }
    for (file, team) in files {
        check_with_retries(signature, file, *team, attempts, pause)?;
    }
    Ok(())
}

/// Run `attempt` until it gives a definite answer, as [`check_with_retries`]
/// describes.
fn retrying(
    mut attempt: impl FnMut() -> Result<(), Failure>,
    attempts: u32,
    pause: Duration,
) -> Result<(), Failure> {
    let mut tries = 0_u32;
    loop {
        tries += 1;
        match attempt() {
            Err(Failure::Incomplete(reason)) => {
                if tries >= attempts {
                    return Err(Failure::Incomplete(format!(
                        "{reason} (after {tries} tries)"
                    )));
                }
                thread::sleep(pause * tries);
            }
            done => return done,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::cell::Cell;
    #[cfg(unix)]
    use std::process::Command as Shell;

    #[cfg(unix)]
    #[test]
    fn the_team_is_read_from_codesign_details() {
        let details = "Identifier=codex\nFormat=Mach-O\nTeamIdentifier=2DC432GLL2\nSealed=x\n";
        assert_eq!(team_identifier(details), Some("2DC432GLL2"));
        assert_eq!(team_identifier("Identifier=x\n"), None);
    }

    #[cfg(unix)]
    #[test]
    fn mach_o_is_recognised_by_content_alone() {
        let path = std::env::temp_dir().join(format!("mg-magic-{}.txt", std::process::id()));
        for (bytes, expected) in [
            (&[0xcf, 0xfa, 0xed, 0xfe, 0x0c][..], true),
            (&[0xca, 0xfe, 0xba, 0xbe][..], true),
            (b"#!/bin/sh\n".as_slice(), false),
            (&[0xcf, 0xfa][..], false),
            (&[][..], false),
        ] {
            assert!(std::fs::write(&path, bytes).is_ok());
            assert_eq!(is_macho(&path).ok(), Some(expected), "{bytes:?}");
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_unsigned_file_is_rejected() {
        let path = std::env::temp_dir().join(format!("mg-unsigned-{}", std::process::id()));
        let _ = std::fs::write(&path, b"#!/bin/sh\n");
        assert!(Codesign.verify(&path, None).is_err());
        let _ = std::fs::remove_file(&path);
    }

    /// A child process that exits with `script`, as a real run would.
    #[cfg(unix)]
    fn ran(script: &str) -> Ran {
        let status = Shell::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .status()
            .unwrap();
        Ran {
            status,
            stderr: String::new(),
        }
    }

    #[cfg(unix)]
    fn ran_with(script: &str, stderr: &str) -> Ran {
        Ran {
            stderr: stderr.to_owned(),
            ..ran(script)
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_zero_status_passes_and_codesigns_own_words_are_a_definite_failure() {
        let file = Path::new("/x/claude");
        assert!(verdict(&ran("exit 0"), file).is_ok());
        let tampered = ran_with(
            "exit 1",
            "/x/claude: invalid signature (code or signature have been modified)\n",
        );
        assert!(matches!(verdict(&tampered, file), Err(Failure::Invalid(_))));
        let unsigned = ran_with("exit 1", "/x/claude: code object is not signed at all\n");
        assert!(matches!(verdict(&unsigned, file), Err(Failure::Invalid(_))));
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_file_or_a_stopped_tool_is_not_called_an_invalid_signature() {
        let file = Path::new("/x/claude");
        let missing = ran_with("exit 1", "/x/claude: No such file or directory\n");
        let failure = verdict(&missing, file).unwrap_err();
        assert!(matches!(failure, Failure::Incomplete(_)), "{failure:?}");
        assert!(failure.to_string().contains("No such file"), "{failure}");
        let killed = Ran {
            status: Shell::new("/bin/sh")
                .arg("-c")
                .arg("kill -9 $$")
                .status()
                .unwrap(),
            stderr: String::new(),
        };
        assert!(matches!(
            verdict(&killed, file),
            Err(Failure::Incomplete(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_child_past_its_time_limit_is_stopped_and_reported_as_incomplete() {
        let started = Instant::now();
        // `exec` makes the shell itself the sleeper, so the kill reaches it.
        let result = run_tool(
            Path::new("/bin/sh"),
            &[OsStr::new("-c"), OsStr::new("exec sleep 30")],
            Path::new("/x/claude"),
            Duration::from_millis(200),
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        match result {
            Err(Failure::Incomplete(reason)) => {
                assert!(reason.contains("did not finish"), "{reason}");
            }
            other => panic!("expected a timeout, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn stderr_of_a_run_is_captured_with_its_status() {
        let ran = run_tool(
            Path::new("/bin/sh"),
            &[OsStr::new("-c"), OsStr::new("echo 'boom here' >&2; exit 3")],
            Path::new("ignored"),
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(ran.status.code(), Some(3));
        assert_eq!(first_line(&ran.stderr), "boom here");
    }

    /// Fails with `results` in turn, then passes. Counts its calls.
    struct Scripted {
        results: Vec<Failure>,
        calls: Cell<u32>,
    }

    impl Signature for Scripted {
        fn verify(&self, _file: &Path, _team: Option<&str>) -> Result<(), String> {
            unreachable!("check is overridden")
        }
        fn check(&self, _file: &Path, _team: Option<&str>) -> Result<(), Failure> {
            let call = self.calls.get();
            self.calls.set(call + 1);
            match self.results.get(call as usize) {
                Some(failure) => Err(failure.clone()),
                None => Ok(()),
            }
        }
    }

    fn incomplete(text: &str) -> Failure {
        Failure::Incomplete(text.to_owned())
    }

    #[test]
    fn a_definite_invalid_signature_fails_once_without_a_retry() {
        let signature = Scripted {
            results: vec![Failure::Invalid("invalid signature".to_owned()); 5],
            calls: Cell::new(0),
        };
        let result = check_with_retries(&signature, Path::new("/x"), None, 3, Duration::ZERO);
        assert_eq!(
            result,
            Err(Failure::Invalid("invalid signature".to_owned()))
        );
        assert_eq!(signature.calls.get(), 1);
    }

    #[test]
    fn an_incomplete_check_that_then_completes_succeeds() {
        let signature = Scripted {
            results: vec![incomplete("busy"), incomplete("busy")],
            calls: Cell::new(0),
        };
        let result = check_with_retries(&signature, Path::new("/x"), None, 3, Duration::ZERO);
        assert_eq!(result, Ok(()));
        assert_eq!(signature.calls.get(), 3);
    }

    #[test]
    fn an_incomplete_check_that_keeps_failing_says_why_after_the_last_try() {
        let signature = Scripted {
            results: vec![incomplete("codesign did not finish"); 10],
            calls: Cell::new(0),
        };
        let result = check_with_retries(&signature, Path::new("/x"), None, 3, Duration::ZERO);
        assert_eq!(signature.calls.get(), 3);
        match result {
            Err(Failure::Incomplete(reason)) => {
                assert!(reason.contains("codesign did not finish"), "{reason}");
                assert!(reason.contains("3 tries"), "{reason}");
            }
            other => panic!("expected incomplete, got {other:?}"),
        }
    }

    /// The default `check_all` is `check` in order, stopping at the first
    /// refusal (so a checker that only has `verify` keeps its behaviour).
    #[test]
    fn check_all_checks_in_order_and_stops_at_the_first_refusal() {
        struct Recording(std::cell::RefCell<Vec<String>>);
        impl Signature for Recording {
            fn verify(&self, file: &Path, team: Option<&str>) -> Result<(), String> {
                let name = file.display().to_string();
                self.0
                    .borrow_mut()
                    .push(format!("{name}:{}", team.unwrap_or("-")));
                if name == "bad" { Err(name) } else { Ok(()) }
            }
        }
        let checker = Recording(std::cell::RefCell::default());
        let result = checker.check_all(&[
            (Path::new("a"), Some("T")),
            (Path::new("b"), None),
            (Path::new("bad"), Some("T")),
            (Path::new("never"), Some("T")),
        ]);
        assert_eq!(result, Err(Failure::Invalid("bad".to_owned())));
        assert_eq!(checker.0.into_inner(), ["a:T", "b:-", "bad:T"]);
        assert_eq!(checker_with_nothing().check_all(&[]), Ok(()));
        assert!(!checker_with_nothing().checks_together());
    }

    /// Checked one by one, each file is retried on its own; checked together,
    /// an unfinished check repeats every file.
    #[test]
    fn retries_are_per_file_unless_the_checker_checks_files_together() {
        struct Counting {
            together: bool,
            unfinished_first: Cell<u32>,
            seen: std::cell::RefCell<Vec<String>>,
        }
        impl Signature for Counting {
            fn verify(&self, _: &Path, _: Option<&str>) -> Result<(), String> {
                unreachable!("check is overridden")
            }
            fn check(&self, file: &Path, _: Option<&str>) -> Result<(), Failure> {
                self.seen.borrow_mut().push(file.display().to_string());
                if file == Path::new("b") && self.unfinished_first.get() > 0 {
                    self.unfinished_first.set(self.unfinished_first.get() - 1);
                    return Err(Failure::Incomplete("busy".to_owned()));
                }
                Ok(())
            }
            fn checks_together(&self) -> bool {
                self.together
            }
        }
        let files = [(Path::new("a"), None), (Path::new("b"), None)];
        for (together, expected) in [
            (false, vec!["a", "b", "b"]),
            (true, vec!["a", "b", "a", "b"]),
        ] {
            let checker = Counting {
                together,
                unfinished_first: Cell::new(1),
                seen: std::cell::RefCell::default(),
            };
            let result = check_all_with_retries(&checker, &files, 3, Duration::ZERO);
            assert_eq!(result, Ok(()));
            assert_eq!(checker.seen.into_inner(), expected, "together: {together}");
        }
    }

    fn checker_with_nothing() -> impl Signature {
        struct Refuses;
        impl Signature for Refuses {
            fn verify(&self, _: &Path, _: Option<&str>) -> Result<(), String> {
                Err("refused".to_owned())
            }
        }
        Refuses
    }
}
