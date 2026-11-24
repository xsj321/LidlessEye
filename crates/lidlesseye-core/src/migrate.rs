//! One-time upgrade from the retired on-disk DuckDB layout.
//!
//! Canonical modes never write a `lidlesseye.duckdb` file. When an older
//! release's file is found under a tenant root, this module exports every
//! table into deterministic hour-partitioned Parquet files and renames the
//! old file aside. The export is idempotent: a target file that already
//! exists is left alone, so a crash part-way through does not duplicate rows
//! on the next start.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use duckdb::Connection;
use tracing::info;

use crate::error::{Error, Result};
use crate::parquet::TABLES;
use crate::store::DB_FILE;

/// What one migration pass accomplished.
#[derive(Debug, Default, Clone, Copy)]
pub struct MigrationStats {
    pub files_written: usize,
    pub rows_copied: u64,
}

/// Export a legacy DuckDB file to Parquet, if one is present.
///
/// Never returns an empty store silently: an unreadable legacy file is a hard
/// error, and the operator must fix or remove it explicitly.
pub fn migrate_legacy(root: &Path) -> Result<MigrationStats> {
    let path = root.join(DB_FILE);
    if !path.is_file() {
        return Ok(MigrationStats::default());
    }
    info!(path = %path.display(), stage = "legacy_migration", "found legacy DuckDB file; exporting to Parquet");

    let conn = Connection::open(&path)
        .map_err(|e| Error::Db(format!("opening {}: {e}", path.display())))?;
    let mut stats = MigrationStats::default();

    for def in TABLES {
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM information_schema.tables WHERE table_name = ?",
                [def.name],
                |row| row.get(0),
            )
            .unwrap_or(0);
        if exists == 0 {
            continue;
        }
        let dir = root.join("parquet").join(def.name);
        std::fs::create_dir_all(&dir)?;
        let mut hours: Vec<i64> = Vec::new();
        if let Some(column) = def.hour_column {
            let sql = format!(
                "SELECT DISTINCT CAST({column}/1_000_000_000/3600 AS BIGINT) FROM {}",
                def.name
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| Error::Db(format!("listing legacy {} hours: {e}", def.name)))?;
            let rows = stmt
                .query_map([], |row| row.get::<_, i64>(0))
                .map_err(|e| Error::Db(format!("listing legacy {} hours: {e}", def.name)))?;
            for row in rows.flatten() {
                hours.push(row);
            }
        } else {
            hours.push(current_hour());
        }
        hours.sort_unstable();

        for hour in hours {
            let target = dir.join(format!("h{hour}-legacy.parquet"));
            if target.exists() {
                continue;
            }
            let filter = def
                .hour_column
                .map(|column| format!("WHERE CAST({column}/1_000_000_000/3600 AS BIGINT) = {hour}"))
                .unwrap_or_default();
            let count: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {} {filter}", def.name),
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            if count <= 0 {
                continue;
            }
            let tmp = dir.join(format!("h{hour}-legacy.parquet.tmp"));
            let sql = format!(
                "COPY (SELECT * FROM {} {filter}) TO '{}' \
                 (FORMAT PARQUET, COMPRESSION ZSTD, ROW_GROUP_SIZE 122880)",
                def.name,
                tmp.to_string_lossy().replace('\\', "/")
            );
            conn.execute_batch(&sql)
                .map_err(|e| Error::Db(format!("exporting legacy {}: {e}", def.name)))?;
            let written: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM read_parquet('{}')",
                        tmp.to_string_lossy().replace('\\', "/")
                    ),
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            if written != count {
                let _ = std::fs::remove_file(&tmp);
                return Err(Error::Db(format!(
                    "legacy {} hour {hour}: wrote {written} rows, expected {count}",
                    def.name
                )));
            }
            std::fs::rename(&tmp, &target)?;
            stats.files_written += 1;
            stats.rows_copied += written.max(0) as u64;
        }
    }
    drop(conn);

    // Keep the old file under a new name instead of deleting it: a failed
    // verification after the rename is recoverable by renaming it back.
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    rename_aside(
        &path,
        &path.with_extension(format!("duckdb.migrated-{stamp}")),
    )?;
    let wal = PathBuf::from(format!("{}.wal", path.to_string_lossy()));
    if wal.is_file() {
        rename_aside(&wal, &wal.with_extension(format!("wal.migrated-{stamp}")))?;
    }
    info!(
        files = stats.files_written,
        rows = stats.rows_copied,
        stage = "legacy_migration",
        "legacy DuckDB file exported; original kept aside"
    );
    Ok(stats)
}

fn rename_aside(from: &Path, to: &Path) -> Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Io(e)),
    }
}

fn current_hour() -> i64 {
    (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 3600) as i64
}
