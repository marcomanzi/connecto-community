//! The environment a child starts with.
//!
//! Children start from an empty environment. Only the variables named here are
//! copied, so credentials in the parent's environment do not reach a session.

use std::ffi::{OsStr, OsString};

/// Parent variables a child may inherit.
#[cfg(unix)]
pub const ENV_ALLOWLIST: &[&str] = &[
    "HOME", "USER", "LOGNAME", "SHELL", "PATH", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE",
];

/// Parent variables a child may inherit.
///
/// Windows programs, PowerShell included, expect the system locations
/// (`SystemRoot`, `ComSpec`, the profile and program folders) to be set and
/// fail in odd ways without them. None of these carries a credential.
#[cfg(windows)]
pub const ENV_ALLOWLIST: &[&str] = &[
    "HOME",
    "USERPROFILE",
    "USERNAME",
    "USERDOMAIN",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "ProgramData",
    "ALLUSERSPROFILE",
    "PUBLIC",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramW6432",
    "CommonProgramFiles",
    "CommonProgramFiles(x86)",
    "CommonProgramW6432",
    "SystemRoot",
    "SystemDrive",
    "windir",
    "ComSpec",
    "PATH",
    "PATHEXT",
    "PSModulePath",
    "TEMP",
    "TMP",
    "COMPUTERNAME",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "OS",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
];

const TERM: &str = "xterm-256color";
const FALLBACK_LOCALE: &str = "en_US.UTF-8";

/// Build a child environment from `lookup`, normally [`std::env::var_os`].
#[must_use]
pub fn child_env(lookup: impl Fn(&OsStr) -> Option<OsString>) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = ENV_ALLOWLIST
        .iter()
        .filter_map(|name| {
            let name = OsString::from(name);
            lookup(&name).map(|value| (name, value))
        })
        .collect();
    env.push(("TERM".into(), TERM.into()));
    env.push(("COLORTERM".into(), "truecolor".into()));
    // Apps launched from Finder have no locale, and a child in the C locale
    // would emit mangled text for anything outside ASCII.
    let has_locale = env
        .iter()
        .any(|(name, _)| name == "LANG" || name == "LC_ALL" || name == "LC_CTYPE");
    if !has_locale {
        env.push(("LANG".into(), FALLBACK_LOCALE.into()));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value<'a>(env: &'a [(OsString, OsString)], name: &str) -> Option<&'a OsStr> {
        env.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_os_str())
    }

    #[test]
    fn only_allowlisted_parent_variables_survive() {
        let env = child_env(|name| Some(name.to_owned()));
        assert_eq!(value(&env, "HOME"), Some(OsStr::new("HOME")));
        assert_eq!(value(&env, "ANTHROPIC_API_KEY"), None);
        assert_eq!(value(&env, "OPENAI_API_KEY"), None);
        assert_eq!(value(&env, "SSH_AUTH_SOCK"), None);
    }

    #[test]
    fn terminal_identity_is_always_set() {
        let env = child_env(|_| None);
        assert_eq!(value(&env, "TERM"), Some(OsStr::new(TERM)));
        assert_eq!(value(&env, "COLORTERM"), Some(OsStr::new("truecolor")));
    }

    #[test]
    fn a_missing_locale_gets_a_utf8_default() {
        let env = child_env(|_| None);
        assert_eq!(value(&env, "LANG"), Some(OsStr::new(FALLBACK_LOCALE)));
    }

    #[test]
    fn an_inherited_locale_is_not_overridden() {
        let env = child_env(|name| (name == "LC_ALL").then(|| "de_DE.UTF-8".into()));
        assert_eq!(value(&env, "LANG"), None);
        assert_eq!(value(&env, "LC_ALL"), Some(OsStr::new("de_DE.UTF-8")));
    }
}
