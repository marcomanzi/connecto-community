//! One terminal's screen, selection and input, and how it is drawn.
//!
//! The window keeps a `Pane` for every terminal of every open project, so a
//! background session keeps its size, selection and scroll. Only the terminal
//! on show is polled, resized and typed into.

use std::sync::Arc;
use std::time::{Duration, Instant};

use connecto_core::{
    AuthState, GridSize, InputModes, InputRefused, Key, KeyPress, Mods, MouseButton, MouseEvent,
    MouseKind, PaneControl, PaneStage, ResizeOutcome, SelectOp, SelectionText, SessionStatus,
    SnapshotOutcome, TermSnapshot, TerminalEntry, TerminalSession, ToolKind, encode_key,
    encode_mouse,
};
use egui::{Align2, Color32, Event, EventFilter, FontId, Id, Pos2, Rect, Sense, Ui, Vec2, pos2};

use crate::keymap;
use crate::links;
use crate::metrics::PaneCounters;
use crate::platform;
use crate::preview;
use crate::render::{self, CellMetrics};
use crate::selection::{self, Selection};
use crate::theme;
use crate::words;

pub(crate) const PANE_PADDING: f32 = 8.0;
pub(crate) const BUTTON_SIZE: Vec2 = Vec2::new(92.0, 24.0);
const NOTICE_LIFETIME: Duration = Duration::from_secs(3);
/// Shown while Connecto types into the terminal by itself.
pub(crate) const INJECTING_TEXT: &str = "Connecto is sending a message…";
/// Shown while a message waits for a draft Connecto cannot move aside.
pub(crate) const WAITING_TEXT: &str = "Connecto has a message waiting: send or clear your line";
const INJECTING_REPAINT: Duration = Duration::from_millis(100);
const CONTENDED_RETRY: Duration = Duration::from_millis(16);
/// How often a drag held past the top or bottom edge scrolls on.
const EDGE_SCROLL_EVERY: Duration = Duration::from_millis(50);
const WARNING_LINE_HEIGHT: f32 = 16.0;
const HEADER_TEXT: Color32 = theme::TEXT_MUTED;
const WARNING_TEXT: Color32 = theme::CORAL_SOFT;
const ERROR_TEXT: Color32 = theme::ERROR;
/// Cyan at about a quarter, over the terminal's own background.
const SELECTION_FILL: Color32 = Color32::from_rgba_premultiplied(0x1d, 0x3e, 0x3f, 0x40);
/// Shown in place of Log in and Restart when the project turned the tool off.
const TOOL_OFF_TEXT: &str =
    "This tool is turned off in the project settings, so it cannot be logged in or restarted.";
const LOGIN_RESET_TEXT: &str =
    "Log in resets this tool's history and settings for this project. Nothing is restored.";

/// One pane to show.
pub struct PaneConfig {
    pub label: String,
    /// The session, or why it could not start. A failed pane shows the reason
    /// and leaves the other panes running.
    pub session: Result<Arc<dyn TerminalSession>, String>,
    /// Lifecycle commands, for a pane whose session is managed.
    pub control: Option<Arc<dyn PaneControl>>,
    /// Which CLI it runs. Absent for a plain shell.
    pub tool: Option<ToolKind>,
}

pub(crate) struct Pane {
    pub(crate) id: Id,
    pub(crate) label: String,
    pub(crate) session: Result<Arc<dyn TerminalSession>, String>,
    pub(crate) control: Option<Arc<dyn PaneControl>>,
    pub(crate) tool: Option<ToolKind>,
    pub(crate) snapshot: TermSnapshot,
    pub(crate) selection: Option<Selection>,
    /// A copy asked for while the emulator was busy, done on a later frame.
    pending_copy: bool,
    /// That copy is the clean one (`connecto_core::copy_text`), not the text
    /// as shown.
    pending_clean: bool,
    /// Where the files named in the terminal are: on this computer, they are
    /// looked up as the pointer rests on them; on a remote host, a path is
    /// asked for when it is clicked.
    pub(crate) files: Files,
    /// The folder relative paths in the terminal are read from: the project's.
    pub(crate) base_dir: Option<std::path::PathBuf>,
    /// The last word looked up as a file, and the file it named, so the disk
    /// is asked once per word the pointer rests on, not once per frame.
    path_lookup: Option<(String, Option<std::path::PathBuf>)>,
    /// Files clicked for a preview this frame, for the window to open.
    preview_requests: Vec<PreviewRequest>,
    /// A clear the session was too busy to take, sent again on a later frame.
    unsent_clear: bool,
    /// When a drag held past an edge last scrolled, so it scrolls at one pace
    /// whatever the frame rate.
    edge_scrolled: Option<Instant>,
    /// The first click on Log in happened: the next one confirms.
    pub(crate) login_armed: bool,
    /// The project's settings turned this terminal's tool off: it cannot be
    /// logged in or restarted.
    pub(crate) tool_off: bool,
    /// A dialog or sheet over the window holds the pointer: no click, move
    /// or wheel reaches the program, only the release of a gesture it saw
    /// start.
    pub(crate) pointer_blocked: bool,
    pub(crate) applied_size: Option<GridSize>,
    pub(crate) emulator_failed: bool,
    pub(crate) notice: Option<(String, Instant)>,
    /// Images pasted this frame (no text was on the clipboard), for the window
    /// to save and give back as paths ([`Pane::paste_saved`]).
    pasted_images: Vec<Arc<egui::ColorImage>>,
    /// Files copied in Finder and pasted here, asked about before they are
    /// sent to a remote terminal's host.
    #[cfg(target_os = "macos")]
    pub(crate) file_paste: crate::clipboard_files::FilePaste,
    /// Copied files whose upload was confirmed this frame, each paste's in
    /// order, for the window to send to the terminal's host.
    #[cfg(target_os = "macos")]
    pasted_files: Vec<Vec<std::path::PathBuf>>,
    /// The session and attachment the open question was asked in: Enter or
    /// Esc may act only while the terminal still shows them.
    #[cfg(target_os = "macos")]
    file_paste_mark: Option<SessionMark>,
    /// This frame's Enter or Esc at the question, acted on at its end.
    #[cfg(target_os = "macos")]
    file_answer: Option<crate::clipboard_files::Answer>,
    /// Where the question's strip and its two lines were drawn, for tests.
    #[cfg(all(target_os = "macos", test))]
    pub(crate) drew_strip: Option<(Rect, Vec<Rect>)>,
    scroll_lines: f32,
    /// The button held while mouse events go to the program, and the cell the
    /// pointer was last reported at.
    mouse_down: Option<MouseButton>,
    mouse_cell: Option<(u16, u16)>,
    contended_streak: u32,
    pub(crate) counters: PaneCounters,
    /// Where the Log in / Start buttons were drawn, for tests.
    #[cfg(test)]
    pub(crate) command_hits: Vec<Rect>,
    /// The last frame showed that Connecto is typing, for tests.
    #[cfg(test)]
    pub(crate) drew_injecting: bool,
    /// The last frame showed that a message waits, for tests.
    #[cfg(test)]
    pub(crate) drew_waiting: bool,
    #[cfg(test)]
    pub(crate) command_ids: Vec<Id>,
}

/// A terminal session as it was when files were pasted or dropped on it.
pub(crate) struct SessionMark {
    session: std::sync::Weak<dyn TerminalSession>,
    generation: u64,
}

impl SessionMark {
    /// Whether the session still runs somewhere (its pane may be gone).
    pub(crate) fn alive(&self) -> bool {
        self.session.strong_count() > 0
    }
}

/// Where pasted or dropped files were saved, to be typed as paths.
pub(crate) enum Saved {
    /// On this computer.
    Here(Vec<std::path::PathBuf>),
    /// On the remote host the terminal runs on, as it wrote the paths.
    OnHost(Vec<String>),
}

impl Pane {
    pub(crate) fn new(scope: &str, config: PaneConfig) -> Self {
        Self {
            id: Id::unique(("terminal-pane", scope.to_owned(), config.label.clone())),
            label: config.label,
            session: config.session,
            control: config.control,
            tool: config.tool,
            snapshot: TermSnapshot::default(),
            selection: None,
            pending_copy: false,
            pending_clean: false,
            files: Files::Local,
            base_dir: None,
            path_lookup: None,
            preview_requests: Vec::new(),
            unsent_clear: false,
            edge_scrolled: None,
            login_armed: false,
            tool_off: false,
            pointer_blocked: false,
            applied_size: None,
            emulator_failed: false,
            notice: None,
            pasted_images: Vec::new(),
            #[cfg(target_os = "macos")]
            file_paste: crate::clipboard_files::FilePaste::default(),
            #[cfg(target_os = "macos")]
            pasted_files: Vec::new(),
            #[cfg(target_os = "macos")]
            file_paste_mark: None,
            #[cfg(target_os = "macos")]
            file_answer: None,
            #[cfg(all(target_os = "macos", test))]
            drew_strip: None,
            scroll_lines: 0.0,
            mouse_down: None,
            mouse_cell: None,
            contended_streak: 0,
            counters: PaneCounters::default(),
            #[cfg(test)]
            command_hits: Vec::new(),
            #[cfg(test)]
            drew_injecting: false,
            #[cfg(test)]
            drew_waiting: false,
            #[cfg(test)]
            command_ids: Vec::new(),
        }
    }

    pub(crate) fn from_entry(scope: &str, entry: &TerminalEntry) -> Self {
        Self::new(
            scope,
            PaneConfig {
                label: entry.id.clone(),
                session: Ok(Arc::clone(&entry.session)),
                control: entry.control.clone(),
                tool: Some(entry.tool),
            },
        )
    }

    /// Take what the registry now says about this terminal.
    pub(crate) fn follow(&mut self, entry: &TerminalEntry) {
        self.tool = Some(entry.tool);
        self.control.clone_from(&entry.control);
        if !matches!(&self.session, Ok(session) if Arc::ptr_eq(session, &entry.session)) {
            self.session = Ok(Arc::clone(&entry.session));
            self.snapshot = TermSnapshot::default();
            self.applied_size = None;
            self.selection = None;
            self.pending_copy = false;
            self.unsent_clear = false;
            self.emulator_failed = false;
        }
    }

    /// Copy the latest screen, or keep the last frame if the emulator is busy.
    fn refresh(&mut self, ctx: &egui::Context) {
        let Ok(session) = &self.session else { return };
        match session.poll_snapshot(&mut self.snapshot) {
            SnapshotOutcome::Updated => {
                self.counters.updated += 1;
                self.contended_streak = 0;
                if let Some(delay) = self.snapshot.repoll_after {
                    ctx.request_repaint_after(delay);
                }
            }
            SnapshotOutcome::Unchanged => self.contended_streak = 0,
            SnapshotOutcome::Contended => {
                self.counters.contended += 1;
                self.contended_streak += 1;
                self.counters.max_contended_streak = self
                    .counters
                    .max_contended_streak
                    .max(self.contended_streak);
                // The reader publishes a frame and wakes the UI. This timer
                // only covers a reader that finished just before being asked.
                // An immediate repaint here would spin when vsync is off.
                ctx.request_repaint_after(CONTENDED_RETRY);
            }
            SnapshotOutcome::Failed => self.emulator_failed = true,
        }
    }

    /// A click makes this window's size the terminal's: on the host it takes
    /// the size back from a remote peer; in a peer, the size is asked for
    /// again, which takes it from the host.
    fn claim_size(&mut self) {
        if let Ok(session) = &self.session {
            session.claim_local_size();
        }
        self.applied_size = None;
    }

    fn apply_size(&mut self, size: GridSize, ctx: &egui::Context) {
        if self.applied_size == Some(size) {
            return;
        }
        let Ok(session) = &self.session else { return };
        match session.resize(size) {
            ResizeOutcome::Applied => {
                self.applied_size = Some(size);
                ctx.request_repaint();
            }
            ResizeOutcome::Retry => ctx.request_repaint(),
            ResizeOutcome::Failed => self.emulator_failed = true,
        }
    }

    fn send(&mut self, bytes: Vec<u8>) {
        if self.session.is_err() {
            return;
        }
        self.clear_selection();
        let Ok(session) = &self.session else { return };
        // Typing returns the view to the live screen, as in other terminals.
        if self.snapshot.display_offset > 0 {
            let offset = i32::try_from(self.snapshot.display_offset).unwrap_or(i32::MAX);
            session.scroll(-offset);
        }
        if let Err(refused) = session.send_input(bytes) {
            self.set_notice(refused);
        }
    }

    fn set_notice(&mut self, refused: InputRefused) {
        self.notice = Some((refused.to_string(), Instant::now()));
    }

    /// The files clicked for a preview since the last call.
    pub(crate) fn take_preview_requests(&mut self) -> Vec<PreviewRequest> {
        std::mem::take(&mut self.preview_requests)
    }

    /// The images pasted since the last call, to be saved by the window.
    pub(crate) fn take_pasted_images(&mut self) -> Vec<Arc<egui::ColorImage>> {
        std::mem::take(&mut self.pasted_images)
    }

    /// A pasted image was saved, or files dropped on a remote terminal reached
    /// its host (their paths go in together, as dropped files' do, for every
    /// kind of terminal), or why not (the pane's notice, and nothing typed).
    /// They go only to the session and attachment `mark` was taken of,
    /// checked again as they are queued.
    pub(crate) fn paste_saved(&mut self, saved: Result<Saved, String>, mark: &SessionMark) {
        const REPLACED: &str =
            "The terminal restarted or reconnected before the files arrived: nothing was typed.";
        let text = match saved {
            Err(why) => return self.tell(why),
            Ok(Saved::Here(paths)) => keymap::dropped_paths_text(&paths),
            Ok(Saved::OnHost(paths)) => keymap::host_paths_text(&paths),
        };
        if !self.still_shows(mark) {
            return self.tell(REPLACED.to_owned());
        }
        let Some(text) = text else { return };
        let bytes = connecto_core::encode_paste(&text, self.snapshot.modes);
        let Ok(session) = self.session.clone() else {
            return;
        };
        self.clear_selection();
        if self.snapshot.display_offset > 0 {
            let offset = i32::try_from(self.snapshot.display_offset).unwrap_or(i32::MAX);
            session.scroll(-offset);
        }
        match session.send_input_on(bytes, mark.generation) {
            Ok(()) => {}
            Err(_) if session.attachment_generation() != mark.generation => {
                self.tell(REPLACED.to_owned());
            }
            Err(refused) => self.set_notice(refused),
        }
    }

    /// The session the pane shows now, and its attachment, to tell later
    /// whether it still does.
    pub(crate) fn session_mark(&self) -> Option<SessionMark> {
        self.session.as_ref().ok().map(|session| SessionMark {
            session: Arc::downgrade(session),
            generation: session.attachment_generation(),
        })
    }

    /// Whether the pane still shows the session `mark` was taken of, over the
    /// same attachment: no restart here, and no reattachment (as after a
    /// restart on a remote host), replaced it.
    pub(crate) fn still_shows(&self, mark: &SessionMark) -> bool {
        matches!((&self.session, mark.session.upgrade()),
            (Ok(now), Some(then)) if Arc::ptr_eq(now, &then)
                && now.attachment_generation() == mark.generation)
    }

    /// Show `text` in the pane's notice.
    pub(crate) fn tell(&mut self, text: String) {
        self.notice = Some((text, Instant::now()));
    }

    /// Files dropped on the terminal: their escaped paths go in as a paste, so
    /// a shell or an agent CLI takes them as typed input. Nothing is run.
    pub(crate) fn drop_files(&mut self, paths: &[std::path::PathBuf]) {
        if let Some(text) = keymap::dropped_paths_text(paths) {
            let bytes = connecto_core::encode_paste(&text, self.snapshot.modes);
            self.send(bytes);
        }
    }
}

/// Files copied in Finder and pasted on a remote terminal (macOS).
#[cfg(target_os = "macos")]
impl Pane {
    /// The copied files whose upload was confirmed since the last call, a
    /// list per paste.
    pub(crate) fn take_pasted_files(&mut self) -> Vec<Vec<std::path::PathBuf>> {
        std::mem::take(&mut self.pasted_files)
    }

    /// Close the question about pasted files unanswered: nothing is sent or
    /// typed.
    pub(crate) fn cancel_file_paste(&mut self) {
        let answered = self.file_answer.take().is_some();
        if self.file_paste.cancel() || answered {
            self.tell(PASTE_CANCELLED.to_owned());
        }
    }

    /// Whether the terminal still shows the session and attachment the open
    /// question was asked in.
    pub(crate) fn file_paste_session_holds(&self) -> bool {
        self.file_paste_mark
            .as_ref()
            .is_some_and(|mark| self.still_shows(mark))
    }

    /// The open question about pasted files, over the bottom of the
    /// terminal: the question, then the keys that answer it, each wrapped to
    /// the terminal's width. Its own layer above the terminal, so no click,
    /// drag or wheel over it reaches the program. Drawing it in this pass is
    /// what lets an Enter in the next one answer it. False when none is open.
    fn draw_file_paste(
        &mut self,
        ctx: &egui::Context,
        painter: &egui::Painter,
        rect: Rect,
    ) -> bool {
        const PAD: f32 = 8.0;
        const GAP: f32 = 4.0;
        let pass = ctx.cumulative_pass_nr();
        let Some(question) = self.file_paste.paint(pass).map(str::to_owned) else {
            #[cfg(test)]
            {
                self.drew_strip = None;
            }
            return false;
        };
        let wrap = (rect.width() - 2.0 * PAD).max(1.0);
        let question = painter.layout(question, FontId::proportional(13.0), theme::TEXT, wrap);
        let hint = painter.layout(
            crate::clipboard_files::HINT.to_owned(),
            FontId::proportional(12.0),
            HEADER_TEXT,
            wrap,
        );
        let height = PAD + question.size().y + GAP + hint.size().y + PAD;
        let strip = Rect::from_min_max(
            pos2(rect.min.x, (rect.max.y - height).max(rect.min.y)),
            rect.max,
        );
        let at_question = pos2(
            strip.center().x - question.size().x / 2.0,
            strip.min.y + PAD,
        );
        let at_hint = pos2(
            strip.center().x - hint.size().x / 2.0,
            at_question.y + question.size().y + GAP,
        );
        #[cfg(test)]
        {
            self.drew_strip = Some((
                strip,
                vec![
                    Rect::from_min_size(at_question, question.size()),
                    Rect::from_min_size(at_hint, hint.size()),
                ],
            ));
        }
        let seen = egui::Area::new(self.id.with("file-paste"))
            .order(egui::Order::Middle)
            .fixed_pos(strip.min)
            .constrain(false)
            .show(ctx, |ui| {
                // Takes the pointer over the whole strip.
                let (_, _) = ui.allocate_exact_size(strip.size(), Sense::click_and_drag());
                let painter = ui.painter();
                painter.rect_filled(strip, 0.0, theme::PANEL_BG);
                painter.hline(
                    strip.x_range(),
                    strip.min.y,
                    egui::Stroke::new(1.0, theme::BORDER),
                );
                painter.galley(at_question, question, theme::TEXT);
                painter.galley(at_hint, hint, HEADER_TEXT);
                // A new area's first pass sizes it, invisibly: the question
                // is on screen only from a pass that is not that.
                !ui.is_sizing_pass() && ui.is_visible() && strip.area() > 0.0
            })
            .inner;
        if seen {
            self.file_paste.mark_seen();
        }
        true
    }

    /// The frame's input, given to the open question about pasted files
    /// before anything else sees it (see `App::draw`).
    pub(crate) fn answer_file_paste(&mut self, events: &[Event]) {
        let answer = self.file_paste.answer(events);
        self.take_file_answer(answer);
    }

    /// An Enter or Esc is held until the end of the frame, when the window
    /// checks again that the question still held the screen and keyboard
    /// ([`Self::settle_file_answer`]): a dialog that arrived meanwhile ends
    /// it. A cancel needs no such check.
    fn take_file_answer(&mut self, answer: crate::clipboard_files::Answer) {
        use crate::clipboard_files::Answer;
        match answer {
            Answer::Waiting => {}
            Answer::Cancelled => self.tell(PASTE_CANCELLED.to_owned()),
            answer => self.file_answer = Some(answer),
        }
    }

    /// Act on this frame's Enter or Esc at the question when `holds` (the
    /// window found, at the end of the frame, that it still held the screen
    /// and keyboard), or else cancel it: nothing sent or typed.
    pub(crate) fn settle_file_answer(&mut self, holds: bool) {
        use crate::clipboard_files::Answer;
        let Some(answer) = self.file_answer.take() else {
            return;
        };
        match answer {
            _ if !holds => self.tell(PASTE_CANCELLED.to_owned()),
            Answer::Upload(paths) => self.pasted_files.push(paths),
            Answer::PasteText(text) => {
                if let Some(bytes) = keymap::translate(&Event::Paste(text), self.snapshot.modes) {
                    self.send(bytes);
                }
            }
            Answer::Waiting | Answer::Cancelled => {}
        }
    }

    /// An Enter or Esc at the question waits for the end of the frame.
    pub(crate) fn has_file_answer(&self) -> bool {
        self.file_answer.is_some()
    }
}

#[cfg(target_os = "macos")]
const PASTE_CANCELLED: &str = "Paste cancelled: nothing was sent or typed.";

impl Pane {
    /// The frame's events, for a terminal that has the keyboard. `had_focus`:
    /// it had it before this frame's click, so a paste was meant for it.
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
    fn handle_input(&mut self, events: &[Event], ctx: &egui::Context, had_focus: bool) {
        let modes = self.snapshot.modes;
        for (at, event) in events.iter().enumerate() {
            #[cfg(target_os = "macos")]
            if let Event::Paste(text) = event
                && had_focus
            {
                use crate::clipboard_files::Pasted;
                match self.file_paste.paste(text) {
                    Pasted::Text => {}
                    Pasted::Refused(why) => {
                        self.tell(why);
                        continue;
                    }
                    Pasted::Asked => {
                        self.file_paste_mark = self.session_mark();
                        // Copied files, asked about before anything is sent
                        // or typed. The rest of the frame's input came with
                        // the paste, before the question was drawn: it can
                        // cancel it (an Enter too), and reaches nothing.
                        let answer = self.file_paste.answer(&events[at + 1..]);
                        self.take_file_answer(answer);
                        break;
                    }
                }
            }
            if let Event::PasteImage(image) = event {
                // Saved and pasted as a path by the window, off this thread.
                self.pasted_images.push(Arc::clone(image));
            } else if self.selection.is_some() && is_copy_as_shown(event) {
                // The text exactly as the screen shows it.
                self.copy_selection(ctx, false);
            } else if matches!(event, Event::Copy) {
                // The clean copy: an agent's margins dropped and the rows it
                // broke put back together.
                if self.selection.is_some() && self.copy_selection(ctx, true) {
                    // Copied, or to be copied once the emulator is free.
                } else if let Some(bytes) = platform::clipboard_event_as_key(event)
                    .and_then(|key| keymap::translate(&key, modes))
                {
                    self.send(bytes);
                }
            } else if let Some(bytes) = platform::clipboard_event_as_key(event)
                .and_then(|key| keymap::translate(&key, modes))
            {
                // Cut, off macOS: Ctrl+X, a key the shell's editors use.
                self.send(bytes);
            } else if is_select_all(event) {
                // The screen as shown.
                self.selection = Some(Selection::new(SelectOp::All));
                self.pending_copy = false;
                self.send_unsent_selection();
            } else if let Some(bytes) = keymap::translate(event, modes) {
                self.send(bytes);
            }
        }
    }

    /// Copy the selection. The only clipboard write there is: it follows a key
    /// press, and session output cannot reach it. False when the selection
    /// has gone (its text scrolled out of history or was rewritten), so the
    /// key is taken as if nothing were selected.
    fn copy_selection(&mut self, ctx: &egui::Context, clean: bool) -> bool {
        let Ok(session) = self.session.clone() else {
            return false;
        };
        self.pending_clean = clean;
        let text = if !self.send_unsent_selection() {
            SelectionText::Busy
        } else if clean {
            session.clean_selection_text()
        } else {
            session.selection_text()
        };
        match text {
            SelectionText::Text(text) => {
                self.pending_copy = false;
                ctx.copy_text(text);
                if platform::COPY_CLEARS_SELECTION {
                    self.clear_selection();
                }
                true
            }
            SelectionText::Busy => {
                self.pending_copy = true;
                ctx.request_repaint_after(CONTENDED_RETRY);
                true
            }
            SelectionText::Nothing => {
                self.pending_copy = false;
                self.selection = None;
                false
            }
        }
    }

    /// Drop the selection, here and in the session.
    fn clear_selection(&mut self) {
        let had = self.selection.take().is_some();
        self.pending_copy = false;
        if had || self.unsent_clear {
            self.unsent_clear = true;
            self.send_unsent_selection();
        }
    }

    /// Give the session what it was too busy to take: a clear, a start or
    /// select-all, and the drag's last cell, in that order. True when nothing
    /// is left to send.
    fn send_unsent_selection(&mut self) -> bool {
        let Ok(session) = &self.session else {
            return true;
        };
        if self.unsent_clear {
            if !session.select(SelectOp::Clear) {
                return false;
            }
            self.unsent_clear = false;
        }
        let Some(selection) = &mut self.selection else {
            return true;
        };
        if let Some(op) = selection.unsent {
            if !session.select(op) {
                return false;
            }
            selection.unsent = None;
        }
        if let Some((row, col)) = selection.unsent_extend {
            if !session.select(SelectOp::Extend { row, col }) {
                return false;
            }
            selection.unsent_extend = None;
        }
        true
    }

    /// Whether the mouse goes to the program: the terminal is a remote one, the
    /// program turned mouse reporting on, and Shift (the usual way to select
    /// text anyway) is not held.
    fn forwards_mouse(&self, shift: bool) -> bool {
        !shift
            && self.snapshot.modes.mouse_reporting()
            && self.session.as_ref().is_ok_and(|s| s.forwards_mouse())
    }

    /// Whether wheel movement goes to the program as wheel reports: it turned
    /// mouse reporting on, and neither Shift nor the link key is held, which
    /// keep the wheel for the terminal's own scrolling.
    fn wheel_to_program(&self, shift: bool, command: bool) -> bool {
        !shift && !command && self.snapshot.modes.mouse_reporting()
    }

    /// The bytes for this frame's pointer events, in order. `rect` is the
    /// terminal's area and `origin` the top-left of its grid.
    /// `owns` says whether the terminal had the pointer at a point: nothing
    /// drawn over it there. A gesture starts only where it does, and is then
    /// followed through its release, outside the terminal too.
    fn mouse_bytes(
        &mut self,
        events: &[Event],
        rect: Rect,
        origin: Pos2,
        cell: CellMetrics,
        owns: impl Fn(Pos2) -> bool,
    ) -> Vec<Vec<u8>> {
        let modes = self.snapshot.modes;
        let at = |pos: Pos2, snapshot: &TermSnapshot| {
            selection::cell_at(pos - origin, cell.width, cell.height, snapshot)
        };
        let mut out = Vec::new();
        for event in events {
            let (kind, pos, modifiers) = match event {
                Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    modifiers,
                } => {
                    let button = match button {
                        egui::PointerButton::Primary => MouseButton::Left,
                        egui::PointerButton::Middle => MouseButton::Middle,
                        egui::PointerButton::Secondary => MouseButton::Right,
                        _ => continue,
                    };
                    if *pressed {
                        // Only a press on the terminal, uncovered, starts
                        // a gesture.
                        if !rect.contains(*pos) || !owns(*pos) {
                            continue;
                        }
                        self.mouse_down = Some(button);
                        (MouseKind::Press(button), *pos, *modifiers)
                    } else {
                        // A release ends the gesture wherever the pointer is.
                        if self.mouse_down != Some(button) {
                            continue;
                        }
                        self.mouse_down = None;
                        (MouseKind::Release(button), *pos, *modifiers)
                    }
                }
                Event::PointerMoved(pos) => {
                    let kind = match self.mouse_down {
                        // A dialog opened mid-drag holds the pointer: the
                        // program sees no motion under it, only the release.
                        Some(_) if self.pointer_blocked => continue,
                        Some(button) => MouseKind::Drag(button),
                        None if rect.contains(*pos) && owns(*pos) => MouseKind::Move,
                        None => continue,
                    };
                    (kind, *pos, egui::Modifiers::NONE)
                }
                _ => continue,
            };
            let (row, col) = at(pos, &self.snapshot);
            // Motion is reported once per cell, not once per pixel.
            if matches!(kind, MouseKind::Drag(_) | MouseKind::Move)
                && self.mouse_cell == Some((col, row))
            {
                continue;
            }
            self.mouse_cell = Some((col, row));
            let event = MouseEvent {
                kind,
                col,
                row,
                mods: Mods {
                    shift: modifiers.shift,
                    alt: modifiers.alt,
                    ctrl: modifiers.ctrl,
                },
            };
            if let Some(bytes) = encode_mouse(event, modes) {
                out.push(bytes);
            }
        }
        out
    }

    /// Wheel lines over the terminal, as presses of the wheel buttons.
    fn wheel_bytes(
        &mut self,
        delta_y: f32,
        pos: Option<Pos2>,
        origin: Pos2,
        cell: CellMetrics,
    ) -> Vec<Vec<u8>> {
        if cell.height <= 0.0 {
            return Vec::new();
        }
        self.scroll_lines += delta_y / cell.height;
        let lines = self.scroll_lines.trunc() as i32;
        self.scroll_lines -= lines as f32;
        let (row, col) = selection::cell_at(
            pos.unwrap_or(origin) - origin,
            cell.width,
            cell.height,
            &self.snapshot,
        );
        let kind = if lines > 0 {
            MouseKind::WheelUp
        } else {
            MouseKind::WheelDown
        };
        (0..lines.unsigned_abs())
            .filter_map(|_| {
                encode_mouse(
                    MouseEvent {
                        kind,
                        col,
                        row,
                        mods: Mods::default(),
                    },
                    self.snapshot.modes,
                )
            })
            .collect()
    }

    fn send_mouse(&mut self, chunks: Vec<Vec<u8>>) {
        let Ok(session) = &self.session else { return };
        for bytes in chunks {
            if let Err(refused) = session.send_input(bytes) {
                self.set_notice(refused);
                return;
            }
        }
    }

    /// Dragging selects, unless the mouse goes to the program. The session
    /// holds the selection on the text, so a drag goes on across a scroll; held
    /// past the top or bottom edge it scrolls the history itself.
    fn handle_selection(
        &mut self,
        response: &egui::Response,
        origin: Pos2,
        cell: CellMetrics,
        ctx: &egui::Context,
    ) {
        let Ok(session) = self.session.clone() else {
            return;
        };
        let locate =
            |pos: Pos2| selection::cell_at(pos - origin, cell.width, cell.height, &self.snapshot);
        let command = ctx.input(|input| input.modifiers.command);
        if response.double_clicked() && !command {
            // The whole word, also when an agent broke it over rows.
            let word = response
                .interact_pointer_pos()
                .map(locate)
                .and_then(|at| words::word_at(&self.snapshot, at));
            self.clear_selection();
            if let Some(word) = word {
                self.unsent_clear = false;
                self.selection = Some(Selection {
                    unsent: Some(SelectOp::Start {
                        row: word.first.0,
                        col: word.first.1,
                    }),
                    unsent_extend: Some(word.last),
                });
                if !self.send_unsent_selection() {
                    ctx.request_repaint_after(CONTENDED_RETRY);
                }
            }
        } else if response.drag_started() {
            let press = ctx.input(|input| input.pointer.press_origin());
            self.pending_copy = false;
            // The new start replaces whatever the session holds.
            self.unsent_clear = false;
            self.selection = press
                .map(locate)
                .map(|(row, col)| Selection::new(SelectOp::Start { row, col }));
            self.send_unsent_selection();
        } else if response.dragged() {
            let Some(pos) = response.interact_pointer_pos() else {
                return;
            };
            if self.selection.is_none() {
                return;
            }
            let lines = selection::edge_scroll(pos.y - origin.y, cell.height, self.snapshot.rows);
            if lines != 0 && !self.snapshot.modes.alt_screen {
                // The alternate screen has no history to scroll into.
                let due = self
                    .edge_scrolled
                    .is_none_or(|at| at.elapsed() >= EDGE_SCROLL_EVERY);
                if due && session.scroll(lines) {
                    self.edge_scrolled = Some(Instant::now());
                }
                ctx.request_repaint_after(EDGE_SCROLL_EVERY);
            }
            if let Some(selection) = &mut self.selection {
                selection.unsent_extend = Some(locate(pos));
            }
            if !self.send_unsent_selection() {
                ctx.request_repaint_after(CONTENDED_RETRY);
            }
        } else if response.clicked() {
            self.clear_selection();
        }
    }

    /// The link or file under the pointer is underlined; with the link key
    /// held it shows a pointing hand and a click opens the link in the browser,
    /// or the file in the preview. Only the line under the pointer is read,
    /// and only while the pointer is over the screen.
    fn handle_link(
        &mut self,
        response: &egui::Response,
        painter: &egui::Painter,
        origin: Pos2,
        cell: CellMetrics,
        command: bool,
    ) {
        if !response.hovered() || cell.width <= 0.0 || cell.height <= 0.0 {
            return;
        }
        let Some(pos) = response.hover_pos() else {
            return;
        };
        let offset = pos - origin;
        let inside = offset.x >= 0.0
            && offset.y >= 0.0
            && offset.x < f32::from(self.snapshot.cols) * cell.width
            && offset.y < f32::from(self.snapshot.rows) * cell.height;
        if !inside {
            return;
        }
        let at = selection::cell_at(offset, cell.width, cell.height, &self.snapshot);
        let (spans, target) = if let Some(link) = links::link_at(&self.snapshot, at) {
            // An agent may have broken the URL over rows itself: the whole word
            // is the link when it is the longer URL.
            match words::word_at(&self.snapshot, at)
                .filter(|word| word.text.len() > link.url.len() && word.text.contains(&link.url))
                .and_then(|word| {
                    let start = word.text.find(&link.url)?;
                    let url = links::openable(&word.text[start..])?.to_owned();
                    Some((word.spans, url))
                }) {
                Some((spans, url)) => (spans, Target::Url(url)),
                None => (link.spans, Target::Url(link.url)),
            }
        } else if let Some((word, file)) = self.file_at(at) {
            (word.spans, Target::File(file))
        } else if let Some(word) = command.then(|| self.remote_file_at(at)).flatten() {
            let path = preview::clean_path(&word.text).to_owned();
            (word.spans, Target::Remote(path))
        } else {
            return;
        };
        let color = Color32::from_rgb(0x6c, 0xd5, 0xe6);
        for (row, start, end) in &spans {
            let y = origin.y + (f32::from(*row) + 1.0) * cell.height - 1.5;
            painter.hline(
                (origin.x + f32::from(*start) * cell.width)
                    ..=(origin.x + f32::from(*end) * cell.width),
                y,
                egui::Stroke::new(1.0, color),
            );
        }
        if command {
            response.ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            if response.clicked() {
                match target {
                    Target::Url(url) => response.ctx.open_url(egui::OpenUrl::new_tab(url)),
                    Target::File(file) => self.preview_requests.push(PreviewRequest::Local(file)),
                    Target::Remote(path) => {
                        self.preview_requests.push(PreviewRequest::Remote(path))
                    }
                }
            }
        } else {
            let verb = match target {
                Target::Url(_) => "open",
                Target::File(_) | Target::Remote(_) => "preview",
            };
            response
                .clone()
                .on_hover_text_at_pointer(format!("{}-click to {verb}", platform::LINK_KEY));
        }
    }

    /// The word at `at` when it names a file on this computer, and the file.
    fn file_at(&mut self, at: selection::CellPos) -> Option<(words::Word, std::path::PathBuf)> {
        if self.files != Files::Local {
            return None;
        }
        let word = words::word_at(&self.snapshot, at)?;
        // A path has a separator or an extension; a bare word is not looked up.
        if !word.text.contains(['/', '.', '\\']) {
            return None;
        }
        let file = match &self.path_lookup {
            Some((text, file)) if *text == word.text => file.clone(),
            _ => {
                let file = preview::resolve(&word.text, self.base_dir.as_deref());
                self.path_lookup = Some((word.text.clone(), file.clone()));
                file
            }
        }?;
        Some((word, file))
    }

    /// The word at `at` when it reads as a path on a remote host: one with a
    /// folder separator or a file extension. Whether it is a file there is
    /// asked only when it is clicked.
    fn remote_file_at(&self, at: selection::CellPos) -> Option<words::Word> {
        if self.files != Files::Remote {
            return None;
        }
        let word = words::word_at(&self.snapshot, at)?;
        preview::looks_like_path(&word.text).then_some(word)
    }

    /// A small copy button at the end of a finished selection. It copies the
    /// clean text, as the copy key does.
    /// `dragging` is a selection drag on the terminal under way: the button
    /// waits for it to end. A press on the button itself keeps it, so its
    /// click completes.
    fn draw_copy_button(
        &mut self,
        ui: &mut Ui,
        rect: Rect,
        origin: Pos2,
        cell: CellMetrics,
        dragging: bool,
    ) {
        let Some(range) = self.snapshot.selection else {
            return;
        };
        if self.selection.is_none() || dragging {
            return;
        }
        let Some(&(row, _, end)) = range.spans(self.snapshot.rows, self.snapshot.cols).last()
        else {
            return;
        };
        let size = Vec2::splat(22.0);
        let x = (origin.x + f32::from(end) * cell.width + 6.0).min(rect.max.x - size.x - 4.0);
        let y = (origin.y + f32::from(row) * cell.height + cell.height / 2.0 - size.y / 2.0)
            .clamp(rect.min.y + 2.0, rect.max.y - size.y - 2.0);
        let button = Rect::from_min_size(pos2(x, y), size);
        let response = ui
            .interact(button, self.id.with("copy-selection"), Sense::click())
            .on_hover_text(format!(
                "Copy, with wrapped lines joined and margins dropped\n{} copies as shown",
                platform::COPY_AS_SHOWN_KEYS
            ));
        let fill = if response.hovered() {
            theme::RAISED_BG
        } else {
            theme::POPOVER_BG
        };
        let painter = ui.painter();
        painter.rect_filled(button, 5.0, fill);
        painter.rect_stroke(
            button,
            5.0,
            egui::Stroke::new(1.0, theme::POPOVER_BORDER),
            egui::StrokeKind::Inside,
        );
        let color = if response.hovered() {
            theme::CYAN
        } else {
            theme::TEXT_MUTED
        };
        crate::icons::copy(painter, button.center(), color, fill);
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if response.clicked() && self.copy_selection(ui.ctx(), true) {
            self.tell("Copied".to_owned());
        }
    }

    /// Wheel movement scrolls history, or moves through a full-screen app,
    /// which has no history of its own.
    fn handle_scroll(&mut self, delta_y: f32, metrics: CellMetrics) {
        if metrics.height <= 0.0 {
            return;
        }
        self.scroll_lines += delta_y / metrics.height;
        let lines = self.scroll_lines.trunc() as i32;
        if lines == 0 {
            return;
        }
        self.scroll_lines -= lines as f32;
        let Ok(session) = &self.session else { return };
        if self.snapshot.modes.alt_screen {
            let bytes = scroll_keys(lines, self.snapshot.modes);
            if let Err(refused) = session.send_input(bytes) {
                self.set_notice(refused);
            }
        } else {
            session.scroll(lines);
        }
    }

    pub(crate) fn stage(&self) -> Option<PaneStage> {
        self.control.as_ref().map(|control| control.stage())
    }

    /// The line a static pane shows in its chip.
    pub(crate) fn status_line(&self) -> (String, Color32) {
        if self.emulator_failed {
            return ("terminal failed".to_owned(), ERROR_TEXT);
        }
        match self.stage() {
            Some(PaneStage::Idle) => return ("no project".to_owned(), HEADER_TEXT),
            Some(PaneStage::Stopped) if self.live() => {
                return ("stopping".to_owned(), WARNING_TEXT);
            }
            Some(PaneStage::Preparing { step }) => return (step, WARNING_TEXT),
            Some(PaneStage::NeedsLogin { auth }) => {
                return (auth_text(&auth).to_owned(), WARNING_TEXT);
            }
            Some(PaneStage::LoggingIn) => {
                return ("logging in".to_owned(), WARNING_TEXT);
            }
            Some(PaneStage::Failed { .. }) => {
                return ("failed to start".to_owned(), ERROR_TEXT);
            }
            Some(PaneStage::Running | PaneStage::Stopped) | None => {}
        }
        match &self.session {
            Err(_) => ("failed to start".to_owned(), ERROR_TEXT),
            Ok(session) => match session.status() {
                SessionStatus::Running => ("running".to_owned(), HEADER_TEXT),
                status @ SessionStatus::Exited {
                    code: 0,
                    signal: None,
                } => (status.to_string(), WARNING_TEXT),
                status => (status.to_string(), ERROR_TEXT),
            },
        }
    }

    /// Hook frames the bridge refused, as a warning for the header, if any.
    pub(crate) fn rejected_frames_warning(&self) -> Option<String> {
        let rejected = self
            .control
            .as_ref()
            .map_or(0, |control| control.rejected_hook_frames());
        (rejected > 0).then(|| format!("{rejected} hook frames refused"))
    }

    fn warnings(&self) -> Arc<Vec<String>> {
        self.control
            .as_ref()
            .map_or_else(Arc::default, |control| control.warnings())
    }

    #[cfg(test)]
    pub(crate) fn warning_lines(&self) -> usize {
        self.warnings().len()
    }

    /// Connecto is typing into this terminal by itself, and holds the user's
    /// keys until it is done.
    pub(crate) fn injecting(&self) -> bool {
        self.control
            .as_ref()
            .is_some_and(|control| control.injecting())
    }

    /// A message waits for the user to send or clear their line.
    pub(crate) fn message_waiting(&self) -> bool {
        self.control
            .as_ref()
            .is_some_and(|control| control.message_waiting())
    }

    /// A small tag at the top right: Connecto is typing (the keys typed now go
    /// in after), or a message waits for the user's line.
    fn draw_injecting(&self, painter: &egui::Painter, rect: Rect, text: &str) {
        let font = FontId::proportional(12.0);
        let galley = painter.layout_no_wrap(text.to_owned(), font, HEADER_TEXT);
        let size = galley.size() + Vec2::new(16.0, 8.0);
        let tag = Rect::from_min_size(
            pos2(
                rect.max.x - size.x - PANE_PADDING,
                rect.min.y + PANE_PADDING,
            ),
            size,
        );
        painter.rect_filled(tag, 4.0, theme::PANEL_BG);
        painter.rect_stroke(
            tag,
            4.0,
            egui::Stroke::new(1.0, theme::BORDER),
            egui::StrokeKind::Inside,
        );
        painter.galley(tag.min + Vec2::new(8.0, 4.0), galley, HEADER_TEXT);
    }

    pub(crate) fn live(&self) -> bool {
        self.control
            .as_ref()
            .is_some_and(|control| control.is_live())
    }

    /// Log in. Log in empties the tool's profile, so it asks first.
    fn draw_commands(&mut self, ui: &mut Ui, origin: Pos2, restart: bool) -> bool {
        let mut restarted = false;
        let Some(control) = self.control.clone() else {
            return false;
        };
        #[cfg(test)]
        {
            self.command_hits.clear();
            self.command_ids.clear();
        }
        let top = origin + Vec2::new(0.0, 28.0);
        if self.tool_off {
            self.login_armed = false;
            ui.painter().text(
                top,
                Align2::LEFT_TOP,
                TOOL_OFF_TEXT,
                FontId::monospace(12.0),
                WARNING_TEXT,
            );
            return false;
        }
        let first = Rect::from_min_size(top, BUTTON_SIZE);
        let second = first.translate(Vec2::new(BUTTON_SIZE.x + 8.0, 0.0));
        if self.login_armed {
            ui.painter().text(
                top + Vec2::new(0.0, BUTTON_SIZE.y + 8.0),
                Align2::LEFT_TOP,
                LOGIN_RESET_TEXT,
                FontId::monospace(12.0),
                WARNING_TEXT,
            );
            let confirm = ui.put(first, theme::primary("Confirm"));
            let cancel = ui.put(second, theme::button("Cancel"));
            #[cfg(test)]
            {
                self.command_hits.extend([confirm.rect, cancel.rect]);
                self.command_ids.extend([confirm.id, cancel.id]);
            }
            if confirm.clicked() {
                self.login_armed = false;
                control.request_login();
            }
            if cancel.clicked() {
                self.login_armed = false;
            }
        } else {
            // Starting is the header's Restart; only Log in lives here.
            let login = ui.put(first, theme::button("Log in"));
            #[cfg(test)]
            {
                self.command_hits.push(login.rect);
                self.command_ids.push(login.id);
            }
            if login.clicked() {
                self.login_armed = true;
            }
            if restart {
                let restart = ui.put(second, theme::primary("Restart"));
                #[cfg(test)]
                {
                    self.command_hits.push(restart.rect);
                    self.command_ids.push(restart.id);
                }
                restarted = restart.clicked();
            }
        }
        restarted
    }

    /// Draw the terminal into `rect`, the whole of the main area. `restart` is
    /// the state word of an exited or failed terminal, which gets a Restart
    /// button here; the result says it was clicked. `input` is the frame's events when the terminal
    /// may take the keyboard, and empty when something else has it.
    pub(crate) fn draw(
        &mut self,
        ui: &mut Ui,
        rect: Rect,
        cell: CellMetrics,
        input: &[Event],
        restart: Option<&str>,
    ) -> bool {
        let mut restarted = false;
        let ctx = ui.ctx().clone();
        let response = ui.interact(rect, self.id, Sense::click_and_drag());
        let had_focus = ctx.memory(|memory| memory.has_focus(self.id));
        if response.clicked() || response.drag_started() {
            response.request_focus();
            self.claim_size();
        }
        let focused = ctx.memory(|memory| memory.has_focus(self.id));
        // A question about pasted files is for a paste made while the
        // terminal had the keyboard: it goes with the keyboard.
        #[cfg(target_os = "macos")]
        if !focused {
            self.cancel_file_paste();
        }
        if focused {
            // Terminal apps need these keys; egui would use them to move focus.
            ctx.memory_mut(|memory| {
                memory.set_focus_lock_filter(
                    self.id,
                    EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                );
            });
            self.handle_input(input, &ctx, had_focus);
        }
        // What a busy emulator refused, also after the button is released.
        if !self.send_unsent_selection() {
            ctx.request_repaint_after(CONTENDED_RETRY);
        }
        if self.pending_copy && !self.copy_selection(&ctx, self.pending_clean) {
            self.pending_copy = false;
        }
        // Cmd (Ctrl off macOS) is the key for links, so a plain click still
        // selects, and still reaches a program that asked for the mouse.
        let (shift, command) = ctx.input(|input| (input.modifiers.shift, input.modifiers.command));
        let forward = self.forwards_mouse(shift) && !command;
        // The pointer is the terminal's only when nothing is over it: not a
        // dialog, a sheet, nor a card drawn on top of it.
        let owns_pointer = !self.pointer_blocked && response.contains_pointer();
        // Where an event happened, the same: decided for each press, not for
        // the whole frame's events.
        let blocked = self.pointer_blocked;
        let layer = ui.layer_id();
        let owns_at = |pos: Pos2| !blocked && ctx.layer_id_at(pos) == Some(layer);
        // The wheel goes to a program that asked for the mouse even in a local
        // terminal, where clicks still select: a full-screen app such as
        // Claude scrolls its own history with it, while Up and Down would walk
        // the prompt history instead.
        let wheel_to_program = forward || self.wheel_to_program(shift, command);
        if response.hovered() && owns_pointer && !forward {
            let delta = ui.input(|input| input.smooth_scroll_delta.y);
            if wheel_to_program {
                let at = ctx.input(|input| input.pointer.hover_pos());
                let origin = rect.min + Vec2::splat(PANE_PADDING);
                let chunks = self.wheel_bytes(delta, at, origin, cell);
                self.send_mouse(chunks);
            } else {
                self.handle_scroll(delta, cell);
            }
        }

        let painter = ui.painter_at(rect);
        let grid_origin = rect.min + Vec2::splat(PANE_PADDING);
        let warnings = self.warnings();
        let strip = warnings.len() as f32 * WARNING_LINE_HEIGHT;
        let grid_area = rect.size() - Vec2::splat(PANE_PADDING * 2.0) - Vec2::new(0.0, strip);
        if forward {
            let events = ctx.input(|input| input.events.clone());
            let mut chunks = self.mouse_bytes(&events, rect, grid_origin, cell, owns_at);
            if response.hovered() && owns_pointer {
                let delta = ui.input(|input| input.smooth_scroll_delta.y);
                let at = ctx.input(|input| input.pointer.hover_pos());
                chunks.extend(self.wheel_bytes(delta, at, grid_origin, cell));
            }
            self.send_mouse(chunks);
        } else {
            self.mouse_down = None;
            self.handle_selection(&response, grid_origin, cell, &ctx);
        }

        self.apply_size(cell.grid_size(grid_area), &ctx);
        self.refresh(&ctx);

        let bg = self.snapshot.default_bg;
        painter.rect_filled(rect, 0.0, Color32::from_rgb(bg.r, bg.g, bg.b));

        let message = self.stage().as_ref().and_then(stage_message);
        match &self.session {
            Ok(_) if message.is_some() => {
                if let Some((text, color, offers_commands)) = message {
                    // A refusal is long and must be read whole.
                    let galley = painter.layout(text, FontId::monospace(12.0), color, grid_area.x);
                    let rows = galley.rows.len().max(1) as f32;
                    let extra_rows = galley.size().y * (rows - 1.0) / rows;
                    painter.galley(grid_origin, galley, color);
                    if offers_commands {
                        restarted = self.draw_commands(
                            ui,
                            grid_origin + Vec2::new(0.0, extra_rows),
                            restart.is_some(),
                        );
                    }
                }
            }
            Ok(_) => {
                render::paint(&painter, grid_origin, &self.snapshot, cell, focused);
                self.handle_link(&response, &painter, grid_origin, cell, command);
                if let Some(word) = restart {
                    restarted = self.draw_restart_bar(ui, rect, word);
                }
                if let Some(range) = self.snapshot.selection {
                    for (row, start, end) in range.spans(self.snapshot.rows, self.snapshot.cols) {
                        let min = grid_origin
                            + Vec2::new(
                                f32::from(start) * cell.width,
                                f32::from(row) * cell.height,
                            );
                        let size = Vec2::new(f32::from(end - start) * cell.width, cell.height);
                        painter.rect_filled(Rect::from_min_size(min, size), 0.0, SELECTION_FILL);
                    }
                }
                self.draw_copy_button(ui, rect, grid_origin, cell, response.dragged());
            }
            Err(reason) => {
                painter.text(
                    grid_origin,
                    Align2::LEFT_TOP,
                    reason,
                    FontId::monospace(12.0),
                    ERROR_TEXT,
                );
            }
        }

        if strip > 0.0 {
            let top = rect.max.y - strip - PANE_PADDING / 2.0;
            let area = Rect::from_min_max(Pos2::new(rect.min.x, top), rect.max);
            painter.rect_filled(area, 0.0, theme::PANEL_BG);
            for (index, line) in warnings.iter().enumerate() {
                painter.text(
                    Pos2::new(grid_origin.x, top + index as f32 * WARNING_LINE_HEIGHT),
                    Align2::LEFT_TOP,
                    line,
                    FontId::proportional(12.0),
                    WARNING_TEXT,
                );
            }
        }

        let injecting = self.live() && self.injecting();
        #[cfg(test)]
        {
            self.drew_injecting = injecting;
        }
        let waiting = !injecting && self.live() && self.message_waiting();
        #[cfg(test)]
        {
            self.drew_waiting = waiting;
        }
        if injecting || waiting {
            let text = if injecting {
                INJECTING_TEXT
            } else {
                WAITING_TEXT
            };
            self.draw_injecting(&painter, rect, text);
            // Watched until it ends: the session wakes the UI at both ends, and
            // this covers a frame drawn just before the end.
            ctx.request_repaint_after(INJECTING_REPAINT);
        }

        #[cfg(target_os = "macos")]
        let asking = self.draw_file_paste(&ctx, &painter, rect);
        #[cfg(not(target_os = "macos"))]
        let asking = false;
        if asking {
            // The question is over the notice's place.
        } else if let Some((notice, since)) = &self.notice {
            let remaining = NOTICE_LIFETIME.saturating_sub(since.elapsed());
            if remaining.is_zero() {
                self.notice = None;
            } else {
                painter.text(
                    rect.center_bottom() - Vec2::new(0.0, 8.0),
                    Align2::CENTER_BOTTOM,
                    notice,
                    FontId::proportional(12.0),
                    WARNING_TEXT,
                );
                ctx.request_repaint_after(remaining);
            }
        }
        restarted
    }

    /// The strip over the bottom of a terminal that ended: its state and
    /// Restart, the one way back.
    fn draw_restart_bar(&mut self, ui: &mut Ui, rect: Rect, word: &str) -> bool {
        let bar = Rect::from_min_max(pos2(rect.min.x, rect.max.y - 38.0), rect.max);
        let painter = ui.painter_at(rect);
        painter.rect_filled(bar, 0.0, theme::PANEL_BG);
        painter.hline(
            bar.x_range(),
            bar.min.y,
            egui::Stroke::new(1.0, theme::BORDER),
        );
        let color = if word == "Failed" {
            ERROR_TEXT
        } else {
            HEADER_TEXT
        };
        painter.text(
            pos2(bar.min.x + 14.0, bar.center().y),
            Align2::LEFT_CENTER,
            word,
            FontId::proportional(12.5),
            color,
        );
        let button = Rect::from_center_size(
            pos2(bar.max.x - 14.0 - 40.0, bar.center().y),
            Vec2::new(80.0, 24.0),
        );
        let response = ui.put(button, theme::primary("Restart"));
        #[cfg(test)]
        {
            self.command_hits = vec![response.rect];
            self.command_ids = vec![response.id];
        }
        response.clicked()
    }
}

const fn auth_text(auth: &AuthState) -> &'static str {
    match auth {
        AuthState::LoggedIn { .. } => "logged in",
        AuthState::LoggedOut => "not logged in",
        AuthState::Unknown => "login status unknown",
    }
}

/// What a pane without a terminal shows, and whether it offers commands.
fn stage_message(stage: &PaneStage) -> Option<(String, Color32, bool)> {
    match stage {
        PaneStage::Preparing { step } => Some((format!("{step}…"), HEADER_TEXT, false)),
        PaneStage::Idle => Some(("Choose a project to start.".to_owned(), HEADER_TEXT, false)),
        PaneStage::Stopped => Some(("Stopped.".to_owned(), HEADER_TEXT, true)),
        PaneStage::NeedsLogin { auth } => Some((
            match auth {
                AuthState::Unknown => {
                    "Login status unknown. The check failed; this does not mean logged out."
                }
                _ => "Not logged in. The session starts after you log in.",
            }
            .to_owned(),
            WARNING_TEXT,
            true,
        )),
        PaneStage::Failed { reason } => Some((reason.clone(), ERROR_TEXT, true)),
        PaneStage::LoggingIn | PaneStage::Running => None,
    }
}

/// What a click on underlined text opens.
enum Target {
    Url(String),
    File(std::path::PathBuf),
    /// A path on the remote host the terminal runs on, as printed.
    Remote(String),
}

/// Where a terminal's files are, for a preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Files {
    Local,
    /// On the remote host the terminal runs on: read through it.
    Remote,
    /// Nowhere a preview can reach.
    None,
}

/// A file clicked for a preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PreviewRequest {
    Local(std::path::PathBuf),
    /// A path as the remote terminal printed it, read from its host.
    Remote(String),
}

/// Copy as shown: Cmd+Shift+C (Ctrl+Shift+C off macOS), with a selection.
fn is_copy_as_shown(event: &Event) -> bool {
    matches!(
        event,
        Event::Key {
            key: egui::Key::C,
            pressed: true,
            modifiers,
            ..
        } if platform::is_copy_as_shown(modifiers)
    )
}

pub(crate) fn scroll_keys(lines: i32, modes: InputModes) -> Vec<u8> {
    let key = if lines > 0 { Key::Up } else { Key::Down };
    let press = KeyPress {
        key,
        mods: Mods::default(),
    };
    let sequence = encode_key(press, modes).unwrap_or_default();
    sequence.repeat(lines.unsigned_abs() as usize)
}

/// Cmd+A, the Mac select-all (Ctrl+Shift+A off macOS, see
/// `platform::is_select_all`). Ctrl+A stays a keystroke for the shell.
fn is_select_all(event: &Event) -> bool {
    matches!(
        event,
        Event::Key {
            key: egui::Key::A,
            pressed: true,
            modifiers,
            ..
        } if platform::is_select_all(modifiers)
    )
}

/// The key that activated the focused control this pass: Enter or Space.
pub(crate) fn activating_key(ui: &Ui, focused: bool) -> Option<egui::Key> {
    if !focused {
        return None;
    }
    [egui::Key::Enter, egui::Key::Space]
        .into_iter()
        .find(|key| ui.input(|input| input.key_pressed(*key)))
}

/// Take out the one press of `key`, and the one release that matches it, which
/// activated a control. For Space, also the one `Text(" ")` the system sends
/// with that press, before or after it. Everything else stays, including a
/// later press or a second space the user really types.
pub(crate) fn drop_activation(events: Vec<Event>, key: egui::Key) -> Vec<Event> {
    let (mut press, mut release) = (true, true);
    let mut text = key == egui::Key::Space;
    events
        .into_iter()
        .filter(|event| match event {
            Event::Key {
                key: event_key,
                pressed,
                ..
            } if *event_key == key => {
                let pending = if *pressed { &mut press } else { &mut release };
                !std::mem::take(pending)
            }
            Event::Text(typed) if text && typed == " " => !std::mem::take(&mut text),
            _ => true,
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    struct Quiet;

    impl TerminalSession for Quiet {
        fn poll_snapshot(&self, _out: &mut TermSnapshot) -> SnapshotOutcome {
            SnapshotOutcome::Unchanged
        }
        fn send_input(&self, _bytes: Vec<u8>) -> Result<(), InputRefused> {
            Ok(())
        }
        fn resize(&self, _size: GridSize) -> ResizeOutcome {
            ResizeOutcome::Applied
        }
        fn scroll(&self, _lines: i32) -> bool {
            true
        }
        fn status(&self) -> SessionStatus {
            SessionStatus::Running
        }
        fn forwards_mouse(&self) -> bool {
            true
        }
    }

    /// A local terminal: the default `forwards_mouse`, so clicks select.
    struct Local;

    impl TerminalSession for Local {
        fn poll_snapshot(&self, _out: &mut TermSnapshot) -> SnapshotOutcome {
            SnapshotOutcome::Unchanged
        }
        fn send_input(&self, _bytes: Vec<u8>) -> Result<(), InputRefused> {
            Ok(())
        }
        fn resize(&self, _size: GridSize) -> ResizeOutcome {
            ResizeOutcome::Applied
        }
        fn scroll(&self, _lines: i32) -> bool {
            true
        }
        fn status(&self) -> SessionStatus {
            SessionStatus::Running
        }
    }

    fn mouse_pane(modes: InputModes) -> Pane {
        let mut pane = Pane::new(
            "t",
            crate::app::PaneConfig {
                label: "remote".into(),
                session: Ok(Arc::new(Quiet)),
                control: None,
                tool: None,
            },
        );
        pane.snapshot.cols = 80;
        pane.snapshot.rows = 24;
        pane.snapshot.modes = modes;
        pane
    }

    fn cell() -> CellMetrics {
        CellMetrics {
            width: 10.0,
            height: 20.0,
        }
    }

    fn press(pos: Pos2, pressed: bool) -> Event {
        Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn a_click_and_a_drag_become_the_programs_mouse_reports() {
        let modes = InputModes {
            mouse_drag: true,
            mouse_sgr: true,
            ..InputModes::default()
        };
        let mut pane = mouse_pane(modes);
        let rect = Rect::from_min_size(pos2(0.0, 0.0), Vec2::new(800.0, 480.0));
        let origin = pos2(0.0, 0.0);
        let bytes = pane.mouse_bytes(
            &[
                press(pos2(35.0, 45.0), true),
                Event::PointerMoved(pos2(36.0, 46.0)),
                Event::PointerMoved(pos2(75.0, 85.0)),
                press(pos2(75.0, 85.0), false),
            ],
            rect,
            origin,
            cell(),
            |_| true,
        );
        let text: Vec<String> = bytes
            .into_iter()
            .map(|b| String::from_utf8(b).unwrap())
            .collect();
        // Press at cell (3,2); the one-pixel move is the same cell and is not
        // reported; the drag to (7,4); then the release.
        assert_eq!(text, ["\x1b[<0;4;3M", "\x1b[<32;8;5M", "\x1b[<0;8;5m"]);
    }

    /// A dialog that opens while the button is held stops the drag's motion
    /// reaching the program; the release still ends the gesture.
    #[test]
    fn a_dialog_opened_mid_drag_stops_its_motion_but_not_its_release() {
        let mut pane = mouse_pane(InputModes {
            mouse_drag: true,
            mouse_sgr: true,
            ..InputModes::default()
        });
        let rect = Rect::from_min_size(pos2(0.0, 0.0), Vec2::new(800.0, 480.0));
        let origin = pos2(0.0, 0.0);
        let text = |bytes: Vec<Vec<u8>>| -> Vec<String> {
            bytes
                .into_iter()
                .map(|b| String::from_utf8(b).unwrap())
                .collect()
        };
        let pressed = pane.mouse_bytes(
            &[press(pos2(35.0, 45.0), true)],
            rect,
            origin,
            cell(),
            |_| true,
        );
        assert_eq!(text(pressed), ["\x1b[<0;4;3M"]);
        pane.pointer_blocked = true;
        let rest = pane.mouse_bytes(
            &[
                Event::PointerMoved(pos2(75.0, 85.0)),
                press(pos2(75.0, 85.0), false),
            ],
            rect,
            origin,
            cell(),
            |_| false,
        );
        assert_eq!(text(rest), ["\x1b[<0;8;5m"], "only the release");
    }

    #[test]
    fn a_press_outside_the_terminal_and_a_plain_move_are_not_reported() {
        let mut pane = mouse_pane(InputModes {
            mouse_click: true,
            mouse_sgr: true,
            ..InputModes::default()
        });
        let rect = Rect::from_min_size(pos2(100.0, 100.0), Vec2::new(400.0, 300.0));
        let bytes = pane.mouse_bytes(
            &[
                press(pos2(10.0, 10.0), true),
                Event::PointerMoved(pos2(200.0, 200.0)),
                press(pos2(10.0, 10.0), false),
            ],
            rect,
            pos2(100.0, 100.0),
            cell(),
            |_| true,
        );
        assert!(bytes.is_empty(), "{bytes:?}");
    }

    #[test]
    fn the_wheel_goes_to_the_program_one_report_per_line() {
        let mut pane = mouse_pane(InputModes {
            mouse_click: true,
            mouse_sgr: true,
            ..InputModes::default()
        });
        let up = pane.wheel_bytes(45.0, Some(pos2(5.0, 5.0)), pos2(0.0, 0.0), cell());
        assert_eq!(up, vec![b"\x1b[<64;1;1M".to_vec(); 2]);
        let down = pane.wheel_bytes(-40.0, Some(pos2(5.0, 5.0)), pos2(0.0, 0.0), cell());
        assert!(!down.is_empty());
        assert!(down.iter().all(|b| b == b"\x1b[<65;1;1M"));
    }

    #[test]
    fn only_a_remote_terminal_with_mouse_reporting_on_takes_the_mouse() {
        let on = InputModes {
            mouse_click: true,
            ..InputModes::default()
        };
        assert!(mouse_pane(on).forwards_mouse(false));
        assert!(!mouse_pane(on).forwards_mouse(true), "Shift selects");
        assert!(!mouse_pane(InputModes::default()).forwards_mouse(false));
    }

    #[test]
    fn the_wheel_goes_to_a_program_that_asked_for_the_mouse_even_locally() {
        let mut pane = mouse_pane(InputModes {
            alt_screen: true,
            mouse_click: true,
            mouse_sgr: true,
            ..InputModes::default()
        });
        pane.session = Ok(Arc::new(Local));
        assert!(!pane.forwards_mouse(false), "clicks still select locally");
        assert!(pane.wheel_to_program(false, false));
        assert!(!pane.wheel_to_program(true, false), "Shift keeps the wheel");
        assert!(!pane.wheel_to_program(false, true));
        let plain = mouse_pane(InputModes {
            alt_screen: true,
            ..InputModes::default()
        });
        assert!(!plain.wheel_to_program(false, false), "no mouse: arrows");
    }

    #[test]
    fn scrolling_up_in_a_full_screen_app_sends_up_arrows() {
        let bytes = scroll_keys(2, InputModes::default());
        assert_eq!(bytes, b"\x1b[A\x1b[A");
    }

    #[test]
    fn scrolling_down_uses_the_application_cursor_sequence_when_set() {
        let modes = InputModes {
            app_cursor: true,
            ..InputModes::default()
        };
        assert_eq!(scroll_keys(-1, modes), b"\x1bOB");
    }
}
