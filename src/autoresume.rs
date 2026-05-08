use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

use anyhow::Context;
use anyhow::Result;
use rusqlite::params;
use rusqlite::Connection;
use rusqlite::OpenFlags;

const STATE_DB: &str = "state_5.sqlite";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveState {
    Active,
    Inactive,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResumeCandidate {
    pub(crate) session_id: String,
    pub(crate) rollout_path: PathBuf,
}

pub(crate) fn inactive_latest_session_id(slot_home: &Path, workspace: &Path) -> Option<String> {
    inactive_latest_session_id_with_probe(slot_home, workspace, lsof_active_state)
        .ok()
        .flatten()
}

pub(crate) fn latest_session_candidate(
    slot_home: &Path,
    workspace: &Path,
) -> Option<ResumeCandidate> {
    latest_resume_candidate(slot_home, workspace).ok().flatten()
}

pub(crate) fn session_candidate_by_id(
    slot_home: &Path,
    session_id: &str,
) -> Option<ResumeCandidate> {
    session_candidate_by_id_inner(slot_home, session_id)
        .ok()
        .flatten()
}

fn inactive_latest_session_id_with_probe(
    slot_home: &Path,
    workspace: &Path,
    active_state: impl Fn(&Path) -> ActiveState,
) -> Result<Option<String>> {
    let Some(candidate) = latest_resume_candidate(slot_home, workspace)? else {
        return Ok(None);
    };
    if !candidate.rollout_path.exists() {
        return Ok(None);
    }
    match active_state(&candidate.rollout_path) {
        ActiveState::Active | ActiveState::Unknown => Ok(None),
        ActiveState::Inactive => Ok(Some(candidate.session_id)),
    }
}

fn latest_resume_candidate(slot_home: &Path, workspace: &Path) -> Result<Option<ResumeCandidate>> {
    let db_path = slot_home.join(STATE_DB);
    if !db_path.exists() {
        return Ok(None);
    }

    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open {}", db_path.display()))?;
    let columns = thread_columns(&conn)?;
    for required in ["id", "rollout_path", "cwd", "source", "archived"] {
        if !columns.contains(required) {
            return Ok(None);
        }
    }

    let updated_expr = if columns.contains("updated_at_ms") {
        "COALESCE(updated_at_ms, updated_at * 1000)"
    } else if columns.contains("updated_at") {
        "updated_at * 1000"
    } else {
        return Ok(None);
    };
    let sql = format!(
        "SELECT id, rollout_path \
         FROM threads \
         WHERE archived = 0 \
           AND cwd = ?1 \
           AND rollout_path <> '' \
           AND source IN ('cli', 'vscode') \
         ORDER BY {updated_expr} DESC, id DESC \
         LIMIT 1"
    );
    let mut statement = conn
        .prepare(&sql)
        .with_context(|| format!("prepare latest session query for {}", db_path.display()))?;
    let mut rows = statement.query(params![workspace.display().to_string()])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };

    Ok(Some(ResumeCandidate {
        session_id: row.get(0)?,
        rollout_path: PathBuf::from(row.get::<_, String>(1)?),
    }))
}

fn session_candidate_by_id_inner(
    slot_home: &Path,
    session_id: &str,
) -> Result<Option<ResumeCandidate>> {
    let db_path = slot_home.join(STATE_DB);
    if !db_path.exists() {
        return Ok(None);
    }

    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open {}", db_path.display()))?;
    let columns = thread_columns(&conn)?;
    for required in ["id", "rollout_path", "source", "archived"] {
        if !columns.contains(required) {
            return Ok(None);
        }
    }

    let mut statement = conn
        .prepare(
            "SELECT id, rollout_path \
             FROM threads \
             WHERE id = ?1 \
               AND archived = 0 \
               AND rollout_path <> '' \
               AND source IN ('cli','vscode') \
             LIMIT 1",
        )
        .with_context(|| format!("prepare session query for {}", db_path.display()))?;
    let mut rows = statement.query(params![session_id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };

    Ok(Some(ResumeCandidate {
        session_id: row.get(0)?,
        rollout_path: PathBuf::from(row.get::<_, String>(1)?),
    }))
}

fn lsof_active_state(path: &Path) -> ActiveState {
    let output = Command::new("lsof")
        .arg("-nP")
        .arg(path)
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(output) if output.status.success() => ActiveState::Active,
        Ok(output)
            if output.status.code() == Some(1)
                && output.stdout.is_empty()
                && output.stderr.is_empty() =>
        {
            ActiveState::Inactive
        }
        Ok(_) | Err(_) => ActiveState::Unknown,
    }
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use rusqlite::Connection;

    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "cx-autoresume-test-{name}-{}-{unique}",
            std::process::id()
        ))
    }

    fn seed_threads(slot_home: &Path, rows: &[(&str, &Path, &Path, i64, &str, i64)]) {
        fs::create_dir_all(slot_home).unwrap();
        let conn = Connection::open(slot_home.join(STATE_DB)).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT NOT NULL,
                cwd TEXT NOT NULL,
                source TEXT NOT NULL,
                archived INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                updated_at_ms INTEGER
             );",
        )
        .unwrap();
        for (id, rollout_path, cwd, updated_at_ms, source, archived) in rows {
            conn.execute(
                "INSERT INTO threads (
                    id, rollout_path, cwd, source, archived, updated_at, updated_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)",
                params![
                    id,
                    rollout_path.display().to_string(),
                    cwd.display().to_string(),
                    source,
                    archived,
                    updated_at_ms,
                ],
            )
            .unwrap();
        }
    }

    #[test]
    fn returns_latest_inactive_session_for_workspace() {
        let root = temp_dir("inactive");
        let slot_home = root.join("slot-home");
        let workspace = root.join("workspace");
        let older = root.join("older.jsonl");
        let latest = root.join("latest.jsonl");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(&older, "").unwrap();
        fs::write(&latest, "").unwrap();
        seed_threads(
            &slot_home,
            &[
                ("older", &older, &workspace, 10, "cli", 0),
                ("latest", &latest, &workspace, 20, "cli", 0),
            ],
        );

        let session_id = inactive_latest_session_id_with_probe(&slot_home, &workspace, |_| {
            ActiveState::Inactive
        })
        .unwrap();

        assert_eq!(session_id, Some(String::from("latest")));
    }

    #[test]
    fn does_not_resume_when_latest_session_is_active() {
        let root = temp_dir("active");
        let slot_home = root.join("slot-home");
        let workspace = root.join("workspace");
        let rollout = root.join("active.jsonl");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(&rollout, "").unwrap();
        seed_threads(
            &slot_home,
            &[("active", &rollout, &workspace, 10, "cli", 0)],
        );

        let session_id =
            inactive_latest_session_id_with_probe(&slot_home, &workspace, |_| ActiveState::Active)
                .unwrap();

        assert_eq!(session_id, None);
    }

    #[test]
    fn ignores_non_interactive_and_archived_sessions() {
        let root = temp_dir("filters");
        let slot_home = root.join("slot-home");
        let workspace = root.join("workspace");
        let exec_rollout = root.join("exec.jsonl");
        let archived_rollout = root.join("archived.jsonl");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(&exec_rollout, "").unwrap();
        fs::write(&archived_rollout, "").unwrap();
        seed_threads(
            &slot_home,
            &[
                ("exec", &exec_rollout, &workspace, 30, "exec", 0),
                ("archived", &archived_rollout, &workspace, 20, "cli", 1),
            ],
        );

        let session_id = inactive_latest_session_id_with_probe(&slot_home, &workspace, |_| {
            ActiveState::Inactive
        })
        .unwrap();

        assert_eq!(session_id, None);
    }

    #[test]
    fn latest_session_candidate_does_not_check_active_state() {
        let root = temp_dir("latest-without-active");
        let slot_home = root.join("slot-home");
        let workspace = root.join("workspace");
        let rollout = root.join("active.jsonl");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(&rollout, "").unwrap();
        seed_threads(
            &slot_home,
            &[("active", &rollout, &workspace, 10, "cli", 0)],
        );

        let candidate = latest_session_candidate(&slot_home, &workspace).unwrap();

        assert_eq!(candidate.session_id, "active");
        assert_eq!(candidate.rollout_path, rollout);
    }

    #[test]
    fn session_candidate_by_id_finds_specific_session() {
        let root = temp_dir("by-id");
        let slot_home = root.join("slot-home");
        let workspace = root.join("workspace");
        let rollout = root.join("chosen.jsonl");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(&rollout, "").unwrap();
        seed_threads(
            &slot_home,
            &[("chosen", &rollout, &workspace, 10, "cli", 0)],
        );

        let candidate = session_candidate_by_id(&slot_home, "chosen").unwrap();

        assert_eq!(candidate.session_id, "chosen");
        assert_eq!(candidate.rollout_path, rollout);
    }
}
