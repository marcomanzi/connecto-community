//! What must happen to text before it goes anywhere: sanitizing and redaction.
//!
//! Redaction runs before delivery, audit and evidence. Order: exact known
//! values, token patterns, values given to a name such as `password`, then
//! runs that look random. If it cannot run, the content is withheld. This is a best effort over known shapes, not a proof of absence.

use std::sync::{Arc, OnceLock, PoisonError, RwLock};

use connecto_core::Redacted;
use regex::Regex;

mod known;
pub use known::{MIN_KNOWN, known_forms, known_set};

/// Largest body after redaction. Longer is denied, never truncated.
pub const MAX_BODY: usize = 4096;

const MARK: &str = "[REDACTED]";
const MIN_ENTROPY_TOKEN: usize = 32;
/// Bits per character. Hex tops out at 4.0, so hashes pass; random base64 does not.
const ENTROPY_LIMIT: f64 = 4.2;
/// The shortest value a `password=` style name hides.
const MIN_NAMED_VALUE: usize = 8;
/// How many separate groups of digits make a one-case run of letters and
/// digits look random (a base32 key has about five in 32 characters; a long
/// name with a year or a number in it has one or two).
const MIN_DIGIT_GROUPS: usize = 3;

/// Why text cannot be sent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RedactError {
    #[error("content withheld: redaction could not run")]
    Withheld,
    #[error("the message is {0} bytes after redaction; the limit is 4096")]
    TooLong(usize),
    #[error("the message is empty")]
    Empty,
}

/// Remove what could steer the terminal: ESC, C0 except newline and tab, DEL
/// and C1. Line endings become `\n`. So a body cannot contain `ESC[201~` and
/// end a bracketed paste early.
#[must_use]
pub fn sanitize(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter(|ch| {
            let code = u32::from(*ch);
            *ch == '\n' || *ch == '\t' || !(code < 0x20 || (0x7f..=0x9f).contains(&code))
        })
        .collect()
}

/// Each known shape and what replaces it (`$1` keeps a captured part).
fn patterns() -> Option<&'static [(Regex, &'static str)]> {
    static COMPILED: OnceLock<Option<Vec<(Regex, &'static str)>>> = OnceLock::new();
    COMPILED
        .get_or_init(|| {
            [
                (
                    r"(?s)-----BEGIN [A-Z0-9 ]+-----.*?-----END [A-Z0-9 ]+-----",
                    MARK,
                ),
                (r"(?i)bearer\s+[A-Za-z0-9._~+/=-]{8,}", MARK),
                (
                    r"eyJ[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{5,}",
                    MARK,
                ),
                (r"sk-[A-Za-z0-9_-]{8,}", MARK),
                (r"gh[pousr]_[A-Za-z0-9]{20,}", MARK),
                (r"github_pat_[A-Za-z0-9_]{20,}", MARK),
                (r"glpat-[A-Za-z0-9_-]{20,}", MARK),
                (r"xox[abprs]-[A-Za-z0-9-]{10,}", MARK),
                (r"AIza[0-9A-Za-z_-]{35}", MARK),
                (r"npm_[A-Za-z0-9]{36}", MARK),
                // A Together AI key (Claude through TogetherLink, Pi).
                (r"tgp_v1_[A-Za-z0-9_-]{16,}", MARK),
                (r"AKIA[0-9A-Z]{16}", MARK),
                // A Discord token: the user id, a time and a signature in
                // base64, which always begins with M, N or O.
                (
                    r"\b[MNO][A-Za-z0-9_-]{23,27}\.[A-Za-z0-9_-]{6}\.[A-Za-z0-9_-]{27,}",
                    MARK,
                ),
                // The password in `scheme://user:password@host`.
                (
                    r"([A-Za-z][A-Za-z0-9+.-]*://[^\s/:@]+:)[^\s/@]+(@)",
                    "${1}[REDACTED]${2}",
                ),
            ]
            .iter()
            .map(|(pattern, with)| Regex::new(pattern).ok().map(|regex| (regex, *with)))
            .collect()
        })
        .as_deref()
}

/// `password=...`, `"api_key": "..."`, `TOKEN: ...`: a name that says the
/// value is a secret, the separator right after it, and the value.
fn named_pattern() -> Option<&'static Regex> {
    static COMPILED: OnceLock<Option<Regex>> = OnceLock::new();
    COMPILED
        .get_or_init(|| {
            Regex::new(
                r#"(?i)((?:password|passwd|secret|token|api[_-]?key)(?:[_-][A-Za-z0-9_-]*)?["']?\s*[:=]\s*["']?)([^\s"',;=][^\s"',;]*)"#,
            )
            .ok()
        })
        .as_ref()
}

/// Whether what follows a secret's name is a secret and not code or prose:
/// long enough, not a call, a placeholder, a path of names or a URL, and with
/// a digit or a symbol in it (`token: undefined` and `secret = other_name`
/// stay readable).
fn named_value_is_secret(value: &str) -> bool {
    value.chars().count() >= MIN_NAMED_VALUE
        && !value.contains(['(', ')', '{', '}', '<', '>', '[', ']', '$'])
        && !value.contains("::")
        && !value.contains("://")
        && value
            .chars()
            .any(|ch| !(ch.is_ascii_alphabetic() || matches!(ch, '_' | '.' | '-')))
}

fn token_pattern() -> Option<&'static Regex> {
    static COMPILED: OnceLock<Option<Regex>> = OnceLock::new();
    COMPILED
        .get_or_init(|| Regex::new(r"[A-Za-z0-9+/=_-]{32,}").ok())
        .as_ref()
}

fn entropy(token: &str) -> f64 {
    let mut counts = [0_u32; 256];
    for byte in token.bytes() {
        counts[usize::from(byte)] += 1;
    }
    let total = token.len() as f64;
    counts
        .iter()
        .filter(|count| **count > 0)
        .map(|count| {
            let share = f64::from(*count) / total;
            -share * share.log2()
        })
        .sum()
}

/// Whether a run reads as a random key although its entropy is under the
/// limit: one case of letters with digits scattered through it, as base32 and
/// base36 keys are. Hex is left alone on purpose (hashes), and so is anything
/// with `_`, `-` or `/` in it (names and paths) or with both cases.
fn looks_random(token: &str) -> bool {
    let body = token.trim_end_matches('=');
    let one_case = body
        .bytes()
        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        || body
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if !one_case || body.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    let mut groups = 0;
    let mut in_digits = false;
    for byte in body.bytes() {
        let digit = byte.is_ascii_digit();
        if digit && !in_digits {
            groups += 1;
        }
        in_digits = digit;
    }
    groups >= MIN_DIGIT_GROUPS
}

/// Where the app's vault values come from: every form of every value to
/// hide (see [`known_set`]), longest first.
pub type KnownSource = Arc<dyn Fn() -> Arc<[String]> + Send + Sync>;

static KNOWN_SOURCE: RwLock<Option<KnownSource>> = RwLock::new(None);

/// Register the vault, once, at startup. From then on every [`redact`],
/// [`prepare`] and [`hide_known`] also removes the vault's values, wherever it
/// is called from.
pub fn set_known_source(source: KnownSource) {
    *KNOWN_SOURCE.write().unwrap_or_else(PoisonError::into_inner) = Some(source);
}

/// The vault's forms to hide now (empty without a vault).
#[must_use]
pub fn vault_known() -> Arc<[String]> {
    let source = KNOWN_SOURCE
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    source.map_or_else(|| Arc::from(Vec::new()), |source| source())
}

/// Replace only the vault's values, and nothing that merely looks secret:
/// for whole screens shown as they are (`connecto read`), where the shape
/// rules would hide too much.
#[must_use]
pub fn hide_known(text: &str) -> String {
    replace_known(text.to_owned(), &vault_known())
}

fn replace_known(mut out: String, known: &[String]) -> String {
    for value in known.iter().filter(|value| !value.is_empty()) {
        if out.contains(value.as_str()) {
            out = out.replace(value.as_str(), MARK);
        }
    }
    out
}

/// Replace secrets. `known` holds exact values to remove first; the vault's
/// values (see [`set_known_source`]) are removed with them.
pub fn redact(text: &str, known: &[String]) -> Result<Redacted<String>, RedactError> {
    let out = replace_known(text.to_owned(), known);
    let mut out = replace_known(out, &vault_known());
    for (pattern, with) in patterns().ok_or(RedactError::Withheld)? {
        out = pattern.replace_all(&out, *with).into_owned();
    }
    let named = named_pattern().ok_or(RedactError::Withheld)?;
    let out = named.replace_all(&out, |found: &regex::Captures<'_>| {
        let part = |index| found.get(index).map_or("", |m| m.as_str());
        if named_value_is_secret(part(2)) {
            format!("{}{MARK}", part(1))
        } else {
            part(0).to_owned()
        }
    });
    let tokens = token_pattern().ok_or(RedactError::Withheld)?;
    let out = tokens
        .replace_all(&out, |found: &regex::Captures<'_>| {
            let token = found.get(0).map_or("", |m| m.as_str());
            if token.len() >= MIN_ENTROPY_TOKEN
                && (entropy(token) >= ENTROPY_LIMIT || looks_random(token))
            {
                MARK.to_owned()
            } else {
                token.to_owned()
            }
        })
        .into_owned();
    Ok(Redacted::assume(out))
}

/// Sanitize, redact and check the limit: the body exactly as it will be sent.
pub fn prepare(text: &str, known: &[String]) -> Result<Redacted<String>, RedactError> {
    let clean = sanitize(text);
    if clean.trim().is_empty() {
        return Err(RedactError::Empty);
    }
    let redacted = redact(&clean, known)?;
    let len = redacted.get().len();
    if len > MAX_BODY {
        return Err(RedactError::TooLong(len));
    }
    Ok(redacted)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn clean(text: &str) -> String {
        redact(text, &[]).unwrap().get().clone()
    }

    #[test]
    fn a_body_cannot_end_a_bracketed_paste() {
        assert_eq!(sanitize("a\x1b[201~b"), "a[201~b");
        assert!(!sanitize("\u{9b}201~").contains('\u{9b}'));
    }

    #[test]
    fn control_characters_go_and_newline_and_tab_stay() {
        assert_eq!(sanitize("a\x00\x07\x08b\tc\nd\x7f"), "ab\tc\nd");
        assert_eq!(sanitize("a\u{85}b"), "ab");
    }

    #[test]
    fn line_endings_are_normalized() {
        assert_eq!(sanitize("a\r\nb\rc"), "a\nb\nc");
    }

    #[test]
    fn unicode_text_survives() {
        assert_eq!(sanitize("héllo → 世界"), "héllo → 世界");
    }

    #[test]
    fn every_listed_token_shape_is_removed() {
        // Made-up tokens, each in pieces so secret scanners do not take this
        // test for a leak.
        for secret in [
            concat!("sk-", "abcdefgh12345678"),
            concat!("sk-ant-", "api03-abc_DEF-123456"),
            concat!("ghp_", "abcdefghijklmnopqrstuvwxyz0123456789"),
            concat!("AKIA", "IOSFODNN7EXAMPLE"),
            concat!("tgp_v1_", "abcdEFGH1234_ijkl-5678"),
            concat!("glpat-", "abcDEF123_ghiJKL-4567"),
            concat!("xoxb-", "123456789012-abcdefABCDEF"),
            concat!("xoxp-", "1234-5678-abcdef"),
            concat!(
                "github_pat_",
                "11ABCDEFG0abcdefghijkl_mnopqrstuvwxyzABCDEFGHIJ"
            ),
            concat!("AIza", "SyA1234567890abcdefghijklmnopqrstuv"),
            concat!("npm_", "abcdefghijklmnopqrstuvwxyz0123456789"),
            concat!(
                "MTIzNDU2Nzg5MDEyMzQ1Njc4",
                ".GabcDE.",
                "abcdefghijklmnopqrstuvwxyz_0123456789-AB"
            ),
            concat!(
                "eyJhbGciOiJIUzI1NiJ9",
                ".eyJzdWIiOiIxMjM0In0",
                ".abcDEF123_-x"
            ),
            "Bearer abc.def-ghi_jkl",
            "bearer abcdefghijkl",
            concat!(
                "-----BEGIN RSA PRIVATE",
                " KEY-----\nabc\ndef\n-----END RSA PRIVATE",
                " KEY-----"
            ),
        ] {
            let text = format!("before {secret} after");
            let out = clean(&text);
            assert_eq!(out, format!("before {MARK} after"), "{secret}");
        }
    }

    #[test]
    fn high_entropy_is_removed_and_ordinary_text_is_not() {
        assert!(clean(concat!("key=", "aB3xQ9zL2mN7vK4pR8tY1wE6uI0oP5sD")).contains(MARK));
        for keep in [
            "a plain sentence about the build",
            "d2d2f3a9c4e1b0f7a6d5c4b3a2e1f0d9c8b7a6e5",
            "11111111-2222-4333-8444-555555555555",
            "/usr/local/share/some/long/path/that/is/fine/really",
        ] {
            assert_eq!(clean(keep), keep);
        }
    }

    #[test]
    fn a_password_in_a_url_goes_and_the_rest_of_the_url_stays() {
        assert_eq!(
            clean("clone https://deploy:hunter2@git.example.com/org/repo.git now"),
            format!("clone https://deploy:{MARK}@git.example.com/org/repo.git now")
        );
        assert_eq!(
            clean("postgres://admin:p%40ss-w0rd@db.internal:5432/app"),
            format!("postgres://admin:{MARK}@db.internal:5432/app")
        );
        for keep in [
            "http://localhost:8080/path",
            "https://example.com:8443/a@b",
            "ssh://git@github.com/org/repo.git",
            "git@github.com:org/repo.git",
            "mail me at someone@example.com: thanks",
        ] {
            assert_eq!(clean(keep), keep);
        }
    }

    #[test]
    fn a_value_given_to_a_secret_name_goes_and_the_name_stays() {
        for (text, expected) in [
            ("password=hunter2hunter", "password=[REDACTED]"),
            ("DB_PASSWORD: Tr0ub4dor&3", "DB_PASSWORD: [REDACTED]"),
            (
                concat!(r#"{"client_secret": "#, r#""abc123def456"}"#),
                r#"{"client_secret": "[REDACTED]"}"#,
            ),
            ("export API_KEY='k9f8d7s6a5'", "export API_KEY='[REDACTED]'"),
            ("api-key = 12345678 # prod", "api-key = [REDACTED] # prod"),
            ("apikey:zz99yy88xx", "apikey:[REDACTED]"),
            (
                concat!("GITHUB_TOKEN=", "abcd1234efgh"),
                "GITHUB_TOKEN=[REDACTED]",
            ),
            ("token_v2: qwerty!uiop", "token_v2: [REDACTED]"),
        ] {
            assert_eq!(clean(text), expected, "{text}");
        }
    }

    #[test]
    fn prose_and_code_about_secrets_stay_readable() {
        for keep in [
            "the token is used for the session and the password is checked",
            "the token: it is short",
            "Secret: the meeting is at noon",
            "password: undefined",
            "token = response.access_token",
            "let token = self.get_token(user1)?;",
            "password: Option<String>,",
            "api_key=${API_KEY_2}",
            "token_url: https://example.com/oauth2/token",
            "secret: crate::auth::Secret2",
            "max_tokens=100000000",
            "tokenizer: cl100k_base",
            "if token == expected_token_1 {",
            "password=short1",
        ] {
            assert_eq!(clean(keep), keep);
        }
    }

    /// A stream of base32 keys from a fixed generator.
    fn base32_keys(count: usize, len: usize) -> Vec<String> {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        (0..count)
            .map(|_| {
                (0..len)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        char::from(ALPHABET[(state >> 32) as usize % ALPHABET.len()])
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn random_base32_keys_of_32_to_39_characters_are_removed() {
        assert_eq!(clean("JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP"), MARK);
        assert_eq!(
            clean("totp k7r2m9x4q1w8e5t3y6u0i2o7p4a9s1d5 ok"),
            format!("totp {MARK} ok")
        );
        for len in [32, 36, 39] {
            let keys = base32_keys(400, len);
            let caught = keys.iter().filter(|key| clean(key) == MARK).count();
            // Entropy alone caught about half of these.
            assert!(caught >= 380, "length {len}: {caught} of 400");
        }
    }

    #[test]
    fn long_names_hashes_paths_and_words_are_not_taken_for_keys() {
        for keep in [
            "this_is_a_very_long_function_name_for_testing_things_2",
            "ANOTHER_VERY_LONG_CONSTANT_NAME_FOR_VERSION_2_OF_3_THINGS",
            "supercalifragilisticexpialidociousandthensomemore",
            "releasenotesforrelease2andrelease3",
            "d2d2f3a9c4e1b0f7a6d5c4b3a2e1f0d9c8b7a6e5",
            "D2D2F3A9C4E1B0F7A6D5C4B3A2E1F0D9C8B7A6E5",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "11111111-2222-4333-8444-555555555555",
            "/usr/local/share/some/long/path/that/is/fine/really",
            "crates/connecto-session/src/project_settings/module_10/v2",
            "node_modules/some-package-name/dist/index-v2-min",
        ] {
            assert_eq!(clean(keep), keep);
        }
        // The digit rule by itself: a name with a number or a year in it, a
        // hash and a two-case run are not what it is for.
        for name in [
            "mycompanyproject2backupfolderarchive2024",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "SomeVeryLongCamelCaseIdentifier2NameFor3Testing4Things",
            "lowercase_with_separators_1_2_3_4_5_6",
        ] {
            assert!(!looks_random(name), "{name}");
        }
    }

    #[test]
    fn exact_known_values_go_first_whatever_they_look_like() {
        let out = redact("my pet is rex", &["rex".to_owned()]).unwrap();
        assert_eq!(out.get(), &format!("my pet is {MARK}"));
    }

    #[test]
    fn an_empty_known_value_is_ignored() {
        assert_eq!(redact("abc", &[String::new()]).unwrap().get(), "abc");
    }

    #[test]
    fn the_limit_applies_after_redaction_and_is_exact() {
        assert!(prepare(&"a ".repeat(2048), &[]).is_ok());
        let over = format!("{}x", "a ".repeat(2048));
        assert_eq!(prepare(&over, &[]), Err(RedactError::TooLong(4097)));
    }

    #[test]
    fn an_empty_body_is_refused() {
        assert_eq!(prepare(" \n\x1b ", &[]), Err(RedactError::Empty));
    }
}
