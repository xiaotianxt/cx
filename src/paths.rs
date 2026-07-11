use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;

#[derive(Debug, Clone)]
pub struct ManagerPaths {
    pub base_codex_home: PathBuf,
    pub manager_dir: PathBuf,
    pub slots_dir: PathBuf,
    pub targets_dir: PathBuf,
    pub rotation_file: PathBuf,
}

impl ManagerPaths {
    pub fn new(manager_dir: Option<PathBuf>) -> Result<Self> {
        let home = home_dir()?;
        let manager_dir = manager_dir
            .or_else(|| std::env::var_os("CX_PROFILE_MANAGER_DIR").map(PathBuf::from))
            .unwrap_or_else(|| home.join(".codex/profile-manager"));
        let base_codex_home = manager_dir
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));
        Ok(Self {
            slots_dir: manager_dir.join("slots"),
            targets_dir: manager_dir.join("targets"),
            rotation_file: manager_dir.join("rotation.txt"),
            manager_dir,
            base_codex_home,
        })
    }

    #[cfg(test)]
    pub(crate) fn from_roots(base_codex_home: PathBuf, manager_dir: PathBuf) -> Self {
        Self {
            slots_dir: manager_dir.join("slots"),
            targets_dir: manager_dir.join("targets"),
            rotation_file: manager_dir.join("rotation.txt"),
            manager_dir,
            base_codex_home,
        }
    }

    pub fn slot_dir(&self, slot: &str) -> PathBuf {
        if slot == "default" {
            return self.base_codex_home.clone();
        }
        self.slots_dir.join(slot)
    }

    pub fn slot_home(&self, slot: &str) -> PathBuf {
        if slot == "default" {
            return self.base_codex_home.clone();
        }
        self.slot_dir(slot).join("home")
    }

    /// Shared SQLite index directory used by all slots so that Desktop and CLI
    /// see threads from every provider. Lives at `~/.codex/sqlite`.
    pub fn shared_sqlite_home(&self) -> PathBuf {
        self.base_codex_home.join("sqlite")
    }

    pub fn target_file(&self, target: &str) -> PathBuf {
        self.targets_dir.join(format!("{target}.toml"))
    }
}

pub fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}
