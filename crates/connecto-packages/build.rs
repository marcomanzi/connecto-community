//! Embeds `builtin/<variant>/...` (the built-in Connecto package) into the
//! binary as a table of `include_bytes!`, each with its executable bit.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Unix: the file's own executable bit, as checked out.
#[cfg(unix)]
fn is_executable(file: &Path) -> io::Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    Ok(fs::metadata(file)?.permissions().mode() & 0o111 != 0)
}

/// Windows has no executable bit, so a checkout cannot carry git's mode.
/// A file that starts with `#!` is a script; in the repository exactly those
/// files are mode 100755, so the table matches the one built on macOS.
#[cfg(not(unix))]
fn is_executable(file: &Path) -> io::Result<bool> {
    use std::io::Read as _;
    let mut head = Vec::new();
    fs::File::open(file)?.take(2).read_to_end(&mut head)?;
    Ok(head == b"#!")
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect(&path, out)?;
        } else if entry.file_name() != ".DS_Store" {
            out.push(path);
        }
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").map_err(io::Error::other)?)
        .join("builtin");
    println!("cargo:rerun-if-changed=builtin");
    let mut files = Vec::new();
    collect(&root, &mut files)?;
    let mut table = String::from("pub static FILES: &[(&str, &str, bool, &[u8])] = &[\n");
    for file in files {
        let rel = file.strip_prefix(&root).map_err(io::Error::other)?;
        let mut parts = rel.components();
        let variant = parts
            .next()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .unwrap_or_default();
        let inside: Vec<String> = parts
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        // A script stays a script when the package is installed.
        let exec = is_executable(&file)?;
        let _ = writeln!(
            table,
            "    ({variant:?}, {:?}, {exec}, include_bytes!({:?})),",
            inside.join("/"),
            file.to_string_lossy()
        );
    }
    table.push_str("];\n");
    let out = PathBuf::from(std::env::var("OUT_DIR").map_err(io::Error::other)?);
    fs::write(out.join("builtin_files.rs"), table)
}
