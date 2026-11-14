//! Terminal fonts.
//!
//! macOS: Menlo ships with the system and covers the box-drawing and symbol
//! ranges TUIs use. Windows: Consolas (with its real bold face), then Cascadia
//! Mono behind it for the glyphs Consolas lacks; both ship with Windows 11.
//! A font that cannot be read is skipped; with none, the app keeps egui's
//! built-in monospace font.

use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily};

const REGULAR: &str = "connecto-mono";
const BOLD: &str = "connecto-mono-bold";

/// The family bold text is drawn with.
pub(crate) fn bold_family() -> FontFamily {
    FontFamily::Name(BOLD.into())
}

/// One system font: its regular and its bold face.
struct Faces {
    regular: FontData,
    bold: FontData,
}

#[cfg(target_os = "macos")]
fn system_faces() -> Vec<Faces> {
    const MENLO_PATH: &str = "/System/Library/Fonts/Menlo.ttc";
    const MENLO_REGULAR_INDEX: u32 = 0;
    const MENLO_BOLD_INDEX: u32 = 1;

    let Ok(bytes) = std::fs::read(MENLO_PATH) else {
        return Vec::new();
    };
    let face = |index| {
        let mut data = FontData::from_owned(bytes.clone());
        data.index = index;
        data
    };
    vec![Faces {
        regular: face(MENLO_REGULAR_INDEX),
        bold: face(MENLO_BOLD_INDEX),
    }]
}

/// The Windows fonts folder, from `WINDIR` (or `SystemRoot`), never a fixed
/// drive.
#[cfg(windows)]
fn system_faces() -> Vec<Faces> {
    let Some(windows) = std::env::var_os("WINDIR").or_else(|| std::env::var_os("SystemRoot"))
    else {
        return Vec::new();
    };
    let folder = std::path::PathBuf::from(windows).join("Fonts");
    let read = |name: &str| {
        std::fs::read(folder.join(name))
            .ok()
            .map(FontData::from_owned)
    };
    let mut faces = Vec::new();
    // Separate files, one face each. A missing bold falls back to the regular.
    if let Some(regular) = read("consola.ttf") {
        let bold = read("consolab.ttf").unwrap_or_else(|| regular.clone());
        faces.push(Faces { regular, bold });
    }
    // A variable font; egui draws its default (regular) instance, so it is
    // its own bold.
    if let Some(regular) = read("CascadiaMono.ttf") {
        faces.push(Faces {
            bold: regular.clone(),
            regular,
        });
    }
    faces
}

#[cfg(not(any(target_os = "macos", windows)))]
fn system_faces() -> Vec<Faces> {
    Vec::new()
}

/// Install the terminal fonts.
pub(crate) fn install(ctx: &egui::Context) {
    let mut definitions = FontDefinitions::default();
    let fallbacks = definitions
        .families
        .get(&FontFamily::Monospace)
        .cloned()
        .unwrap_or_default();

    let mut bold_stack = fallbacks.clone();
    // The system fonts go in front of egui's own, in order.
    for (position, faces) in system_faces().into_iter().enumerate() {
        let (regular, bold) = if position == 0 {
            (REGULAR.to_owned(), BOLD.to_owned())
        } else {
            (
                format!("{REGULAR}-{position}"),
                format!("{BOLD}-{position}"),
            )
        };
        definitions
            .font_data
            .insert(regular.clone(), Arc::new(faces.regular));
        definitions
            .font_data
            .insert(bold.clone(), Arc::new(faces.bold));
        if let Some(stack) = definitions.families.get_mut(&FontFamily::Monospace) {
            stack.insert(position, regular);
        }
        bold_stack.insert(position, bold);
    }
    definitions.families.insert(bold_family(), bold_stack);
    ctx.set_fonts(definitions);
}
