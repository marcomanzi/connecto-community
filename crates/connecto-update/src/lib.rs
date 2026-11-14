//! Connecto updating itself.
//!
//! - [`feed`]: `update.json` at `connecto.studio/downloads/`, an ed25519-signed
//!   feed naming the newest release and, per platform, its archive with size
//!   and SHA-256. The app holds the public key (`update-public-key.txt`, built
//!   in); the release tool holds the private one.
//! - [`service`]: checks at launch and daily (or when asked), downloads and
//!   checks a newer release in the background, then waits for the person:
//!   "Update and restart" or "Later".
//! - [`install`]: download, SHA-256, unpack; on macOS the new bundle must be
//!   intact, signed by the same Developer ID team and accepted by Gatekeeper
//!   (notarized). On Windows the install folder is fixed,
//!   `%LOCALAPPDATA%\Programs\Connecto`.
//! - [`helper`]: the hand-over script that swaps the new version in after the
//!   app quits, starts it, and puts the previous one back if it does not start.
//! - [`state`]: what is kept between launches, and what a launch after an
//!   update means.

pub mod feed;
pub mod helper;
pub mod install;
pub mod service;
pub mod state;
pub mod version;

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use service::{Config, Updater};
pub use version::Version;

/// Where the feed is published.
pub const FEED_URL: &str = "https://connecto.studio/downloads/update.json";

/// The public key built into this build: the text of
/// `crates/connecto-update/update-public-key.txt`. Empty: no updates.
const PUBLIC_KEY_TEXT: &str = include_str!("../update-public-key.txt");

/// The update public key of this build, if it has one.
#[must_use]
pub fn built_in_key() -> Option<ed25519_dalek::VerifyingKey> {
    let text = PUBLIC_KEY_TEXT.trim();
    if text.is_empty() {
        return None;
    }
    feed::public_key_from_text(text).ok()
}

/// What to do at app start, before the window: settle an update that may be
/// under way. A new version that has just been installed tells the hand-over
/// it started once it has been up a while (on a thread of its own). Returns a
/// notice for the person, if there is one.
#[must_use]
pub fn start_of_app(data_dir: &Path, current: Version) -> Option<String> {
    let launch = state::on_launch(state::Pending::load(data_dir), current);
    let notice = state::settle_launch(data_dir, launch);
    if let state::Launch::Updated { to, .. } = launch {
        let dir = data_dir.to_path_buf();
        let _ = std::thread::Builder::new()
            .name("update-healthy".into())
            .spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(state::HEALTHY_AFTER_SECS));
                state::mark_healthy(&dir, &to);
            });
    }
    notice
}

/// Start the updater for the app.
#[must_use]
pub fn start_for_app(
    data_dir: PathBuf,
    current: Version,
    exe: PathBuf,
    wake: Arc<dyn Fn() + Send + Sync>,
    notice: Option<String>,
) -> Arc<Updater> {
    let feed_url = std::env::var("CONNECTO_UPDATE_FEED_URL")
        .ok()
        .filter(|url| url.starts_with("https://"))
        .unwrap_or_else(|| FEED_URL.to_owned());
    Updater::start(Config {
        data_dir,
        current,
        feed_url,
        key: built_in_key(),
        exe,
        wake,
        notice,
    })
}
