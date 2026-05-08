//! Persistent Telegram channel state.
//!
//! State schema, route binding, and private state-file writes live here so the
//! channel driver does not need to know storage details.

use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;

use crate::paths::ManagerPaths;
use crate::session;
use crate::session::ChannelId;
use crate::session::CreateSessionRequest;
use crate::session::SessionId;

use super::api::TelegramMessage;
use super::transcript::TelegramActivityState;
use super::transcript::TelegramStatusState;
use super::transcript::TelegramThinkingState;
use super::work_topic_title;
use super::AppThreadPortalEntry;

const TELEGRAM_STATE_SCHEMA_VERSION: u64 = 1;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) struct TelegramState {
    pub(super) schema_version: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) last_update_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) trusted_routes: Vec<TelegramRoute>,
    pub(super) bindings: Vec<TelegramBinding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) active_routes: Vec<TelegramActiveRoute>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) struct TelegramBinding {
    pub(super) chat_id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) message_thread_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) alias: Option<String>,
    pub(super) channel_id: ChannelId,
    pub(super) session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) app_thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) app_thread_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) app_thread_cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) topic_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) panel_message_id: Option<i64>,
    #[serde(default)]
    pub(super) telegram_paused: bool,
    #[serde(default)]
    pub(super) topic_created_by_adapter: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) watch_proxy_offset: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) watch_rollout_offset: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) watch_source: Option<TelegramWatchSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) watch_last_agent_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) watch_activity: Option<TelegramActivityState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) watch_thinking: Option<TelegramThinkingState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) watch_status: Option<TelegramStatusState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) watch_pending_approvals: Vec<TelegramPendingApproval>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) enum TelegramWatchSource {
    Proxy,
    Rollout,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) struct TelegramPendingApproval {
    #[serde(default)]
    pub(super) connection_id: u64,
    pub(super) request_id: String,
    pub(super) command: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) struct TelegramActiveRoute {
    pub(super) chat_id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) message_thread_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) alias: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub(super) struct TelegramRoute {
    pub(super) chat_id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) message_thread_id: Option<i64>,
}

impl TelegramState {
    pub(super) fn empty() -> Self {
        Self {
            schema_version: TELEGRAM_STATE_SCHEMA_VERSION,
            last_update_id: None,
            trusted_routes: Vec::new(),
            bindings: Vec::new(),
            active_routes: Vec::new(),
        }
    }

    pub(super) fn trust_route(&mut self, route: &TelegramRoute) {
        if !self.trusted_routes.contains(route) {
            self.trusted_routes.push(route.clone());
        }
    }

    pub(super) fn binding_for_route(
        &self,
        route: &TelegramRoute,
        alias: Option<&str>,
    ) -> Option<&TelegramBinding> {
        self.bindings.iter().find(|binding| {
            binding.chat_id == route.chat_id
                && binding.message_thread_id == route.message_thread_id
                && binding.alias.as_deref() == alias
        })
    }

    pub(super) fn active_binding_for_route(
        &self,
        route: &TelegramRoute,
    ) -> Option<&TelegramBinding> {
        let active_alias = self.active_alias_for_route(route);
        self.binding_for_route(route, active_alias)
            .or_else(|| self.binding_for_route(route, None))
    }

    pub(super) fn active_alias_for_route(&self, route: &TelegramRoute) -> Option<&str> {
        self.active_routes
            .iter()
            .find(|active| {
                active.chat_id == route.chat_id
                    && active.message_thread_id == route.message_thread_id
            })
            .and_then(|active| active.alias.as_deref())
    }

    pub(super) fn binding_for_route_mut(
        &mut self,
        route: &TelegramRoute,
        alias: Option<&str>,
    ) -> Option<&mut TelegramBinding> {
        self.bindings.iter_mut().find(|binding| {
            binding.chat_id == route.chat_id
                && binding.message_thread_id == route.message_thread_id
                && binding.alias.as_deref() == alias
        })
    }

    pub(super) fn bind_route(
        &mut self,
        paths: &ManagerPaths,
        route: &TelegramRoute,
        alias: Option<&str>,
    ) -> Result<TelegramBinding> {
        if let Some(binding) = self.binding_for_route(route, alias) {
            return Ok(binding.clone());
        }
        let channel_id = route.channel_id(alias)?;
        let result = session::create_session(
            paths,
            CreateSessionRequest {
                session_id: None,
                channel_id: channel_id.clone(),
            },
        )?;
        let binding = TelegramBinding {
            chat_id: route.chat_id,
            message_thread_id: route.message_thread_id,
            alias: alias.map(str::to_string),
            channel_id,
            session_id: result.session.session_id,
            app_thread_id: None,
            app_thread_title: None,
            app_thread_cwd: None,
            topic_title: None,
            panel_message_id: None,
            telegram_paused: false,
            topic_created_by_adapter: false,
            watch_proxy_offset: None,
            watch_rollout_offset: None,
            watch_source: None,
            watch_last_agent_message: None,
            watch_activity: None,
            watch_thinking: None,
            watch_status: None,
            watch_pending_approvals: Vec::new(),
        };
        self.bindings.push(binding.clone());
        self.set_active_route(route, alias);
        Ok(binding)
    }

    pub(super) fn bind_app_thread(
        &mut self,
        paths: &ManagerPaths,
        route: &TelegramRoute,
        app_thread: &AppThreadPortalEntry,
        topic_created_by_adapter: bool,
    ) -> Result<TelegramBinding> {
        let mut binding = self.bind_route(paths, route, None)?;
        let existing_session_id = binding.session_id.clone();
        if let Some(stored) = self.binding_for_route_mut(route, None) {
            stored.app_thread_id = Some(app_thread.thread_id.clone());
            stored.app_thread_title = app_thread.title.clone();
            stored.app_thread_cwd = Some(app_thread.cwd.clone());
            stored.topic_title = Some(work_topic_title(app_thread));
            stored.panel_message_id = None;
            stored.telegram_paused = false;
            stored.topic_created_by_adapter = topic_created_by_adapter;
            stored.watch_proxy_offset = None;
            stored.watch_rollout_offset = None;
            stored.watch_source = None;
            stored.watch_last_agent_message = None;
            stored.watch_activity = None;
            stored.watch_thinking = None;
            stored.watch_status = None;
            stored.watch_pending_approvals.clear();
            binding = stored.clone();
        }
        debug_assert_eq!(binding.session_id, existing_session_id);
        Ok(binding)
    }

    pub(super) fn set_topic_title(&mut self, route: &TelegramRoute, title: &str) {
        for binding in self.bindings.iter_mut().filter(|binding| {
            binding.chat_id == route.chat_id && binding.message_thread_id == route.message_thread_id
        }) {
            binding.topic_title = Some(title.to_string());
        }
    }

    pub(super) fn panel_message_id_for_route(&self, route: &TelegramRoute) -> Option<i64> {
        self.bindings
            .iter()
            .find(|binding| {
                binding.chat_id == route.chat_id
                    && binding.message_thread_id == route.message_thread_id
            })
            .and_then(|binding| binding.panel_message_id)
    }

    pub(super) fn remember_panel_message(&mut self, route: &TelegramRoute, message_id: i64) {
        for binding in self.bindings.iter_mut().filter(|binding| {
            binding.chat_id == route.chat_id && binding.message_thread_id == route.message_thread_id
        }) {
            binding.panel_message_id = Some(message_id);
        }
    }

    pub(super) fn set_active_route(&mut self, route: &TelegramRoute, alias: Option<&str>) {
        if let Some(active) = self.active_routes.iter_mut().find(|active| {
            active.chat_id == route.chat_id && active.message_thread_id == route.message_thread_id
        }) {
            active.alias = alias.map(str::to_string);
            return;
        }
        self.active_routes.push(TelegramActiveRoute {
            chat_id: route.chat_id,
            message_thread_id: route.message_thread_id,
            alias: alias.map(str::to_string),
        });
    }

    pub(super) fn remove_route_binding(
        &mut self,
        route: &TelegramRoute,
        alias: Option<&str>,
    ) -> bool {
        let original_len = self.bindings.len();
        self.bindings.retain(|binding| {
            !(binding.chat_id == route.chat_id
                && binding.message_thread_id == route.message_thread_id
                && binding.alias.as_deref() == alias)
        });
        if original_len != self.bindings.len() {
            self.active_routes.retain(|active| {
                !(active.chat_id == route.chat_id
                    && active.message_thread_id == route.message_thread_id
                    && active.alias.as_deref() == alias)
            });
            return true;
        }
        false
    }

    pub(super) fn remove_all_route_bindings(&mut self, route: &TelegramRoute) -> bool {
        let original_len = self.bindings.len();
        self.bindings.retain(|binding| {
            !(binding.chat_id == route.chat_id
                && binding.message_thread_id == route.message_thread_id)
        });
        if original_len != self.bindings.len() {
            self.active_routes.retain(|active| {
                !(active.chat_id == route.chat_id
                    && active.message_thread_id == route.message_thread_id)
            });
            return true;
        }
        false
    }
}

impl TelegramRoute {
    pub(super) fn from_message(message: &TelegramMessage) -> Self {
        Self {
            chat_id: message.chat.id,
            message_thread_id: message.message_thread_id,
        }
    }

    pub(super) fn channel_id(&self, alias: Option<&str>) -> Result<ChannelId> {
        let mut raw = format!("telegram:{}", self.chat_id);
        if let Some(thread_id) = self.message_thread_id {
            raw.push_str(&format!(":topic:{thread_id}"));
        }
        if let Some(alias) = alias {
            raw.push_str(":session:");
            raw.push_str(alias);
        }
        ChannelId::parse(raw)
    }

    pub(super) fn display(&self) -> String {
        match self.message_thread_id {
            Some(thread_id) => format!("{} topic {}", self.chat_id, thread_id),
            None => self.chat_id.to_string(),
        }
    }
}

pub(super) fn read_state(paths: &ManagerPaths) -> Result<TelegramState> {
    let path = paths.telegram_channel_state_file();
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TelegramState::empty());
        }
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    let state = serde_json::from_str::<TelegramState>(&content)
        .with_context(|| format!("parse {}", path.display()))?;
    if state.schema_version != TELEGRAM_STATE_SCHEMA_VERSION {
        anyhow::bail!(
            "unsupported Telegram state schema version: {}",
            state.schema_version
        );
    }
    Ok(state)
}

pub(super) fn write_state(paths: &ManagerPaths, state: &TelegramState) -> Result<()> {
    fs::create_dir_all(paths.serve_channels_dir())
        .with_context(|| format!("create {}", paths.serve_channels_dir().display()))?;
    set_private_dir_permissions(&paths.serve_dir())?;
    set_private_dir_permissions(&paths.serve_channels_dir())?;

    let path = paths.telegram_channel_state_file();
    let tmp_path = paths.serve_channels_dir().join("telegram.json.tmp");
    let content = serde_json::to_vec_pretty(state).context("serialize Telegram state")?;
    let mut file = private_open_for_write(&tmp_path)?;
    file.write_all(&content)
        .with_context(|| format!("write {}", tmp_path.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("write {}", tmp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync {}", tmp_path.display()))?;
    fs::rename(&tmp_path, &path)
        .with_context(|| format!("rename {} to {}", tmp_path.display(), path.display()))?;
    set_private_file_permissions(&path)?;
    Ok(())
}

fn private_open_for_write(path: &Path) -> Result<fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

fn set_private_dir_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod 0700 {}", path.display()))?;
    }
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 0600 {}", path.display()))?;
    }
    Ok(())
}
