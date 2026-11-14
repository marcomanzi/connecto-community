//! The host side of remote sessions as the app wires it: a numbered ring per
//! launched terminal, and the list of terminals a peer may see.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use connecto_core::{OutputTap, TapFactory, TermActivity, ToolKind, WorkspaceControl};

use crate::host::{HostTerminal, HostTerminals, RingTap};
use crate::seq::{RING_BYTES, Ring};
use crate::wire::ManageOp;

/// The ring of each terminal's current session, by `<project>/<terminal>`.
#[derive(Default)]
pub struct RingBook {
    rings: Mutex<HashMap<String, Arc<Ring>>>,
    /// Whether output is kept. Off (the start), a terminal's bytes are only
    /// numbered: no memory goes to output nobody can look at, and a terminal
    /// that was running when hosting is turned on is shared at once, starting
    /// from a repaint of its screen.
    record: Arc<std::sync::atomic::AtomicBool>,
}

impl RingBook {
    pub fn set_recording(&self, on: bool) {
        self.record.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    #[must_use]
    pub fn ring(&self, key: &str) -> Option<Arc<Ring>> {
        self.rings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .cloned()
    }
}

impl TapFactory for RingBook {
    fn tap_for(&self, key: &str) -> Arc<dyn OutputTap> {
        let ring = Arc::new(Ring::new(RING_BYTES));
        let old = self
            .rings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.to_owned(), Arc::clone(&ring));
        // A restart is a new screen: whoever follows the old session is let go,
        // and attaches again to the new one.
        if let Some(old) = old {
            old.close();
        }
        Arc::new(RingTap::new(ring, Arc::clone(&self.record)))
    }
}

/// The wire's byte for a status dot.
#[must_use]
pub const fn activity_byte(activity: TermActivity) -> u8 {
    match activity {
        TermActivity::NotStarted => 0,
        TermActivity::Starting => 1,
        TermActivity::NeedsLogin => 2,
        TermActivity::Working => 3,
        TermActivity::NeedsUser => 4,
        TermActivity::Idle => 5,
        TermActivity::Interrupted => 6,
        TermActivity::Done => 7,
        TermActivity::Exited => 8,
        TermActivity::Failed => 9,
        TermActivity::Plain => 10,
    }
}

#[must_use]
pub const fn activity_from_byte(byte: u8) -> TermActivity {
    match byte {
        1 => TermActivity::Starting,
        2 => TermActivity::NeedsLogin,
        3 => TermActivity::Working,
        4 => TermActivity::NeedsUser,
        5 => TermActivity::Idle,
        6 => TermActivity::Interrupted,
        7 => TermActivity::Done,
        8 => TermActivity::Exited,
        9 => TermActivity::Failed,
        10 => TermActivity::Plain,
        _ => TermActivity::NotStarted,
    }
}

/// The wire's byte for a tool: 1 Claude, 2 Codex, 3 shell.
#[must_use]
pub const fn tool_byte(tool: ToolKind) -> u8 {
    match tool {
        ToolKind::Claude => 1,
        ToolKind::Codex => 2,
        ToolKind::Shell => 3,
    }
}

#[must_use]
pub const fn tool_from_byte(byte: u8) -> ToolKind {
    match byte {
        2 => ToolKind::Codex,
        3 => ToolKind::Shell,
        _ => ToolKind::Claude,
    }
}

/// Every open project's terminals that have a ring, numbered for the wire.
pub struct WorkspaceTerminals {
    workspace: Arc<dyn WorkspaceControl>,
    book: Arc<RingBook>,
    ids: Mutex<(u32, HashMap<String, u32>)>,
}

impl WorkspaceTerminals {
    #[must_use]
    pub fn new(workspace: Arc<dyn WorkspaceControl>, book: Arc<RingBook>) -> Self {
        Self {
            workspace,
            book,
            ids: Mutex::new((0, HashMap::new())),
        }
    }

    /// A number for `key` that stays the same for the life of the app.
    fn id_of(&self, key: &str) -> u32 {
        let mut ids = self.ids.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(id) = ids.1.get(key) {
            return *id;
        }
        ids.0 += 1;
        let id = ids.0;
        ids.1.insert(key.to_owned(), id);
        id
    }
}

impl WorkspaceTerminals {
    /// The project's registry and the terminal's own id, for a wire id.
    fn locate(&self, id: u32) -> Option<(Arc<dyn connecto_core::TerminalsControl>, String)> {
        let key = {
            let ids = self.ids.lock().unwrap_or_else(PoisonError::into_inner);
            ids.1
                .iter()
                .find(|(_, v)| **v == id)
                .map(|(k, _)| k.clone())?
        };
        let (project, terminal) = key.split_once('/')?;
        let view = self.workspace.view();
        let found = view.projects.iter().find(|p| p.id == project)?;
        Some((found.terminals.clone()?, terminal.to_owned()))
    }
}

impl HostTerminals for WorkspaceTerminals {
    fn manage(&self, op: &ManageOp) -> Result<Option<u32>, String> {
        match op {
            ManageOp::Add { tool, name } => {
                let view = self.workspace.view();
                let terminals = view
                    .active
                    .as_ref()
                    .and_then(|id| view.projects.iter().find(|p| &p.id == id))
                    .and_then(|p| p.terminals.clone())
                    .ok_or_else(|| "no project is open on that machine".to_owned())?;
                let name = name.clone().filter(|n| !n.trim().is_empty());
                terminals.add(tool_from_byte(*tool), name)?;
                Ok(None)
            }
            ManageOp::Close { terminal } => {
                let (terminals, id) = self.locate(*terminal).ok_or("no such terminal")?;
                terminals.close(&id).map(|()| None)
            }
            ManageOp::Restart { terminal } => {
                let (terminals, id) = self.locate(*terminal).ok_or("no such terminal")?;
                terminals.restart(&id).map(|()| None)
            }
            ManageOp::Rename { terminal, name } => {
                let (terminals, id) = self.locate(*terminal).ok_or("no such terminal")?;
                let name = name.clone().filter(|n| !n.trim().is_empty());
                terminals.rename(&id, name).map(|()| None)
            }
        }
    }

    fn set_recording(&self, on: bool) {
        self.book.set_recording(on);
    }

    fn list(&self) -> Vec<HostTerminal> {
        let mut out = Vec::new();
        for project in &self.workspace.view().projects {
            let Some(terminals) = &project.terminals else {
                continue;
            };
            for entry in &terminals.view().terminals {
                let key = format!("{}/{}", project.id, entry.id);
                let Some(ring) = self.book.ring(&key) else {
                    continue;
                };
                out.push(HostTerminal {
                    id: self.id_of(&key),
                    name: format!(
                        "{}: {}",
                        project.name,
                        entry.name.as_deref().unwrap_or(&entry.id)
                    ),
                    tool: tool_byte(entry.tool),
                    attempt: 0,
                    activity: activity_byte(entry.state),
                    session: Arc::clone(&entry.session),
                    ring,
                });
            }
        }
        out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_restart_closes_the_old_ring_and_numbers_afresh() {
        let book = RingBook::default();
        let first = book.tap_for("p/claude-1");
        first.feed(b"abc", &mut |_| {});
        let old = book.ring("p/claude-1").unwrap();
        assert_eq!(old.end(), 3);
        let second = book.tap_for("p/claude-1");
        second.feed(b"x", &mut |_| {});
        assert!(old.is_closed());
        let new = book.ring("p/claude-1").unwrap();
        assert_eq!(new.end(), 1);
        assert!(!new.is_closed());
        assert!(book.ring("p/other").is_none());
    }

    #[test]
    fn output_is_kept_only_while_recording_and_always_numbered() {
        let book = RingBook::default();
        let tap = book.tap_for("p/t");
        tap.feed(b"before", &mut |_| {});
        let ring = book.ring("p/t").unwrap();
        assert_eq!(ring.end(), 6, "numbered while off");
        assert!(matches!(
            ring.read(0, 10),
            crate::seq::RingRead::Evicted { .. }
        ));
        book.set_recording(true);
        tap.feed(b"after", &mut |_| {});
        assert_eq!(ring.end(), 11);
        assert!(matches!(
            ring.read(6, 10),
            crate::seq::RingRead::Data { .. }
        ));
        book.set_recording(false);
        tap.feed(b"x", &mut |_| {});
        assert!(matches!(
            ring.read(6, 10),
            crate::seq::RingRead::Evicted { .. }
        ));
    }

    #[test]
    fn the_status_dot_and_tool_survive_the_wire() {
        for byte in 0..=9 {
            assert_eq!(activity_byte(activity_from_byte(byte)), byte);
        }
        assert_eq!(tool_from_byte(tool_byte(ToolKind::Codex)), ToolKind::Codex);
        assert_eq!(tool_from_byte(tool_byte(ToolKind::Shell)), ToolKind::Shell);
        assert_eq!(
            activity_from_byte(activity_byte(TermActivity::Plain)),
            TermActivity::Plain
        );
        assert_eq!(
            tool_from_byte(tool_byte(ToolKind::Claude)),
            ToolKind::Claude
        );
    }
}
