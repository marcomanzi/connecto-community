//! The desktop window: a sidebar of open projects and their terminals, and the
//! terminal on show, drawn from snapshots.
//!
//! This crate depends only on `connecto-core`. It reaches a session through
//! [`connecto_core::TerminalSession`], whose methods never block, so nothing on
//! the UI thread can wait on a child process or the network.

mod app;
mod chooser;
#[cfg(target_os = "macos")]
mod clipboard_files;
mod commands;
mod confirm;
mod dialog;
mod dots;
#[cfg(feature = "ui-test-driver")]
mod driver;
mod fonts;
mod forms;
mod header;
mod history;
mod hosts;
mod icons;
mod job_question;
mod keymap;
mod links;
mod markdown;
mod memory;
mod menu;
mod metrics;
mod notice;
mod offer_strip;
mod operations;
mod pairing;
mod pane;
mod pasted;
mod pending;
mod picker;
mod platform;
mod preview;
mod project_files;
mod project_settings;
mod providers;
// The relay panel is not drawn since project-tree-ui (R20). It stays compiled
// and tested so it can come back.
mod agentic_works;
mod automations;
#[allow(dead_code)]
mod relay_panel;
mod remote_vaults;
mod rename;
mod render;
mod selection;
mod settings;
mod setup_window;
mod sheet;
#[cfg(feature = "frame-capture")]
mod shots;
mod skills;
#[cfg(test)]
mod testkit;
#[cfg(test)]
mod tests;
mod theme;
mod token_opt;
mod tree;
mod update_strip;
mod upload;
mod utility;
mod vault_ui;
mod watchdog;
mod words;

pub use app::run_with_memory;
pub use app::{App, Diagnostics, PaneConfig, WakeSlot, run};
pub use metrics::MetricsSink;
pub use setup_window::run_setup;
#[cfg(feature = "frame-capture")]
pub use shots::ScreenshotSink;
