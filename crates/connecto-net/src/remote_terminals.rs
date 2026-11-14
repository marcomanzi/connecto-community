//! A connected host's terminals, as a registry the window can draw like a
//! project's. Opening, closing and renaming remote terminals is not part of
//! this version, so those commands say so.

use std::sync::Arc;

use connecto_core::{
    SaveState, TermActivity, TerminalEntry, TerminalSession, TerminalsControl, TerminalsView,
};

use crate::hosting::{activity_from_byte, tool_byte, tool_from_byte};
use crate::reconnect::HostHandle;
use crate::wire::ManageOp;

const NOT_YET: &str = "this is not available on a remote host yet";

pub struct RemoteTerminals(pub Arc<HostHandle>);

fn parse_id(id: &str) -> Result<u32, String> {
    id.parse().map_err(|_| "no such terminal".to_owned())
}

impl RemoteTerminals {
    /// Hand a request to the host. It is refused at once when there is no
    /// connection; the host's answer shows in the view's `last_error`.
    fn send(&self, op: ManageOp) -> Result<(), String> {
        if !self.0.is_connected() {
            return Err("not connected to that host".to_owned());
        }
        self.0.manage_async(op);
        Ok(())
    }
}

impl TerminalsControl for RemoteTerminals {
    fn view(&self) -> Arc<TerminalsView> {
        let terminals = self
            .0
            .terminals()
            .into_iter()
            .map(|t| {
                let activity = match t.session.status() {
                    connecto_core::SessionStatus::Exited { .. } => TermActivity::Exited,
                    connecto_core::SessionStatus::Unknown { .. } => TermActivity::Starting,
                    connecto_core::SessionStatus::Running => activity_from_byte(t.info.activity),
                };
                TerminalEntry {
                    id: t.info.id.to_string(),
                    name: Some(t.info.name.clone()),
                    parent: None,
                    asked_by: None,
                    tool: tool_from_byte(t.info.tool),
                    state: activity,
                    finished_turns: 0,
                    session: Arc::clone(&t.session) as Arc<dyn TerminalSession>,
                    control: None,
                    desired_running: t.info.running,
                    activity: 0,
                }
            })
            .collect();
        Arc::new(TerminalsView {
            terminals,
            history: Arc::default(),
            transcript: None,
            // The host's terminals can be added to and closed while connected.
            save: if self.0.is_connected() {
                SaveState::Saved
            } else {
                SaveState::ReadOnly
            },
            last_error: self.0.last_error(),
            warning: None,
        })
    }

    fn add(&self, tool: connecto_core::ToolKind, name: Option<String>) -> Result<(), String> {
        self.send(ManageOp::Add {
            tool: tool_byte(tool),
            name,
        })
    }

    fn close(&self, id: &str) -> Result<(), String> {
        self.send(ManageOp::Close {
            terminal: parse_id(id)?,
        })
    }

    fn rename(&self, id: &str, name: Option<String>) -> Result<(), String> {
        self.send(ManageOp::Rename {
            terminal: parse_id(id)?,
            name,
        })
    }

    fn restart(&self, id: &str) -> Result<(), String> {
        self.send(ManageOp::Restart {
            terminal: parse_id(id)?,
        })
    }

    fn continue_from(&self, _history_id: &str) -> Result<(), String> {
        Err(NOT_YET.to_owned())
    }

    fn request_transcript(&self, _history_id: &str) -> Result<(), String> {
        Err(NOT_YET.to_owned())
    }
}
