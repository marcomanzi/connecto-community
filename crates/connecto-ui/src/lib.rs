//! The desktop window: a sidebar of open projects and their terminals, and the
//! terminal on show, drawn from snapshots.
//!
//! This crate depends only on `connecto-core`. It reaches a session through
//! [`connecto_core::TerminalSession`], whose methods never block, so nothing on
//! the UI thread can wait on a child process or the network.

mod app;
mod chooser;
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
mod keymap;
mod links;
mod menu;
mod metrics;
mod pairing;
mod pane;
mod picker;
mod platform;
mod project_settings;
// The relay panel is not drawn since project-tree-ui (R20). It stays compiled
// and tested so it can come back.
mod automations;
#[allow(dead_code)]
mod relay_panel;
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
mod tree;
mod update_strip;
mod utility;
mod watchdog;

pub use app::{App, Diagnostics, PaneConfig, WakeSlot, run};
pub use metrics::MetricsSink;
pub use setup_window::run_setup;
#[cfg(feature = "frame-capture")]
pub use shots::ScreenshotSink;
