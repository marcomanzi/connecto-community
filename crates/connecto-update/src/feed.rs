//! The update feed, `update.json`, and its signature.
//!
//! The file is `{"signed": "<the feed, as JSON text>", "signature": "<base64>"}`:
//! an ed25519 signature over the exact bytes of `signed`, so nothing has to be
//! re-encoded the same way to check it. The feed inside names the newest
//! release and, for each platform, the archive the updater installs, with its
//! size and SHA-256. The app holds only the public key; the release tool
//! (`connecto-update-feed`) holds the private one.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::version::Version;

/// The feed format this build reads and writes.
pub const FORMAT: u64 = 1;

/// Largest archive accepted (a release is about 16 MB).
const MAX_SIZE: u64 = 1 << 30;

/// A platform the updater serves, as the feed names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacAarch64,
    MacX86_64,
    WindowsX86_64,
}

impl Platform {
    pub const ALL: [Self; 3] = [Self::MacAarch64, Self::MacX86_64, Self::WindowsX86_64];

    /// The key in the feed's `files`.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::MacAarch64 => "macos-aarch64",
            Self::MacX86_64 => "macos-x86_64",
            Self::WindowsX86_64 => "windows-x86_64",
        }
    }

    /// The archive the release scripts make for `version`.
    #[must_use]
    pub fn archive_name(self, version: &Version) -> String {
        match self {
            Self::MacAarch64 | Self::MacX86_64 => {
                format!("connecto-{version}-{}.app.zip", self.key())
            }
            Self::WindowsX86_64 => format!("connecto-{version}-{}.zip", self.key()),
        }
    }

    /// The platform this build runs on, if the updater serves it.
    #[must_use]
    pub fn current() -> Option<Self> {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("macos", "aarch64") => Some(Self::MacAarch64),
            ("macos", "x86_64") => Some(Self::MacX86_64),
            ("windows", "x86_64") => Some(Self::WindowsX86_64),
            _ => None,
        }
    }
}

/// One archive the updater can install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    /// Absolute `https://` URL.
    pub url: String,
    pub size: u64,
    /// Lowercase hex.
    pub sha256: String,
}

/// What the signed part of `update.json` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feed {
    pub version: Version,
    /// The release date, as the release tool wrote it.
    pub published: String,
    pub notes: String,
    /// By [`Platform::key`].
    pub files: BTreeMap<String, Artifact>,
}

impl Feed {
    /// The archive for `platform`, if this release has one.
    #[must_use]
    pub fn artifact(&self, platform: Platform) -> Option<&Artifact> {
        self.files.get(platform.key())
    }

    /// The feed as the JSON text that gets signed.
    #[must_use]
    pub fn to_json(&self) -> String {
        let files: Map<String, Value> = self
            .files
            .iter()
            .map(|(key, a)| {
                (
                    key.clone(),
                    json!({"url": a.url, "size": a.size, "sha256": a.sha256}),
                )
            })
            .collect();
        let value = json!({
            "format": FORMAT,
            "version": self.version.to_string(),
            "published": self.published,
            "notes": self.notes,
            "files": files,
        });
        serde_json::to_string_pretty(&value).unwrap_or_default()
    }

    /// Read and check the signed part.
    ///
    /// # Errors
    /// When it is not a feed of [`FORMAT`], or an entry is not usable.
    pub fn parse(text: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(text).map_err(|_| "the feed is not JSON")?;
        if value.get("format").and_then(Value::as_u64) != Some(FORMAT) {
            return Err("the feed is of a format this version does not know".to_owned());
        }
        let field = |name: &str| value.get(name).and_then(Value::as_str);
        let version: Version = field("version").ok_or("the feed has no version")?.parse()?;
        let mut files = BTreeMap::new();
        if let Some(map) = value.get("files").and_then(Value::as_object) {
            for (key, entry) in map {
                files.insert(key.clone(), artifact(key, entry)?);
            }
        } else {
            return Err("the feed has no files".to_owned());
        }
        Ok(Self {
            version,
            published: field("published").unwrap_or_default().to_owned(),
            notes: field("notes").unwrap_or_default().to_owned(),
            files,
        })
    }
}

fn artifact(key: &str, entry: &Value) -> Result<Artifact, String> {
    let bad = |why: &str| format!("the feed's {key} entry {why}");
    let url = entry
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("has no url"))?;
    if !url.starts_with("https://") || url.chars().any(char::is_whitespace) {
        return Err(bad("is not an https:// address"));
    }
    let size = entry
        .get("size")
        .and_then(Value::as_u64)
        .filter(|size| (1..=MAX_SIZE).contains(size))
        .ok_or_else(|| bad("has no usable size"))?;
    let sha256 = entry
        .get("sha256")
        .and_then(Value::as_str)
        .filter(|hex| {
            hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        })
        .ok_or_else(|| bad("has no usable sha256"))?;
    Ok(Artifact {
        url: url.to_owned(),
        size,
        sha256: sha256.to_owned(),
    })
}

/// `update.json` for `feed`, signed with `key`.
#[must_use]
pub fn sign(feed: &Feed, key: &SigningKey) -> String {
    let signed = feed.to_json();
    let signature = key.sign(signed.as_bytes());
    let file = json!({"signed": signed, "signature": B64.encode(signature.to_bytes())});
    let mut text = serde_json::to_string_pretty(&file).unwrap_or_default();
    text.push('\n');
    text
}

/// The feed in `update_json`, once its signature checks out against `key`.
///
/// # Errors
/// When the file is not a signed feed, the signature does not verify (wrong
/// key, or anything changed), or the feed inside is not usable.
pub fn verify(update_json: &[u8], key: &VerifyingKey) -> Result<Feed, String> {
    let value: Value =
        serde_json::from_slice(update_json).map_err(|_| "the update feed is not JSON")?;
    let signed = value
        .get("signed")
        .and_then(Value::as_str)
        .ok_or("the update feed is not signed")?;
    let signature = value
        .get("signature")
        .and_then(Value::as_str)
        .and_then(|text| B64.decode(text).ok())
        .and_then(|bytes| <[u8; 64]>::try_from(bytes.as_slice()).ok())
        .ok_or("the update feed's signature is malformed")?;
    key.verify_strict(signed.as_bytes(), &Signature::from_bytes(&signature))
        .map_err(|_| "the update feed's signature does not verify".to_owned())?;
    Feed::parse(signed)
}

/// A public key as the app keeps it: 32 bytes, base64.
///
/// # Errors
/// When it is not one.
pub fn public_key_from_text(text: &str) -> Result<VerifyingKey, String> {
    let bytes = B64
        .decode(text.trim())
        .ok()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes.as_slice()).ok())
        .ok_or("the update public key is not 32 bytes of base64")?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| "the update public key is not valid".to_owned())
}

/// A private key as the release tool keeps it: the 32-byte seed, base64.
///
/// # Errors
/// When it is not one.
pub fn signing_key_from_text(text: &str) -> Result<SigningKey, String> {
    let bytes = B64
        .decode(text.trim())
        .ok()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes.as_slice()).ok())
        .ok_or("the update signing key is not 32 bytes of base64")?;
    Ok(SigningKey::from_bytes(&bytes))
}

/// The public half of `key`, as [`public_key_from_text`] reads it.
#[must_use]
pub fn public_key_text(key: &SigningKey) -> String {
    B64.encode(key.verifying_key().to_bytes())
}

/// The private key's text, as [`signing_key_from_text`] reads it.
#[must_use]
pub fn signing_key_text(key: &SigningKey) -> String {
    B64.encode(key.to_bytes())
}

/// A new random signing key.
///
/// # Errors
/// When the system has no random bytes to give.
pub fn new_signing_key() -> Result<SigningKey, String> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|err| format!("no random bytes: {err}"))?;
    Ok(SigningKey::from_bytes(&seed))
}

/// The SHA-256 of a file, lowercase hex, and its size.
///
/// # Errors
/// When the file cannot be read.
pub fn sha256_file(path: &Path) -> std::io::Result<(String, u64)> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        hasher.update(&buf[..n]);
    }
    let hex = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok((hex, size))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
pub(crate) mod tests {
    use super::*;

    /// The dev key pair for tests. Its private half is here on purpose and is
    /// never used for a real feed.
    pub(crate) fn dev_key() -> SigningKey {
        SigningKey::from_bytes(&[7; 32])
    }

    pub(crate) fn feed(version: &str) -> Feed {
        let mut files = BTreeMap::new();
        for platform in Platform::ALL {
            files.insert(
                platform.key().to_owned(),
                Artifact {
                    url: format!(
                        "https://connecto.studio/downloads/{}",
                        platform.archive_name(&version.parse().unwrap())
                    ),
                    size: 1234,
                    sha256: "ab".repeat(32),
                },
            );
        }
        Feed {
            version: version.parse().unwrap(),
            published: "2026-11-14".to_owned(),
            notes: "Faster.".to_owned(),
            files,
        }
    }

    #[test]
    fn a_signed_feed_verifies_and_reads_back_whole() {
        let key = dev_key();
        let text = sign(&feed("0.1.9"), &key);
        let read = verify(text.as_bytes(), &key.verifying_key()).unwrap();
        assert_eq!(read, feed("0.1.9"));
        assert_eq!(
            read.artifact(Platform::MacAarch64).unwrap().url,
            "https://connecto.studio/downloads/connecto-0.1.9-macos-aarch64.app.zip"
        );
        assert!(
            read.artifact(Platform::WindowsX86_64)
                .unwrap()
                .url
                .ends_with("connecto-0.1.9-windows-x86_64.zip")
        );
    }

    #[test]
    fn a_changed_feed_or_another_key_does_not_verify() {
        let key = dev_key();
        let text = sign(&feed("0.1.9"), &key);
        // Any change to the signed text: the version, a URL, a hash.
        for (from, to) in [
            ("0.1.9", "0.2.0"),
            ("connecto.studio", "connecto.evil"),
            ("abab", "abac"),
        ] {
            let changed = text.replacen(from, to, 1);
            assert_ne!(changed, text);
            assert!(
                verify(changed.as_bytes(), &key.verifying_key()).is_err(),
                "{from}"
            );
        }
        let other = SigningKey::from_bytes(&[8; 32]);
        assert!(verify(text.as_bytes(), &other.verifying_key()).is_err());
        // Not signed at all, or a signature of the wrong length.
        assert!(verify(feed("0.1.9").to_json().as_bytes(), &key.verifying_key()).is_err());
        let short = text.replace("\"signature\": \"", "\"signature\": \"AAAA");
        assert!(verify(short.as_bytes(), &key.verifying_key()).is_err());
        assert!(verify(b"not json", &key.verifying_key()).is_err());
    }

    #[test]
    fn unusable_entries_are_refused_even_when_signed() {
        let key = dev_key();
        let mut bad = feed("0.1.9");
        bad.files.get_mut("macos-aarch64").unwrap().url = "http://connecto.studio/x.zip".to_owned();
        assert!(verify(sign(&bad, &key).as_bytes(), &key.verifying_key()).is_err());
        let mut bad = feed("0.1.9");
        bad.files.get_mut("macos-aarch64").unwrap().sha256 = "AB".repeat(32);
        assert!(verify(sign(&bad, &key).as_bytes(), &key.verifying_key()).is_err());
        let mut bad = feed("0.1.9");
        bad.files.get_mut("macos-aarch64").unwrap().size = 0;
        assert!(verify(sign(&bad, &key).as_bytes(), &key.verifying_key()).is_err());
        assert!(Feed::parse(r#"{"format":2,"version":"0.1.9","files":{}}"#).is_err());
        assert!(Feed::parse(r#"{"format":1,"version":"0.1","files":{}}"#).is_err());
        assert!(Feed::parse(r#"{"format":1,"version":"0.1.9"}"#).is_err());
    }

    #[test]
    fn keys_round_trip_through_their_text() {
        let key = new_signing_key().unwrap();
        let again = signing_key_from_text(&signing_key_text(&key)).unwrap();
        assert_eq!(again.to_bytes(), key.to_bytes());
        let public = public_key_from_text(&public_key_text(&key)).unwrap();
        assert_eq!(public, key.verifying_key());
        assert!(public_key_from_text("short").is_err());
        assert!(signing_key_from_text("").is_err());
    }

    #[test]
    fn the_sha256_of_a_file_is_the_standard_one() {
        let path = std::env::temp_dir().join(format!("connecto-update-sha-{}", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        let (hex, size) = sha256_file(&path).unwrap();
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(size, 3);
        let _ = std::fs::remove_file(&path);
    }
}
