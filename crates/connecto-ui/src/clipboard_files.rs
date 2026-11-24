//! Files copied in Finder (Cmd+C) and pasted on a remote host's terminal.
//! egui hands the terminal only the clipboard's text, so a paste on a remote
//! terminal also reads the pasteboard's file URLs. When each is a file path
//! URL of this computer, the terminal asks first, in a strip over its bottom
//! ([`FilePaste`]): a fresh Enter sends exactly the files listed to the host,
//! as dropped files are sent; Esc pastes the text; anything else cancels.
//!
//! Reading them is pasteboard work only: no file is looked at on the UI
//! thread. The upload worker checks them as it checks dropped files.
//!
//! macOS only. On Windows and Linux a paste stays the clipboard's text, as
//! before; dropping files on a remote terminal sends them there.

use std::path::PathBuf;
use std::sync::Arc;

use egui::Event;

/// One file reference on the pasteboard, as written there.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CopiedFile {
    /// Its `public.file-url`.
    pub(crate) url: String,
    /// The path a file path URL names. None for a file reference URL
    /// (`file:///.file/id=…`), which only the file system could name.
    pub(crate) path: Option<PathBuf>,
}

/// Reads the pasteboard's file references, in order, when a paste is
/// handled: empty when it holds none.
pub(crate) type ClipboardFiles = Arc<dyn Fn() -> Vec<CopiedFile> + Send + Sync>;

/// The system pasteboard. Tests use pasteboards of their own.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn system() -> ClipboardFiles {
    Arc::new(|| read(&objc2_app_kit::NSPasteboard::generalPasteboard()))
}

fn read(board: &objc2_app_kit::NSPasteboard) -> Vec<CopiedFile> {
    use objc2_foundation::{NSString, NSURL};
    objc2::rc::autoreleasepool(|_| {
        // The type's name itself: AppKit's constant for it is an extern
        // static, which only unsafe code may read.
        let url_type = NSString::from_str("public.file-url");
        let Some(items) = board.pasteboardItems() else {
            return Vec::new();
        };
        items
            .iter()
            .filter_map(|item| item.stringForType(&url_type))
            .map(|url| {
                // A path URL's path is read from the URL itself; a reference
                // URL's would be looked up on disk, so it is not read.
                let path = NSURL::URLWithString(&url)
                    .filter(|url| !url.isFileReferenceURL())
                    .and_then(|url| url.path())
                    .map(|path| PathBuf::from(path.to_string()));
                CopiedFile {
                    url: url.to_string(),
                    path,
                }
            })
            .collect()
    })
}

/// The paths a paste offers to send, when every file reference on the
/// pasteboard is a file path URL of this computer (so each can be named
/// without the file system). None, the paste being its text, when there is
/// none or one is not.
fn offered(copied: Vec<CopiedFile>) -> Option<Vec<PathBuf>> {
    if copied.is_empty() {
        return None;
    }
    copied
        .into_iter()
        .map(|file| {
            let path = file.path.filter(|path| path.is_absolute())?;
            (local_file_url(&file.url) && path.file_name().is_some()).then_some(path)
        })
        .collect()
}

/// A `file:` URL with no host but this computer (none, or `localhost`), and
/// neither query nor fragment: a file reference here, not one a URL of
/// another kind or host was taken for.
fn local_file_url(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once(':') else {
        return false;
    };
    let Some(rest) = rest.strip_prefix("//") else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or_default();
    scheme.eq_ignore_ascii_case("file")
        && (host.is_empty() || host.eq_ignore_ascii_case("localhost"))
        && rest.len() > host.len()
        && !url.contains(['?', '#'])
}

/// The most characters of the file names a question shows, and of one name.
const MAX_LISTED: usize = 60;
const MAX_NAME: usize = 32;

/// The keys that answer the question, on a line of their own under it.
pub(crate) const HINT: &str = "[Enter] upload   [Esc] paste as text";

/// The question a paste of `paths` asks in its terminal.
fn question(paths: &[PathBuf]) -> String {
    let count = match paths.len() {
        1 => "1 file".to_owned(),
        n => format!("{n} files"),
    };
    let mut listed = String::new();
    let mut shown = 0;
    for path in paths {
        let name = short(&path.file_name().unwrap_or_default().to_string_lossy());
        let sep = if listed.is_empty() { "" } else { ", " };
        if shown > 0 && listed.chars().count() + sep.len() + name.chars().count() > MAX_LISTED {
            break;
        }
        listed.push_str(sep);
        listed.push_str(&name);
        shown += 1;
    }
    let more = paths.len() - shown;
    if more > 0 {
        listed.push_str(&format!(" and {more} more"));
    }
    format!("Upload {count} to the host? {listed}")
}

/// `name` with no control character, cut to [`MAX_NAME`] characters.
fn short(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|ch| if ch.is_control() { '?' } else { ch })
        .collect();
    if clean.chars().count() <= MAX_NAME {
        return clean;
    }
    let mut cut: String = clean.chars().take(MAX_NAME - 1).collect();
    cut.push('…');
    cut
}

/// What a paste on a terminal became.
#[derive(Debug, PartialEq)]
pub(crate) enum Pasted {
    /// Its text: no files, or files it does not offer.
    Text,
    /// A question about the files on the pasteboard.
    Asked,
    /// Too many files: why nothing is sent or typed.
    Refused(String),
}

/// What the question's answer asks for.
#[derive(Debug, PartialEq)]
pub(crate) enum Answer {
    /// No answer yet: the question stays.
    Waiting,
    /// Send exactly these files, the ones listed.
    Upload(Vec<PathBuf>),
    /// Type this, the pasted text.
    PasteText(String),
    /// Nothing sent or typed.
    Cancelled,
}

/// The question a remote terminal asks about files pasted on it.
#[derive(Default)]
pub(crate) struct FilePaste {
    /// Where a paste looks for copied files: set by the window for a remote
    /// terminal. None: a paste is its text.
    pub(crate) reader: Option<ClipboardFiles>,
    prompt: Option<Prompt>,
}

struct Prompt {
    paths: Vec<PathBuf>,
    text: String,
    question: String,
    /// The pass it was last drawn in, visibly or not: it is on show only
    /// while drawn in every pass (see `App::answer_file_paste`).
    painted_at: Option<u64>,
    /// It has been visible on screen: only now can an Enter or Esc be an
    /// answer to it. Its first drawing may be egui's invisible sizing pass.
    seen: bool,
}

impl FilePaste {
    /// A paste of `text` (what egui took from the clipboard).
    pub(crate) fn paste(&mut self, text: &str) -> Pasted {
        let Some(read) = &self.reader else {
            return Pasted::Text;
        };
        let copied = read();
        if copied.is_empty() {
            return Pasted::Text;
        }
        if let Some(why) = crate::upload::too_many(copied.len()) {
            return Pasted::Refused(why);
        }
        let Some(paths) = offered(copied) else {
            return Pasted::Text;
        };
        self.prompt = Some(Prompt {
            question: question(&paths),
            paths,
            text: text.to_owned(),
            painted_at: None,
            seen: false,
        });
        Pasted::Asked
    }

    pub(crate) fn is_open(&self) -> bool {
        self.prompt.is_some()
    }

    /// The question, to draw in pass `pass`.
    pub(crate) fn paint(&mut self, pass: u64) -> Option<&str> {
        let prompt = self.prompt.as_mut()?;
        prompt.painted_at = Some(pass);
        Some(&prompt.question)
    }

    /// The question was drawn visibly (not in a sizing pass): an Enter or
    /// Esc after this one can answer it.
    pub(crate) fn mark_seen(&mut self) {
        if let Some(prompt) = &mut self.prompt {
            prompt.seen = true;
        }
    }

    fn seen(&self) -> bool {
        self.prompt.as_ref().is_some_and(|prompt| prompt.seen)
    }

    /// The pass the question was last drawn in.
    pub(crate) fn painted_at(&self) -> Option<u64> {
        self.prompt.as_ref()?.painted_at
    }

    #[cfg(test)]
    pub(crate) fn question(&self) -> Option<&str> {
        self.prompt.as_ref().map(|prompt| prompt.question.as_str())
    }

    /// Close the question unanswered. True when one was open.
    pub(crate) fn cancel(&mut self) -> bool {
        self.prompt.take().is_some()
    }

    /// The answer `events` give the open question. Enter alone, pressed
    /// after the question was seen on screen, sends the files; Esc alone,
    /// after that too, pastes the text; before it was seen either cancels, as
    /// any other key, typed or composed text, paste, copy or cut, or
    /// the window losing focus, cancels. A key's release, the pointer and the
    /// like are no answer. The caller takes all of the frame's keyboard
    /// input, answer or not, from everything else.
    pub(crate) fn answer(&mut self, events: &[Event]) -> Answer {
        if self.prompt.is_none() {
            return Answer::Waiting;
        }
        for event in events {
            let answer = match event {
                Event::Key {
                    key: egui::Key::Enter,
                    pressed: true,
                    repeat: false,
                    modifiers,
                    ..
                } if modifiers.is_none() && self.seen() => {
                    let prompt = self.prompt.take();
                    Answer::Upload(prompt.map(|p| p.paths).unwrap_or_default())
                }
                Event::Key {
                    key: egui::Key::Escape,
                    pressed: true,
                    modifiers,
                    ..
                } if modifiers.is_none() && self.seen() => {
                    let prompt = self.prompt.take();
                    Answer::PasteText(prompt.map(|p| p.text).unwrap_or_default())
                }
                event if cancels(event) => {
                    self.prompt = None;
                    Answer::Cancelled
                }
                _ => continue,
            };
            return answer;
        }
        Answer::Waiting
    }
}

/// Input that answers the question by cancelling it.
fn cancels(event: &Event) -> bool {
    matches!(
        event,
        Event::Key { pressed: true, .. }
            | Event::Text(_)
            | Event::Ime(_)
            | Event::Paste(_)
            | Event::PasteImage(_)
            | Event::Copy
            | Event::Cut
            | Event::WindowFocused(false)
    )
}

/// The frame's keyboard input, all of which an open question takes (a key's
/// release too, so no half of a keystroke reaches anything).
pub(crate) fn is_keyboard(event: &Event) -> bool {
    cancels(event) || matches!(event, Event::Key { .. })
}

/// A pasteboard as Finder leaves it after copying `paths`: their file URLs.
#[cfg(test)]
pub(crate) fn finder_copy(paths: &[PathBuf]) -> Vec<CopiedFile> {
    paths
        .iter()
        .map(|path| CopiedFile {
            url: format!("file://{}", path.display()),
            path: Some(path.clone()),
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn file(url: &str, path: &str) -> CopiedFile {
        CopiedFile {
            url: url.to_owned(),
            path: Some(PathBuf::from(path)),
        }
    }

    fn reference() -> CopiedFile {
        CopiedFile {
            url: "file:///.file/id=6571367.2773272/".to_owned(),
            path: None,
        }
    }

    fn key(key: egui::Key, modifiers: egui::Modifiers) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    fn enter() -> Event {
        key(egui::Key::Enter, egui::Modifiers::NONE)
    }

    /// The question drawn visibly, as after its first frame on screen.
    fn show(paste: &mut FilePaste) {
        assert!(paste.paint(1).is_some());
        paste.mark_seen();
    }

    fn asked(files: Vec<CopiedFile>) -> FilePaste {
        let mut paste = FilePaste {
            reader: Some(Arc::new(move || files.clone())),
            prompt: None,
        };
        assert_eq!(paste.paste("a.txt"), Pasted::Asked);
        paste
    }

    #[test]
    fn only_named_file_urls_of_this_computer_are_offered() {
        let both = vec![
            file("file:///x/a%20b.txt", "/x/a b.txt"),
            file("FILE://LOCALHOST/x/c.png", "/x/c.png"),
        ];
        assert_eq!(
            offered(both),
            Some(vec![PathBuf::from("/x/a b.txt"), PathBuf::from("/x/c.png")])
        );
        assert_eq!(offered(Vec::new()), None);
        // AppKit gives a path for a "file URL" that is a bare path, a web
        // URL, or another host's file URL: none is a file of this computer,
        // and one of them makes the whole paste text.
        for url in [
            "/tmp/a.txt",
            "https://example.org/tmp/a.txt",
            "file://other.example/tmp/a.txt",
            "file:/tmp/a.txt",
            "file:///tmp/a.txt?x",
            "file:///tmp/a.txt#x",
            "file://",
        ] {
            let copied = vec![file("file:///x/b.txt", "/x/b.txt"), file(url, "/tmp/a.txt")];
            assert_eq!(offered(copied), None, "{url}");
        }
        assert_eq!(offered(vec![file("file:///x/a.txt", "x/a.txt")]), None);
        assert_eq!(offered(vec![file("file:///", "/")]), None);
    }

    /// A file reference URL could be named only by the file system, which
    /// the UI thread does not touch: such a paste is its text, not a
    /// question that cannot say which file it would send.
    #[test]
    fn a_file_reference_url_is_no_offer() {
        assert_eq!(offered(vec![reference()]), None);
        let mixed = vec![file("file:///x/a.txt", "/x/a.txt"), reference()];
        let mut paste = FilePaste {
            reader: Some(Arc::new(move || mixed.clone())),
            prompt: None,
        };
        assert_eq!(paste.paste("a.txt"), Pasted::Text);
        assert!(!paste.is_open());
    }

    #[test]
    fn the_question_counts_the_files_and_lists_their_names_shortened() {
        assert_eq!(
            question(&[PathBuf::from("/x/a.txt")]),
            "Upload 1 file to the host? a.txt"
        );
        let many: Vec<PathBuf> = (0..12)
            .map(|n| PathBuf::from(format!("/x/report-{n}.txt")))
            .collect();
        let asked = question(&many);
        assert!(
            asked.starts_with("Upload 12 files to the host? report-0.txt, report-1.txt, "),
            "{asked}"
        );
        assert!(asked.ends_with(" more"), "{asked}");
        let long = PathBuf::from(format!("/x/{}.txt\u{1b}", "é".repeat(80)));
        let asked = question(&[long]);
        assert!(asked.contains(&format!("{}…", "é".repeat(31))), "{asked}");
        assert!(!asked.contains('\u{1b}'), "{asked}");
    }

    #[test]
    fn too_many_files_are_refused_before_anything_else() {
        // Counted before any is looked at: these would not even be offered.
        let many = vec![reference(); connecto_core::terminals::MAX_UPLOAD_FILES + 1];
        let mut paste = FilePaste {
            reader: Some(Arc::new(move || many.clone())),
            prompt: None,
        };
        assert!(matches!(paste.paste("x"), Pasted::Refused(why) if why.contains("At most")));
        assert!(!paste.is_open());
    }

    /// Only a fresh Enter, pressed once the question was drawn, sends: one
    /// that came with the paste never does, and cancels instead.
    #[test]
    fn only_an_enter_after_the_question_was_seen_sends_the_files() {
        let files = vec![file("file:///x/a.txt", "/x/a.txt")];
        let mut paste = asked(files.clone());
        assert_eq!(paste.answer(&[enter()]), Answer::Cancelled);
        assert!(!paste.is_open());

        // Drawn only in egui's invisible sizing pass: not seen yet.
        let mut paste = asked(files.clone());
        assert!(paste.paint(1).is_some());
        assert_eq!(paste.painted_at(), Some(1));
        assert_eq!(paste.answer(&[enter()]), Answer::Cancelled);

        let mut paste = asked(files);
        show(&mut paste);
        let release = Event::Key {
            key: egui::Key::V,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        };
        assert_eq!(paste.answer(&[release]), Answer::Waiting);
        let repeated = Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: true,
            modifiers: egui::Modifiers::NONE,
        };
        assert_eq!(paste.answer(&[repeated]), Answer::Cancelled);

        let mut paste = asked(vec![file("file:///x/a.txt", "/x/a.txt")]);
        show(&mut paste);
        assert_eq!(
            paste.answer(&[enter()]),
            Answer::Upload(vec![PathBuf::from("/x/a.txt")])
        );
        assert!(!paste.is_open());
    }

    /// Esc, once the question was seen, pastes the text; before that it
    /// cancels. Any other keystroke, its text, composed (IME) text, a
    /// shortcut, a paste, or the window losing focus cancels.
    #[test]
    fn esc_pastes_the_text_and_any_other_input_cancels() {
        let files = vec![file("file:///x/a.txt", "/x/a.txt")];
        let esc = key(egui::Key::Escape, egui::Modifiers::NONE);
        let mut paste = asked(files.clone());
        paste.paint(1);
        assert_eq!(paste.answer(std::slice::from_ref(&esc)), Answer::Cancelled);
        let mut paste = asked(files.clone());
        show(&mut paste);
        assert_eq!(
            paste.answer(&[key(egui::Key::Escape, egui::Modifiers::NONE)]),
            Answer::PasteText("a.txt".into())
        );
        for event in [
            key(egui::Key::Y, egui::Modifiers::NONE),
            Event::Text("y".into()),
            Event::Ime(egui::ImeEvent::Commit("字".into())),
            key(egui::Key::B, egui::Modifiers::COMMAND),
            key(egui::Key::Enter, egui::Modifiers::SHIFT),
            Event::Paste("x".into()),
            Event::Copy,
            Event::WindowFocused(false),
        ] {
            let mut paste = asked(files.clone());
            show(&mut paste);
            assert_eq!(
                paste.answer(std::slice::from_ref(&event)),
                Answer::Cancelled,
                "{event:?}"
            );
            assert!(is_keyboard(&event) || matches!(event, Event::WindowFocused(false)));
        }
    }

    /// The pasteboard as AppKit gives it, from a pasteboard of the tests' own
    /// (never the clipboard): file URLs with spaces, non-ASCII characters and
    /// an escaped `#` are read as their paths, and a file reference URL is
    /// read with none, so the paste is not offered.
    #[test]
    fn file_urls_on_a_pasteboard_are_read_without_the_file_system() {
        use objc2::runtime::ProtocolObject;
        use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardWriting};
        use objc2_foundation::{NSArray, NSString, NSURL};

        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let named = dir.join("a file #1 naïve 文件.txt");
        std::fs::write(&named, b"a").unwrap();
        let referenced = dir.join("b.png");
        std::fs::write(&referenced, b"b").unwrap();
        let named_url = NSURL::fileURLWithPath(&NSString::from_str(named.to_str().unwrap()));
        let reference = NSURL::fileURLWithPath(&NSString::from_str(referenced.to_str().unwrap()))
            .fileReferenceURL()
            .unwrap();
        let reference = reference.absoluteString().unwrap().to_string();
        assert!(reference.starts_with("file:///.file/id="), "{reference}");

        let board = NSPasteboard::pasteboardWithName(&NSString::from_str(
            "com.connecto.tests.clipboard-files",
        ));
        board.clearContents();
        let text_type = NSString::from_str("public.utf8-plain-text");
        let url_type = NSString::from_str("public.file-url");
        let first = NSPasteboardItem::new();
        first.setString_forType(&named_url.absoluteString().unwrap(), &url_type);
        first.setString_forType(&NSString::from_str("names"), &text_type);
        let second = NSPasteboardItem::new();
        second.setString_forType(&NSString::from_str(&reference), &url_type);
        let text = NSPasteboardItem::new();
        text.setString_forType(&NSString::from_str("/tmp/only text"), &text_type);
        let items: Vec<_> = [first, second, text]
            .into_iter()
            .map(ProtocolObject::<dyn NSPasteboardWriting>::from_retained)
            .collect();
        assert!(board.writeObjects(&NSArray::from_retained_slice(&items)));
        let copied = read(&board);
        board.clearContents();

        assert_eq!(copied.len(), 2, "{copied:?}");
        assert!(copied[0].url.contains("%20%231%20"), "{}", copied[0].url);
        let read_named = copied[0].path.clone().unwrap();
        assert_eq!(read_named.parent(), Some(dir.as_path()));
        assert!(
            read_named.to_str().unwrap().ends_with("文件.txt"),
            "{read_named:?}"
        );
        assert_eq!(std::fs::read(&read_named).unwrap(), b"a");
        assert_eq!(copied[1].path, None);
        assert_eq!(offered(copied.clone()), None);
        assert_eq!(offered(copied[..1].to_vec()), Some(vec![read_named]));
    }
}
