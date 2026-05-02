use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use rusqlite::Connection;

pub(super) fn merge_sqlite_databases(canonical_path: &Path, slot_path: &Path) -> Result<()> {
    let mut conn = Connection::open(canonical_path)
        .with_context(|| format!("open {}", canonical_path.display()))?;
    conn.execute(
        "ATTACH DATABASE ?1 AS slot_db",
        [slot_path.display().to_string()],
    )
    .with_context(|| format!("attach {}", slot_path.display()))?;

    {
        let transaction = conn.transaction().context("start sqlite merge")?;

        let tables = {
            let mut statement = transaction
                .prepare(
                    "SELECT name FROM slot_db.sqlite_master \
                     WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                )
                .context("list slot sqlite tables")?;
            let tables = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            tables
        };

        for table in tables {
            if sqlite_table_exists(&transaction, &table)? {
                let ident = quote_sqlite_ident(&table);
                let sql = format!("INSERT OR IGNORE INTO {ident} SELECT * FROM slot_db.{ident}");
                transaction
                    .execute(&sql, [])
                    .with_context(|| format!("merge sqlite table {table}"))?;
            }
        }

        transaction.commit().context("commit sqlite merge")?;
    }

    conn.execute("DETACH DATABASE slot_db", [])
        .context("detach slot sqlite")?;
    Ok(())
}

fn sqlite_table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let exists = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM main.sqlite_master WHERE type = 'table' AND name = ?1)",
            [table],
            |row| row.get::<_, bool>(0),
        )
        .context("check sqlite table")?;
    Ok(exists)
}

fn quote_sqlite_ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
