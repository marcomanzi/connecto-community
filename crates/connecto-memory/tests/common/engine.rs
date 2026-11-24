//! An engine with its threads over the fake host, for the engine tests.

use std::path::PathBuf;
use std::sync::Arc;

use connecto_core::memory::{MemoryControl, ProjectMemoryView};
use connecto_memory::testing::FakeHost;
use connecto_memory::{Engine, EngineConfig, OpenProject};

pub struct World {
    pub dir: tempfile::TempDir,
    pub host: Arc<FakeHost>,
    pub engine: Engine,
}

impl Drop for World {
    fn drop(&mut self) {
        self.engine.shutdown();
    }
}

pub fn world(projects: &[&str]) -> World {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(FakeHost::new());
    host.set_projects(
        projects
            .iter()
            .map(|id| {
                let path = dir.path().join("repos").join(id);
                std::fs::create_dir_all(&path).unwrap();
                OpenProject {
                    id: (*id).to_owned(),
                    path,
                }
            })
            .collect(),
    );
    let engine = Engine::start(EngineConfig {
        data_root: dir.path().join("data"),
        host: Arc::clone(&host) as Arc<dyn connecto_memory::Host>,
    })
    .unwrap();
    World { dir, host, engine }
}

pub fn view(w: &World, id: &str) -> Option<ProjectMemoryView> {
    w.engine.view().project(id).cloned()
}

pub fn db(w: &World, id: &str) -> PathBuf {
    w.dir
        .path()
        .join("data/projects")
        .join(id)
        .join("memory/memory.db")
}
