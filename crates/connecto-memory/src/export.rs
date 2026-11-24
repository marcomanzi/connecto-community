//! Export (spec §8.5): one full JSON file, and a Markdown folder with one
//! file per room. Written under the project's `memory/exports/` or a folder
//! the user chose; each export gets its own names (`memory-<ms>.json`,
//! `memory-<ms>/`), so nothing earlier is overwritten.
//!
//! Only stored text is written, and stored text is already redacted: the
//! export never reaches back for raw session text.

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use connecto_core::memory::{
    AnnotationTarget, ExportFormat, Memory, MemoryCard, Room, VersionEntry,
};
use serde_json::{Value, json};

use crate::codec::{annotation_to_value, evidence_to_value, memory_to_value};
use crate::error::Result;
use crate::read::Read;

/// Bump when the JSON layout changes.
pub const EXPORT_FORMAT_VERSION: u32 = 1;

/// Write `contents` to `path` through a temporary file and a rename.
fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// The whole memory as one JSON value.
pub fn to_json(read: &Read<'_>, now_ms: u64) -> Result<Value> {
    let memories: Vec<Value> = read.memory_rows()?.iter().map(memory_to_value).collect();
    let annotations: Vec<Value> = read
        .annotations()?
        .iter()
        .map(annotation_to_value)
        .collect();
    let evidence: Vec<Value> = read.all_evidence()?.iter().map(evidence_to_value).collect();
    let relations: Vec<Value> = read
        .relations()?
        .iter()
        .map(|(from, r)| json!({ "from": from.as_str(), "kind": r.kind.word(), "to": r.target.as_str() }))
        .collect();
    let redirects: Vec<Value> = read
        .redirects()?
        .iter()
        .map(|(address, id)| json!({ "address": address, "memory": id.as_str() }))
        .collect();
    let versions: Vec<Value> = read
        .versions(usize::MAX >> 1)?
        .iter()
        .map(|v: &VersionEntry| {
            json!({
                "version": v.version,
                "published_ms": v.published_ms,
                "cycle": v.cycle.word(),
                "operations": v.operations,
                "reverts": v.reverts,
            })
        })
        .collect();
    Ok(json!({
        "format": "connecto-memory-export",
        "format_version": EXPORT_FORMAT_VERSION,
        "project_id": read.project_id()?,
        "memory_version": read.version()?,
        "exported_ms": now_ms,
        "memories": memories,
        "annotations": annotations,
        "evidence": evidence,
        "relations": relations,
        "redirects": redirects,
        "versions": versions,
    }))
}

fn memory_markdown(out: &mut String, memory: &Memory, card: &MemoryCard) {
    let _ = writeln!(out, "## {}\n", memory.summary.trim());
    let _ = writeln!(out, "- id: `{}`, address: `{}`", memory.id, memory.address);
    let _ = writeln!(
        out,
        "- kind: {}, standing: {}",
        memory.kind.word(),
        memory.standing.word()
    );
    let _ = writeln!(out, "- status: {}", memory.status.word());
    let f = memory.freshness;
    let _ = writeln!(
        out,
        "- freshness: last confirmed {}, anchors changed: {}, open annotations: {}",
        f.last_confirmed_ms
            .map_or_else(|| "never".to_owned(), |ms| format!("at {ms} ms")),
        if f.anchors_changed { "yes" } else { "no" },
        f.open_annotations
    );
    let scopes: Vec<String> = memory
        .scopes
        .iter()
        .map(|s| {
            if s.gone {
                format!("{} (gone)", s.scope)
            } else {
                s.scope.to_string()
            }
        })
        .collect();
    let _ = writeln!(out, "- scope: {}", scopes.join(", "));
    let sources: Vec<String> = card
        .sources
        .iter()
        .map(|(kind, n)| format!("{} ×{n}", kind.word()))
        .collect();
    let _ = writeln!(out, "- sources: {}", sources.join(", "));
    for relation in &memory.relations {
        let _ = writeln!(out, "- {} `{}`", relation.kind.word(), relation.target);
    }
    for anchor in &memory.anchors {
        let _ = writeln!(out, "- anchor: `{}` at `{}`", anchor.path, anchor.blob);
    }
    if !memory.details.trim().is_empty() {
        let _ = writeln!(out, "\n{}", memory.details.trim());
    }
    for a in &card.open {
        let _ = writeln!(
            out,
            "\n> **{}** `{}` ({}): {}",
            a.kind.word(),
            a.id,
            a.origin,
            a.text.trim().replace('\n', "\n> ")
        );
    }
    out.push('\n');
}

/// One room as Markdown: its memories with their open annotations beside
/// them, then the open notes suggested for the room.
pub fn room_markdown(read: &Read<'_>, room: Room) -> Result<String> {
    let mut out = format!("# {} (`{}`)\n\n", room.label(), room.word());
    let cards = read.room(room)?;
    if cards.is_empty() {
        out.push_str("No memories.\n\n");
    }
    for card in &cards {
        if let Some(memory) = read.memory(&card.id)? {
            memory_markdown(&mut out, &memory, card);
        }
    }
    let notes: Vec<_> = read
        .open_annotations()?
        .into_iter()
        .filter(|a| matches!(a.target, AnnotationTarget::New { room: r } if r == room))
        .collect();
    if !notes.is_empty() {
        out.push_str("## Open notes for this room\n\n");
        for a in notes {
            let _ = writeln!(
                out,
                "- **{}** `{}` ({}): {}",
                a.kind.word(),
                a.id,
                a.origin,
                a.text.trim().replace('\n', " ")
            );
        }
    }
    Ok(out)
}

/// Export into `dir` (created if missing). Returns the paths written: the
/// JSON file and/or the Markdown folder.
pub fn export(
    read: &Read<'_>,
    dir: &Path,
    format: ExportFormat,
    now_ms: u64,
) -> Result<Vec<PathBuf>> {
    fs::create_dir_all(dir)?;
    let mut written = Vec::new();
    if matches!(format, ExportFormat::Json | ExportFormat::Both) {
        let path = dir.join(format!("memory-{now_ms}.json"));
        let text = serde_json::to_string_pretty(&to_json(read, now_ms)?).unwrap_or_default();
        write_atomic(&path, &text)?;
        written.push(path);
    }
    if matches!(format, ExportFormat::Markdown | ExportFormat::Both) {
        let folder = dir.join(format!("memory-{now_ms}"));
        fs::create_dir_all(&folder)?;
        for room in Room::ALL.iter().filter(|r| r.holds_memories()) {
            write_atomic(
                &folder.join(format!("{}.md", room.word())),
                &room_markdown(read, *room)?,
            )?;
        }
        written.push(folder);
    }
    Ok(written)
}
