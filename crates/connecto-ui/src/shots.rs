//! Saving what the renderer drew, for verification.
//!
//! This reads the frame back from the GPU, so it shows what was really
//! painted, and needs no screen-recording permission. Encoding and disk writes
//! run on their own thread. Off unless a directory is configured, because the
//! files contain whatever the sessions were showing.

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, bounded};
use egui::ColorImage;

const INTERVAL: Duration = Duration::from_secs(1);
const MAX_PENDING: usize = 2;
/// Stops an unattended run from filling the disk.
const MAX_SHOTS: u32 = 300;

/// Takes a screenshot once per interval.
pub struct ScreenshotSink {
    tx: Sender<Arc<ColorImage>>,
    last: Option<Instant>,
    taken: u32,
}

impl ScreenshotSink {
    /// Start saving numbered PNG files in `dir`.
    pub fn to_dir(dir: PathBuf) -> std::io::Result<Self> {
        fs::create_dir_all(&dir)?;
        let (tx, rx) = bounded::<Arc<ColorImage>>(MAX_PENDING);
        thread::Builder::new()
            .name("screenshot-writer".into())
            .spawn(move || {
                for (index, image) in rx.into_iter().enumerate() {
                    let path = dir.join(format!("shot-{index:04}.png"));
                    if let Err(err) = write_png(&path, &image) {
                        eprintln!("could not save a screenshot: {err}");
                    }
                }
            })?;
        Ok(Self {
            tx,
            last: None,
            taken: 0,
        })
    }

    pub(crate) fn request_if_due(&mut self, ctx: &egui::Context) {
        if self.taken >= MAX_SHOTS {
            return;
        }
        if self.last.is_none_or(|last| last.elapsed() >= INTERVAL) {
            self.last = Some(Instant::now());
            self.taken += 1;
            let tx = self.tx.clone();
            ctx.request_screenshot(move |image| {
                // A full queue loses one screenshot, never a frame.
                let _ = tx.try_send(image);
            });
        }
        ctx.request_repaint_after(INTERVAL);
    }
}

fn write_png(path: &Path, image: &ColorImage) -> Result<(), png::EncodingError> {
    let [width, height] = image.size;
    let width = u32::try_from(width).unwrap_or(0);
    let height = u32::try_from(height).unwrap_or(0);
    let mut encoder = png::Encoder::new(BufWriter::new(File::create(path)?), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    let bytes: Vec<u8> = image.pixels.iter().flat_map(|p| p.to_array()).collect();
    writer.write_image_data(&bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use egui::Color32;

    use super::*;

    #[test]
    fn a_frame_is_saved_as_a_valid_png() {
        let dir = std::env::temp_dir().join(format!("connecto-shot-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("frame.png");
        let image = ColorImage::new([2, 1], vec![Color32::RED, Color32::BLUE]);
        write_png(&path, &image).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        fs::remove_dir_all(&dir).unwrap();
    }
}
