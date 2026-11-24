//! What a binding may say. Checked when the person sets one and again when
//! the file is read, so a hand-edited file cannot widen what terminals can do.

use std::path::{Component, Path};

use connecto_core::Binding;

const MAX_URL: usize = 2048;
const MAX_HEADER: usize = 64;
const MAX_PREFIX: usize = 64;
const MAX_PROGRAMS: usize = 16;

/// Headers the proxy sets itself, or that change how a request is framed.
const RESERVED_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "upgrade",
    "te",
    "trailer",
    "keep-alive",
    "expect",
];

/// Programs that would run whatever they are told: given a secret, they give
/// it to anything. Matched on the file name, and on a version after it
/// (`python3.12`).
const INTERPRETERS: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "fish",
    "dash",
    "ksh",
    "csh",
    "tcsh",
    "nu",
    "pwsh",
    "powershell",
    "cmd",
    "env",
    "sudo",
    "doas",
    "su",
    "nohup",
    "nice",
    "time",
    "timeout",
    "xargs",
    "exec",
    "script",
    "busybox",
    "python",
    "perl",
    "ruby",
    "node",
    "deno",
    "bun",
    "php",
    "lua",
    "tclsh",
    "wish",
    "osascript",
    "awk",
    "gawk",
    "mawk",
    "nawk",
    "sed",
    "make",
    "npx",
    "npm",
    "pnpm",
    "yarn",
    "uv",
    "uvx",
    "pipx",
    "java",
    "jshell",
    "irb",
    "ghci",
    "R",
    "Rscript",
    "julia",
    "swift",
    "open",
    "launchctl",
];

/// Variables that change how any program starts, or that Connecto sets.
const RESERVED_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "SHELL",
    "USER",
    "LOGNAME",
    "TMPDIR",
    "TERM",
    "IFS",
    "ENV",
    "BASH_ENV",
    "PS4",
    "PROMPT_COMMAND",
    "NODE_OPTIONS",
    "PYTHONPATH",
    "PYTHONSTARTUP",
    "PERL5LIB",
    "PERL5OPT",
    "RUBYOPT",
    "RUBYLIB",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "EDITOR",
    "VISUAL",
    "PAGER",
    "LESSOPEN",
];
const RESERVED_ENV_PREFIXES: &[&str] = &["LD_", "DYLD_", "CONNECTO_", "MAGNETO_"];

/// Whether `binding` may be kept.
///
/// # Errors
/// What is wrong, for the person.
pub fn check_binding(binding: &Binding) -> Result<(), String> {
    match binding {
        Binding::Http {
            base_url,
            header,
            prefix,
        } => {
            check_url(base_url)?;
            check_header(header)?;
            check_prefix(prefix)
        }
        Binding::Command { programs, env } => {
            if programs.is_empty() {
                return Err("A command binding names at least one program.".to_owned());
            }
            if programs.len() > MAX_PROGRAMS {
                return Err(format!(
                    "A command binding names at most {MAX_PROGRAMS} programs."
                ));
            }
            for program in programs {
                check_program(program)?;
            }
            check_env(env)
        }
        Binding::AgenticWorks | Binding::GoogleServiceAccount | Binding::Ssh => Ok(()),
    }
}

/// `https://host[:port][/path]`, or `http://` to this machine only. No
/// user name, query or fragment: the proxy adds the path the terminal asks
/// for to it.
fn check_url(url: &str) -> Result<(), String> {
    let bad = |why: &str| Err(format!("The address {why}."));
    if url.len() > MAX_URL {
        return bad("is too long");
    }
    if url.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
        return bad("has a space or a control character in it");
    }
    let (scheme, rest) = url.split_once("://").ok_or("The address needs https://.")?;
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.is_empty() {
        return bad("has no host");
    }
    if authority.contains('@') {
        return bad("cannot carry a user name or password; that is what the secret is for");
    }
    if rest.contains(['?', '#']) {
        return bad("cannot have a query or a fragment");
    }
    let host = host_of(authority);
    if host.is_empty() {
        return bad("has no host");
    }
    match scheme {
        "https" => Ok(()),
        "http" if matches!(host, "127.0.0.1" | "localhost" | "[::1]") => Ok(()),
        "http" => bad("must use https (http only to this machine)"),
        _ => bad("must begin with https://"),
    }
}

/// The host part of `host[:port]` (an IPv6 literal keeps its brackets).
fn host_of(authority: &str) -> &str {
    if authority.starts_with('[') {
        return authority
            .find(']')
            .and_then(|end| authority.get(..=end))
            .unwrap_or(authority);
    }
    authority.split(':').next().unwrap_or(authority)
}

fn check_header(header: &str) -> Result<(), String> {
    if header.is_empty() || header.len() > MAX_HEADER {
        return Err(format!("A header name is 1 to {MAX_HEADER} characters."));
    }
    if !header
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err("A header name has only letters, digits and `-`.".to_owned());
    }
    let lower = header.to_ascii_lowercase();
    if RESERVED_HEADERS.contains(&lower.as_str()) || lower.starts_with("proxy-") {
        return Err(format!(
            "The header {header} is set by the connection itself."
        ));
    }
    Ok(())
}

fn check_prefix(prefix: &str) -> Result<(), String> {
    if prefix.len() > MAX_PREFIX {
        return Err(format!(
            "What goes before the secret is at most {MAX_PREFIX} characters."
        ));
    }
    if !prefix.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        return Err("What goes before the secret is plain printable text.".to_owned());
    }
    Ok(())
}

fn check_program(program: &str) -> Result<(), String> {
    let path = Path::new(program);
    if !path.is_absolute() {
        return Err(format!("{program} is not a full path to a program."));
    }
    if path
        .components()
        .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err(format!("{program} has `.` or `..` in it."));
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    let interpreter = INTERPRETERS.iter().any(|word| {
        name.strip_prefix(word)
            .is_some_and(|rest| rest.chars().all(|ch| ch.is_ascii_digit() || ch == '.'))
    });
    if interpreter {
        return Err(format!(
            "{name} runs whatever it is told, so it would hand the secret to anything. Name the program that needs the secret instead."
        ));
    }
    Ok(())
}

fn check_env(env: &str) -> Result<(), String> {
    let mut chars = env.chars();
    let starts = chars
        .next()
        .is_some_and(|ch| ch.is_ascii_uppercase() || ch == '_');
    if !starts
        || env.len() > 64
        || !chars.all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
    {
        return Err("A variable name is capitals, digits and `_`, at most 64.".to_owned());
    }
    if RESERVED_ENV.contains(&env) || RESERVED_ENV_PREFIXES.iter().any(|p| env.starts_with(p)) {
        return Err(format!(
            "{env} changes how programs start, so it cannot carry a secret."
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http(url: &str, header: &str, prefix: &str) -> Binding {
        Binding::Http {
            base_url: url.into(),
            header: header.into(),
            prefix: prefix.into(),
        }
    }

    fn command(program: &str, env: &str) -> Binding {
        Binding::Command {
            programs: vec![program.into()],
            env: env.into(),
        }
    }

    #[test]
    fn https_urls_pass_and_http_only_to_this_machine() {
        for good in [
            "https://api.github.com",
            "https://api.example.com:8443/v1",
            "http://127.0.0.1:9000",
            "http://localhost/x",
            "http://[::1]:80",
        ] {
            assert!(
                check_binding(&http(good, "Authorization", "Bearer ")).is_ok(),
                "{good}"
            );
        }
        for bad in [
            "api.github.com",
            "http://api.github.com",
            "ftp://x",
            "https://",
            "https://user:pw@host",
            "https://host/?q=1",
            "https://host/#f",
            "https://ho st",
            "http://127.0.0.1.evil.com",
            "http://localhost.evil.com",
        ] {
            assert!(
                check_binding(&http(bad, "Authorization", "")).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn framing_headers_and_odd_prefixes_are_refused() {
        for bad in [
            "Host",
            "content-length",
            "Proxy-Authorization",
            "X Y",
            "",
            "a:b",
        ] {
            assert!(check_binding(&http("https://h", bad, "")).is_err(), "{bad}");
        }
        assert!(check_binding(&http("https://h", "x-api-key", "")).is_ok());
        assert!(check_binding(&http("https://h", "Authorization", "Bearer\r\nX: y")).is_err());
    }

    #[test]
    fn interpreters_and_relative_programs_are_refused() {
        for bad in [
            "gh",
            "/bin/sh",
            "/bin/bash",
            "/usr/bin/env",
            "/usr/bin/python3",
            "/opt/homebrew/bin/python3.12",
            "/usr/local/bin/node",
            "/usr/bin/../bin/gh",
        ] {
            assert!(check_binding(&command(bad, "GH_TOKEN")).is_err(), "{bad}");
        }
        for good in [
            "/opt/homebrew/bin/gh",
            "/usr/local/bin/aws",
            "/usr/bin/ssh",
            "/opt/homebrew/bin/shellcheck",
        ] {
            assert!(check_binding(&command(good, "GH_TOKEN")).is_ok(), "{good}");
        }
    }

    #[test]
    fn variables_that_steer_a_start_are_refused() {
        for bad in [
            "PATH",
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "CONNECTO_X",
            "gh_token",
            "1A",
            "",
        ] {
            assert!(
                check_binding(&command("/opt/homebrew/bin/gh", bad)).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_command_binding_needs_a_program() {
        let empty = Binding::Command {
            programs: Vec::new(),
            env: "GH_TOKEN".into(),
        };
        assert!(check_binding(&empty).is_err());
    }
}
