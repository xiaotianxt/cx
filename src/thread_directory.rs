//! Global thread directory for the broker.
//!
//! Threads are user-facing resources. Workers are runtime locations. This
//! directory keeps those two concepts separate so slot rotation and worker
//! restarts do not change the identity clients route by.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::RwLock;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

use crate::paths::ManagerPaths;
use crate::session;
use crate::worker_pool::WorkerId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum LoadedStatus {
    NotLoaded,
    Loaded,
    Active,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ThreadRecord {
    pub(crate) thread_id: String,
    pub(crate) cx_session_id: Option<String>,
    pub(crate) codex_session_id: Option<String>,
    pub(crate) cwd: String,
    pub(crate) path: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) origin_slot: Option<String>,
    pub(crate) owner_worker_id: Option<WorkerId>,
    pub(crate) loaded_status: LoadedStatus,
    pub(crate) active_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub(crate) active_turn_ids: BTreeSet<String>,
    pub(crate) updated_at_unix: u64,
}

#[derive(Debug, Default)]
pub(crate) struct ThreadDirectory {
    inner: RwLock<BTreeMap<String, ThreadRecord>>,
    completed_turns: RwLock<BTreeMap<String, VecDeque<String>>>,
}

const COMPLETED_TURN_CACHE_LIMIT: usize = 128;

impl ThreadDirectory {
    pub(crate) fn seed_from_sessions(&self, paths: &ManagerPaths) -> Result<()> {
        for record in session::list_sessions(paths)? {
            let Some(app_thread) = record.app_thread else {
                continue;
            };
            let origin_slot = slot_from_thread_path(paths, app_thread.path.as_deref())
                .or_else(|| real_slot(app_thread.slot));
            self.upsert(ThreadRecord {
                thread_id: app_thread.thread_id,
                cx_session_id: Some(record.session_id.to_string()),
                codex_session_id: app_thread.codex_session_id,
                cwd: app_thread.cwd,
                path: app_thread.path,
                title: app_thread.title,
                origin_slot,
                owner_worker_id: None,
                loaded_status: LoadedStatus::NotLoaded,
                active_turn_id: None,
                active_turn_ids: BTreeSet::new(),
                updated_at_unix: app_thread.updated_at_unix,
            })?;
        }
        Ok(())
    }

    pub(crate) fn upsert(&self, record: ThreadRecord) -> Result<()> {
        let mut records = self
            .inner
            .write()
            .map_err(|_| anyhow::anyhow!("thread directory lock poisoned"))?;
        records
            .entry(record.thread_id.clone())
            .and_modify(|existing| merge_thread_record(existing, &record))
            .or_insert(record);
        Ok(())
    }

    pub(crate) fn upsert_thread_value(
        &self,
        worker_id: WorkerId,
        slot: &str,
        thread: &Value,
    ) -> Result<Option<ThreadRecord>> {
        let Some(thread_id) = thread.get("id").and_then(Value::as_str) else {
            return Ok(None);
        };
        let mut status = thread_status(thread);
        let owner_worker_id = match status {
            LoadedStatus::NotLoaded => None,
            LoadedStatus::Loaded | LoadedStatus::Active => Some(worker_id),
        };
        let mut active_turn_ids = in_progress_turn_ids(thread);
        self.remove_completed_turns(thread_id, &mut active_turn_ids)?;
        if status == LoadedStatus::Active && active_turn_ids.is_empty() {
            status = LoadedStatus::Loaded;
        }
        let owner_worker_id = match status {
            LoadedStatus::NotLoaded => None,
            LoadedStatus::Loaded | LoadedStatus::Active => owner_worker_id,
        };
        let active_turn_id = active_turn_ids
            .iter()
            .next_back()
            .filter(|turn_id| !turn_id.is_empty())
            .cloned();
        let record = ThreadRecord {
            thread_id: thread_id.to_string(),
            cx_session_id: None,
            codex_session_id: thread
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_string),
            cwd: thread
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            path: thread
                .get("path")
                .and_then(Value::as_str)
                .map(str::to_string),
            title: thread
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string),
            origin_slot: Some(slot.to_string()),
            owner_worker_id,
            loaded_status: status,
            active_turn_id,
            active_turn_ids,
            updated_at_unix: thread
                .get("updatedAt")
                .and_then(Value::as_i64)
                .map(|updated| updated.max(0) as u64)
                .unwrap_or_else(unix_now_secs),
        };
        self.upsert(record.clone())?;
        Ok(Some(record))
    }

    pub(crate) fn mark_turn_started(
        &self,
        thread_id: &str,
        worker_id: WorkerId,
        turn_id: Option<String>,
    ) -> Result<bool> {
        let turn_key = turn_id.unwrap_or_default();
        if !turn_key.is_empty() && self.turn_was_completed(thread_id, &turn_key)? {
            return Ok(false);
        }
        let mut records = self
            .inner
            .write()
            .map_err(|_| anyhow::anyhow!("thread directory lock poisoned"))?;
        if let Some(record) = records.get_mut(thread_id) {
            record.owner_worker_id = Some(worker_id);
            record.loaded_status = LoadedStatus::Active;
            record.active_turn_ids.insert(turn_key.clone());
            record.active_turn_id = latest_nonempty_turn_id(&record.active_turn_ids);
            record.updated_at_unix = unix_now_secs();
        }
        Ok(true)
    }

    pub(crate) fn mark_turn_completed(&self, thread_id: &str, turn_id: Option<&str>) -> Result<()> {
        if let Some(turn_id) = turn_id.filter(|turn_id| !turn_id.is_empty()) {
            let mut completed_turns = self
                .completed_turns
                .write()
                .map_err(|_| anyhow::anyhow!("thread directory completed-turn lock poisoned"))?;
            let turns = completed_turns.entry(thread_id.to_string()).or_default();
            if !turns.iter().any(|completed| completed == turn_id) {
                turns.push_back(turn_id.to_string());
            }
            while turns.len() > COMPLETED_TURN_CACHE_LIMIT {
                turns.pop_front();
            }
        }
        let mut records = self
            .inner
            .write()
            .map_err(|_| anyhow::anyhow!("thread directory lock poisoned"))?;
        if let Some(record) = records.get_mut(thread_id) {
            if let Some(turn_id) = turn_id {
                record.active_turn_ids.remove(turn_id);
            } else {
                record.active_turn_ids.clear();
            }
            if record.active_turn_ids.is_empty() {
                record.active_turn_id = None;
                record.loaded_status = LoadedStatus::Loaded;
            } else {
                record.active_turn_id = latest_nonempty_turn_id(&record.active_turn_ids);
                record.loaded_status = LoadedStatus::Active;
            }
            record.updated_at_unix = unix_now_secs();
        }
        Ok(())
    }

    pub(crate) fn mark_worker_unavailable(&self, worker_id: &WorkerId) -> Result<usize> {
        let mut records = self
            .inner
            .write()
            .map_err(|_| anyhow::anyhow!("thread directory lock poisoned"))?;
        let mut changed = 0usize;
        for record in records.values_mut() {
            if record.owner_worker_id.as_ref() == Some(worker_id) {
                record.owner_worker_id = None;
                record.loaded_status = LoadedStatus::NotLoaded;
                record.active_turn_id = None;
                record.active_turn_ids.clear();
                record.updated_at_unix = unix_now_secs();
                changed = changed.saturating_add(1);
            }
        }
        Ok(changed)
    }

    pub(crate) fn record(&self, thread_id: &str) -> Result<Option<ThreadRecord>> {
        let records = self
            .inner
            .read()
            .map_err(|_| anyhow::anyhow!("thread directory lock poisoned"))?;
        Ok(records.get(thread_id).cloned())
    }

    pub(crate) fn owner_worker(&self, thread_id: &str) -> Result<Option<WorkerId>> {
        Ok(self
            .record(thread_id)?
            .and_then(|record| record.owner_worker_id))
    }

    pub(crate) fn records(&self) -> Result<Vec<ThreadRecord>> {
        let records = self
            .inner
            .read()
            .map_err(|_| anyhow::anyhow!("thread directory lock poisoned"))?;
        Ok(records.values().cloned().collect())
    }

    fn remove_completed_turns(
        &self,
        thread_id: &str,
        turn_ids: &mut BTreeSet<String>,
    ) -> Result<()> {
        if turn_ids.is_empty() {
            return Ok(());
        }
        let completed = self
            .completed_turns
            .read()
            .map_err(|_| anyhow::anyhow!("thread directory completed-turn lock poisoned"))?
            .get(thread_id)
            .cloned();
        if let Some(completed) = completed {
            turn_ids.retain(|turn_id| !completed.iter().any(|completed| completed == turn_id));
        }
        Ok(())
    }

    fn turn_was_completed(&self, thread_id: &str, turn_id: &str) -> Result<bool> {
        Ok(self
            .completed_turns
            .read()
            .map_err(|_| anyhow::anyhow!("thread directory completed-turn lock poisoned"))?
            .get(thread_id)
            .is_some_and(|turns| turns.iter().any(|completed| completed == turn_id)))
    }
}

fn merge_thread_record(existing: &mut ThreadRecord, incoming: &ThreadRecord) {
    let replace_metadata_fields = should_replace_metadata(existing, incoming);
    let replace_runtime = should_replace_runtime(existing, incoming);
    if replace_metadata_fields {
        replace_metadata(existing, incoming);
    } else {
        fill_missing_metadata(existing, incoming);
    }
    if replace_runtime {
        existing.owner_worker_id = incoming.owner_worker_id.clone();
        existing.loaded_status = incoming.loaded_status;
        existing.active_turn_id = incoming.active_turn_id.clone();
        existing.active_turn_ids = incoming.active_turn_ids.clone();
    }
    if replace_metadata_fields || replace_runtime {
        existing.updated_at_unix = existing.updated_at_unix.max(incoming.updated_at_unix);
    }
}

fn replace_metadata(existing: &mut ThreadRecord, incoming: &ThreadRecord) {
    if incoming.cx_session_id.is_some() {
        existing.cx_session_id = incoming.cx_session_id.clone();
    }
    if incoming.codex_session_id.is_some() {
        existing.codex_session_id = incoming.codex_session_id.clone();
    }
    if !incoming.cwd.is_empty() {
        existing.cwd = incoming.cwd.clone();
    }
    if incoming.path.is_some() {
        existing.path = incoming.path.clone();
    }
    if incoming.title.is_some() {
        existing.title = incoming.title.clone();
    }
    if incoming.origin_slot.is_some() {
        existing.origin_slot = incoming.origin_slot.clone();
    }
}

fn fill_missing_metadata(existing: &mut ThreadRecord, incoming: &ThreadRecord) {
    if existing.cx_session_id.is_none() {
        existing.cx_session_id = incoming.cx_session_id.clone();
    }
    if existing.codex_session_id.is_none() {
        existing.codex_session_id = incoming.codex_session_id.clone();
    }
    if existing.cwd.is_empty() && !incoming.cwd.is_empty() {
        existing.cwd = incoming.cwd.clone();
    }
    if existing.path.is_none() {
        existing.path = incoming.path.clone();
    }
    if existing.title.is_none() {
        existing.title = incoming.title.clone();
    }
    if existing.origin_slot.is_none() {
        existing.origin_slot = incoming.origin_slot.clone();
    }
}

fn should_replace_metadata(existing: &ThreadRecord, incoming: &ThreadRecord) -> bool {
    let existing_priority = status_priority(existing.loaded_status);
    let incoming_priority = status_priority(incoming.loaded_status);
    incoming_priority > existing_priority
        || (incoming_priority == existing_priority
            && incoming.updated_at_unix > existing.updated_at_unix)
}

fn should_replace_runtime(existing: &ThreadRecord, incoming: &ThreadRecord) -> bool {
    let existing_priority = status_priority(existing.loaded_status);
    let incoming_priority = status_priority(incoming.loaded_status);
    incoming_priority > existing_priority
        || (incoming_priority == existing_priority
            && incoming.updated_at_unix > existing.updated_at_unix)
}

fn status_priority(status: LoadedStatus) -> u8 {
    match status {
        LoadedStatus::NotLoaded => 0,
        LoadedStatus::Loaded => 1,
        LoadedStatus::Active => 2,
    }
}

fn real_slot(slot: Option<String>) -> Option<String> {
    slot.filter(|slot| slot != "broker")
}

fn slot_from_thread_path(paths: &ManagerPaths, path: Option<&str>) -> Option<String> {
    let path = Path::new(path?);
    let relative = path.strip_prefix(&paths.slots_dir).ok()?;
    let mut components = relative.components();
    let slot = components.next()?.as_os_str().to_str()?;
    let home = components.next()?.as_os_str().to_str()?;
    (home == "home")
        .then(|| slot.to_string())
        .and_then(|slot| real_slot(Some(slot)))
}

fn thread_status(thread: &Value) -> LoadedStatus {
    match thread
        .get("status")
        .and_then(|status| status.get("type"))
        .and_then(Value::as_str)
    {
        Some("active") => LoadedStatus::Active,
        Some("notLoaded") => LoadedStatus::NotLoaded,
        _ => LoadedStatus::Loaded,
    }
}

fn in_progress_turn_ids(thread: &Value) -> BTreeSet<String> {
    thread
        .get("turns")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|turn| turn.get("status").and_then(Value::as_str) == Some("inProgress"))
        .filter_map(|turn| turn.get("id").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

fn latest_nonempty_turn_id(turn_ids: &BTreeSet<String>) -> Option<String> {
    turn_ids
        .iter()
        .rev()
        .find(|turn_id| !turn_id.is_empty())
        .cloned()
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn temp_paths(name: &str) -> ManagerPaths {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "cx-thread-directory-test-{name}-{}-{unique}",
            std::process::id()
        ));
        ManagerPaths::from_roots(root.join("codex"), root.join("profile-manager"))
    }

    #[test]
    fn thread_value_sets_active_owner() {
        let directory = ThreadDirectory::default();
        let worker = WorkerId::new("dia4", 1);
        let thread = json!({
            "id": "thread-1",
            "cwd": "/repo",
            "status": {"type": "active"},
            "turns": [{"id": "turn-1", "status": "inProgress"}],
            "createdAt": 1,
            "updatedAt": 2,
            "source": "cli",
            "preview": ""
        });

        let record = directory
            .upsert_thread_value(worker.clone(), "dia4", &thread)
            .unwrap()
            .unwrap();

        assert_eq!(record.owner_worker_id, Some(worker));
        assert_eq!(record.loaded_status, LoadedStatus::Active);
        assert_eq!(record.active_turn_id, Some("turn-1".to_string()));
    }

    #[test]
    fn seed_from_sessions_prefers_slot_from_thread_path() {
        let paths = temp_paths("seed-path-slot");
        let channel_id = session::ChannelId::parse("terminal").unwrap();
        let session = session::create_session(
            &paths,
            session::CreateSessionRequest {
                session_id: None,
                channel_id,
            },
        )
        .unwrap()
        .session;
        let rollout_path = paths
            .slot_home("dia1")
            .join("sessions/2026/05/08/rollout-thread-1.jsonl");
        session::bind_app_thread(
            &paths,
            session::BindAppThreadRequest {
                session_id: session.session_id,
                app_thread: session::AppThreadBinding {
                    thread_id: String::from("thread-1"),
                    codex_session_id: None,
                    cwd: String::from("/repo"),
                    title: None,
                    slot: Some(String::from("dia4")),
                    generation: 0,
                    path: Some(rollout_path.display().to_string()),
                    updated_at_unix: 10,
                },
            },
        )
        .unwrap();

        let directory = ThreadDirectory::default();
        directory.seed_from_sessions(&paths).unwrap();

        let record = directory.record("thread-1").unwrap().unwrap();
        assert_eq!(record.origin_slot.as_deref(), Some("dia1"));
    }

    #[test]
    fn not_loaded_summary_does_not_steal_active_owner() {
        let directory = ThreadDirectory::default();
        let active_worker = WorkerId::new("dia4", 1);
        let stale_worker = WorkerId::new("dia5", 2);
        let active = json!({
            "id": "thread-1",
            "cwd": "/repo",
            "status": {"type": "active"},
            "turns": [{"id": "turn-1", "status": "inProgress"}],
            "updatedAt": 10
        });
        let not_loaded = json!({
            "id": "thread-1",
            "cwd": "/repo",
            "status": {"type": "notLoaded"},
            "turns": [],
            "updatedAt": 20
        });

        directory
            .upsert_thread_value(active_worker.clone(), "dia4", &active)
            .unwrap();
        directory
            .upsert_thread_value(stale_worker, "dia5", &not_loaded)
            .unwrap();

        let record = directory.record("thread-1").unwrap().unwrap();
        assert_eq!(record.owner_worker_id, Some(active_worker));
        assert_eq!(record.loaded_status, LoadedStatus::Active);
        assert_eq!(record.active_turn_id, Some("turn-1".to_string()));
    }

    #[test]
    fn stale_lower_priority_summary_does_not_overwrite_thread_metadata() {
        let directory = ThreadDirectory::default();
        let active_worker = WorkerId::new("dia4", 1);
        let stale_worker = WorkerId::new("dia5", 2);
        let active = json!({
            "id": "thread-1",
            "cwd": "/repo",
            "path": "/correct.jsonl",
            "name": "correct",
            "status": {"type": "active"},
            "turns": [{"id": "turn-1", "status": "inProgress"}],
            "updatedAt": 20
        });
        let stale = json!({
            "id": "thread-1",
            "cwd": "/other",
            "path": "/wrong.jsonl",
            "name": "wrong",
            "status": {"type": "notLoaded"},
            "turns": [],
            "updatedAt": 30
        });

        directory
            .upsert_thread_value(active_worker.clone(), "dia4", &active)
            .unwrap();
        directory
            .upsert_thread_value(stale_worker, "dia5", &stale)
            .unwrap();

        let record = directory.record("thread-1").unwrap().unwrap();
        assert_eq!(record.owner_worker_id, Some(active_worker));
        assert_eq!(record.cwd, "/repo");
        assert_eq!(record.path.as_deref(), Some("/correct.jsonl"));
        assert_eq!(record.title.as_deref(), Some("correct"));
        assert_eq!(record.origin_slot.as_deref(), Some("dia4"));
    }

    #[test]
    fn not_loaded_summary_has_no_owner() {
        let directory = ThreadDirectory::default();
        let worker = WorkerId::new("dia5", 2);
        let not_loaded = json!({
            "id": "thread-1",
            "cwd": "/repo",
            "status": {"type": "notLoaded"},
            "turns": [],
            "updatedAt": 20
        });

        let record = directory
            .upsert_thread_value(worker, "dia5", &not_loaded)
            .unwrap()
            .unwrap();

        assert_eq!(record.owner_worker_id, None);
        assert_eq!(record.loaded_status, LoadedStatus::NotLoaded);
    }

    #[test]
    fn stale_same_priority_summary_does_not_steal_loaded_owner() {
        let directory = ThreadDirectory::default();
        let new_worker = WorkerId::new("dia5", 2);
        let old_worker = WorkerId::new("dia4", 1);
        let newer_loaded = json!({
            "id": "thread-1",
            "cwd": "/repo",
            "status": {"type": "idle"},
            "turns": [],
            "updatedAt": 20
        });
        let older_loaded = json!({
            "id": "thread-1",
            "cwd": "/repo",
            "status": {"type": "idle"},
            "turns": [],
            "updatedAt": 10
        });

        directory
            .upsert_thread_value(new_worker.clone(), "dia5", &newer_loaded)
            .unwrap();
        directory
            .upsert_thread_value(old_worker, "dia4", &older_loaded)
            .unwrap();

        let record = directory.record("thread-1").unwrap().unwrap();
        assert_eq!(record.owner_worker_id, Some(new_worker));
        assert_eq!(record.loaded_status, LoadedStatus::Loaded);
    }

    #[test]
    fn same_priority_same_timestamp_summary_does_not_flip_owner_or_metadata() {
        let directory = ThreadDirectory::default();
        let first_worker = WorkerId::new("dia4", 1);
        let second_worker = WorkerId::new("dia5", 2);
        let first = json!({
            "id": "thread-1",
            "cwd": "/repo",
            "path": "/first.jsonl",
            "status": {"type": "idle"},
            "turns": [],
            "updatedAt": 20
        });
        let second = json!({
            "id": "thread-1",
            "cwd": "/other",
            "path": "/second.jsonl",
            "status": {"type": "idle"},
            "turns": [],
            "updatedAt": 20
        });

        directory
            .upsert_thread_value(first_worker.clone(), "dia4", &first)
            .unwrap();
        directory
            .upsert_thread_value(second_worker, "dia5", &second)
            .unwrap();

        let record = directory.record("thread-1").unwrap().unwrap();
        assert_eq!(record.owner_worker_id, Some(first_worker));
        assert_eq!(record.cwd, "/repo");
        assert_eq!(record.path.as_deref(), Some("/first.jsonl"));
        assert_eq!(record.origin_slot.as_deref(), Some("dia4"));
    }

    #[test]
    fn active_turn_set_keeps_thread_active_until_all_turns_complete() {
        let directory = ThreadDirectory::default();
        let worker = WorkerId::new("dia4", 1);
        directory
            .upsert(ThreadRecord {
                thread_id: String::from("thread-1"),
                cx_session_id: None,
                codex_session_id: None,
                cwd: String::from("/repo"),
                path: None,
                title: None,
                origin_slot: Some(String::from("dia4")),
                owner_worker_id: None,
                loaded_status: LoadedStatus::NotLoaded,
                active_turn_id: None,
                active_turn_ids: BTreeSet::new(),
                updated_at_unix: 1,
            })
            .unwrap();

        directory
            .mark_turn_started("thread-1", worker.clone(), Some(String::from("turn-1")))
            .unwrap();
        directory
            .mark_turn_started("thread-1", worker, Some(String::from("turn-2")))
            .unwrap();
        directory
            .mark_turn_completed("thread-1", Some("turn-2"))
            .unwrap();

        let record = directory.record("thread-1").unwrap().unwrap();
        assert_eq!(record.loaded_status, LoadedStatus::Active);
        assert!(record.active_turn_ids.contains("turn-1"));
        assert!(!record.active_turn_ids.contains("turn-2"));

        directory
            .mark_turn_completed("thread-1", Some("turn-1"))
            .unwrap();
        let record = directory.record("thread-1").unwrap().unwrap();
        assert_eq!(record.loaded_status, LoadedStatus::Loaded);
        assert!(record.active_turn_ids.is_empty());
    }

    #[test]
    fn completed_turn_blocks_late_started_response() {
        let directory = ThreadDirectory::default();
        let worker = WorkerId::new("dia4", 1);
        directory
            .upsert(ThreadRecord {
                thread_id: String::from("thread-1"),
                cx_session_id: None,
                codex_session_id: None,
                cwd: String::from("/repo"),
                path: None,
                title: None,
                origin_slot: Some(String::from("dia4")),
                owner_worker_id: Some(worker.clone()),
                loaded_status: LoadedStatus::Loaded,
                active_turn_id: None,
                active_turn_ids: BTreeSet::new(),
                updated_at_unix: 1,
            })
            .unwrap();

        directory
            .mark_turn_completed("thread-1", Some("turn-1"))
            .unwrap();
        let accepted = directory
            .mark_turn_started("thread-1", worker, Some(String::from("turn-1")))
            .unwrap();

        let record = directory.record("thread-1").unwrap().unwrap();
        assert!(!accepted);
        assert_eq!(record.loaded_status, LoadedStatus::Loaded);
        assert!(record.active_turn_ids.is_empty());
    }

    #[test]
    fn completed_turn_filters_stale_active_summary() {
        let directory = ThreadDirectory::default();
        let worker = WorkerId::new("dia4", 1);
        let active = json!({
            "id": "thread-1",
            "cwd": "/repo",
            "status": {"type": "active"},
            "turns": [{"id": "turn-1", "status": "inProgress"}],
            "updatedAt": 20
        });

        directory
            .mark_turn_completed("thread-1", Some("turn-1"))
            .unwrap();
        directory
            .upsert_thread_value(worker, "dia4", &active)
            .unwrap();

        let record = directory.record("thread-1").unwrap().unwrap();
        assert_eq!(record.loaded_status, LoadedStatus::Loaded);
        assert!(record.active_turn_ids.is_empty());
    }

    #[test]
    fn worker_unavailable_clears_owned_runtime_state() {
        let directory = ThreadDirectory::default();
        let worker = WorkerId::new("dia4", 1);
        let active = json!({
            "id": "thread-1",
            "cwd": "/repo",
            "status": {"type": "active"},
            "turns": [{"id": "turn-1", "status": "inProgress"}],
            "updatedAt": 20
        });

        directory
            .upsert_thread_value(worker.clone(), "dia4", &active)
            .unwrap();

        assert_eq!(directory.mark_worker_unavailable(&worker).unwrap(), 1);
        let record = directory.record("thread-1").unwrap().unwrap();
        assert_eq!(record.owner_worker_id, None);
        assert_eq!(record.loaded_status, LoadedStatus::NotLoaded);
        assert_eq!(record.active_turn_id, None);
    }
}
