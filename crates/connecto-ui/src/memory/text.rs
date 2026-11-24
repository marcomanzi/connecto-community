//! Words, numbers, times and colours the memory section shares.

use connecto_core::memory::{
    Annotation, AnnotationOrigin, AnnotationTarget, BootstrapState, EvidenceKind, Freshness, Room,
    RunOutcome, Scope, SourceRef,
};
use egui::Color32;

use crate::theme;

/// `1234567` as `1,234,567`.
pub(super) fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A duration as people read it: `850 ms`, `42 s`, `3 min 5 s`, `1 h 2 min`.
pub(super) fn duration(ms: u64) -> String {
    let secs = ms / 1000;
    if ms < 1000 {
        format!("{ms} ms")
    } else if secs < 60 {
        format!("{secs} s")
    } else if secs < 3600 {
        format!("{} min {} s", secs / 60, secs % 60)
    } else {
        format!("{} h {} min", secs / 3600, secs % 3600 / 60)
    }
}

/// How long ago, for an age: `5 min`, `3 h`, `2 days`.
pub(super) fn age(then_ms: u64, now_ms: u64) -> String {
    let secs = now_ms.saturating_sub(then_ms) / 1000;
    if secs < 60 {
        format!("{secs} s")
    } else if secs < 3600 {
        format!("{} min", secs / 60)
    } else if secs < 86_400 {
        format!("{} h", secs / 3600)
    } else {
        let days = secs / 86_400;
        format!("{days} day{}", if days == 1 { "" } else { "s" })
    }
}

/// A moment to come: "today 02:30", "tomorrow 02:30", "in 3 days, 02:30".
pub(super) fn ahead(at_ms: u64, now_ms: u64, offset_s: i32) -> String {
    let local = |ms: u64| i64::try_from(ms / 1000).unwrap_or(i64::MAX) + i64::from(offset_s);
    let at = local(at_ms);
    let days = at.div_euclid(86_400) - local(now_ms).div_euclid(86_400);
    let secs = at.rem_euclid(86_400);
    let clock = format!("{:02}:{:02}", secs / 3600, secs % 3600 / 60);
    match days {
        ..=0 => format!("today {clock}"),
        1 => format!("tomorrow {clock}"),
        n => format!("in {n} days, {clock}"),
    }
}
/// The freshness of a memory, in words, apart from its status.
pub(super) fn freshness(f: &Freshness, now_ms: u64, offset_s: i32) -> Vec<(String, Color32)> {
    let mut parts = Vec::new();
    parts.push(match f.last_confirmed_ms {
        Some(ms) => (
            format!("confirmed {}", crate::utility::ago(ms, now_ms, offset_s)),
            theme::TEXT_ROW,
        ),
        None => ("never confirmed".to_owned(), theme::FAINT),
    });
    if f.anchors_changed {
        parts.push(("anchored files changed".to_owned(), theme::AMBER));
    }
    if f.open_annotations > 0 {
        parts.push((
            format!(
                "{} open note{}",
                f.open_annotations,
                if f.open_annotations == 1 { "" } else { "s" }
            ),
            theme::AMBER,
        ));
    }
    parts
}

/// How a run ended, and its colour.
pub(super) fn outcome(outcome: &RunOutcome) -> (String, Color32) {
    let turned_down = |n: u32| {
        if n == 0 {
            String::new()
        } else {
            format!(" · {n} turned down by the validator")
        }
    };
    match outcome {
        RunOutcome::Running => ("Running".to_owned(), theme::CYAN),
        RunOutcome::Annotated { written, rejected } => (
            format!(
                "Wrote {written} note{}{}",
                if *written == 1 { "" } else { "s" },
                turned_down(*rejected)
            ),
            theme::GREEN,
        ),
        RunOutcome::Published {
            version,
            applied,
            rejected,
        } => (
            format!(
                "Published version {version} · {applied} operation{} applied{}",
                if *applied == 1 { "" } else { "s" },
                turned_down(*rejected)
            ),
            theme::GREEN,
        ),
        RunOutcome::NoChange => ("Nothing to change".to_owned(), theme::TEXT_MUTED),
        RunOutcome::HeldByPause => (
            "Held by Pause: nothing published, it runs again after Resume".to_owned(),
            theme::AMBER,
        ),
        RunOutcome::Refused { reason, .. } => (format!("Nothing kept: {reason}"), theme::AMBER),
        RunOutcome::Held { reason } => (
            format!("On hold: {reason} (no attempt used; it resumes once that is solved)"),
            theme::AMBER,
        ),
        RunOutcome::Cancelled => (
            "Cancelled (Stop Everything, you, or the app closing)".to_owned(),
            theme::AMBER,
        ),
        RunOutcome::Interrupted => (
            "Interrupted: the app stopped during the run".to_owned(),
            theme::AMBER,
        ),
        RunOutcome::Failed { reason } => (format!("Failed: {reason}"), theme::ERROR),
    }
}

/// Where the initial build stands, and its colour.
pub(super) fn bootstrap_state(state: &BootstrapState) -> (String, Color32) {
    match state {
        BootstrapState::NotStarted => ("Not started".to_owned(), theme::FAINT),
        BootstrapState::Queued => ("Queued".to_owned(), theme::AMBER),
        BootstrapState::Running => ("Running".to_owned(), theme::CYAN),
        BootstrapState::Interrupted => (
            "Interrupted: it can resume from its last checkpoint".to_owned(),
            theme::AMBER,
        ),
        BootstrapState::Failed { reason } => (format!("Failed: {reason}"), theme::ERROR),
        BootstrapState::Completed => ("Completed".to_owned(), theme::GREEN),
    }
}

/// A scope as the user reads it.
pub(super) fn scope(scope: &Scope, gone: bool) -> String {
    let base = match scope {
        Scope::Project => "whole project".to_owned(),
        Scope::Component(path) => format!("component {path}"),
        Scope::Branch(name) => format!("branch {name}"),
        Scope::Worktree(path) => format!("worktree {path}"),
    };
    if gone {
        let what = if matches!(scope, Scope::Worktree(_)) {
            "worktree"
        } else {
            "branch"
        };
        format!("{base} ({what} no longer present)")
    } else {
        base
    }
}

/// Sources of a memory or annotation, counted by kind in the order of
/// [`EvidenceKind::ALL`].
pub(super) fn count_sources(sources: &[SourceRef]) -> Vec<(EvidenceKind, u32)> {
    EvidenceKind::ALL
        .iter()
        .filter_map(|kind| {
            let n = sources.iter().filter(|s| s.kind == *kind).count();
            (n > 0).then(|| (*kind, u32::try_from(n).unwrap_or(u32::MAX)))
        })
        .collect()
}

/// Who wrote an annotation.
pub(super) fn origin(origin: &AnnotationOrigin) -> String {
    match origin {
        AnnotationOrigin::DevelopmentCycle => "the development cycle".to_owned(),
        AnnotationOrigin::Agent { terminal } => format!("an agent in {terminal}"),
        AnnotationOrigin::User => "you".to_owned(),
    }
}

/// The annotation's sources, by id and kind.
pub(super) fn annotation_sources(a: &Annotation) -> String {
    if a.sources.is_empty() {
        return "no sources".to_owned();
    }
    a.sources
        .iter()
        .map(|s| format!("{} ({})", s.evidence, s.kind.label()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What an annotation is about.
pub(super) fn target(a: &Annotation) -> String {
    match &a.target {
        AnnotationTarget::Memory(id) => format!("about {id}"),
        AnnotationTarget::New { room } => format!("new note for {}", room.label()),
    }
}

/// What a room holds, in one line, for the palace overview and a room's list.
pub(super) const fn room_about(room: Room) -> &'static str {
    match room {
        Room::Entrance => {
            "The brief agents read first. It is generated from the other rooms, so it holds no memories of its own."
        }
        Room::Architecture => {
            "How the project is built: its parts, how they fit together and how data flows between them."
        }
        Room::Workshop => "How things are done: building, testing, releasing and diagnosing.",
        Room::Decisions => "Choices already made, with the reasons behind them.",
        Room::Problems => "Known defects, traps and recurring mistakes, and how to avoid them.",
        Room::OpenWork => "Work in progress and work on hold.",
        Room::Conventions => "Style and the working rules to follow.",
        Room::Archive => {
            "Superseded or inactive memories. Readable, but left out of the brief and, by default, of search."
        }
    }
}

/// "No memories yet", "1 memory", "3 memories".
pub(super) fn memories(n: u32) -> String {
    if n == 0 {
        "No memories yet".to_owned()
    } else if n == 1 {
        "1 memory".to_owned()
    } else {
        format!("{n} memories")
    }
}

/// A memory's title in a list: its first line, never empty.
pub(super) fn title(summary: &str) -> &str {
    let first = summary.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        "(untitled memory)"
    } else {
        first
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_group_thousands() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1000), "1,000");
        assert_eq!(count(2_000_000), "2,000,000");
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration(850), "850 ms");
        assert_eq!(duration(42_000), "42 s");
        assert_eq!(duration(185_000), "3 min 5 s");
        assert_eq!(duration(3_720_000), "1 h 2 min");
    }

    #[test]
    fn moments_ahead_name_the_day() {
        let now = 1_700_000_000_000 - 1_700_000_000_000 % 86_400_000 + 3_600_000;
        assert_eq!(ahead(now + 3_600_000, now, 0), "today 02:00");
        assert_eq!(ahead(now + 86_400_000, now, 0), "tomorrow 01:00");
        assert_eq!(ahead(now + 3 * 86_400_000, now, 0), "in 3 days, 01:00");
    }

    #[test]
    fn a_gone_scope_says_so() {
        assert_eq!(
            scope(&Scope::Branch("feat".into()), true),
            "branch feat (branch no longer present)"
        );
        assert_eq!(scope(&Scope::Project, false), "whole project");
    }
}
