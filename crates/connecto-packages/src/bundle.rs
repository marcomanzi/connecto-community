//! A package as one file, to hand to someone else: `<name>.connecto-package`.
//!
//! The file is JSON: the manifest, the record of how the package was made
//! (`ORIGIN.md`, when there is one) and every installable file, with which of
//! them are executable. Text files are kept as text so the file reads well in
//! a diff; anything else is base64. What only belongs to this machine (the
//! authoring session, the guides Connecto writes) is never exported; the guides
//! are written again on import.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use connecto_hook::package::{FILE_NAME, Manifest};
use serde_json::{Map, Value, json};

use crate::guide::ORIGIN;
use crate::package::{Files, read_files};

/// The extension an exported package gets.
pub const EXTENSION: &str = "connecto-package";
const FORMAT: &str = "connecto-package";
const FORMAT_VERSION: u64 = 1;
/// The most an import reads: more than any package may hold, with room for the
/// encoding.
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_FILES: usize = 2000;
const MAX_ORIGIN_BYTES: usize = 1024 * 1024;

/// What an exported file holds, read back.
#[derive(Debug, PartialEq, Eq)]
pub struct Bundle {
    pub manifest: Manifest,
    pub origin: Option<String>,
    pub files: Files,
}

/// The bundle of the package folder `dir`.
pub fn read_package(dir: &Path) -> Result<Bundle, String> {
    let text = fs::read_to_string(dir.join(FILE_NAME))
        .map_err(|e| format!("cannot read {FILE_NAME}: {e}"))?;
    let manifest = Manifest::parse(&text).map_err(|errors| errors.join("; "))?;
    let origin = fs::symlink_metadata(dir.join(ORIGIN))
        .is_ok_and(|m| m.is_file())
        .then(|| fs::read_to_string(dir.join(ORIGIN)).ok())
        .flatten()
        .filter(|o| o.len() <= MAX_ORIGIN_BYTES);
    let files = read_files(dir)?;
    Ok(Bundle {
        manifest,
        origin,
        files,
    })
}

/// The text of the exported file.
#[must_use]
pub fn encode(bundle: &Bundle) -> String {
    let mut manifest = Map::new();
    manifest.insert("name".into(), json!(bundle.manifest.name));
    manifest.insert("description".into(), json!(bundle.manifest.description));
    if let Some(base) = &bundle.manifest.extends {
        manifest.insert("extends".into(), json!(base));
    }
    let files: Vec<Value> = bundle
        .files
        .files
        .iter()
        .map(|(path, bytes)| {
            let mut file = Map::new();
            file.insert("path".into(), json!(path));
            match std::str::from_utf8(bytes) {
                Ok(text) => file.insert("text".into(), json!(text)),
                Err(_) => file.insert("base64".into(), json!(base64_encode(bytes))),
            };
            if bundle.files.exec.contains(path) {
                file.insert("exec".into(), json!(true));
            }
            Value::Object(file)
        })
        .collect();
    let mut out = Map::new();
    out.insert("format".into(), json!(FORMAT));
    out.insert("formatVersion".into(), json!(FORMAT_VERSION));
    out.insert("package".into(), Value::Object(manifest));
    if let Some(origin) = &bundle.origin {
        out.insert("origin".into(), json!(origin));
    }
    out.insert("files".into(), Value::Array(files));
    let mut text = serde_json::to_string_pretty(&Value::Object(out)).unwrap_or_default();
    text.push('\n');
    text
}

/// Read an exported file back, checking everything: the format, the manifest,
/// and that every path stays inside the tool folders.
pub fn decode(text: &str) -> Result<Bundle, String> {
    let not = || "this is not a Connecto package file".to_owned();
    let value: Value = serde_json::from_str(text).map_err(|_| not())?;
    let object = value.as_object().ok_or_else(not)?;
    if object.get("format").and_then(Value::as_str) != Some(FORMAT) {
        return Err(not());
    }
    match object.get("formatVersion").and_then(Value::as_u64) {
        Some(FORMAT_VERSION) => {}
        Some(v) if v > FORMAT_VERSION => {
            return Err(
                "this package was exported by a newer Connecto: update to import it".into(),
            );
        }
        _ => return Err(not()),
    }
    let mut manifest = object
        .get("package")
        .and_then(Value::as_object)
        .ok_or_else(not)?
        .clone();
    // The version is this machine's: an imported package starts at 1.
    manifest.insert("version".into(), json!(1));
    let manifest = Manifest::parse(&Value::Object(manifest).to_string())
        .map_err(|errors| format!("the package's manifest: {}", errors.join("; ")))?;
    let origin = match object.get("origin") {
        None | Some(Value::Null) => None,
        Some(Value::String(o)) if o.len() <= MAX_ORIGIN_BYTES => Some(o.clone()),
        Some(_) => return Err(format!("{ORIGIN} in the file is not valid")),
    };
    let list = object
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(not)?;
    if list.len() > MAX_FILES {
        return Err("the package has too many files".into());
    }
    let mut files = BTreeMap::new();
    let mut exec = BTreeSet::new();
    for entry in list {
        let path = entry.get("path").and_then(Value::as_str).ok_or_else(not)?;
        if !crate::fsx::tool_path(path) {
            return Err(format!("the file lists a path it may not hold: {path:?}"));
        }
        let bytes = match (
            entry.get("text").and_then(Value::as_str),
            entry.get("base64").and_then(Value::as_str),
        ) {
            (Some(text), None) => text.as_bytes().to_vec(),
            (None, Some(data)) => {
                base64_decode(data).ok_or_else(|| format!("{path}: damaged contents"))?
            }
            _ => return Err(format!("{path}: no contents")),
        };
        if entry.get("exec").and_then(Value::as_bool) == Some(true) {
            exec.insert(path.to_owned());
        }
        if files.insert(path.to_owned(), bytes).is_some() {
            return Err(format!("{path} is listed twice"));
        }
    }
    Ok(Bundle {
        manifest,
        origin,
        files: Files { files, exec },
    })
}

/// Read the exported file at `path`.
pub fn read_file(path: &Path) -> Result<Bundle, String> {
    let meta = fs::metadata(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if meta.len() > MAX_FILE_BYTES {
        return Err("the file is too large to be a package".into());
    }
    let text =
        fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    decode(&text)
}

/// Write the bundle's files into the new, empty package folder `folder`.
pub fn write_into(folder: &Path, bundle: &Bundle) -> std::io::Result<()> {
    use crate::fsx::write_atomic;
    write_atomic(
        folder,
        FILE_NAME,
        bundle.manifest.to_text().as_bytes(),
        None,
    )?;
    if let Some(origin) = &bundle.origin {
        write_atomic(folder, ORIGIN, origin.as_bytes(), None)?;
    }
    for (path, bytes) in &bundle.files.files {
        let mode = bundle.files.exec.contains(path).then_some(0o755);
        write_atomic(folder, path, bytes, mode)?;
        if mode.is_some() {
            crate::platform::make_executable(&folder.join(path))?;
        }
    }
    Ok(())
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let clean: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !clean.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(clean.len() / 4 * 3);
    for (index, chunk) in clean.chunks(4).enumerate() {
        let last = index + 1 == clean.len() / 4;
        let pad = chunk.iter().rev().take_while(|&&b| b == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for &b in &chunk[..4 - pad] {
            let v = ALPHABET.iter().position(|&a| a == b)?;
            n = (n << 6) | u32::try_from(v).ok()?;
        }
        n <<= 6 * u32::try_from(pad).ok()?;
        let [_, a, b, c] = n.to_be_bytes();
        out.extend_from_slice(&[a, b, c][..3 - pad]);
    }
    Some(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn sample() -> Bundle {
        let mut files = Files::default();
        files.files.insert(
            ".claude/skills/a/SKILL.md".into(),
            b"---\nname: a\n---\n".to_vec(),
        );
        files
            .files
            .insert(".claude/hooks/run.sh".into(), b"#!/bin/sh\necho\n".to_vec());
        files.exec.insert(".claude/hooks/run.sh".into());
        files
            .files
            .insert(".agents/skills/b/logo.bin".into(), vec![0, 255, 128, 7]);
        Bundle {
            manifest: Manifest {
                name: "Team".into(),
                description: "Tools".into(),
                version: 1,
                extends: Some("connecto".into()),
                requires: Vec::new(),
            },
            origin: Some("# Origin\n".into()),
            files,
        }
    }

    #[test]
    fn a_package_round_trips_through_the_file() {
        let bundle = sample();
        let text = encode(&bundle);
        assert!(text.contains("\"format\": \"connecto-package\""), "{text}");
        assert_eq!(decode(&text).unwrap(), bundle);
    }

    #[test]
    fn base64_round_trips_every_length() {
        for len in 0..10u8 {
            let bytes: Vec<u8> = (0..len).map(|i| i.wrapping_mul(37)).collect();
            assert_eq!(base64_decode(&base64_encode(&bytes)).unwrap(), bytes);
        }
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert!(base64_decode("TW=u").is_none());
    }

    #[test]
    fn a_path_outside_the_tool_folders_is_refused() {
        for bad in [
            "../x",
            ".claude/../../x",
            "/etc/passwd",
            "package.json",
            ".git/config",
        ] {
            let text = json!({
                "format": FORMAT, "formatVersion": 1,
                "package": {"name": "X"},
                "files": [{"path": bad, "text": "x"}],
            })
            .to_string();
            assert!(decode(&text).is_err(), "{bad}");
        }
    }

    #[test]
    fn other_files_and_newer_formats_are_refused_with_a_reason() {
        assert!(decode("{}").unwrap_err().contains("not a Connecto package"));
        let newer =
            json!({"format": FORMAT, "formatVersion": 9, "package": {"name": "X"}, "files": []});
        assert!(decode(&newer.to_string()).unwrap_err().contains("newer"));
    }

    #[test]
    fn the_imported_version_starts_at_one() {
        let text = json!({
            "format": FORMAT, "formatVersion": 1,
            "package": {"name": "X", "version": 40},
            "files": [],
        })
        .to_string();
        assert_eq!(decode(&text).unwrap().manifest.version, 1);
    }
}
