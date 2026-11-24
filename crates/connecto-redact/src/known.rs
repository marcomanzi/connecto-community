//! The shapes an exact secret takes once a program has encoded it.
//!
//! A secret Connecto holds (a vault value) is redacted by its exact text, and
//! also as it reads after the common encodings a command line applies on its
//! own: base64 (standard and URL-safe, at each of the three byte offsets it
//! can start at inside a longer input), hex in both cases, and percent
//! encoding. That stops a careless `base64`, `xxd -p` or URL from showing it;
//! a program set on hiding a value can always encode it some other way, which
//! is why Connecto's own design keeps vault values out of terminals.

/// The shortest form worth hiding: a shorter one would match ordinary text.
pub const MIN_KNOWN: usize = 8;

const STANDARD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL_SAFE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// `value` and its encoded forms, each at least [`MIN_KNOWN`] long, without
/// repeats. A value shorter than [`MIN_KNOWN`] gives nothing.
#[must_use]
pub fn known_forms(value: &str) -> Vec<String> {
    if value.len() < MIN_KNOWN {
        return Vec::new();
    }
    let bytes = value.as_bytes();
    let mut forms = vec![value.to_owned()];
    for alphabet in [STANDARD, URL_SAFE] {
        for offset in 0..3 {
            forms.push(base64_core(bytes, offset, alphabet));
        }
    }
    forms.push(hex(bytes, b"0123456789abcdef"));
    forms.push(hex(bytes, b"0123456789ABCDEF"));
    forms.push(percent(bytes));
    let mut out: Vec<String> = Vec::with_capacity(forms.len());
    for form in forms {
        if form.len() >= MIN_KNOWN && !out.contains(&form) {
            out.push(form);
        }
    }
    out
}

/// Every form of every value, longest first, so a long form is replaced
/// before a shorter one inside it.
#[must_use]
pub fn known_set<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for value in values {
        for form in known_forms(value) {
            if !out.contains(&form) {
                out.push(form);
            }
        }
    }
    out.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    out
}

/// The base64 characters that depend only on `bytes`, when `bytes` starts
/// `offset` bytes into the encoded input (0, 1 or 2) and anything may follow:
/// the characters that mix in a neighbouring byte, and padding, are left out.
fn base64_core(bytes: &[u8], offset: usize, alphabet: &[u8; 64]) -> String {
    let mut input = vec![0_u8; offset];
    input.extend_from_slice(bytes);
    let encoded = base64(&input, alphabet);
    // The first character made only of `bytes` begins at bit offset*8.
    let start = (offset * 8).div_ceil(6);
    // The last one ends at or before the end of `bytes`.
    let end = (input.len() * 8) / 6;
    encoded
        .get(start..end.min(encoded.len()))
        .unwrap_or_default()
        .to_owned()
}

fn base64(input: &[u8], alphabet: &[u8; 64]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = chunk.len() + 1;
        for i in 0..chars {
            let index = (n >> (18 - 6 * i)) & 63;
            out.push(char::from(alphabet[index as usize]));
        }
    }
    out
}

fn hex(bytes: &[u8], digits: &[u8; 16]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(digits[usize::from(byte >> 4)]));
        out.push(char::from(digits[usize::from(byte & 15)]));
    }
    out
}

/// Percent encoding of everything outside the unreserved set (RFC 3986).
fn percent(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for byte in bytes {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(*byte));
        } else {
            out.push('%');
            out.push(char::from(b"0123456789ABCDEF"[usize::from(byte >> 4)]));
            out.push(char::from(b"0123456789ABCDEF"[usize::from(byte & 15)]));
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const SECRET: &str = "s3cr3t/value+with=symbols";

    #[test]
    fn a_short_value_gives_nothing() {
        assert!(known_forms("short").is_empty());
    }

    #[test]
    fn base64_matches_a_reference_encoding() {
        assert_eq!(base64(b"hello world", STANDARD), "aGVsbG8gd29ybGQ");
        assert_eq!(base64(b"\xfb\xff", URL_SAFE), "-_8");
    }

    #[test]
    fn the_value_is_found_base64_encoded_at_every_offset_inside_other_text() {
        let forms = known_forms(SECRET);
        for prefix in ["", "a", "ab", "abc", "abcd"] {
            for suffix in ["", "z", "zz"] {
                let input = format!("{prefix}{SECRET}{suffix}");
                for alphabet in [STANDARD, URL_SAFE] {
                    let encoded = base64(input.as_bytes(), alphabet);
                    assert!(
                        forms.iter().any(|form| encoded.contains(form.as_str())),
                        "not found in base64 of {input:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn hex_and_percent_forms_are_included() {
        let forms = known_forms(SECRET);
        assert!(forms.contains(&hex(SECRET.as_bytes(), b"0123456789abcdef")));
        assert!(forms.contains(&hex(SECRET.as_bytes(), b"0123456789ABCDEF")));
        assert!(forms.contains(&"s3cr3t%2Fvalue%2Bwith%3Dsymbols".to_owned()));
    }

    #[test]
    fn redacting_with_the_set_hides_every_form() {
        let known = known_set([SECRET]);
        let encoded = base64(format!("x{SECRET}").as_bytes(), STANDARD);
        let text = format!(
            "raw {SECRET} b64 {encoded} hex {} url {}",
            hex(SECRET.as_bytes(), b"0123456789abcdef"),
            percent(SECRET.as_bytes()),
        );
        let out = crate::redact(&text, &known).unwrap().get().clone();
        assert!(!out.contains(SECRET), "{out}");
        assert!(!out.contains(&known_forms(SECRET)[1]), "{out}");
        assert!(!out.contains("733363723374"), "{out}");
        assert!(!out.contains("s3cr3t%2F"), "{out}");
    }

    #[test]
    fn the_set_is_longest_first_without_repeats() {
        let set = known_set([SECRET, SECRET, "another-long-secret"]);
        assert!(set.windows(2).all(|pair| pair[0].len() >= pair[1].len()));
        let mut unique = set.clone();
        unique.dedup();
        assert_eq!(unique.len(), set.len());
    }
}
