use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use rusqlite::params;
use rusqlite::Connection;
use rusqlite::Error;
use rusqlite::ErrorCode;
use rusqlite::OpenFlags;

use crate::paths::ManagerPaths;

use super::ThreadUsage;
use super::STATE_DB;

pub(super) fn state_db_paths(
    paths: &ManagerPaths,
    slot_filters: &BTreeSet<String>,
) -> Result<Vec<PathBuf>> {
    let mut candidates = Vec::new();
    candidates.push(paths.base_codex_home.join(STATE_DB));

    if slot_filters.is_empty() {
        if paths.slots_dir.is_dir() {
            for entry in fs::read_dir(&paths.slots_dir)
                .with_context(|| format!("read {}", paths.slots_dir.display()))?
            {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    candidates.push(entry.path().join("home").join(STATE_DB));
                }
            }
        }
    } else {
        for slot in slot_filters {
            candidates.push(paths.slot_home(slot).join(STATE_DB));
        }
    }

    let mut seen = BTreeSet::new();
    let mut db_paths = Vec::new();
    for candidate in candidates {
        if !candidate.exists() {
            continue;
        }
        let canonical = fs::canonicalize(&candidate)
            .with_context(|| format!("resolve {}", candidate.display()))?;
        if seen.insert(canonical.clone()) {
            db_paths.push(canonical);
        }
    }
    db_paths.sort();
    Ok(db_paths)
}

pub(super) fn read_threads(
    db_path: &Path,
    paths: &ManagerPaths,
    min_since: i64,
) -> Result<Vec<ThreadUsage>> {
    let conn = open_state_connection(db_path)?;
    let columns = thread_columns(&conn)?;
    if !columns.contains("updated_at") || !columns.contains("tokens_used") {
        return Ok(Vec::new());
    }

    let model_provider_expr = optional_column(&columns, "model_provider");
    let model_expr = optional_column(&columns, "model");
    let rollout_path_expr = optional_column(&columns, "rollout_path");
    let sql = format!(
        "SELECT id, updated_at, tokens_used, {model_provider_expr}, {model_expr}, {rollout_path_expr} \
         FROM threads \
         WHERE tokens_used > 0 AND updated_at >= ?1"
    );
    let mut statement = conn
        .prepare(&sql)
        .with_context(|| format!("prepare stats query for {}", db_path.display()))?;
    let rows = statement.query_map(params![min_since], |row| {
        let rollout_path: String = row.get(5)?;
        Ok(ThreadUsage {
            id: row.get(0)?,
            updated_at: row.get(1)?,
            tokens: row.get::<_, i64>(2)?.max(0) as u64,
            provider: empty_as_unknown(row.get::<_, String>(3)?),
            model: empty_as_unknown(row.get::<_, String>(4)?),
            slot: infer_slot_from_rollout_path(&rollout_path, paths),
        })
    })?;

    let mut usages = Vec::new();
    for row in rows {
        usages.push(row?);
    }
    Ok(usages)
}

pub(super) fn read_rollout_paths(
    db_path: &Path,
    paths: &ManagerPaths,
    slot_filters: &BTreeSet<String>,
) -> Result<Vec<PathBuf>> {
    let conn = open_state_connection(db_path)?;
    let columns = thread_columns(&conn)?;
    if !columns.contains("rollout_path") {
        return Ok(Vec::new());
    }

    let mut statement = conn
        .prepare("SELECT rollout_path FROM threads WHERE rollout_path <> ''")
        .with_context(|| format!("prepare calibration query for {}", db_path.display()))?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut paths_out = Vec::new();
    for row in rows {
        let rollout_path = row?;
        if !slot_filters.is_empty()
            && !slot_filters.contains(&infer_slot_from_rollout_path(&rollout_path, paths))
        {
            continue;
        }
        paths_out.push(PathBuf::from(rollout_path));
    }
    Ok(paths_out)
}

fn open_state_connection(db_path: &Path) -> Result<Connection> {
    match open_validated_connection(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(conn) => Ok(conn),
        Err(err) if is_cannot_open(&err) => {
            let conn = Connection::open_with_flags(
                db_path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
            )
            .and_then(|conn| {
                conn.pragma_update(None, "query_only", true)?;
                validate_connection(&conn)?;
                Ok(conn)
            })
            .with_context(|| {
                format!(
                    "open {} read-write after read-only open failed",
                    db_path.display()
                )
            })?;
            Ok(conn)
        }
        Err(err) => Err(err).with_context(|| format!("open {}", db_path.display())),
    }
}

fn open_validated_connection(
    db_path: &Path,
    flags: OpenFlags,
) -> std::result::Result<Connection, Error> {
    let conn = Connection::open_with_flags(db_path, flags)?;
    validate_connection(&conn)?;
    Ok(conn)
}

fn validate_connection(conn: &Connection) -> std::result::Result<(), Error> {
    conn.query_row("SELECT count(*) FROM sqlite_schema", [], |_| Ok(()))
}

fn is_cannot_open(err: &Error) -> bool {
    matches!(err, Error::SqliteFailure(error, _) if error.code == ErrorCode::CannotOpen)
}

fn thread_columns(conn: &Connection) -> Result<BTreeSet<String>> {
    let mut statement = conn.prepare("PRAGMA table_info(threads)")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    let mut columns = BTreeSet::new();
    for row in rows {
        columns.insert(row?);
    }
    Ok(columns)
}

fn optional_column(columns: &BTreeSet<String>, name: &str) -> String {
    if columns.contains(name) {
        name.to_string()
    } else {
        format!("'' AS {name}")
    }
}

fn empty_as_unknown(value: String) -> String {
    if value.trim().is_empty() {
        "unknown".to_string()
    } else {
        value
    }
}

pub(super) fn infer_slot_from_rollout_path(rollout_path: &str, paths: &ManagerPaths) -> String {
    let normalized = rollout_path.replace('\\', "/");
    let manager_dir = paths.manager_dir.display().to_string().replace('\\', "/");
    let marker = format!("{}/slots/", manager_dir.trim_end_matches('/'));
    if let Some(index) = normalized.find(&marker) {
        let rest = &normalized[index + marker.len()..];
        if let Some(slot) = rest.split('/').next().filter(|slot| !slot.is_empty()) {
            return slot.to_string();
        }
    }
    "base".to_string()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use super::*;

    fn temp_paths(name: &str) -> ManagerPaths {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "cx-stats-db-test-{name}-{}-{unique}",
            std::process::id()
        ));
        ManagerPaths {
            base_codex_home: root.join("codex"),
            manager_dir: root.join("profile-manager"),
            slots_dir: root.join("profile-manager/slots"),
            targets_dir: root.join("profile-manager/targets"),
            rotation_file: root.join("profile-manager/rotation.txt"),
        }
    }

    #[test]
    fn reads_wal_database_when_sidecars_are_missing() {
        let paths = temp_paths("missing-wal-sidecars");
        fs::create_dir_all(&paths.base_codex_home).expect("create codex home");
        let db_path = paths.base_codex_home.join(STATE_DB);
        {
            let conn = Connection::open(&db_path).expect("open writable db");
            conn.execute_batch(
                "PRAGMA journal_mode = WAL;
                 CREATE TABLE threads (
                   id TEXT PRIMARY KEY,
                   updated_at INTEGER NOT NULL,
                   tokens_used INTEGER NOT NULL
                 );
                 INSERT INTO threads (id, updated_at, tokens_used)
                 VALUES ('thread-1', 123, 456);
                 PRAGMA wal_checkpoint(TRUNCATE);",
            )
            .expect("seed db");
        }
        let _ = fs::remove_file(format!("{}-wal", db_path.display()));
        let _ = fs::remove_file(format!("{}-shm", db_path.display()));

        let usages = read_threads(&db_path, &paths, 0).expect("read threads");

        assert_eq!(usages.len(), 1);
        assert_eq!(usages[0].id, "thread-1");
        assert_eq!(usages[0].tokens, 456);
    }
}
