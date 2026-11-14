//! One terminal's screen, selection and input, and how it is drawn.
//!
//! The window keeps a `Pane` for every terminal of every open project, so a
//! background session keeps its size, selection and scroll. Only the terminal
//! on show is polled, resized and typed into.

use std::sync::Arc;
use std::time::{Duration, Instant};

use connecto_core::{
    AuthState, GridSize, InputModes, InputRefused, Key, KeyPress, Mods, MouseButton, MouseEvent,
    MouseKind, PaneControl, PaneStage, ResizeOutcome, SessionStatus, SnapshotOutcome, TermSnapshot,
    TerminalEntry, TerminalSession, ToolKind, encode_key, encode_mouse,
};
use egui::{Align2, Color32, Event, EventFilter, FontId, Id, Pos2, Rect, Sense, Ui, Vec2, pos2};

use crate::keymap;
use crate::links;
use crate::metrics::PaneCounters;
use crate::platform;
use crate::render::{self, CellMetrics};
use crate::selection::Selection;
use crate::theme;

pub(crate) const PANE_PADDING: f32 = 8.0;
pub(crate) const BUTTON_SIZE: Vec2 = Vec2::new(92.0, 24.0);
const NOTICE_LIFETIME: Duration = Duration::from_secs(3);
const CONTENDED_RETRY: Duration = Duration::from_millis(16);
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
    /// The first click on Log in happened: the next one confirms.
    pub(crate) login_armed: bool,
    /// The project's settings turned this terminal's tool off: it cannot be
    /// logged in or restarted.
    pub(crate) tool_off: bool,
    pub(crate) applied_size: Option<GridSize>,
    pub(crate) emulator_failed: bool,
    pub(crate) notice: Option<(String, Instant)>,
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
    #[cfg(test)]
    pub(crate) command_ids: Vec<Id>,
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
            login_armed: false,
            tool_off: false,
            applied_size: None,
            emulator_failed: false,
            notice: None,
            scroll_lines: 0.0,
            mouse_down: None,
            mouse_cell: None,
            contended_streak: 0,
            counters: PaneCounters::default(),
            #[cfg(test)]
            command_hits: Vec::new(),
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
        let Ok(session) = &self.session else { return };
        self.selection = None;
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

    /// Files dropped on the terminal: their escaped paths go in as a paste, so
    /// a shell or an agent CLI takes them as typed input. Nothing is run.
    pub(crate) fn drop_files(&mut self, paths: &[std::path::PathBuf]) {
        if let Some(text) = keymap::dropped_paths_text(paths) {
            let bytes = connecto_core::encode_paste(&text, self.snapshot.modes);
            self.send(bytes);
        }
    }

    fn handle_input(&mut self, events: &[Event], ctx: &egui::Context) {
        let modes = self.snapshot.modes;
        for event in events {
            if matches!(event, Event::Copy) {
                // The only clipboard write there is. It follows a key press,
                // and session output cannot reach it.
                if let Some(selection) = self.selection {
                    ctx.copy_text(selection.text(&self.snapshot));
                    if platform::COPY_CLEARS_SELECTION {
                        self.selection = None;
                    }
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
                // The visible screen, as the snapshot holds nothing above it.
                self.selection = Selection::all(&self.snapshot);
            } else if let Some(bytes) = keymap::translate(event, modes) {
                self.send(bytes);
            }
        }
    }

    /// Whether the mouse goes to the program: the terminal is a remote one, the
    /// program turned mouse reporting on, and Shift (the usual way to select
    /// text anyway) is not held.
    fn forwards_mouse(&self, shift: bool) -> bool {
        !shift
            && self.snapshot.modes.mouse_reporting()
            && self.session.as_ref().is_ok_and(|s| s.forwards_mouse())
    }

    /// The bytes for this frame's pointer events, in order. `rect` is the
    /// terminal's area and `origin` the top-left of its grid.
    fn mouse_bytes(
        &mut self,
        events: &[Event],
        rect: Rect,
        origin: Pos2,
        cell: CellMetrics,
    ) -> Vec<Vec<u8>> {
        let modes = self.snapshot.modes;
        let at = |pos: Pos2, snapshot: &TermSnapshot| {
            Selection::cell_at(pos - origin, cell.width, cell.height, snapshot)
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
                        // Only a press on the terminal starts a gesture.
                        if !rect.contains(*pos) {
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
                        Some(button) => MouseKind::Drag(button),
                        None if rect.contains(*pos) => MouseKind::Move,
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
        let (row, col) = Selection::cell_at(
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

    /// Dragging selects, unless the mouse goes to the program.
    fn handle_selection(
        &mut self,
        response: &egui::Response,
        origin: Pos2,
        cell: CellMetrics,
        ctx: &egui::Context,
    ) {
        let locate =
            |pos: Pos2| Selection::cell_at(pos - origin, cell.width, cell.height, &self.snapshot);
        if response.drag_started() {
            let press = ctx.input(|input| input.pointer.press_origin());
            self.selection = press.map(locate).map(Selection::at);
        } else if response.dragged() {
            let head = response.interact_pointer_pos().map(locate);
            if let (Some(selection), Some(head)) = (&mut self.selection, head) {
                selection.head = head;
            }
        } else if response.clicked() {
            self.selection = None;
        }
    }

    /// The link under the pointer is underlined; with the link key held it
    /// shows a pointing hand and a click opens it. Only the line under the
    /// pointer is read, and only while the pointer is over the screen.
    fn handle_link(
        &self,
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
        let at = Selection::cell_at(offset, cell.width, cell.height, &self.snapshot);
        let Some(link) = links::link_at(&self.snapshot, at) else {
            return;
        };
        let color = Color32::from_rgb(0x6c, 0xd5, 0xe6);
        for (row, start, end) in &link.spans {
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
                response.ctx.open_url(egui::OpenUrl::new_tab(link.url));
            }
        } else {
            response
                .clone()
                .on_hover_text_at_pointer(format!("{}-click to open", platform::LINK_KEY));
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
        if response.clicked() || response.drag_started() {
            response.request_focus();
        }
        let focused = ctx.memory(|memory| memory.has_focus(self.id));
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
            self.handle_input(input, &ctx);
        }
        // Cmd (Ctrl off macOS) is the key for links, so a plain click still
        // selects, and still reaches a program that asked for the mouse.
        let (shift, command) = ctx.input(|input| (input.modifiers.shift, input.modifiers.command));
        let forward = self.forwards_mouse(shift) && !command;
        if response.hovered() && !forward {
            let delta = ui.input(|input| input.smooth_scroll_delta.y);
            self.handle_scroll(delta, cell);
        }

        let painter = ui.painter_at(rect);
        let grid_origin = rect.min + Vec2::splat(PANE_PADDING);
        let warnings = self.warnings();
        let strip = warnings.len() as f32 * WARNING_LINE_HEIGHT;
        let grid_area = rect.size() - Vec2::splat(PANE_PADDING * 2.0) - Vec2::new(0.0, strip);
        if forward {
            let events = ctx.input(|input| input.events.clone());
            let mut chunks = self.mouse_bytes(&events, rect, grid_origin, cell);
            if response.hovered() {
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
                if let Some(selection) = self.selection {
                    for (row, start, end) in selection.spans(&self.snapshot) {
                        let min = grid_origin
                            + Vec2::new(
                                f32::from(start) * cell.width,
                                f32::from(row) * cell.height,
                            );
                        let size = Vec2::new(f32::from(end - start) * cell.width, cell.height);
                        painter.rect_filled(Rect::from_min_size(min, size), 0.0, SELECTION_FILL);
                    }
                }
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

        if let Some((notice, since)) = &self.notice {
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
        );
        let text: Vec<String> = bytes
            .into_iter()
            .map(|b| String::from_utf8(b).unwrap())
            .collect();
        // Press at cell (3,2); the one-pixel move is the same cell and is not
        // reported; the drag to (7,4); then the release.
        assert_eq!(text, ["\x1b[<0;4;3M", "\x1b[<32;8;5M", "\x1b[<0;8;5m"]);
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
