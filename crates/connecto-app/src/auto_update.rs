//! Installing an update by itself (ADR 0030): what holds a restart back, and
//! the restart. The updater decides when; this is the app's side.

use std::sync::Arc;
use std::time::Duration;

use connecto_core::{PaneControl, PaneStage};
use connecto_ui::WakeSlot;

use crate::Running;

/// A restart waits this long after the person last typed in a terminal, so a
/// prompt being written is not cut.
const TYPING_QUIET: Duration = Duration::from_secs(30);

/// Whether a key typed `quiet` ago is recent enough to hold a restart.
fn typing(quiet: Option<Duration>) -> bool {
    quiet.is_some_and(|quiet| quiet < TYPING_QUIET)
}

/// What blocks a restart for an update now, if anything: a pairing or a file
/// transfer (remote), a package install, a terminal being prepared, started
/// or signed in, or the person typing in the window or a terminal.
pub(crate) fn blocker(
    running: &Running,
    slot: &WakeSlot,
    data_root: &std::path::Path,
) -> Option<String> {
    if let Some(why) = running
        .remote
        .as_ref()
        .and_then(|remote| remote.restart_blocker())
    {
        return Some(why);
    }
    if connecto_session::install_gate::busy(data_root) {
        return Some("A package install or a terminal start is under way.".to_owned());
    }
    let panes = running
        .workspace
        .as_ref()
        .map(|workspace| workspace.panes())
        .unwrap_or_default();
    if panes.iter().any(|pane| {
        matches!(
            pane.stage(),
            PaneStage::Preparing { .. } | PaneStage::LoggingIn
        )
    }) {
        return Some("A terminal is being prepared or signed in.".to_owned());
    }
    if typing(slot.quiet_for()) || panes.iter().any(|pane| typing(pane.quiet_for())) {
        return Some("You are typing.".to_owned());
    }
    None
}

/// Start the updater's automatic install. Its restart quits as a stop signal
/// does: the window is asked to close, then everything is shut down (which
/// marks the turns the update cuts) and the process ends, so the hand-over
/// can put the new version in place and start it.
pub(crate) fn start(running: &Arc<Running>, slot: &Arc<WakeSlot>, data_root: std::path::PathBuf) {
    let Some(updater) = running.updates.as_ref() else {
        return;
    };
    let for_blocker = Arc::clone(running);
    let for_restart = Arc::clone(running);
    let slot = Arc::clone(slot);
    let typing_slot = Arc::clone(&slot);
    updater.start_auto(connecto_update::AutoInstall {
        blocker: Box::new(move || blocker(&for_blocker, &typing_slot, &data_root)),
        restart: Box::new(move || {
            slot.request_close();
            std::thread::sleep(crate::WINDOW_CLOSE_WAIT);
            crate::shut_down(&for_restart);
            std::process::exit(0);
        }),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_recent_typing_holds_a_restart() {
        assert!(!typing(None));
        assert!(typing(Some(Duration::from_secs(1))));
        assert!(!typing(Some(TYPING_QUIET)));
    }
}
