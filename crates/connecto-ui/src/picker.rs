//! The system folder panel.

use std::path::PathBuf;

#[cfg(not(feature = "ui-test-driver"))]
pub(crate) fn pick_folder() -> Option<PathBuf> {
    rfd::FileDialog::new().pick_folder()
}

/// The OS panel runs in a system service no test can drive. This skips only the
/// panel: everything after it is the normal path.
#[cfg(feature = "ui-test-driver")]
pub(crate) fn pick_folder() -> Option<PathBuf> {
    std::env::var_os("CONNECTO_UI_PICK")
        .map(PathBuf::from)
        .or_else(|| rfd::FileDialog::new().pick_folder())
}
