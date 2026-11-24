//! A file named in the terminal, previewed in the window: Cmd-click (Ctrl off
//! macOS) a path such as `docs/plan.md` or `/tmp/shot.png`. Markdown is drawn
//! as a document, images are shown, other text is shown as it is (selectable).
//! The file is read off the UI thread; nothing in it is run or fetched. A
//! remote host's terminal has its files read through the host, and such a
//! file can be downloaded to this computer.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use connecto_core::TerminalsControl;
use egui::{
    Align2, Color32, ColorImage, Context, FontId, Frame, Id, Margin, Order, Rect, RichText, Sense,
    Stroke, TextureHandle, TextureOptions, Ui, UiBuilder, pos2, vec2,
};

use crate::{icons, markdown, theme};

/// Text past this is cut, so a huge log does not stall the window.
const MAX_TEXT: u64 = 4 * 1024 * 1024;
/// Image files past this are not decoded.
const MAX_IMAGE: u64 = 64 * 1024 * 1024;
/// Nor are images of more pixels than this (a 64-megapixel photo).
const MAX_PIXELS: u64 = 64_000_000;
/// The longest path looked up.
const MAX_PATH: usize = 4096;
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "ico"];
const MARKDOWN_EXTENSIONS: &[&str] = &["md", "markdown", "mdx", "mdown"];
/// How long a download's outcome shows.
const NOTICE: Duration = Duration::from_secs(5);

/// What the file turned out to hold.
enum Content {
    Loading,
    Markdown(String),
    Text {
        text: String,
        cut: bool,
    },
    /// The image as shown (scaled down to fit a texture) and its own size.
    Image(Arc<ColorImage>, [u32; 2]),
    Failed(String),
}

/// What the reading thread fills in.
struct State {
    content: Content,
    /// The file's full path, as the machine it is on names it. A remote
    /// file's is the path as printed until its host answers.
    path: String,
    /// A remote file's bytes, kept for a download.
    bytes: Option<Arc<Vec<u8>>>,
}

/// An open preview.
pub(crate) struct Preview {
    state: Arc<Mutex<State>>,
    /// The file is on a remote host, so it can be downloaded.
    remote: bool,
    texture: Option<TextureHandle>,
    /// Markdown can be read as its source too.
    show_source: bool,
    /// What the last download did, and when: filled in by the thread that
    /// saves it.
    notice: Arc<Mutex<Option<(String, Instant)>>>,
}

/// What the preview asks of the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreviewAction {
    Close,
}

impl Preview {
    /// Start reading `path` on this computer, waking `ctx` when it is ready.
    pub(crate) fn open(ctx: &Context, path: PathBuf) -> Self {
        let shown = path.display().to_string();
        Self::start(ctx, shown, false, move |max_side| {
            (load(&path, max_side), None, None)
        })
    }

    /// Start reading `path`, as remote terminal `terminal` printed it, from
    /// the host it runs on.
    pub(crate) fn open_remote(
        ctx: &Context,
        terminals: Arc<dyn TerminalsControl>,
        terminal: String,
        path: String,
    ) -> Self {
        Self::start(ctx, path.clone(), true, move |max_side| {
            match terminals.fetch_file(&terminal, &path) {
                Ok((full, bytes)) => {
                    let content = decode(&full, &bytes, max_side);
                    (content, Some(full), Some(Arc::new(bytes)))
                }
                Err(why) => (
                    Content::Failed(if why.contains("project folder") {
                        format!(
                            "The file could not be read: {why}. Only files in the terminal's project folder are read from here: for another file, ask the agent there to run `connecto send-file <path>`, and it is offered here to download."
                        )
                    } else {
                        format!("The file could not be read: {why}.")
                    }),
                    None,
                    None,
                ),
            }
        })
    }

    fn start(
        ctx: &Context,
        path: String,
        remote: bool,
        read: impl FnOnce(usize) -> (Content, Option<String>, Option<Arc<Vec<u8>>>) + Send + 'static,
    ) -> Self {
        let state = Arc::new(Mutex::new(State {
            content: Content::Loading,
            path,
            bytes: None,
        }));
        let (slot, ctx) = (Arc::clone(&state), ctx.clone());
        let max_side = ctx.input(|input| input.max_texture_side);
        let spawned = std::thread::Builder::new()
            .name("connecto-preview".to_owned())
            .spawn(move || {
                let (content, path, bytes) = read(max_side);
                let mut state = slot.lock().unwrap_or_else(PoisonError::into_inner);
                state.content = content;
                if let Some(path) = path {
                    state.path = path;
                }
                state.bytes = bytes;
                drop(state);
                ctx.request_repaint();
            });
        if spawned.is_err() {
            state.lock().unwrap_or_else(PoisonError::into_inner).content =
                Content::Failed("The file could not be read.".to_owned());
        }
        Self {
            state,
            remote,
            texture: None,
            show_source: false,
            notice: Arc::default(),
        }
    }

    /// The file's path and whether it can be downloaded, for a test.
    #[cfg(test)]
    pub(crate) fn describe(&self) -> (String, bool) {
        let state = self.state();
        (state.path.clone(), state.bytes.is_some())
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The kind of preview a file gets, from its name.
fn is_image(path: &str) -> bool {
    has_extension(path, IMAGE_EXTENSIONS)
}

fn is_markdown(path: &str) -> bool {
    has_extension(path, MARKDOWN_EXTENSIONS)
}

fn has_extension(path: &str, list: &[&str]) -> bool {
    split_path(path)
        .0
        .rsplit_once('.')
        .is_some_and(|(_, ext)| list.iter().any(|known| ext.eq_ignore_ascii_case(known)))
}

/// A path's file name and its folder, split at its last `/` or `\`: a remote
/// host's path may be another system's.
fn split_path(path: &str) -> (&str, &str) {
    match path.rfind(['/', '\\']) {
        Some(at) if at + 1 < path.len() => (&path[at + 1..], &path[..at.max(1)]),
        _ => (path, ""),
    }
}

/// Decode an image no larger than [`MAX_PIXELS`], scaled down to fit a
/// texture of `max_side` points a side.
fn load_image(bytes: &[u8], max_side: usize) -> Content {
    let failed = |error: image::ImageError| {
        Content::Failed(format!("The image could not be decoded: {error}."))
    };
    let reader = || {
        image::ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .map_err(image::ImageError::IoError)
    };
    // The size is read from the header before anything is decoded.
    let (width, height) = match reader().and_then(image::ImageReader::into_dimensions) {
        Ok(size) => size,
        Err(error) => return failed(error),
    };
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Content::Failed("The image is too large to preview.".to_owned());
    }
    let mut reader = match reader() {
        Ok(reader) => reader,
        Err(error) => return failed(error),
    };
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(width);
    limits.max_image_height = Some(height);
    limits.max_alloc = Some(MAX_PIXELS * 8);
    reader.limits(limits);
    let mut decoded = match reader.decode() {
        Ok(decoded) => decoded,
        Err(error) => return failed(error),
    };
    let max_side = u32::try_from(max_side.max(1)).unwrap_or(u32::MAX);
    if decoded.width() > max_side || decoded.height() > max_side {
        decoded = decoded.resize(max_side, max_side, image::imageops::FilterType::Triangle);
    }
    let rgba = decoded.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Content::Image(
        Arc::new(ColorImage::from_rgba_unmultiplied(size, rgba.as_raw())),
        [width, height],
    )
}

/// Read a file on this computer: an image whole, text up to [`MAX_TEXT`].
fn load(path: &Path, max_side: usize) -> Content {
    let size = match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => meta.len(),
        Ok(_) => return Content::Failed("This is not a file.".to_owned()),
        Err(error) => return Content::Failed(format!("The file could not be read: {error}.")),
    };
    let name = path.display().to_string();
    let image = is_image(&name);
    if image && size > MAX_IMAGE {
        return Content::Failed("The image is too large to preview.".to_owned());
    }
    // Text one byte past the limit, so a longer file is known to be cut.
    let limit = if image { MAX_IMAGE } else { MAX_TEXT + 1 };
    let mut bytes = Vec::new();
    let read = std::fs::File::open(path).and_then(|file| file.take(limit).read_to_end(&mut bytes));
    if let Err(error) = read {
        return Content::Failed(format!("The file could not be read: {error}."));
    }
    decode(&name, &bytes, max_side)
}

/// What a file's bytes show as, by its name and what they hold. Text past
/// [`MAX_TEXT`] is cut.
fn decode(name: &str, bytes: &[u8], max_side: usize) -> Content {
    if is_image(name) {
        if bytes.len() as u64 > MAX_IMAGE {
            return Content::Failed("The image is too large to preview.".to_owned());
        }
        return load_image(bytes, max_side);
    }
    let cut = bytes.len() as u64 > MAX_TEXT;
    let shown = &bytes[..bytes
        .len()
        .min(usize::try_from(MAX_TEXT).unwrap_or(usize::MAX))];
    if shown.contains(&0) {
        return Content::Failed("This is a binary file, which has no preview.".to_owned());
    }
    let text = match std::str::from_utf8(shown) {
        Ok(text) => text.to_owned(),
        // A cut can split a character; anything else is not text.
        Err(error) if cut && error.error_len().is_none() => {
            String::from_utf8_lossy(&shown[..error.valid_up_to()]).into_owned()
        }
        Err(_) => {
            return Content::Failed("This file is not text, so it has no preview.".to_owned());
        }
    };
    if is_markdown(name) && !cut {
        Content::Markdown(text)
    } else {
        Content::Text { text, cut }
    }
}

/// A word from the terminal as a path: without `file://`, trailing
/// punctuation, and a `:line[:col]` or `#L…` after the name, as tools print
/// them.
pub(crate) fn clean_path(word: &str) -> &str {
    let word = word.strip_prefix("file://").unwrap_or(word);
    let word = word.trim_end_matches(['.', ',', ':', ';']);
    let word = word.split_once("#L").map_or(word, |(path, _)| path);
    strip_position(word)
}

/// Whether a word reads as a file's path, for a host that is asked only when
/// it is clicked: a path with a folder separator, or a name with an
/// extension (`notes.md`). Not a URL, a version (`v1.2`) or a share.
pub(crate) fn looks_like_path(word: &str) -> bool {
    let path = clean_path(word);
    if path.is_empty()
        || path.len() > MAX_PATH
        || path.contains("://")
        || path.starts_with("\\\\")
        || path.starts_with("//")
        || path.chars().any(char::is_control)
    {
        return false;
    }
    if path.contains(['/', '\\']) {
        return !path.ends_with(['/', '\\']);
    }
    path.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty()
            && (1..=8).contains(&ext.len())
            && ext.chars().all(|c| c.is_ascii_alphanumeric())
            && ext.chars().any(|c| c.is_ascii_alphabetic())
    })
}

/// The file a word in the terminal names, if it names one that exists: an
/// absolute path, `~/…`, or a path relative to `base` (the project's folder).
/// A `:line[:col]` or `#L…` after the name, as tools print them, is dropped.
pub(crate) fn resolve(word: &str, base: Option<&Path>) -> Option<PathBuf> {
    let word = clean_path(word);
    if word.is_empty() || word.len() > MAX_PATH || word.chars().any(char::is_control) {
        return None;
    }
    // A network share or a device path is never touched: just asking whether
    // it exists can reach out to another machine, or hang the window.
    if word.starts_with("\\\\") || word.starts_with("//") {
        return None;
    }
    let path = if let Some(rest) = word.strip_prefix("~/") {
        std::env::home_dir()?.join(rest)
    } else {
        let path = PathBuf::from(word);
        if path.is_absolute() {
            path
        } else {
            base?.join(path)
        }
    };
    if reaches_network(&path) {
        return None;
    }
    path.is_file().then_some(path)
}

/// Whether looking at `path` could reach another machine: on Windows a UNC
/// or device path, whatever the word it came from (`~` or the project folder
/// may be on a share).
#[cfg(windows)]
fn reaches_network(path: &Path) -> bool {
    use std::path::{Component, Prefix};
    match path.components().next() {
        Some(Component::Prefix(prefix)) => {
            !matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
        }
        _ => false,
    }
}

#[cfg(not(windows))]
fn reaches_network(path: &Path) -> bool {
    // Compared as text: as a path, `//` is only the root.
    path.as_os_str().as_encoded_bytes().starts_with(b"//")
}

/// `name:12` and `name:12:4` without the position.
fn strip_position(word: &str) -> &str {
    let mut word = word;
    for _ in 0..2 {
        match word.rsplit_once(':') {
            Some((name, number))
                if !name.is_empty()
                    && !number.is_empty()
                    && number.chars().all(|c| c.is_ascii_digit()) =>
            {
                word = name;
            }
            _ => break,
        }
    }
    word
}

const HEADER: f32 = 48.0;

/// Draw the preview over the whole window. `esc` closes it, as does a click
/// outside the card or on its close button.
pub(crate) fn show(ctx: &Context, preview: &mut Preview, esc: bool) -> Option<PreviewAction> {
    let mut action = esc.then_some(PreviewAction::Close);
    let screen = ctx.content_rect();
    let margin = vec2(
        (screen.width() * 0.06).clamp(16.0, 80.0),
        (screen.height() * 0.05).clamp(16.0, 60.0),
    );
    let card = screen.shrink2(margin);
    let area = egui::Area::new(Id::unique("file-preview"))
        .order(Order::Tooltip)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            ui.set_clip_rect(screen);
            ui.painter()
                .rect_filled(screen, 0.0, Color32::from_black_alpha(160));
            let backdrop = ui.interact(screen, Id::unique("file-preview-backdrop"), Sense::click());
            if backdrop.clicked()
                && backdrop
                    .interact_pointer_pos()
                    .is_some_and(|pos| !card.contains(pos))
            {
                action = Some(PreviewAction::Close);
            }
            ui.scope_builder(UiBuilder::new().max_rect(card), |ui| {
                Frame::NONE
                    .fill(theme::SHEET_BG)
                    .stroke(Stroke::new(1.0, theme::POPOVER_BORDER))
                    .corner_radius(f32::from(theme::RADIUS))
                    .show(ui, |ui| {
                        ui.set_min_size(card.size());
                        ui.set_max_size(card.size());
                        if header(ui, preview, card) {
                            action = Some(PreviewAction::Close);
                        }
                        let body =
                            Rect::from_min_max(pos2(card.min.x, card.min.y + HEADER), card.max);
                        ui.scope_builder(UiBuilder::new().max_rect(body), |ui| {
                            content(ui, preview, body);
                        });
                    });
            });
        });
    ctx.memory_mut(|memory| memory.set_modal_layer(area.response.layer_id));
    action
}

/// The title row: the file's name and folder, the source switch for Markdown,
/// Copy path and close. True when close was clicked.
fn header(ui: &mut Ui, preview: &mut Preview, card: Rect) -> bool {
    let bar = Rect::from_min_size(card.min, vec2(card.width(), HEADER));
    let painter = ui.painter_at(bar);
    painter.hline(bar.x_range(), bar.max.y, Stroke::new(1.0, theme::BORDER));
    let (path, is_md, downloadable) = {
        let state = preview.state();
        (
            state.path.clone(),
            matches!(state.content, Content::Markdown(_)),
            state.bytes.is_some(),
        )
    };
    let (name, folder) = split_path(&path);
    let (name, folder) = (name.to_owned(), folder.to_owned());
    let name_galley = painter.layout_no_wrap(name.clone(), FontId::proportional(15.0), theme::TEXT);
    let name_width = name_galley.size().x;
    painter.galley(
        pos2(
            bar.min.x + 16.0,
            bar.center().y - name_galley.size().y / 2.0,
        ),
        name_galley,
        theme::TEXT,
    );
    let buttons_width = 130.0 + 100.0 * f32::from(u8::from(is_md) + u8::from(downloadable));
    let folder_room = (bar.width() - 16.0 - name_width - 12.0 - buttons_width).max(0.0);
    if folder_room > 40.0 {
        let galley = painter.layout(
            folder,
            FontId::proportional(12.0),
            theme::FAINT,
            folder_room,
        );
        let mut job = (*galley.job).clone();
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
        painter.galley(
            pos2(
                bar.min.x + 16.0 + name_width + 12.0,
                bar.center().y - galley.size().y / 2.0,
            ),
            galley,
            theme::FAINT,
        );
    }
    let close_center = pos2(bar.max.x - 26.0, bar.center().y);
    let close_rect = Rect::from_center_size(close_center, vec2(26.0, 26.0));
    let close = ui.interact(close_rect, Id::unique("file-preview-close"), Sense::click());
    if close.hovered() {
        ui.painter().rect_filled(close_rect, 4.0, theme::ROW_HOVER);
    }
    icons::cross(ui.painter(), close_center, 5.0, theme::TEXT_MUTED);
    let close = close.on_hover_text("Close (Esc)");

    let copy_rect = Rect::from_min_size(
        pos2(close_rect.min.x - 8.0 - 92.0, bar.center().y - 12.0),
        vec2(92.0, 24.0),
    );
    if ui.put(copy_rect, theme::button("Copy path")).clicked() {
        ui.ctx().copy_text(path.clone());
    }
    let mut next = copy_rect.translate(vec2(-8.0 - 92.0, 0.0));
    if downloadable {
        let download = ui
            .put(next, theme::button("Download"))
            .on_hover_text("Save a copy of this file on this computer");
        if download.clicked() {
            download_file(ui.ctx(), preview, &name);
        }
        next = next.translate(vec2(-8.0 - 92.0, 0.0));
    }
    if is_md {
        let source_rect = next;
        let label = if preview.show_source {
            "Formatted"
        } else {
            "Source"
        };
        if ui.put(source_rect, theme::button(label)).clicked() {
            preview.show_source = !preview.show_source;
        }
    }
    close.clicked()
}

/// Save a remote file's bytes where the user picks, in Downloads by
/// default. The bytes are written off the UI thread, to a file beside the
/// target that then replaces it, so a failed save leaves a file already there
/// as it was. The preview says how it went.
fn download_file(ctx: &Context, preview: &Preview, name: &str) {
    let Some(bytes) = preview.state().bytes.clone() else {
        return;
    };
    let mut dialog = rfd::FileDialog::new().set_file_name(name);
    if let Some(downloads) = std::env::home_dir().map(|home| home.join("Downloads"))
        && downloads.is_dir()
    {
        dialog = dialog.set_directory(downloads);
    }
    let Some(target) = dialog.save_file() else {
        return;
    };
    let notice = Arc::clone(&preview.notice);
    let tell = move |text: String| {
        *notice.lock().unwrap_or_else(PoisonError::into_inner) = Some((text, Instant::now()));
    };
    tell("Saving…".to_owned());
    let ctx = ctx.clone();
    let spawned = std::thread::Builder::new()
        .name("connecto-download".to_owned())
        .spawn({
            let tell = tell.clone();
            move || {
                tell(match save(&target, &bytes) {
                    Ok(()) => format!("Saved to {}", target.display()),
                    Err(error) => format!("The file could not be saved: {error}."),
                });
                ctx.request_repaint();
            }
        });
    if spawned.is_err() {
        tell("The file could not be saved.".to_owned());
    }
}

/// Write `bytes` to `target` through a new file of its own beside it (made
/// exclusively, under a name no other save uses), which replaces `target`
/// only once it is whole.
/// Save `bytes` in `folder` as a new file named `name`, or `name (1)`,
/// `name (2)`… before its extension: the first name free at the moment it
/// is taken. Nothing there is ever replaced, also when two saves of the same
/// name run at once. The file is whole before it takes its name.
pub(crate) fn save_new(folder: &Path, name: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    use std::io::Write;
    let mut part = tempfile::Builder::new()
        .prefix(".connecto-")
        .suffix(".part")
        .tempfile_in(folder)?;
    part.write_all(bytes)?;
    part.as_file().sync_all()?;
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    for n in 0..10_000u32 {
        let candidate = if n == 0 {
            folder.join(name)
        } else {
            folder.join(format!("{stem} ({n}){ext}"))
        };
        match part.persist_noclobber(&candidate) {
            Ok(_) => return Ok(candidate),
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                part = error.file;
            }
            Err(error) => return Err(error.error),
        }
    }
    Err(std::io::Error::other(format!(
        "every name like {name} is taken in that folder"
    )))
}

pub(crate) fn save(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let folder = target
        .parent()
        .filter(|folder| !folder.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    // A short name of its own, so a long target name still fits.
    let mut part = tempfile::Builder::new()
        .prefix(".connecto-")
        .suffix(".part")
        .tempfile_in(folder)?;
    part.write_all(bytes)?;
    part.as_file().sync_all()?;
    part.persist(target).map_err(|error| error.error)?;
    Ok(())
}

fn content(ui: &mut Ui, preview: &mut Preview, body: Rect) {
    let mut shown_notice = preview
        .notice
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some((notice, at)) = &*shown_notice {
        let left = NOTICE.saturating_sub(at.elapsed());
        if left.is_zero() {
            *shown_notice = None;
        } else {
            ui.painter().text(
                pos2(body.min.x + 12.0, body.max.y - 8.0),
                Align2::LEFT_BOTTOM,
                notice,
                FontId::proportional(11.0),
                theme::TEXT_MUTED,
            );
            ui.ctx().request_repaint_after(left);
        }
    }
    drop(shown_notice);
    let state = Arc::clone(&preview.state);
    let state = state.lock().unwrap_or_else(PoisonError::into_inner);
    let content = &state.content;
    let padded = body.shrink2(vec2(24.0, 16.0));
    match content {
        Content::Loading => {
            ui.painter().text(
                body.center(),
                Align2::CENTER_CENTER,
                if preview.remote {
                    "Reading the file from the host…"
                } else {
                    "Loading…"
                },
                FontId::proportional(13.0),
                theme::TEXT_MUTED,
            );
        }
        Content::Failed(why) => {
            let why = if state.bytes.is_some() {
                format!("{why} Download it to open it on this computer.")
            } else {
                why.clone()
            };
            ui.painter().text(
                body.center(),
                Align2::CENTER_CENTER,
                why,
                FontId::proportional(13.0),
                theme::TEXT_MUTED,
            );
        }
        Content::Image(image, original) => {
            let texture = preview.texture.get_or_insert_with(|| {
                ui.ctx().load_texture(
                    format!("preview:{}", state.path),
                    Arc::clone(image),
                    TextureOptions::LINEAR,
                )
            });
            let size = texture.size_vec2();
            let scale = (padded.width() / size.x)
                .min(padded.height() / size.y)
                .clamp(0.01, 1.0);
            let shown = Rect::from_center_size(padded.center(), size * scale);
            // Transparent parts show on a plain backdrop.
            ui.painter().rect_filled(shown, 0.0, theme::RAISED_BG);
            ui.painter().image(
                texture.id(),
                shown,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::WHITE,
            );
            // The image's own size, and the zoom against it.
            let zoom = shown.width() / original[0].max(1) as f32;
            ui.painter().text(
                pos2(body.max.x - 12.0, body.max.y - 8.0),
                Align2::RIGHT_BOTTOM,
                format!(
                    "{} × {}{}",
                    original[0],
                    original[1],
                    if zoom < 0.995 {
                        format!("  ·  {:.0}%", zoom * 100.0)
                    } else {
                        String::new()
                    }
                ),
                FontId::proportional(11.0),
                theme::FAINT,
            );
        }
        Content::Markdown(text) if !preview.show_source => {
            let text = text.clone();
            drop(state);
            scroll(ui, padded, |ui| {
                ui.set_max_width(padded.width().min(860.0));
                markdown::show(ui, &text);
            });
        }
        Content::Markdown(text) | Content::Text { text, .. } => {
            let cut = matches!(content, Content::Text { cut: true, .. });
            let text = text.clone();
            drop(state);
            scroll(ui, padded, |ui| {
                if cut {
                    ui.label(
                        RichText::new("Only the first 4 MB are shown.")
                            .size(12.0)
                            .color(theme::AMBER),
                    );
                }
                source_view(ui, &text);
            });
        }
    }
}

fn scroll(ui: &mut Ui, area: Rect, body: impl FnOnce(&mut Ui)) {
    ui.scope_builder(UiBuilder::new().max_rect(area), |ui| {
        egui::ScrollArea::both()
            .id_salt("file-preview-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                Frame::NONE.inner_margin(Margin::same(2)).show(ui, body);
            });
    });
}

/// Text as it is, with line numbers, selectable to copy.
fn source_view(ui: &mut Ui, text: &str) {
    let lines = text.lines().count().max(1);
    let digits = lines.to_string().len();
    let numbers: String = (1..=lines).map(|n| format!("{n:>digits$}\n")).collect();
    ui.horizontal_top(|ui| {
        ui.label(
            RichText::new(numbers.trim_end())
                .family(egui::FontFamily::Monospace)
                .size(13.0)
                .color(theme::FAINT),
        );
        ui.add_space(8.0);
        let mut view = text;
        ui.add(
            egui::TextEdit::multiline(&mut view)
                .font(FontId::monospace(13.0))
                .text_color(theme::TEXT_ROW)
                .frame(Frame::NONE)
                .margin(Margin::ZERO)
                .desired_width(f32::INFINITY)
                .desired_rows(lines),
        );
    });
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_path_is_found_with_or_without_its_position() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("docs")).unwrap();
        let file = dir.path().join("docs/plan.md");
        std::fs::write(&file, "# Plan").unwrap();
        let base = Some(dir.path());
        for word in [
            "docs/plan.md",
            "docs/plan.md:12",
            "docs/plan.md:12:4",
            "docs/plan.md#L3-L9",
            "docs/plan.md.",
        ] {
            assert_eq!(
                resolve(word, base).as_deref(),
                Some(file.as_path()),
                "{word}"
            );
        }
        let absolute = file.display().to_string();
        assert_eq!(resolve(&absolute, None).as_deref(), Some(file.as_path()));
        assert_eq!(
            resolve(&format!("file://{absolute}"), None).as_deref(),
            Some(file.as_path())
        );
        assert_eq!(resolve("docs/missing.md", base), None);
        assert_eq!(resolve("docs", base), None, "a folder");
        assert_eq!(resolve("docs/plan.md", None), None, "relative needs a base");
        assert_eq!(resolve("\\\\server\\share\\a.md", base), None);
        assert_eq!(resolve("//server/share/a.md", base), None);
    }

    #[test]
    fn a_remote_path_is_told_from_other_words_by_its_shape() {
        for path in [
            "docs/plan.md",
            "docs/plan.md:12",
            "./run.sh",
            "~/notes.txt",
            "/var/log/app.log",
            "README.md",
            "C:\\work\\a.png",
        ] {
            assert!(looks_like_path(path), "{path}");
        }
        for word in [
            "now",
            "v1.2",
            "1.5",
            "https://example.com/a.md",
            "//server/share/a.md",
            "\\\\server\\a.md",
            "docs/",
            "e.",
        ] {
            assert!(!looks_like_path(word), "{word}");
        }
        assert_eq!(clean_path("file:///tmp/a.md:3:1."), "/tmp/a.md");
    }

    #[test]
    fn a_hosts_path_splits_into_name_and_folder_on_either_separator() {
        assert_eq!(
            split_path("/host/web/docs/plan.md"),
            ("plan.md", "/host/web/docs")
        );
        assert_eq!(split_path("C:\\web\\plan.md"), ("plan.md", "C:\\web"));
        assert_eq!(split_path("/plan.md"), ("plan.md", "/"));
        assert_eq!(split_path("plan.md"), ("plan.md", ""));
    }

    #[test]
    fn a_remote_files_bytes_decode_as_a_local_file_would() {
        assert!(matches!(decode("/h/a.md", b"# A", 4096), Content::Markdown(t) if t == "# A"));
        assert!(matches!(
            decode("C:\\h\\a.MD", b"# A", 4096),
            Content::Markdown(_)
        ));
        assert!(matches!(
            decode("/h/a.bin", &[0, 1], 4096),
            Content::Failed(_)
        ));
        let big = vec![b'x'; usize::try_from(MAX_TEXT).unwrap() + 10];
        assert!(
            matches!(decode("/h/a.md", &big, 4096), Content::Text { cut: true, text } if text.len() as u64 == MAX_TEXT)
        );
    }

    #[test]
    fn a_download_replaces_a_file_only_once_it_is_whole() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("plan.md");
        std::fs::write(&target, "old").unwrap();
        save(&target, b"new").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no part file left"
        );
        // A target that cannot be written leaves nothing behind.
        assert!(save(&dir.path().join("missing/plan.md"), b"x").is_err());
    }

    #[test]
    fn files_load_as_their_kind() {
        let dir = tempfile::tempdir().unwrap();
        let md = dir.path().join("a.md");
        std::fs::write(&md, "# A").unwrap();
        assert!(matches!(load(&md, 4096), Content::Markdown(text) if text == "# A"));
        let rs = dir.path().join("a.rs");
        std::fs::write(&rs, "fn main() {}").unwrap();
        assert!(matches!(load(&rs, 4096), Content::Text { cut: false, .. }));
        let bin = dir.path().join("a.bin");
        std::fs::write(&bin, [0u8, 1, 2]).unwrap();
        assert!(matches!(load(&bin, 4096), Content::Failed(_)));
        let png = dir.path().join("a.png");
        let image = ColorImage::new([3, 2], vec![Color32::RED; 6]);
        std::fs::write(&png, crate::pasted::png(&image).unwrap()).unwrap();
        assert!(matches!(load(&png, 4096), Content::Image(image, [3, 2]) if image.size == [3, 2]));
    }
}
