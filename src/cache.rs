use std::fs;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use rusqlite::Connection;
use rusqlite::OpenFlags;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::paths::ManagerPaths;

pub(crate) mod entries {
    pub(crate) const PRICE_CACHE: &str = "price-cache.json";
    pub(crate) const STATS_CALIBRATION: &str = "stats-calibration.json";
    pub(crate) const STATS_ROLLOUT_SQLITE: &str = "stats-rollout-cache.sqlite";
    pub(crate) const LEGACY_STATS_ROLLOUT_JSON: &str = "stats-rollout-cache.json";
    pub(crate) const USAGE_RATE_STATE: &str = "usage-rate-state.json";
    pub(crate) const USAGE_SLOT_CACHE_DIR: &str = "usage-cache/slots";
}

#[derive(Debug, Clone)]
pub(crate) struct CacheStore {
    root: PathBuf,
}

#[derive(Debug)]
pub(crate) struct SqliteCache {
    conn: Option<Connection>,
    writable: bool,
}

impl CacheStore {
    pub(crate) fn new(paths: &ManagerPaths) -> Self {
        Self {
            root: paths.manager_dir.clone(),
        }
    }

    pub(crate) fn path(&self, relative: impl AsRef<Path>) -> PathBuf {
        self.root.join(relative)
    }

    pub(crate) fn read_json<T>(
        &self,
        relative: impl AsRef<Path>,
        valid: impl FnOnce(&T) -> bool,
    ) -> Option<T>
    where
        T: DeserializeOwned,
    {
        read_json_path(&self.path(relative), valid)
    }

    pub(crate) fn write_json<T>(&self, relative: impl AsRef<Path>, value: &T) -> Result<PathBuf>
    where
        T: Serialize,
    {
        let path = self.path(relative);
        write_json_path(&path, value)?;
        Ok(path)
    }

    pub(crate) fn remove_file_if_present(&self, relative: impl AsRef<Path>) -> Result<bool> {
        let path = self.path(relative);
        if !path.is_file() {
            return Ok(false);
        }
        fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        Ok(true)
    }

    pub(crate) fn open_sqlite(
        &self,
        relative: impl AsRef<Path>,
        schema_version: u64,
        initialize: impl Fn(&Connection) -> Result<()>,
    ) -> SqliteCache {
        let path = self.path(relative);
        if fs::create_dir_all(&self.root).is_ok() {
            if let Ok(conn) = Connection::open(&path) {
                if initialize(&conn).is_ok() {
                    return SqliteCache {
                        conn: Some(conn),
                        writable: true,
                    };
                }
            }
        }

        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .and_then(|conn| {
                validate_sqlite_schema(&conn, schema_version)?;
                Ok(conn)
            })
            .ok();
        SqliteCache {
            conn,
            writable: false,
        }
    }
}

impl SqliteCache {
    pub(crate) fn conn_mut(&mut self) -> Option<&mut Connection> {
        self.conn.as_mut()
    }

    pub(crate) fn is_writable(&self) -> bool {
        self.writable
    }
}

pub(crate) fn read_json_path<T>(path: &Path, valid: impl FnOnce(&T) -> bool) -> Option<T>
where
    T: DeserializeOwned,
{
    let content = fs::read_to_string(path).ok()?;
    let value = serde_json::from_str::<T>(&content).ok()?;
    valid(&value).then_some(value)
}

pub(crate) fn write_json_path<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize,
{
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    let mut content = serde_json::to_vec_pretty(value)?;
    content.push(b'\n');

    let tmp_path = tmp_path_for(path);
    fs::write(&tmp_path, content).with_context(|| format!("write {}", tmp_path.display()))?;
    fs::rename(&tmp_path, path)
        .inspect_err(|_err| {
            let _ignored = fs::remove_file(&tmp_path);
        })
        .with_context(|| format!("write {}", path.display()))
}

pub(crate) fn validate_sqlite_schema(
    conn: &Connection,
    schema_version: u64,
) -> rusqlite::Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let expected = i64::try_from(schema_version).unwrap_or(i64::MAX);
    if version != expected {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(())
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("cache");
    path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()))
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use serde::Deserialize;
    use serde::Serialize;

    use super::*;

    #[derive(Debug, Deserialize, Serialize)]
    struct FixtureCache {
        #[serde(rename = "schemaVersion")]
        schema_version: u64,
        value: String,
    }

    fn temp_store(name: &str) -> CacheStore {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        CacheStore {
            root: std::env::temp_dir().join(format!(
                "cx-cache-test-{name}-{}-{unique}",
                std::process::id()
            )),
        }
    }

    #[test]
    fn json_cache_round_trips_with_schema_validator() {
        let store = temp_store("json");
        let cache = FixtureCache {
            schema_version: 2,
            value: "cached".to_string(),
        };

        let path = store.write_json("nested/cache.json", &cache).unwrap();
        let loaded = read_json_path(&path, |cache: &FixtureCache| cache.schema_version == 2)
            .expect("valid cache loads");
        assert_eq!(loaded.value, "cached");
        assert!(store
            .read_json::<FixtureCache>("nested/cache.json", |cache| { cache.schema_version == 1 })
            .is_none());

        let _ = fs::remove_dir_all(store.root);
    }

    #[test]
    fn sqlite_cache_initializes_writable_database() {
        let store = temp_store("sqlite");
        let mut cache = store.open_sqlite("cache.sqlite", 7, |conn| {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS values_table (value TEXT NOT NULL);
                 PRAGMA user_version = 7;",
            )?;
            Ok(())
        });

        assert!(cache.is_writable());
        let conn = cache.conn_mut().expect("sqlite cache opens");
        conn.execute("INSERT INTO values_table (value) VALUES ('cached')", [])
            .unwrap();
        let value: String = conn
            .query_row("SELECT value FROM values_table", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "cached");

        let _ = fs::remove_dir_all(store.root);
    }
}
