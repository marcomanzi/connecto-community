//! The strip that offers a file from a connected machine: an agent there ran
//! `connecto send-file`, and this window says "build box offers report.html
//! (2.1 MB) · Download / Dismiss". A download is saved in Downloads under a
//! name no file there has yet, then the strip offers Open and Show in Finder.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use connecto_core::{RemoteControl, RemoteOffer};
use egui::{Rect, RichText, Stroke, Ui, vec2};

use crate::forms::Out;
use crate::theme;

/// The strip's height, as the update strip's.
const HEIGHT: f32 = crate::update_strip::HEIGHT;

/// Which offer: the host's name and the offer's id.
type Key = (String, u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Download {
    Saving { name: String },
    Saved(PathBuf),
    Failed { name: String, why: String },
}

/// What the window remembers about offers.
#[derive(Default)]
pub(crate) struct OfferState {
    /// Offers the person closed, or downloaded and then closed.
    closed: HashSet<Key>,
    /// Downloads, filled in by the thread that saves each.
    downloads: Arc<Mutex<HashMap<Key, Download>>>,
    /// Where files are saved; `None` is the Downloads folder.
    #[cfg(test)]
    pub(crate) folder: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OfferAction {
    Download(Key),
    Close(Key),
    Open(PathBuf),
    Reveal(PathBuf),
}

/// "2.1 MB", "12 KB".
pub(crate) fn size(bytes: u64) -> String {
    if bytes < 1024 * 1024 {
        format!("{} KB", bytes.div_ceil(1024).max(1))
    } else {
        #[allow(clippy::cast_precision_loss)]
        let mb = bytes as f64 / (1024.0 * 1024.0);
        format!("{mb:.1} MB")
    }
}

fn downloads_folder() -> PathBuf {
    let home = crate::platform::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let downloads = home.join("Downloads");
    if downloads.is_dir() { downloads } else { home }
}

impl OfferState {
    fn folder(&self) -> PathBuf {
        #[cfg(test)]
        if let Some(folder) = &self.folder {
            return folder.clone();
        }
        downloads_folder()
    }

    pub(crate) fn download(&self) -> HashMap<Key, Download> {
        self.downloads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Start saving offer `key` off the UI thread, waking `ctx` when done.
    pub(crate) fn start(
        &mut self,
        ctx: &egui::Context,
        remote: &Arc<dyn RemoteControl>,
        offer: &RemoteOffer,
    ) {
        let key = (offer.host.clone(), offer.id);
        let name = offer.name.clone();
        let downloads = Arc::clone(&self.downloads);
        let set = move |state: Download| {
            downloads
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(key.clone(), state);
        };
        set(Download::Saving { name: name.clone() });
        let (remote, ctx, folder) = (Arc::clone(remote), ctx.clone(), self.folder());
        let host = offer.host.clone();
        let id = offer.id;
        let spawned = std::thread::Builder::new()
            .name("connecto-offer-download".to_owned())
            .spawn({
                let set = set.clone();
                let name = name.clone();
                move || {
                    let saved = remote.download_offer(&host, id).and_then(|(file, bytes)| {
                        // Only a plain name from the peer: never a folder.
                        let file = Path::new(&file)
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .filter(|n| !n.starts_with('.'))
                            .unwrap_or_else(|| name.clone());
                        crate::preview::save_new(&folder, &file, &bytes)
                            .map_err(|e| format!("it could not be saved: {e}"))
                    });
                    set(match saved {
                        Ok(path) => Download::Saved(path),
                        Err(why) => Download::Failed { name, why },
                    });
                    ctx.request_repaint();
                }
            });
        if spawned.is_err() {
            set(Download::Failed {
                name,
                why: "it could not be started".to_owned(),
            });
        }
    }

    pub(crate) fn close(&mut self, key: Key) {
        self.downloads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&key);
        self.closed.insert(key);
    }
}

/// One line and its buttons.
pub(crate) struct Line {
    pub(crate) text: String,
    pub(crate) buttons: Vec<(&'static str, &'static str, OfferAction)>,
}

/// What the strip says now: a finished download first, then the oldest offer
/// not closed, then one being saved.
pub(crate) fn line_for(offers: &[RemoteOffer], state: &OfferState) -> Option<Line> {
    let downloads = state.download();
    for offer in offers {
        let key = (offer.host.clone(), offer.id);
        match downloads.get(&key) {
            Some(Download::Saved(path)) => {
                let shown = path.file_name().map_or_else(
                    || path.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                );
                return Some(Line {
                    text: format!("Saved {shown} from {} to Downloads.", offer.host),
                    buttons: vec![
                        ("offer-open", "Open", OfferAction::Open(path.clone())),
                        (
                            "offer-reveal",
                            "Show in Finder",
                            OfferAction::Reveal(path.clone()),
                        ),
                        ("offer-close", "Close", OfferAction::Close(key)),
                    ],
                });
            }
            Some(Download::Failed { name, why }) => {
                return Some(Line {
                    text: format!("{name} from {} was not downloaded: {why}", offer.host),
                    buttons: vec![
                        (
                            "offer-download",
                            "Try again",
                            OfferAction::Download(key.clone()),
                        ),
                        ("offer-close", "Close", OfferAction::Close(key)),
                    ],
                });
            }
            _ => {}
        }
    }
    // A saved file whose offer is gone (it expired) still says where it is.
    for (key, download) in &downloads {
        if let Download::Saved(path) = download
            && !offers.iter().any(|o| o.host == key.0 && o.id == key.1)
        {
            return Some(Line {
                text: format!("Saved {} from {}.", path.display(), key.0),
                buttons: vec![
                    ("offer-open", "Open", OfferAction::Open(path.clone())),
                    (
                        "offer-reveal",
                        "Show in Finder",
                        OfferAction::Reveal(path.clone()),
                    ),
                    ("offer-close", "Close", OfferAction::Close(key.clone())),
                ],
            });
        }
    }
    if let Some(offer) = offers.iter().find(|o| {
        let key = (o.host.clone(), o.id);
        !state.closed.contains(&key) && !downloads.contains_key(&key)
    }) {
        let key = (offer.host.clone(), offer.id);
        return Some(Line {
            text: format!(
                "{} offers {} ({}) from {} in {}.",
                offer.host,
                offer.name,
                size(offer.bytes),
                offer.terminal,
                offer.project
            ),
            buttons: vec![
                (
                    "offer-download",
                    "Download",
                    OfferAction::Download(key.clone()),
                ),
                ("offer-close", "Dismiss", OfferAction::Close(key)),
            ],
        });
    }
    downloads.values().find_map(|download| match download {
        Download::Saving { name } => Some(Line {
            text: format!("Downloading {name}…"),
            buttons: Vec::new(),
        }),
        _ => None,
    })
}

pub(crate) fn draw(ui: &mut Ui, area: Rect, line: &Line, out: &mut Out<OfferAction>) -> Rect {
    let strip = Rect::from_min_size(area.min, vec2(area.width(), HEIGHT));
    let painter = ui.painter_at(strip);
    painter.rect_filled(strip, 0.0, theme::RAISED_BG);
    painter.hline(
        strip.x_range(),
        strip.max.y - 0.5,
        Stroke::new(1.0, theme::BORDER),
    );
    let inner = strip.shrink2(vec2(14.0, 4.0));
    ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
        ui.horizontal_centered(|ui| {
            out.say(ui, RichText::new(&line.text).size(12.5).color(theme::TEXT));
            for (key, label, action) in &line.buttons {
                if out.button(ui, key, label, true) {
                    out.actions.push(action.clone());
                }
            }
        });
    });
    Rect::from_min_max(
        egui::pos2(area.min.x, strip.max.y.min(area.max.y)),
        area.max,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn offer(id: u64) -> RemoteOffer {
        RemoteOffer {
            host: "build box".into(),
            id,
            project: "web".into(),
            terminal: "claude-1".into(),
            name: "report.html".into(),
            bytes: 2 * 1024 * 1024 + 100_000,
        }
    }

    #[test]
    fn an_offer_reads_with_its_size_and_who_offered_it() {
        let state = OfferState::default();
        let line = line_for(&[offer(1)], &state).unwrap();
        assert_eq!(
            line.text,
            "build box offers report.html (2.1 MB) from claude-1 in web."
        );
        assert_eq!(
            line.buttons[0].2,
            OfferAction::Download(("build box".into(), 1))
        );
        let mut state = state;
        state.close(("build box".into(), 1));
        assert!(line_for(&[offer(1)], &state).is_none());
    }

    #[test]
    fn a_saved_name_never_replaces_a_file_already_there() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"mine").unwrap();
        let saved = crate::preview::save_new(dir.path(), "a.txt", b"new").unwrap();
        assert_eq!(saved, dir.path().join("a (1).txt"));
        assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"mine");
        let readme = crate::preview::save_new(dir.path(), "README", b"r").unwrap();
        assert_eq!(readme, dir.path().join("README"));
        let again = crate::preview::save_new(dir.path(), "README", b"r2").unwrap();
        assert_eq!(again, dir.path().join("README (1)"));
    }

    /// Many saves of one name at once: each gets a name of its own, and
    /// every file holds what was saved under it.
    #[test]
    fn saves_of_one_name_at_once_never_overwrite_each_other() {
        let dir = Arc::new(tempfile::tempdir().unwrap());
        let threads: Vec<_> = (0..8u8)
            .map(|n| {
                let dir = Arc::clone(&dir);
                std::thread::spawn(move || {
                    let path = crate::preview::save_new(dir.path(), "r.html", &[n; 64]).unwrap();
                    (n, path)
                })
            })
            .collect();
        let saved: Vec<(u8, PathBuf)> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        let names: HashSet<&PathBuf> = saved.iter().map(|(_, p)| p).collect();
        assert_eq!(names.len(), 8, "each its own name");
        for (n, path) in &saved {
            assert_eq!(std::fs::read(path).unwrap(), vec![*n; 64]);
        }
    }
}
