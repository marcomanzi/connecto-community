//! Images pasted on a terminal (Cmd+V with only an image on the clipboard,
//! such as a screenshot). A terminal takes text only, so the image is saved
//! as a PNG through [`PastedFiles`] (the project's attachments folder) and its
//! path is pasted, as a dropped file's is. Agents read the image from there.

use std::path::PathBuf;
use std::sync::Arc;

use connecto_core::PastedFiles;
use egui::ColorImage;

/// The image as a PNG file's bytes.
pub(crate) fn png(image: &ColorImage) -> Result<Vec<u8>, String> {
    let [width, height] = image.size;
    let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height)) else {
        return Err("The pasted image is too large.".to_owned());
    };
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|_| "The pasted image could not be encoded.".to_owned())?;
        let rgba: Vec<u8> = image
            .pixels
            .iter()
            .flat_map(|pixel| pixel.to_srgba_unmultiplied())
            .collect();
        writer
            .write_image_data(&rgba)
            .map_err(|_| "The pasted image could not be encoded.".to_owned())?;
    }
    Ok(out)
}

/// A name for a paste made at `now`, unique enough to read; the store adds a
/// unique prefix.
pub(crate) fn name(now: chrono::DateTime<chrono::Local>) -> String {
    format!("pasted-{}.png", now.format("%Y%m%d-%H%M%S-%3f"))
}

/// Encodes and saves `image` for `project`: the path to paste, or why not.
pub(crate) fn save(
    files: &dyn PastedFiles,
    project: &str,
    image: &ColorImage,
) -> Result<PathBuf, String> {
    let bytes = png(image)?;
    files.save(project, &name(chrono::Local::now()), &bytes)
}

/// [`save`] off the UI thread. `done` gets the outcome, then the window is
/// asked to draw again.
pub(crate) fn save_in_background(
    files: Arc<dyn PastedFiles>,
    project: String,
    image: Arc<ColorImage>,
    ctx: egui::Context,
    done: impl FnOnce(Result<PathBuf, String>) + Send + 'static,
) {
    let spawned = std::thread::Builder::new()
        .name("pasted-image".into())
        .spawn(move || {
            done(save(&*files, &project, &image));
            ctx.request_repaint();
        });
    if spawned.is_err() {
        // `done` went with the closure: nothing to tell. Rare (no thread).
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Writes into a folder of the test's own, as the attachments store would.
    pub(crate) struct Folder(pub(crate) tempfile::TempDir, pub(crate) Mutex<Vec<String>>);

    impl Folder {
        pub(crate) fn new() -> Arc<Self> {
            Arc::new(Self(tempfile::tempdir().unwrap(), Mutex::default()))
        }
    }

    impl PastedFiles for Folder {
        fn save(&self, project: &str, name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
            self.1.lock().unwrap().push(project.to_owned());
            let path = self.0.path().join(name);
            std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
            Ok(path)
        }
    }

    #[test]
    fn a_pasted_image_is_saved_as_a_png_named_for_the_moment() {
        let image = ColorImage::new([3, 2], vec![egui::Color32::RED; 6]);
        let folder = Folder::new();
        let path = save(&*folder, "p1", &image).unwrap();
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with("pasted-") && name.ends_with(".png"),
            "{name}"
        );
        let bytes = std::fs::read(&path).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let reader = decoder.read_info().unwrap();
        assert_eq!((reader.info().width, reader.info().height), (3, 2));
        assert_eq!(folder.1.lock().unwrap().as_slice(), ["p1"]);
    }
}
