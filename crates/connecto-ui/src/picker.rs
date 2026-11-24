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

/// Where to save an exported package: the system save panel, starting with
/// `file_name`.
#[cfg(not(feature = "ui-test-driver"))]
pub(crate) fn save_package(file_name: &str) -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_file_name(file_name)
        .add_filter("Connecto package", &[PACKAGE_EXT])
        .save_file()
}

#[cfg(feature = "ui-test-driver")]
pub(crate) fn save_package(file_name: &str) -> Option<PathBuf> {
    std::env::var_os("CONNECTO_UI_SAVE")
        .map(PathBuf::from)
        .or_else(|| {
            rfd::FileDialog::new()
                .set_file_name(file_name)
                .add_filter("Connecto package", &[PACKAGE_EXT])
                .save_file()
        })
}

/// An exported package to import: the system open panel.
#[cfg(not(feature = "ui-test-driver"))]
pub(crate) fn open_package() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .add_filter("Connecto package", &[PACKAGE_EXT])
        .pick_file()
}

#[cfg(feature = "ui-test-driver")]
pub(crate) fn open_package() -> Option<PathBuf> {
    std::env::var_os("CONNECTO_UI_OPEN")
        .map(PathBuf::from)
        .or_else(|| {
            rfd::FileDialog::new()
                .add_filter("Connecto package", &[PACKAGE_EXT])
                .pick_file()
        })
}

/// The extension of an exported package (`connecto_packages::bundle::EXTENSION`;
/// this crate does not depend on that one).
pub(crate) const PACKAGE_EXT: &str = "connecto-package";
