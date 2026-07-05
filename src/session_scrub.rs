use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use rusqlite::params;
use rusqlite::Connection;
use rusqlite::OptionalExtension;
use serde_json::Value;
use time::macros::format_description;
use time::OffsetDateTime;

use crate::auth;
use crate::paths::ManagerPaths;
const STATE_DB_FILENAME: &str = "state_5.sqlite";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedResume {
    pub(crate) session_id: String,
    pub(crate) scrubbed: bool,
}

#[derive(Debug, Clone)]
struct ThreadRow {
    id: String,
    rollout_path: PathBuf,
    created_at: i64,
    updated_at: i64,
    source: String,
    model_provider: String,
    cwd: String,
    title: String,
    sandbox_policy: String,
    approval_mode: String,
    tokens_used: i64,
    has_user_event: i64,
    archived: i64,
    archived_at: Option<i64>,
    git_sha: Option<String>,
    git_branch: Option<String>,
    git_origin_url: Option<String>,
    cli_version: String,
    first_user_message: String,
    agent_nickname: Option<String>,
    agent_role: Option<String>,
    memory_mode: String,
    model: Option<String>,
    reasoning_effort: Option<String>,
    agent_path: Option<String>,
    created_at_ms: Option<i64>,
    updated_at_ms: Option<i64>,
    thread_source: Option<String>,
    preview: String,
    recency_at: i64,
    recency_at_ms: i64,
}

#[derive(Debug, Clone)]
struct LocatedThread {
    slot: String,
    row: ThreadRow,
}

pub(crate) fn prepare_cross_slot_resume(
    paths: &ManagerPaths,
    target_slot: &str,
    requested_session_id: &str,
) -> Result<PreparedResume> {
    if !looks_like_uuid(requested_session_id) {
        return Ok(PreparedResume {
            session_id: requested_session_id.to_string(),
            scrubbed: false,
        });
    }

    let target_db = paths.slot_sqlite_home(target_slot).join(STATE_DB_FILENAME);
    if thread_exists(&target_db, requested_session_id)? {
        return Ok(PreparedResume {
            session_id: requested_session_id.to_string(),
            scrubbed: false,
        });
    }

    let Some(source) = find_source_thread(paths, target_slot, requested_session_id)? else {
        return Ok(PreparedResume {
            session_id: requested_session_id.to_string(),
            scrubbed: false,
        });
    };

    let new_session_id = new_uuid_v4()?;
    let target_provider = target_model_provider(paths, target_slot)?;
    let scrubbed_path = write_scrubbed_rollout(
        paths,
        target_slot,
        &source.row.rollout_path,
        requested_session_id,
        &new_session_id,
        &target_provider,
    )
    .with_context(|| {
        format!(
            "scrub rollout {} from slot {}",
            source.row.rollout_path.display(),
            source.slot
        )
    })?;
    upsert_scrubbed_thread(
        &target_db,
        source.row,
        &new_session_id,
        &scrubbed_path,
        &target_provider,
    )?;

    Ok(PreparedResume {
        session_id: new_session_id,
        scrubbed: true,
    })
}

fn find_source_thread(
    paths: &ManagerPaths,
    target_slot: &str,
    session_id: &str,
) -> Result<Option<LocatedThread>> {
    for slot in state_db_slot_names(paths)? {
        if slot == target_slot {
            continue;
        }
        let sqlite_path = paths.slot_sqlite_home(&slot).join(STATE_DB_FILENAME);
        let Some(row) = read_thread_row(&sqlite_path, session_id)? else {
            continue;
        };
        return Ok(Some(LocatedThread { slot, row }));
    }
    Ok(None)
}

fn state_db_slot_names(paths: &ManagerPaths) -> Result<Vec<String>> {
    let mut slots = BTreeSet::new();
    slots.insert("default".to_string());
    match fs::read_dir(&paths.slots_dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                if entry.path().join("home").is_dir() {
                    slots.insert(entry.file_name().to_string_lossy().to_string());
                }
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("read {}", paths.slots_dir.display())),
    }
    Ok(slots.into_iter().collect())
}

fn target_model_provider(paths: &ManagerPaths, target_slot: &str) -> Result<String> {
    let auth = auth::read_slot_auth(&paths.slot_dir(target_slot), Some(&paths.base_codex_home))?;
    Ok(auth.provider.unwrap_or_else(|| "openai".to_string()))
}

fn thread_exists(db_path: &Path, session_id: &str) -> Result<bool> {
    Ok(read_thread_row(db_path, session_id)?.is_some())
}

fn read_thread_row(db_path: &Path, session_id: &str) -> Result<Option<ThreadRow>> {
    if !db_path.exists() {
        return Ok(None);
    }
    let conn = Connection::open(db_path).with_context(|| format!("open {}", db_path.display()))?;
    conn.query_row(
        "SELECT id, rollout_path, created_at, updated_at, source, model_provider, cwd, title,
                sandbox_policy, approval_mode, tokens_used, has_user_event, archived, archived_at,
                git_sha, git_branch, git_origin_url, cli_version, first_user_message,
                agent_nickname, agent_role, memory_mode, model, reasoning_effort, agent_path,
                created_at_ms, updated_at_ms, thread_source, preview, recency_at, recency_at_ms
         FROM threads
         WHERE id = ?1",
        params![session_id],
        |row| {
            Ok(ThreadRow {
                id: row.get(0)?,
                rollout_path: PathBuf::from(row.get::<_, String>(1)?),
                created_at: row.get(2)?,
                updated_at: row.get(3)?,
                source: row.get(4)?,
                model_provider: row.get(5)?,
                cwd: row.get(6)?,
                title: row.get(7)?,
                sandbox_policy: row.get(8)?,
                approval_mode: row.get(9)?,
                tokens_used: row.get(10)?,
                has_user_event: row.get(11)?,
                archived: row.get(12)?,
                archived_at: row.get(13)?,
                git_sha: row.get(14)?,
                git_branch: row.get(15)?,
                git_origin_url: row.get(16)?,
                cli_version: row.get(17)?,
                first_user_message: row.get(18)?,
                agent_nickname: row.get(19)?,
                agent_role: row.get(20)?,
                memory_mode: row.get(21)?,
                model: row.get(22)?,
                reasoning_effort: row.get(23)?,
                agent_path: row.get(24)?,
                created_at_ms: row.get(25)?,
                updated_at_ms: row.get(26)?,
                thread_source: row.get(27)?,
                preview: row.get(28)?,
                recency_at: row.get(29)?,
                recency_at_ms: row.get(30)?,
            })
        },
    )
    .optional()
    .with_context(|| format!("read thread {session_id} from {}", db_path.display()))
}

fn write_scrubbed_rollout(
    paths: &ManagerPaths,
    target_slot: &str,
    source_rollout: &Path,
    old_session_id: &str,
    new_session_id: &str,
    target_provider: &str,
) -> Result<PathBuf> {
    let now = OffsetDateTime::now_utc();
    let dest_dir = paths
        .slot_home(target_slot)
        .join("cx-scrubbed-sessions")
        .join(now.year().to_string())
        .join(format!("{:02}", u8::from(now.month())))
        .join(format!("{:02}", now.day()));
    fs::create_dir_all(&dest_dir).with_context(|| format!("create {}", dest_dir.display()))?;
    let stem = now.format(format_description!(
        "[year]-[month]-[day]T[hour]-[minute]-[second]"
    ))?;
    let dest_path = dest_dir.join(format!("rollout-{stem}-{new_session_id}.jsonl"));

    let content = fs::read_to_string(source_rollout)
        .with_context(|| format!("read {}", source_rollout.display()))?;
    let mut output = String::new();
    for (line_index, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let mut value: Value = serde_json::from_str(line).with_context(|| {
            format!(
                "parse {} line {}",
                source_rollout.display(),
                line_index.saturating_add(1)
            )
        })?;
        if !scrub_rollout_line(&mut value, old_session_id, new_session_id, target_provider) {
            continue;
        }
        output.push_str(&serde_json::to_string(&value)?);
        output.push('\n');
    }
    fs::write(&dest_path, output).with_context(|| format!("write {}", dest_path.display()))?;
    Ok(dest_path)
}

fn scrub_rollout_line(
    value: &mut Value,
    old_session_id: &str,
    new_session_id: &str,
    target_provider: &str,
) -> bool {
    if value.get("type").and_then(Value::as_str) == Some("session_meta") {
        if let Some(payload) = value.get_mut("payload").and_then(Value::as_object_mut) {
            replace_string_field(payload, "id", old_session_id, new_session_id);
            replace_string_field(payload, "session_id", old_session_id, new_session_id);
            payload.insert(
                "model_provider".to_string(),
                Value::String(target_provider.to_string()),
            );
        }
    }

    if value.get("type").and_then(Value::as_str) == Some("response_item") {
        let Some(payload) = value.get_mut("payload") else {
            return true;
        };
        if response_item_should_drop(payload) {
            return false;
        }
        scrub_value(payload);
        if response_item_is_empty_after_scrub(payload) {
            return false;
        }
        if response_item_should_drop(payload) {
            return false;
        }
        return true;
    }

    scrub_value(value);
    true
}

fn response_item_should_drop(payload: &Value) -> bool {
    matches!(
        payload.get("type").and_then(Value::as_str),
        Some("compaction") | Some("compaction_summary")
    )
}

fn response_item_is_empty_after_scrub(payload: &Value) -> bool {
    match payload.get("type").and_then(Value::as_str) {
        Some("agent_message") => payload
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty),
        Some("reasoning") => {
            let summary_empty = payload
                .get("summary")
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty);
            let content_empty = payload
                .get("content")
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty);
            summary_empty && content_empty
        }
        Some("context_compaction") => !payload
            .as_object()
            .is_some_and(|object| object.keys().any(|key| key != "type")),
        _ => false,
    }
}

fn replace_string_field(
    map: &mut serde_json::Map<String, Value>,
    key: &str,
    old_value: &str,
    new_value: &str,
) {
    if map.get(key).and_then(Value::as_str) == Some(old_value) {
        map.insert(key.to_string(), Value::String(new_value.to_string()));
    }
}

fn scrub_value(value: &mut Value) {
    match value {
        Value::Array(items) => {
            for item in items.iter_mut() {
                scrub_value(item);
            }
            items.retain(|item| {
                item.get("type").and_then(Value::as_str) != Some("encrypted_content")
            });
        }
        Value::Object(map) => {
            map.remove("encrypted_content");
            for value in map.values_mut() {
                scrub_value(value);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn upsert_scrubbed_thread(
    target_db: &Path,
    mut source: ThreadRow,
    new_session_id: &str,
    scrubbed_path: &Path,
    target_provider: &str,
) -> Result<()> {
    let parent = target_db
        .parent()
        .with_context(|| format!("state db has no parent: {}", target_db.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let conn =
        Connection::open(target_db).with_context(|| format!("open {}", target_db.display()))?;

    source.id = new_session_id.to_string();
    source.rollout_path = scrubbed_path.to_path_buf();
    source.model_provider = target_provider.to_string();
    source.title = format!("{} (scrubbed)", source.title);

    conn.execute(
        "INSERT INTO threads (
            id, rollout_path, created_at, updated_at, source, model_provider, cwd, title,
            sandbox_policy, approval_mode, tokens_used, has_user_event, archived, archived_at,
            git_sha, git_branch, git_origin_url, cli_version, first_user_message,
            agent_nickname, agent_role, memory_mode, model, reasoning_effort, agent_path,
            created_at_ms, updated_at_ms, thread_source, preview, recency_at, recency_at_ms
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
            ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31
         )
         ON CONFLICT(id) DO UPDATE SET
            rollout_path = excluded.rollout_path,
            model_provider = excluded.model_provider,
            title = excluded.title,
            preview = excluded.preview,
            updated_at = excluded.updated_at,
            updated_at_ms = excluded.updated_at_ms,
            recency_at = excluded.recency_at,
            recency_at_ms = excluded.recency_at_ms",
        params![
            source.id,
            source.rollout_path.display().to_string(),
            source.created_at,
            source.updated_at,
            source.source,
            source.model_provider,
            source.cwd,
            source.title,
            source.sandbox_policy,
            source.approval_mode,
            source.tokens_used,
            source.has_user_event,
            source.archived,
            source.archived_at,
            source.git_sha,
            source.git_branch,
            source.git_origin_url,
            source.cli_version,
            source.first_user_message,
            source.agent_nickname,
            source.agent_role,
            source.memory_mode,
            source.model,
            source.reasoning_effort,
            source.agent_path,
            source.created_at_ms,
            source.updated_at_ms,
            source.thread_source,
            source.preview,
            source.recency_at,
            source.recency_at_ms,
        ],
    )
    .with_context(|| format!("upsert scrubbed thread into {}", target_db.display()))?;
    Ok(())
}

fn looks_like_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && [8, 13, 18, 23]
            .into_iter()
            .all(|index| bytes[index] == b'-')
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit())
}

fn new_uuid_v4() -> Result<String> {
    let mut bytes = [0_u8; 16];
    match fs::File::open("/dev/urandom").and_then(|mut file| file.read_exact(&mut bytes)) {
        Ok(()) => {}
        Err(_) => {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            bytes.copy_from_slice(&nanos.to_be_bytes());
        }
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths(name: &str) -> ManagerPaths {
        let root = std::env::temp_dir().join(format!(
            "cx-session-scrub-test-{name}-{}",
            std::process::id()
        ));
        ManagerPaths::from_roots(root.join("codex"), root.join("profile-manager"))
    }

    fn create_state_db(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                source TEXT NOT NULL,
                model_provider TEXT NOT NULL,
                cwd TEXT NOT NULL,
                title TEXT NOT NULL,
                sandbox_policy TEXT NOT NULL,
                approval_mode TEXT NOT NULL,
                tokens_used INTEGER NOT NULL DEFAULT 0,
                has_user_event INTEGER NOT NULL DEFAULT 0,
                archived INTEGER NOT NULL DEFAULT 0,
                archived_at INTEGER,
                git_sha TEXT,
                git_branch TEXT,
                git_origin_url TEXT,
                cli_version TEXT NOT NULL DEFAULT '',
                first_user_message TEXT NOT NULL DEFAULT '',
                agent_nickname TEXT,
                agent_role TEXT,
                memory_mode TEXT NOT NULL DEFAULT 'enabled',
                model TEXT,
                reasoning_effort TEXT,
                agent_path TEXT,
                created_at_ms INTEGER,
                updated_at_ms INTEGER,
                thread_source TEXT,
                preview TEXT NOT NULL DEFAULT '',
                recency_at INTEGER NOT NULL DEFAULT 0,
                recency_at_ms INTEGER NOT NULL DEFAULT 0
            );",
        )
        .unwrap();
    }

    fn insert_thread(db_path: &Path, id: &str, rollout_path: &Path) {
        let conn = Connection::open(db_path).unwrap();
        conn.execute(
            "INSERT INTO threads (
                id, rollout_path, created_at, updated_at, source, model_provider, cwd, title,
                sandbox_policy, approval_mode, tokens_used, has_user_event, archived, cli_version,
                first_user_message, memory_mode, preview, recency_at, recency_at_ms
             ) VALUES (?1, ?2, 1, 2, 'cli', 'ollama', '/tmp/project', 'title',
                       'workspace-write', 'on-request', 0, 1, 0, 'test', 'hello',
                       'enabled', 'preview', 2, 2000)",
            params![id, rollout_path.display().to_string()],
        )
        .unwrap();
    }

    #[test]
    fn cross_slot_resume_writes_scrubbed_rollout_and_target_db_row() {
        let paths = temp_paths("cross-slot");
        let old_id = "019efd2e-bf53-72a3-bd64-40c466dacb8a";
        fs::create_dir_all(paths.slot_home("ollama")).unwrap();
        fs::create_dir_all(paths.slot_home("dia4")).unwrap();
        fs::create_dir_all(paths.slot_dir("ollama")).unwrap();
        fs::create_dir_all(paths.slot_dir("dia4")).unwrap();
        fs::write(paths.slot_dir("dia4").join("overrides.conf"), "").unwrap();

        let source_rollout = paths
            .slot_home("ollama")
            .join("sessions/2026/07/05/rollout-2026-07-05T00-00-00-019efd2e-bf53-72a3-bd64-40c466dacb8a.jsonl");
        fs::create_dir_all(source_rollout.parent().unwrap()).unwrap();
        fs::write(
            &source_rollout,
            format!(
                "{}\n{}\n{}\n{}\n",
                serde_json::json!({
                    "type": "session_meta",
                    "payload": {
                        "id": old_id,
                        "session_id": old_id,
                        "timestamp": "2026-07-05T00:00:00Z",
                        "cwd": "/tmp/project",
                        "source": "cli",
                        "model_provider": "ollama"
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "reasoning",
                        "summary": [],
                        "encrypted_content": "bad"
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "agent_message",
                        "author": "a",
                        "recipient": "b",
                        "content": [
                            {"type": "encrypted_content", "encrypted_content": "bad"},
                            {"type": "input_text", "text": "keep"}
                        ]
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "compaction_summary",
                        "encrypted_content": "bad"
                    }
                }),
            ),
        )
        .unwrap();

        create_state_db(&paths.slot_sqlite_home("ollama").join(STATE_DB_FILENAME));
        create_state_db(&paths.slot_sqlite_home("dia4").join(STATE_DB_FILENAME));
        insert_thread(
            &paths.slot_sqlite_home("ollama").join(STATE_DB_FILENAME),
            old_id,
            &source_rollout,
        );

        let prepared = prepare_cross_slot_resume(&paths, "dia4", old_id).unwrap();

        assert!(prepared.scrubbed);
        assert_ne!(prepared.session_id, old_id);
        let row = read_thread_row(
            &paths.slot_sqlite_home("dia4").join(STATE_DB_FILENAME),
            &prepared.session_id,
        )
        .unwrap()
        .unwrap();
        assert_eq!(row.model_provider, "openai");
        let scrubbed = fs::read_to_string(row.rollout_path).unwrap();
        assert!(!scrubbed.contains("encrypted_content"));
        assert!(scrubbed.contains(&prepared.session_id));
        assert!(!scrubbed.contains(old_id));
        assert!(scrubbed.contains("\"input_text\""));
        assert!(!scrubbed.contains("\"reasoning\""));
        assert!(!scrubbed.contains("compaction_summary"));

        let _ = fs::remove_dir_all(&paths.base_codex_home);
        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn same_slot_resume_is_not_scrubbed() {
        let paths = temp_paths("same-slot");
        let id = "019efd2e-bf53-72a3-bd64-40c466dacb8a";
        fs::create_dir_all(paths.slot_home("dia4")).unwrap();
        fs::create_dir_all(paths.slot_dir("dia4")).unwrap();
        fs::write(paths.slot_dir("dia4").join("overrides.conf"), "").unwrap();
        let db = paths.slot_sqlite_home("dia4").join(STATE_DB_FILENAME);
        create_state_db(&db);
        insert_thread(&db, id, Path::new("/tmp/rollout.jsonl"));

        let prepared = prepare_cross_slot_resume(&paths, "dia4", id).unwrap();

        assert_eq!(
            prepared,
            PreparedResume {
                session_id: id.to_string(),
                scrubbed: false,
            }
        );

        let _ = fs::remove_dir_all(&paths.base_codex_home);
        let _ = fs::remove_dir_all(&paths.manager_dir);
    }
}
