//! Skills and plugins as packages: Connecto Home, the built-in `connecto`
//! package, packages users make, per-project settings, and installing the
//! packages into each project (with a lockfile, so updates and removals are
//! exact).
//!
//! The UI holds an `Arc<dyn connecto_core::PackagesControl>` (see [`Packages`]);
//! everything here runs on its own thread.

pub mod commit;
mod engine;
mod fsx;
pub mod guide;
pub mod home;
mod host;
pub mod install;
pub mod package;
mod platform;
mod session;
pub mod state;

use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use connecto_core::{PackagesControl, PackagesView, ProjectSettings};
use connecto_session::Wake;
use crossbeam_channel::{Sender, bounded};

pub use engine::{Cmd, Config, Core, NO_HOME, TICK};
pub use host::{Host, ProjectInfo, ProjectRef};
/// Where the built-in package puts the PowerShell status line scripts on
/// Windows, relative to the project (next to the `.sh` originals, same base
/// names). `connecto-adapter` cannot depend on this crate (this crate depends
/// on it through `connecto-session`), so it names them itself; a test in
/// `package.rs` pins these values.
pub use package::{STATUSLINE_PS1, SUBAGENT_STATUSLINE_PS1};

/// The running manager, as the UI's handle.
pub struct Packages {
    view: Arc<ArcSwap<PackagesView>>,
    tx: Sender<Cmd>,
    data_root: PathBuf,
    user_home: PathBuf,
    host: Arc<dyn Host>,
}

impl Packages {
    /// Start the manager for the app, with the given host (the app's
    /// workspace and automations). `wake` is called whenever the view changes.
    pub fn start(data_root: PathBuf, host: Arc<dyn Host>, wake: Wake) -> std::io::Result<Self> {
        // `HOME`; `USERPROFILE` on Windows.
        let user_home = std::env::var_os(platform::HOME_VAR)
            .map(PathBuf::from)
            .ok_or_else(|| std::io::Error::other(format!("{} is not set", platform::HOME_VAR)))?;
        Self::with(Config {
            data_root,
            user_home,
            host,
            wake,
        })
    }

    /// Start with parts of your own (tests).
    pub fn with(config: Config) -> std::io::Result<Self> {
        let (data_root, user_home, host) = (
            config.data_root.clone(),
            config.user_home.clone(),
            Arc::clone(&config.host),
        );
        let core = Core::new(config);
        let view = core.view();
        let (tx, rx) = bounded::<Cmd>(64);
        std::thread::Builder::new()
            .name("packages".into())
            .spawn(move || engine::run(core, &rx))?;
        Ok(Self {
            view,
            tx,
            data_root,
            user_home,
            host,
        })
    }

    fn send(&self, cmd: Cmd) -> Result<(), String> {
        self.tx
            .try_send(cmd)
            .map_err(|_| "busy: try again".to_owned())
    }

    /// Stop the manager's thread. Also happens on drop.
    pub fn stop(&self) {
        let _ = self.tx.try_send(Cmd::Stop);
    }

    fn has_home(&self) -> Result<(), String> {
        if self.view().home.is_some() {
            Ok(())
        } else {
            Err(NO_HOME.to_owned())
        }
    }

    /// A user package that is in the list (not the built-in one).
    fn user_package(&self, id: &str) -> Result<(), String> {
        if id == package::BUILTIN_ID {
            return Err("the built-in package cannot be changed".to_owned());
        }
        if self.view().packages.iter().any(|p| p.id == id) {
            Ok(())
        } else {
            Err("no such package".to_owned())
        }
    }
}

impl Drop for Packages {
    fn drop(&mut self) {
        self.stop();
    }
}

impl PackagesControl for Packages {
    fn view(&self) -> Arc<PackagesView> {
        self.view.load_full()
    }

    fn set_home(&self, path: PathBuf) -> Result<(), String> {
        let real = home::validate(&path, &self.data_root, &self.user_home)?;
        self.send(Cmd::SetHome(real))
    }

    fn set_default_package(&self, package: Option<&str>) -> Result<(), String> {
        if let Some(id) = package
            && !self
                .view()
                .packages
                .iter()
                .any(|p| p.id == id && p.errors.is_empty())
        {
            return Err("no such package".to_owned());
        }
        self.send(Cmd::SetDefault(package.map(str::to_owned)))
    }

    fn create_package(&self, name: &str, extends: Option<&str>) -> Result<(), String> {
        self.has_home()?;
        let name = connecto_hook::package::check_name(name)?;
        if let Some(base) = extends
            && !self
                .view()
                .packages
                .iter()
                .any(|p| p.id == base && p.errors.is_empty())
        {
            return Err("the package to extend does not exist".to_owned());
        }
        self.send(Cmd::Create {
            name: name.to_owned(),
            extends: extends.map(str::to_owned),
        })
    }

    fn edit_package(&self, id: &str) -> Result<(), String> {
        self.has_home()?;
        self.user_package(id)?;
        self.send(Cmd::Edit(id.to_owned()))
    }

    fn delete_package(&self, id: &str) -> Result<(), String> {
        self.has_home()?;
        self.user_package(id)?;
        self.send(Cmd::Delete(id.to_owned()))
    }

    fn set_project_settings(
        &self,
        project_id: &str,
        settings: ProjectSettings,
    ) -> Result<(), String> {
        if !self.host.projects().iter().any(|p| p.id == project_id) {
            return Err("no such project".to_owned());
        }
        let view = self.view();
        for id in &settings.packages {
            if !view.packages.iter().any(|p| p.id == *id) {
                return Err(format!("no such package: {id}"));
            }
        }
        self.send(Cmd::SetProject(project_id.to_owned(), settings))
    }

    fn refresh_project(&self, project_id: &str) {
        let _ = self.send(Cmd::Refresh(project_id.to_owned()));
    }
}
