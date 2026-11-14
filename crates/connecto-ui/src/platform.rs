//! What differs between the platforms the window runs on, kept in one place.
//!
//! macOS is the reference: every item behind `cfg(target_os = "macos")` here
//! is what the code did before the Windows port. The other platforms (Windows
//! first) get their equivalent next to it.
//!
//! Keys: on macOS the app's chords use Cmd, which a terminal never needs. Off
//! macOS there is no such key (egui reports Ctrl as `command`), and Ctrl+<key>
//! belongs to the shell, so the app takes Ctrl+Shift+<key> instead, the way
//! Windows Terminal does. A macOS Cmd+Shift or Cmd+Alt chord becomes
//! Ctrl+Shift+Alt. [`as_mac_chord`] turns such a press into the macOS chord, so
//! the command table in `commands.rs` stays the one table.

use egui::{Event, Key, Modifiers};

// ---------- words ----------

/// How the user's computer is named in the window.
#[cfg(target_os = "macos")]
pub(crate) const THIS_COMPUTER: &str = "this Mac";
#[cfg(windows)]
pub(crate) const THIS_COMPUTER: &str = "this PC";
#[cfg(not(any(target_os = "macos", windows)))]
pub(crate) const THIS_COMPUTER: &str = "this computer";

/// Where saved secrets live, said when it cannot be reached: Connecto's
/// secrets file, or the system store when the person chose it.
pub(crate) const KEY_STORE: &str = "secret store";

/// The sentence for a key store that cannot be reached.
pub(crate) const KEY_STORE_UNAVAILABLE: &str = "Connecto's secret store is unavailable.";

/// The Settings choice to keep secrets in the system store instead; `None`
/// where there is no system store to offer.
#[cfg(target_os = "macos")]
pub(crate) const SYSTEM_STORE_CHOICE: Option<&str> = Some("Store secrets in the system keychain");
#[cfg(windows)]
pub(crate) const SYSTEM_STORE_CHOICE: Option<&str> =
    Some("Store secrets in Windows Credential Manager");
#[cfg(not(any(target_os = "macos", windows)))]
pub(crate) const SYSTEM_STORE_CHOICE: Option<&str> = None;

/// Shortcut text for a hint, as a `&'static str`: `kbd!("O")` is `⌘O` on
/// macOS and `Ctrl+Shift+O` elsewhere; `kbd!(alt "1")` is `⌘⌥1` on macOS and
/// `Ctrl+Shift+Alt+1` elsewhere.
#[cfg(target_os = "macos")]
macro_rules! kbd {
    (alt $key:literal) => {
        concat!("⌘⌥", $key)
    };
    ($key:literal) => {
        concat!("⌘", $key)
    };
}
#[cfg(not(target_os = "macos"))]
macro_rules! kbd {
    (alt $key:literal) => {
        concat!("Ctrl+Shift+Alt+", $key)
    };
    ($key:literal) => {
        concat!("Ctrl+Shift+", $key)
    };
}
pub(crate) use kbd;

/// The empty window's invitation to open a project, the same on every
/// platform now that Windows starts the agents too (M3).
pub(crate) const OPEN_PROJECT_INVITE: &str =
    "Pick a folder. Connecto starts Claude and Codex in it, and reopens it as you left it.";

/// The sidebar's hint while no project is open. The ＋ (U+FF0B) is not in
/// the fonts the window draws with off macOS, so there it is a plain +.
#[cfg(target_os = "macos")]
pub(crate) const NO_PROJECT_HINT: &str = concat!("Open one with ＋ or ", kbd!("O"), ".");
#[cfg(not(target_os = "macos"))]
pub(crate) const NO_PROJECT_HINT: &str = concat!("Open one with + or ", kbd!("O"), ".");

/// The band at the top of the sidebar that the macOS traffic lights sit in.
/// Off macOS the window has a system title bar, so there is none.
#[cfg(target_os = "macos")]
pub(crate) const LIGHTS_BAND: f32 = 44.0;
#[cfg(not(target_os = "macos"))]
pub(crate) const LIGHTS_BAND: f32 = 0.0;

/// The key that makes a click open a link.
#[cfg(target_os = "macos")]
pub(crate) const LINK_KEY: &str = "Cmd";
#[cfg(not(target_os = "macos"))]
pub(crate) const LINK_KEY: &str = "Ctrl";

// ---------- keys ----------

/// The key a shortcut is matched on. With Shift held, the logical key of a
/// digit or a punctuation key is the shifted symbol (Ctrl+Shift+1 arrives as
/// `!` on a US layout), so off macOS those use the physical key. Letters keep
/// the logical key, so the shortcut follows the keyboard layout.
#[cfg(target_os = "macos")]
pub(crate) fn shortcut_key(key: Key, _physical: Option<Key>) -> Key {
    key
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn shortcut_key(key: Key, physical: Option<Key>) -> Key {
    let letter = key.name().len() == 1 && key.name().chars().all(|c| c.is_ascii_uppercase());
    if letter { key } else { physical.unwrap_or(key) }
}

/// The press as a macOS chord. On macOS it is the press itself.
#[cfg(target_os = "macos")]
pub(crate) fn as_mac_chord(_key: Key, m: Modifiers) -> Modifiers {
    m
}

/// The press as a macOS chord: Ctrl+Shift is Cmd, and Ctrl+Shift+Alt is
/// Cmd+Alt on a digit (the new-agent chords) and Cmd+Shift otherwise. Ctrl+Tab
/// and Ctrl+Shift+Tab are the same chords on every platform, so Tab is left as
/// it is. A press that already has Cmd (only synthetic input does off macOS)
/// is left as it is too.
#[cfg(not(target_os = "macos"))]
pub(crate) fn as_mac_chord(key: Key, m: Modifiers) -> Modifiers {
    if m.mac_cmd || key == Key::Tab || !(m.ctrl && m.shift) {
        return m;
    }
    let digit = matches!(
        key,
        Key::Num0
            | Key::Num1
            | Key::Num2
            | Key::Num3
            | Key::Num4
            | Key::Num5
            | Key::Num6
            | Key::Num7
            | Key::Num8
            | Key::Num9
    );
    Modifiers {
        alt: m.alt && digit,
        ctrl: false,
        shift: m.alt && !digit,
        mac_cmd: true,
        command: true,
    }
}

/// Whether a key press with these modifiers is the app's, never the
/// terminal's. On macOS that is any press with Cmd. Off macOS egui sets
/// `command` for Ctrl, which the shell needs (Ctrl+C, Ctrl+R, …), so only a
/// real Cmd counts; the app's Ctrl+Shift chords are taken out before the
/// terminal sees them (`commands::take`).
#[cfg(target_os = "macos")]
pub(crate) fn app_owns(modifiers: &Modifiers) -> bool {
    modifiers.mac_cmd || modifiers.command
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn app_owns(modifiers: &Modifiers) -> bool {
    modifiers.mac_cmd
}

/// Select-all on the terminal: Cmd+A on macOS. Off macOS Ctrl+A is the
/// shell's, so it is Ctrl+Shift+A (as in Windows Terminal); Cmd+A is still
/// read, as synthetic input sends it.
#[cfg(target_os = "macos")]
pub(crate) fn is_select_all(modifiers: &Modifiers) -> bool {
    modifiers.mac_cmd && !modifiers.shift && !modifiers.alt
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn is_select_all(modifiers: &Modifiers) -> bool {
    (modifiers.mac_cmd && !modifiers.shift && !modifiers.alt)
        || (modifiers.ctrl && modifiers.shift && !modifiers.alt)
}

/// Off macOS the system turns Ctrl+C and Ctrl+X into Copy and Cut before the
/// window sees a key. With nothing selected they are the shell's keys, so they
/// go to the terminal as the Ctrl keys they were (Ctrl+C interrupts, as in
/// Windows Terminal). On macOS copying is Cmd+C and Ctrl+C arrives as a key.
#[cfg(target_os = "macos")]
pub(crate) fn clipboard_event_as_key(_event: &Event) -> Option<Event> {
    None
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn clipboard_event_as_key(event: &Event) -> Option<Event> {
    let key = match event {
        Event::Copy => Key::C,
        Event::Cut => Key::X,
        _ => return None,
    };
    Some(Event::Key {
        key,
        physical_key: Some(key),
        pressed: true,
        repeat: false,
        modifiers: Modifiers::CTRL,
    })
}

/// Whether copying a selection also clears it. Off macOS Ctrl+C is both copy
/// and interrupt, so after a copy the next Ctrl+C must reach the shell.
#[cfg(target_os = "macos")]
pub(crate) const COPY_CLEARS_SELECTION: bool = false;
#[cfg(not(target_os = "macos"))]
pub(crate) const COPY_CLEARS_SELECTION: bool = true;

// ---------- dropped files ----------

/// One dropped path as the shell reads it. Unix shells: a backslash before
/// every character the shell would read as syntax.
#[cfg(not(windows))]
pub(crate) fn quote_dropped_path(path: &str, out: &mut String) {
    for c in path.chars() {
        if !(c.is_alphanumeric() || "_-./,:@%+=~".contains(c)) {
            out.push('\\');
        }
        out.push(c);
    }
}

/// One dropped path as PowerShell reads it: as is when it has only plain
/// characters, else in single quotes (nothing inside is expanded) with each
/// `'` doubled. A backslash is the path separator here, not an escape.
#[cfg(windows)]
pub(crate) fn quote_dropped_path(path: &str, out: &mut String) {
    let plain = path
        .chars()
        .all(|c| c.is_alphanumeric() || "_-./\\:".contains(c));
    if plain {
        out.push_str(path);
        return;
    }
    out.push('\'');
    for c in path.chars() {
        if c == '\'' {
            out.push('\'');
        }
        out.push(c);
    }
    out.push('\'');
}

// ---------- system ----------

/// The home folder from the environment, if it is set: `HOME`, else
/// `USERPROFILE`.
#[cfg(not(windows))]
pub(crate) fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

/// The home folder from the environment, if it is set: `USERPROFILE` (what
/// Windows itself uses; `HOME` may be set by other tools), else `HOME`.
#[cfg(windows)]
pub(crate) fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

/// Whether the user asked the system for less motion.
#[cfg(target_os = "macos")]
pub(crate) fn reduce_motion() -> bool {
    std::process::Command::new("defaults")
        .args(["read", "com.apple.universalaccess", "reduceMotion"])
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).trim() == "1")
}

/// Off macOS the setting is not read. Windows keeps it as "Animation effects"
/// (`SystemParametersInfo(SPI_GETCLIENTAREAANIMATION)`), which needs an unsafe
/// call; the only motion it would stop is the setup window's spinner and glow.
#[cfg(not(target_os = "macos"))]
pub(crate) fn reduce_motion() -> bool {
    false
}

/// Open a web page in the default browser.
#[cfg(not(windows))]
pub(crate) fn open_web_page(_ctx: &egui::Context, url: &str) {
    let _ = std::process::Command::new("open").arg(url).spawn();
}

/// Open a web page in the default browser, through eframe (the `webbrowser`
/// crate, which starts the registered browser with the URL as one argument,
/// no shell). Only http and https pages are opened.
#[cfg(windows)]
pub(crate) fn open_web_page(ctx: &egui::Context, url: &str) {
    if is_web_url(url) {
        ctx.open_url(egui::OpenUrl::new_tab(url));
    }
}

#[cfg(any(windows, test))]
fn is_web_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://"))
        && !url.chars().any(|c| c.is_control() || c.is_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_urls_are_web_urls() {
        assert!(is_web_url("https://claude.ai/download"));
        assert!(is_web_url("HTTP://example.test/a?b=c"));
        assert!(!is_web_url("file:///C:/Windows/system32/calc.exe"));
        assert!(!is_web_url("C:\\Windows\\notepad.exe"));
        assert!(!is_web_url("https://example.test/a b"));
        assert!(!is_web_url("javascript:alert(1)"));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn ctrl_shift_chords_are_the_mac_cmd_chords() {
        let ctrl_shift = Modifiers {
            ctrl: true,
            shift: true,
            command: true,
            ..Modifiers::NONE
        };
        let with_alt = Modifiers {
            alt: true,
            ..ctrl_shift
        };
        let cmd = Modifiers {
            mac_cmd: true,
            command: true,
            ..Modifiers::NONE
        };
        assert_eq!(as_mac_chord(Key::O, ctrl_shift), cmd);
        assert_eq!(
            as_mac_chord(Key::W, with_alt),
            Modifiers { shift: true, ..cmd }
        );
        assert_eq!(
            as_mac_chord(Key::Num1, with_alt),
            Modifiers { alt: true, ..cmd }
        );
        assert_eq!(as_mac_chord(Key::Tab, ctrl_shift), ctrl_shift);
        assert_eq!(as_mac_chord(Key::C, Modifiers::CTRL), Modifiers::CTRL);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn shifted_digits_and_symbols_match_on_the_physical_key() {
        assert_eq!(
            shortcut_key(Key::Exclamationmark, Some(Key::Num1)),
            Key::Num1
        );
        assert_eq!(shortcut_key(Key::O, Some(Key::P)), Key::O);
        assert_eq!(shortcut_key(Key::Period, None), Key::Period);
    }

    #[cfg(windows)]
    #[test]
    fn dropped_paths_are_quoted_for_powershell() {
        let quoted = |path: &str| {
            let mut out = String::new();
            quote_dropped_path(path, &mut out);
            out
        };
        assert_eq!(quoted(r"C:\work\plain.txt"), r"C:\work\plain.txt");
        assert_eq!(quoted(r"C:\a b\it's.png"), r"'C:\a b\it''s.png'");
        assert_eq!(quoted(r"C:\$(calc).png"), r"'C:\$(calc).png'");
    }
}
