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
        self.slots_dir.join(slot)
    }

    pub fn slot_home(&self, slot: &str) -> PathBuf {
        self.slot_dir(slot).join("home")
    }

    pub fn slot_sqlite_home(&self, slot: &str) -> PathBuf {
        self.slot_home(slot).join("sqlite")
    }

    pub fn target_file(&self, target: &str) -> PathBuf {
        self.targets_dir.join(format!("{target}.toml"))
    }

    pub fn serve_dir(&self) -> PathBuf {
        self.manager_dir.join("serve")
    }

    pub fn remote_tui_sqlite_home(&self) -> PathBuf {
        self.serve_dir()
            .join("remote-tui")
            .join(std::process::id().to_string())
            .join("sqlite")
    }

    pub fn serve_state_file(&self) -> PathBuf {
        self.serve_dir().join("default.json")
    }

    pub fn serve_control_socket(&self) -> PathBuf {
        self.serve_dir().join("control.sock")
    }

    pub fn serve_sessions_dir(&self) -> PathBuf {
        self.serve_dir().join("sessions")
    }

    pub fn serve_session_file(&self, session_id: &str) -> PathBuf {
        self.serve_sessions_dir().join(format!("{session_id}.json"))
    }

    pub fn serve_event_journal_file(&self) -> PathBuf {
        self.serve_dir().join("events.ndjson")
    }

    pub fn serve_channels_dir(&self) -> PathBuf {
        self.serve_dir().join("channels")
    }

    pub fn telegram_channel_state_file(&self) -> PathBuf {
        self.serve_channels_dir().join("telegram.json")
    }

    pub fn service_dir(&self) -> PathBuf {
        self.manager_dir.join("service")
    }

    pub fn service_state_file(&self) -> PathBuf {
        self.service_dir().join("default.json")
    }

    pub fn service_log_file(&self) -> PathBuf {
        self.service_dir().join("default.log")
    }

    pub fn service_token_file(&self) -> PathBuf {
        self.service_dir().join("tokens.json")
    }

    pub fn service_launchd_plist_file(&self) -> Result<PathBuf> {
        Ok(home_dir()?.join("Library/LaunchAgents/dev.xiaotian.cx.service.plist"))
    }
}

pub fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}
