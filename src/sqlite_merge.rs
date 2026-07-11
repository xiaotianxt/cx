use std::fs;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use rusqlite::Connection;
use rusqlite::OpenFlags;

use crate::paths::ManagerPaths;

const MIGRATION_ID: &str = "shared-sqlite-v1";
const DATABASES: &[DatabaseSpec] = &[
    DatabaseSpec {
        file_name: "state_5.sqlite",
        tables: &[
            TableSpec::latest("threads", "id", "updated_at_ms", Some("updated_at")),
            TableSpec::missing("thread_dynamic_tools"),
            TableSpec::missing("thread_spawn_edges"),
            TableSpec::latest("agent_jobs", "id", "updated_at", None),
            TableSpec::missing("agent_job_items"),
            TableSpec::missing("external_agent_config_imports"),
        ],
    },
    DatabaseSpec {
        file_name: "memories_1.sqlite",
        tables: &[TableSpec::latest(
            "stage1_outputs",
            "thread_id",
            "source_updated_at",
            None,
        )],
    },
    DatabaseSpec {
        file_name: "goals_1.sqlite",
        tables: &[TableSpec::latest(
            "thread_goals",
            "thread_id",
            "updated_at_ms",
            None,
        )],
    },
];

#[derive(Debug, Clone, Copy)]
struct DatabaseSpec {
    file_name: &'static str,
    tables: &'static [TableSpec],
}

#[derive(Debug, Clone, Copy)]
struct TableSpec {
    name: &'static str,
    conflict: ConflictPolicy,
}

impl TableSpec {
    const fn missing(name: &'static str) -> Self {
        Self {
            name,
            conflict: ConflictPolicy::KeepTarget,
        }
    }

    const fn latest(
        name: &'static str,
        key: &'static str,
        updated: &'static str,
        fallback_updated: Option<&'static str>,
    ) -> Self {
        Self {
            name,
            conflict: ConflictPolicy::Latest {
                key,
                updated,
                fallback_updated,
            },
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ConflictPolicy {
    KeepTarget,
    Latest {
        key: &'static str,
        updated: &'static str,
        fallback_updated: Option<&'static str>,
    },
}

#[derive(Debug)]
pub struct MergeReport {
    pub shared_sqlite_home: PathBuf,
    pub sources: Vec<String>,
    pub source_threads: u64,
    pub shared_threads: u64,
    pub dry_run: bool,
}

#[derive(Debug)]
struct SlotSource {
    slot: String,
    sqlite_home: PathBuf,
}

pub fn merge_slot_databases(paths: &ManagerPaths, dry_run: bool) -> Result<MergeReport> {
    let sources = discover_slot_sources(paths)?;
    let source_threads = sources
        .iter()
        .map(|source| count_threads(&source.sqlite_home.join("state_5.sqlite")))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .sum();

    if !dry_run {
        fs::create_dir_all(paths.shared_sqlite_home())
            .with_context(|| format!("create {}", paths.shared_sqlite_home().display()))?;
        for database in DATABASES {
            merge_database_family(paths, &sources, *database)?;
        }
    }

    let shared_threads = count_threads(&paths.shared_sqlite_home().join("state_5.sqlite"))?;
    let report = MergeReport {
        shared_sqlite_home: paths.shared_sqlite_home(),
        sources: sources.into_iter().map(|source| source.slot).collect(),
        source_threads,
        shared_threads,
        dry_run,
    };
    if !dry_run {
        write_migration_marker(paths, &report)?;
    }
    Ok(report)
}

pub fn run_startup_migration(paths: &ManagerPaths, force: bool) -> Result<()> {
    if !force && migration_is_current(paths)? {
        return Ok(());
    }
    let report = match merge_slot_databases(paths, false) {
        Ok(report) => report,
        Err(err) if !force => {
            eprintln!("cx upgrade: shared SQLite migration deferred: {err:#}");
            return Ok(());
        }
        Err(err) => return Err(err),
    };
    if !report.sources.is_empty() {
        eprintln!(
            "cx upgrade: merged {} legacy SQLite source{} into {} ({} unique threads)",
            report.sources.len(),
            if report.sources.len() == 1 { "" } else { "s" },
            report.shared_sqlite_home.display(),
            report.shared_threads
        );
    }
    Ok(())
}

pub(crate) fn migration_marker(paths: &ManagerPaths) -> PathBuf {
    paths
        .manager_dir
        .join("state/upgrades")
        .join(format!("{MIGRATION_ID}.json"))
}

fn write_migration_marker(paths: &ManagerPaths, report: &MergeReport) -> Result<()> {
    let marker = migration_marker(paths);
    fs::create_dir_all(marker.parent().expect("migration marker has parent"))?;
    let value = serde_json::json!({
        "schemaVersion": 2,
        "migrationId": MIGRATION_ID,
        "sources": report.sources,
        "sourceThreads": report.source_threads,
        "sharedThreads": report.shared_threads,
    });
    fs::write(&marker, serde_json::to_vec_pretty(&value)?)
        .with_context(|| format!("write {}", marker.display()))
}

fn migration_is_current(paths: &ManagerPaths) -> Result<bool> {
    let marker = migration_marker(paths);
    let Ok(bytes) = fs::read(&marker) else {
        return Ok(false);
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Ok(false);
    };
    Ok(
        value.get("migrationId").and_then(|value| value.as_str()) == Some(MIGRATION_ID)
            && value.get("schemaVersion").and_then(|value| value.as_u64()) == Some(2)
            && paths.shared_sqlite_home().join("state_5.sqlite").is_file(),
    )
}

fn discover_slot_sources(paths: &ManagerPaths) -> Result<Vec<SlotSource>> {
    let mut sources = Vec::new();
    if paths.base_codex_home.join("state_5.sqlite").is_file() {
        sources.push(SlotSource {
            slot: "legacy-base".to_string(),
            sqlite_home: paths.base_codex_home.clone(),
        });
    }
    if !paths.slots_dir.is_dir() {
        return Ok(sources);
    }
    for entry in fs::read_dir(&paths.slots_dir)
        .with_context(|| format!("read {}", paths.slots_dir.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let sqlite_home = entry.path().join("home/sqlite");
        if !sqlite_home.join("state_5.sqlite").is_file() {
            continue;
        }
        sources.push(SlotSource {
            slot: entry.file_name().to_string_lossy().to_string(),
            sqlite_home,
        });
    }
    sources.sort_by(|left, right| left.slot.cmp(&right.slot));
    Ok(sources)
}

fn count_threads(path: &Path) -> Result<u64> {
    if !path.is_file() {
        return Ok(0);
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open {}", path.display()))?;
    let count = conn
        .query_row("SELECT count(*) FROM threads", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap_or(0);
    Ok(count.max(0) as u64)
}

fn merge_database_family(
    paths: &ManagerPaths,
    sources: &[SlotSource],
    database: DatabaseSpec,
) -> Result<()> {
    let target = paths.shared_sqlite_home().join(database.file_name);
    if !target.is_file() {
        let seed = sources
            .iter()
            .filter_map(|source| {
                let path = source.sqlite_home.join(database.file_name);
                path.is_file().then_some(path)
            })
            .map(|path| Ok((database_schema_score(&path, database.tables)?, path)))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .max_by_key(|(score, _)| *score);
        if let Some((_, seed)) = seed {
            clone_database(&seed, &target)?;
        }
    }
    for source in sources {
        let source_path = source.sqlite_home.join(database.file_name);
        if !source_path.is_file() {
            continue;
        }
        merge_database(&source_path, &target, database.tables).with_context(|| {
            format!(
                "merge slot {} database {}",
                source.slot,
                source_path.display()
            )
        })?;
    }
    Ok(())
}

fn database_schema_score(path: &Path, tables: &[TableSpec]) -> Result<usize> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open {} for schema inspection", path.display()))?;
    let mut score = 0;
    for table in tables {
        if !table_exists(&conn, "main", table.name)? {
            continue;
        }
        score += table_columns(&conn, "main", table.name)?.len();
    }
    Ok(score)
}

fn clone_database(source: &Path, target: &Path) -> Result<()> {
    let conn = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open {}", source.display()))?;
    conn.execute("VACUUM INTO ?1", [target.display().to_string()])
        .with_context(|| format!("clone {} to {}", source.display(), target.display()))?;
    Ok(())
}

fn merge_database(source: &Path, target: &Path, tables: &[TableSpec]) -> Result<()> {
    let conn = Connection::open(target).with_context(|| format!("open {}", target.display()))?;
    conn.execute(
        "ATTACH DATABASE ?1 AS source_db",
        [source.display().to_string()],
    )
    .with_context(|| format!("attach {}", source.display()))?;
    let result = merge_attached_tables(&conn, tables);
    let detach_result = conn.execute("DETACH DATABASE source_db", []);
    result?;
    detach_result.context("detach source database")?;
    Ok(())
}

fn merge_attached_tables(conn: &Connection, tables: &[TableSpec]) -> Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        for table in tables {
            let columns = common_columns(conn, table.name)?;
            if columns.is_empty() {
                continue;
            }
            if let ConflictPolicy::Latest {
                key,
                updated,
                fallback_updated,
            } = table.conflict
            {
                update_newer_rows(conn, table.name, &columns, key, updated, fallback_updated)?;
            }
            insert_missing_rows(conn, table.name, &columns)?;
        }
        Ok(())
    })();
    if result.is_ok() {
        conn.execute_batch("COMMIT")?;
    } else {
        let _ = conn.execute_batch("ROLLBACK");
    }
    result
}

fn common_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    if !table_exists(conn, "main", table)? || !table_exists(conn, "source_db", table)? {
        return Ok(Vec::new());
    }
    let target = table_columns(conn, "main", table)?;
    let source = table_columns(conn, "source_db", table)?;
    Ok(target
        .into_iter()
        .filter(|column| source.contains(column))
        .collect())
}

fn table_exists(conn: &Connection, database: &str, table: &str) -> Result<bool> {
    let sql = format!(
        "SELECT EXISTS(SELECT 1 FROM {database}.sqlite_master WHERE type='table' AND name=?1)"
    );
    Ok(conn.query_row(&sql, [table], |row| row.get(0))?)
}

fn table_columns(conn: &Connection, database: &str, table: &str) -> Result<Vec<String>> {
    let sql = format!("PRAGMA {database}.table_info({})", quote_ident(table));
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

fn update_newer_rows(
    conn: &Connection,
    table: &str,
    columns: &[String],
    key: &str,
    updated: &str,
    fallback_updated: Option<&str>,
) -> Result<()> {
    if !columns.iter().any(|column| column == key)
        || !columns.iter().any(|column| column == updated)
    {
        return Ok(());
    }
    let table_ident = quote_ident(table);
    let key_ident = quote_ident(key);
    let assignments = columns
        .iter()
        .filter(|column| column.as_str() != key)
        .map(|column| {
            let ident = quote_ident(column);
            format!(
                "{ident} = (SELECT source.{ident} FROM source_db.{table_ident} AS source WHERE source.{key_ident} = main.{table_ident}.{key_ident})"
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    if assignments.is_empty() {
        return Ok(());
    }
    let source_freshness = freshness_expr("source", updated, fallback_updated, columns);
    let target_freshness = freshness_expr(
        &format!("main.{table_ident}"),
        updated,
        fallback_updated,
        columns,
    );
    let sql = format!(
        "UPDATE main.{table_ident} SET {assignments}
         WHERE EXISTS (
             SELECT 1 FROM source_db.{table_ident} AS source
             WHERE source.{key_ident} = main.{table_ident}.{key_ident}
               AND {source_freshness} > {target_freshness}
         )"
    );
    conn.execute(&sql, [])?;
    Ok(())
}

fn freshness_expr(
    qualifier: &str,
    updated: &str,
    fallback_updated: Option<&str>,
    columns: &[String],
) -> String {
    let updated = format!("{qualifier}.{}", quote_ident(updated));
    match fallback_updated.filter(|fallback| columns.iter().any(|column| column == fallback)) {
        Some(fallback) => format!(
            "COALESCE({updated}, {qualifier}.{} * 1000, 0)",
            quote_ident(fallback)
        ),
        None => format!("COALESCE({updated}, 0)"),
    }
}

fn insert_missing_rows(conn: &Connection, table: &str, columns: &[String]) -> Result<()> {
    let table = quote_ident(table);
    let columns = columns
        .iter()
        .map(|column| quote_ident(column))
        .collect::<Vec<_>>();
    let column_list = columns.join(", ");
    let source_columns = columns
        .iter()
        .map(|column| format!("source.{column}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "INSERT OR IGNORE INTO main.{table} ({column_list})
         SELECT {source_columns} FROM source_db.{table} AS source"
    );
    conn.execute(&sql, [])?;
    Ok(())
}

fn quote_ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use rusqlite::params;

    use super::*;

    fn temp_paths(name: &str) -> ManagerPaths {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "cx-merge-test-{name}-{}-{unique}",
            std::process::id()
        ));
        ManagerPaths::from_roots(root.join("codex"), root.join("codex/profile-manager"))
    }

    fn create_state_db(path: &Path, rows: &[(&str, &str, i64)]) {
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
                updated_at_ms INTEGER,
                preview TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE thread_spawn_edges (
                parent_thread_id TEXT NOT NULL,
                child_thread_id TEXT NOT NULL PRIMARY KEY,
                status TEXT NOT NULL
            );",
        )
        .unwrap();
        for (id, provider, updated_at_ms) in rows {
            conn.execute(
                "INSERT INTO threads
                 (id, rollout_path, created_at, updated_at, source, model_provider, updated_at_ms, preview)
                 VALUES (?1, ?2, 1, ?3, 'cli', ?4, ?5, ?4)",
                params![
                    id,
                    format!("/tmp/{id}.jsonl"),
                    updated_at_ms / 1000,
                    provider,
                    updated_at_ms
                ],
            )
            .unwrap();
        }
    }

    #[test]
    fn merge_adds_missing_threads_and_keeps_newest_duplicates() {
        let paths = temp_paths("latest");
        create_state_db(
            &paths.shared_sqlite_home().join("state_5.sqlite"),
            &[("aaa", "openai", 3_000), ("bbb", "openai", 1_000)],
        );
        create_state_db(
            &paths.slot_home("pku").join("sqlite/state_5.sqlite"),
            &[
                ("aaa", "pku-old", 2_000),
                ("bbb", "pku", 4_000),
                ("ccc", "pku", 5_000),
            ],
        );

        let report = merge_slot_databases(&paths, false).unwrap();

        assert_eq!(report.sources, vec!["pku"]);
        assert_eq!(report.shared_threads, 3);
        assert!(migration_marker(&paths).is_file());
        let conn = Connection::open(report.shared_sqlite_home.join("state_5.sqlite")).unwrap();
        let rows = conn
            .prepare("SELECT id, model_provider FROM threads ORDER BY id")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                ("aaa".to_string(), "openai".to_string()),
                ("bbb".to_string(), "pku".to_string()),
                ("ccc".to_string(), "pku".to_string()),
            ]
        );

        let _ = fs::remove_dir_all(&paths.base_codex_home);
    }

    #[test]
    fn merge_handles_source_with_older_schema() {
        let paths = temp_paths("schema");
        create_state_db(
            &paths.shared_sqlite_home().join("state_5.sqlite"),
            &[("aaa", "openai", 1_000)],
        );
        let source = paths.slot_home("old").join("sqlite/state_5.sqlite");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        let conn = Connection::open(&source).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                source TEXT NOT NULL,
                model_provider TEXT NOT NULL
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads VALUES ('bbb', '/tmp/bbb.jsonl', 2, 2, 'cli', 'old')",
            [],
        )
        .unwrap();

        let report = merge_slot_databases(&paths, false).unwrap();

        assert_eq!(report.shared_threads, 2);
        let _ = fs::remove_dir_all(&paths.base_codex_home);
    }

    #[test]
    fn missing_target_is_seeded_from_the_most_complete_schema() {
        let paths = temp_paths("best-seed");
        let old = paths.slot_home("aaa-old").join("sqlite/state_5.sqlite");
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        let conn = Connection::open(&old).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL, source TEXT NOT NULL, model_provider TEXT NOT NULL
            );
            INSERT INTO threads VALUES ('old', '/tmp/old.jsonl', 1, 1, 'cli', 'old');",
        )
        .unwrap();
        create_state_db(
            &paths.slot_home("zzz-new").join("sqlite/state_5.sqlite"),
            &[("new", "cx", 2_000)],
        );

        let report = merge_slot_databases(&paths, false).unwrap();
        let conn = Connection::open(report.shared_sqlite_home.join("state_5.sqlite")).unwrap();
        assert!(table_columns(&conn, "main", "threads")
            .unwrap()
            .contains(&"preview".to_string()));
        let count: i64 = conn
            .query_row("SELECT count(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);

        let _ = fs::remove_dir_all(&paths.base_codex_home);
    }

    #[test]
    fn dry_run_does_not_create_shared_database() {
        let paths = temp_paths("dry-run");
        create_state_db(
            &paths.slot_home("pku").join("sqlite/state_5.sqlite"),
            &[("aaa", "pku", 1_000)],
        );

        let report = merge_slot_databases(&paths, true).unwrap();

        assert!(report.dry_run);
        assert_eq!(report.source_threads, 1);
        assert!(!paths.shared_sqlite_home().join("state_5.sqlite").exists());
        assert!(!migration_marker(&paths).exists());
        let _ = fs::remove_dir_all(&paths.base_codex_home);
    }

    #[test]
    fn startup_marker_retires_legacy_sources_from_the_hot_path() {
        let paths = temp_paths("source-changed");
        let source = paths.slot_home("pku").join("sqlite/state_5.sqlite");
        create_state_db(&source, &[("aaa", "pku", 1_000)]);
        merge_slot_databases(&paths, false).unwrap();
        assert!(migration_is_current(&paths).unwrap());

        let conn = Connection::open(&source).unwrap();
        conn.execute(
            "INSERT INTO threads
             (id, rollout_path, created_at, updated_at, source, model_provider, updated_at_ms, preview)
             VALUES ('bbb', '/tmp/bbb.jsonl', 2, 2, 'cli', 'pku', 2000, 'pku')",
            [],
        )
        .unwrap();
        drop(conn);

        assert!(migration_is_current(&paths).unwrap());
        run_startup_migration(&paths, false).unwrap();
        assert!(migration_is_current(&paths).unwrap());
        let conn = Connection::open(paths.shared_sqlite_home().join("state_5.sqlite")).unwrap();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);

        run_startup_migration(&paths, true).unwrap();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);

        let _ = fs::remove_dir_all(&paths.base_codex_home);
    }

    #[test]
    fn startup_does_not_remerge_when_existing_thread_is_updated() {
        let paths = temp_paths("source-metadata-changed");
        let source = paths.slot_home("pku").join("sqlite/state_5.sqlite");
        create_state_db(&source, &[("aaa", "pku", 1_000)]);
        merge_slot_databases(&paths, false).unwrap();
        assert!(migration_is_current(&paths).unwrap());

        let conn = Connection::open(&source).unwrap();
        conn.execute(
            "UPDATE threads SET updated_at = 2, updated_at_ms = 2000 WHERE id = 'aaa'",
            [],
        )
        .unwrap();
        drop(conn);

        assert!(migration_is_current(&paths).unwrap());
        let _ = fs::remove_dir_all(&paths.base_codex_home);
    }

    #[test]
    fn startup_does_not_remerge_when_shared_target_changes() {
        let paths = temp_paths("shared-target-changed");
        let source = paths.slot_home("pku").join("sqlite/state_5.sqlite");
        create_state_db(&source, &[("aaa", "pku", 1_000)]);
        merge_slot_databases(&paths, false).unwrap();
        assert!(migration_is_current(&paths).unwrap());

        let target = paths.shared_sqlite_home().join("state_5.sqlite");
        let conn = Connection::open(&target).unwrap();
        conn.execute(
            "INSERT INTO threads
             (id, rollout_path, created_at, updated_at, source, model_provider, updated_at_ms, preview)
             VALUES ('bbb', '/tmp/bbb.jsonl', 2, 2, 'cli', 'cx', 2000, 'cx')",
            [],
        )
        .unwrap();
        drop(conn);

        assert!(migration_is_current(&paths).unwrap());
        let _ = fs::remove_dir_all(&paths.base_codex_home);
    }
}
