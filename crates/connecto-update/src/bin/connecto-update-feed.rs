//! The release tool for the update feed. Not shipped.
//!
//! ```text
//! connecto-update-feed keygen <private-key-file>
//!     Make a key pair. The private key goes to the file (0600, never commit
//!     it); the public key is printed: put it in
//!     crates/connecto-update/update-public-key.txt.
//! connecto-update-feed sign --version <V> --dist <folder> [--base-url <url>] [--notes <text>] [--out <file>]
//!     Write update.json for the archives of version V found in the folder,
//!     signed with the private key from CONNECTO_UPDATE_SIGNING_KEY (the key
//!     text), or the file named by CONNECTO_UPDATE_SIGNING_KEY_FILE, or
//!     scripts/release/update-signing-key.local.
//! connecto-update-feed verify <update.json>
//!     Check a feed against the public key built into this build.
//! ```
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use connecto_update::Version;
use connecto_update::feed::{self, Artifact, Feed, Platform};

const DEFAULT_BASE_URL: &str = "https://connecto.studio/downloads/";
const DEFAULT_KEY_FILE: &str = "scripts/release/update-signing-key.local";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("keygen") => keygen(&args[1..]),
        Some("sign") => sign(&args[1..]),
        Some("verify") => verify(&args[1..]),
        _ => Err("usage: connecto-update-feed keygen <file> | sign --version <V> --dist <folder> [--base-url <url>] [--notes <text>] [--out <file>] | verify <update.json>".to_owned()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("connecto-update-feed: {why}");
            ExitCode::FAILURE
        }
    }
}

fn keygen(args: &[String]) -> Result<(), String> {
    let [file] = args else {
        return Err("keygen takes the file for the private key".to_owned());
    };
    let path = Path::new(file);
    if path.exists() {
        return Err(format!("{file} exists: not overwritten"));
    }
    let key = feed::new_signing_key()?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    options
        .open(path)
        .and_then(|mut f| writeln!(f, "{}", feed::signing_key_text(&key)))
        .map_err(|err| format!("{file}: {err}"))?;
    println!("private key written to {file} (keep it secret, never commit it)");
    println!("public key (put it in crates/connecto-update/update-public-key.txt):");
    println!("{}", feed::public_key_text(&key));
    Ok(())
}

fn signing_key() -> Result<ed25519_dalek::SigningKey, String> {
    if let Ok(text) = std::env::var("CONNECTO_UPDATE_SIGNING_KEY")
        && !text.trim().is_empty()
    {
        return feed::signing_key_from_text(&text);
    }
    let file = std::env::var("CONNECTO_UPDATE_SIGNING_KEY_FILE")
        .ok()
        .filter(|f| !f.is_empty())
        .unwrap_or_else(|| DEFAULT_KEY_FILE.to_owned());
    let text = std::fs::read_to_string(&file).map_err(|_| {
        format!("no update signing key: set CONNECTO_UPDATE_SIGNING_KEY, or CONNECTO_UPDATE_SIGNING_KEY_FILE, or put it in {DEFAULT_KEY_FILE}")
    })?;
    feed::signing_key_from_text(&text)
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|at| args.get(at + 1))
        .cloned()
}

fn sign(args: &[String]) -> Result<(), String> {
    let version: Version = flag(args, "--version")
        .ok_or("sign needs --version")?
        .parse()?;
    let dist = PathBuf::from(flag(args, "--dist").ok_or("sign needs --dist")?);
    let mut base = flag(args, "--base-url").unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
    if !base.ends_with('/') {
        base.push('/');
    }
    let out = flag(args, "--out").map_or_else(|| dist.join("update.json"), PathBuf::from);
    let key = signing_key()?;
    let mut files = BTreeMap::new();
    for platform in Platform::ALL {
        let name = platform.archive_name(&version);
        let path = dist.join(&name);
        if !path.is_file() {
            eprintln!(
                "note: {name} is not in {}: {} gets no update",
                dist.display(),
                platform.key()
            );
            continue;
        }
        let (sha256, size) = feed::sha256_file(&path).map_err(|err| format!("{name}: {err}"))?;
        files.insert(
            platform.key().to_owned(),
            Artifact {
                url: format!("{base}{name}"),
                size,
                sha256,
            },
        );
    }
    if files.is_empty() {
        return Err(format!("no archive of {version} in {}", dist.display()));
    }
    let published = flag(args, "--published").unwrap_or_default();
    let text = feed::sign(
        &Feed {
            version,
            published,
            notes: flag(args, "--notes").unwrap_or_default(),
            files,
        },
        &key,
    );
    // The feed must check out against the key it was signed with.
    feed::verify(text.as_bytes(), &key.verifying_key())?;
    std::fs::write(&out, text).map_err(|err| format!("{}: {err}", out.display()))?;
    println!(
        "wrote {} (public key {})",
        out.display(),
        feed::public_key_text(&key)
    );
    Ok(())
}

fn verify(args: &[String]) -> Result<(), String> {
    let [file] = args else {
        return Err("verify takes the update.json".to_owned());
    };
    let key = connecto_update::built_in_key().ok_or("this build has no update public key")?;
    let bytes = std::fs::read(file).map_err(|err| format!("{file}: {err}"))?;
    let feed = feed::verify(&bytes, &key)?;
    println!(
        "{file}: signed, version {}, platforms {:?}",
        feed.version,
        feed.files.keys().collect::<Vec<_>>()
    );
    Ok(())
}
