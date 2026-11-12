//! Storage backed by DuckDB 2.0.
//!
//! The earlier implementation wrote immutable `.leseg` files and maintained its
//! own header layout, frame dictionary, CRC32 checksums and crash-safe rename.
//! That is a storage engine, and it had to be re-verified whenever any of it
//! changed. DuckDB now owns that job: transactional writes, compression, zone
//! maps, checkpointing and recovery are its problem.
//!
//! What is left on our side is the part that is actually specific to
//! profiling — a small schema:
//!
//! ```sql
//! batches(batch_id, profile_type, service_name, labels, start_ns, end_ns,
//!         total_value, sample_count, bytes, created_ns)
//! samples(batch_id, profile_type, service_name, labels, start_ns, end_ns,
//!         stack_id, value)
//! stacks (stack_id, frames BLOB)        -- packed little-endian u32 frame ids
//! frames (frame_id, module, symbol, file, line)
//! ```
//!
//! `samples` repeats the series columns instead of joining `batches` on every
//! query. Profiles are append-only and a batch is one (service, type, label
//! set, time window), so the repeated values are constant over a contiguous
//! run of rows and DuckDB's run-length compression makes them nearly free.
//! Paying that on write buys a single-table scan with no join on the read path,
//! which is the only part that has to be fast.
//!
//! Stacks are stored as packed ids rather than as a delimited string: a stack
//! is walked on every flamegraph query, and turning text into frame ids at
//! query time would move work from ingest (once) to query (every time).
//! `frames` is a global dictionary, so the same function appears once no matter
//! how many batches reference it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use duckdb::{Connection, params};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::error::{Error, Result};
use crate::model::{Frame, ProfileType, Report, SERVICE_NAME_LABEL};
use crate::selector::{MatchOp, Selector};
use crate::time::now_ns;

/// Version of the table layout. Bumped when a change needs a migration.
pub const SCHEMA_VERSION: u32 = 1;

/// File name of the database inside a store root.
pub const DB_FILE: &str = "lidlesseye.duckdb";

/// Hard ceiling on rows returned by one [`Store::scan`], so a query over a
/// month of data fails as a truncated answer instead of as an OOM kill.
pub const MAX_SCAN_ROWS: usize = 2_000_000;

/// Tunables for the storage engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreConfig {
    /// Directory holding the database file.
    pub root: PathBuf,
    /// Delete data whose window ended longer ago than this. `None` keeps
    /// everything.
    pub retention_ns: Option<u64>,
    /// Soft cap for the estimated size of the retained batches
    /// (`SUM(batches.bytes)`), not the file size — DuckDB does not shrink the
    /// file on `DELETE`. Oldest batches are evicted first.
    pub max_disk_bytes: Option<u64>,
    /// Worker threads DuckDB may use. `None` lets it pick.
    pub threads: Option<usize>,
}

impl StoreConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        StoreConfig {
            root: root.into(),
            retention_ns: None,
            max_disk_bytes: None,
            threads: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct StoreStats {
    /// Number of ingested batches. Named `segment_count` for the `/metrics`
    /// contract; one batch is what used to be one segment file.
    pub segment_count: usize,
    /// Physical size of the database file.
    pub total_bytes: u64,
    /// Size the retained batches are estimated to occupy. This is what the
    /// disk cap is measured against: DuckDB marks deleted rows free inside
    /// existing row groups instead of shrinking the file, so the physical size
    /// is not a usable eviction signal.
    pub logical_bytes: u64,
    pub total_value: u64,
    pub earliest_ns: Option<u64>,
    pub latest_ns: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct PruneStats {
    pub removed_segments: usize,
    pub removed_bytes: u64,
}

/// One ingested batch: the unit that used to be a segment file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchMeta {
    pub batch_id: u64,
    pub profile_type: ProfileType,
    pub service_name: String,
    pub labels: BTreeMap<String, String>,
    pub start_ns: u64,
    pub end_ns: u64,
    pub total_value: u64,
    pub sample_count: u32,
    /// Approximate on-disk contribution, used for tenant size accounting.
    pub bytes: u64,
    pub created_ns: u64,
}

impl BatchMeta {
    pub fn overlaps(&self, from_ns: u64, until_ns: u64) -> bool {
        self.end_ns > from_ns && self.start_ns < until_ns
    }

    pub fn label(&self, key: &str) -> Option<&str> {
        if key == SERVICE_NAME_LABEL {
            Some(&self.service_name)
        } else {
            self.labels.get(key).map(|s| s.as_str())
        }
    }
}

/// Aggregated information about one series (service + label set + type).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeriesInfo {
    pub service_name: String,
    pub profile_type: String,
    pub labels: BTreeMap<String, String>,
    pub total_value: u64,
    pub segment_count: usize,
    pub start_ns: u64,
    pub end_ns: u64,
}

/// One `(stack, value)` pair of a scan, already summed over the whole window.
#[derive(Debug, Clone)]
pub struct SampleRow {
    pub stack_id: u64,
    pub value: u64,
    /// Value of the group-by label, decoded from the batch labels.
    pub group: Option<String>,
}

/// Everything a query needs from storage, in one round trip per table.
pub struct ScanResult {
    pub rows: Vec<SampleRow>,
    /// `stack_id` -> frame ids, leaf first.
    pub stacks: HashMap<u64, Vec<u32>>,
    pub frames: HashMap<u32, Frame>,
    /// True when the query hit [`MAX_SCAN_ROWS`] or the row cap of the query.
    pub truncated: bool,
}

/// Parameters of a [`Store::scan`].
pub struct ScanQuery<'a> {
    pub profile_type: &'a ProfileType,
    pub from_ns: u64,
    pub until_ns: u64,
    pub selector: &'a Selector,
    pub group_by: Option<&'a str>,
}

impl<'a> ScanQuery<'a> {
    pub fn new(
        profile_type: &'a ProfileType,
        from_ns: u64,
        until_ns: u64,
        selector: &'a Selector,
    ) -> Self {
        ScanQuery {
            profile_type,
            from_ns,
            until_ns,
            selector,
            group_by: None,
        }
    }

    pub fn with_group_by(mut self, label: Option<&'a str>) -> Self {
        self.group_by = label.filter(|label| !label.trim().is_empty());
        self
    }
}

/// Frame identity used to deduplicate across the frames that arrive in one
/// batch. Values are owned because the cache outlives a single report.
type FrameKey = (String, String, String, u32);

/// The rows one append will insert, staged before the transaction opens.
struct PendingBatch {
    /// `(frame_id, frame)` for frames the database does not have yet.
    frames: Vec<(u32, Frame)>,
    /// `(stack_id, packed frame ids)`.
    stacks: Vec<(u64, Vec<u32>)>,
    /// `(stack_id, value)`.
    samples: Vec<(u64, u64)>,
}

#[derive(Default)]
struct Ids {
    next_frame: u32,
    next_stack: u64,
    next_batch: u64,
}

/// Thread-safe profile store.
///
/// DuckDB's Rust binding has no separate `Database` handle: a `Connection`
/// owns the engine, and opening the same file twice in one process is a lock
/// conflict rather than a second reader. Every operation therefore goes
/// through one connection behind a mutex. Queries are still parallel *inside*
/// DuckDB (`threads`), which is where a large scan actually spends its time;
/// what is serialized is the short per-query setup.
pub struct Store {
    cfg: StoreConfig,
    conn: Mutex<Connection>,
    /// `(module, symbol, file, line)` -> frame id, for the write path only.
    frame_cache: Mutex<HashMap<FrameKey, u32>>,
    ids: Mutex<Ids>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("root", &self.cfg.root)
            .field("db", &self.path())
            .finish()
    }
}

impl Store {
    /// Open (and create if needed) the database under `cfg.root`.
    pub fn open(cfg: StoreConfig) -> Result<Self> {
        std::fs::create_dir_all(&cfg.root)?;
        let path = cfg.root.join(DB_FILE);

        let mut config = duckdb::Config::default();
        if let Some(threads) = cfg.threads {
            config = config
                .threads(threads as i64)
                .map_err(|err| Error::Db(format!("setting DuckDB threads: {err}")))?;
        }
        let conn = Connection::open_with_flags(&path, config)
            .map_err(|err| Error::Db(format!("opening {}: {err}", path.display())))?;
        create_schema(&conn)?;

        let ids = {
            let max_frame: i64 = conn
                .query_row("SELECT COALESCE(MAX(frame_id), 0) FROM frames", [], |row| {
                    row.get(0)
                })
                .unwrap_or(0);
            let max_stack: i64 = conn
                .query_row("SELECT COALESCE(MAX(stack_id), 0) FROM stacks", [], |row| {
                    row.get(0)
                })
                .unwrap_or(0);
            let max_batch: i64 = conn
                .query_row(
                    "SELECT COALESCE(MAX(batch_id), 0) FROM batches",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            // Counters resume *after* the highest id in use; taking `max`
            // verbatim would hand the next append an id that already exists.
            // frame ids are u32 on the wire, so the counter may not wrap.
            Ids {
                next_frame: u32::try_from(max_frame.saturating_add(1)).unwrap_or(u32::MAX),
                next_stack: max_stack.max(0) as u64 + 1,
                next_batch: max_batch.max(0) as u64 + 1,
            }
        };

        let store = Store {
            cfg,
            conn: Mutex::new(conn),
            frame_cache: Mutex::new(HashMap::new()),
            ids: Mutex::new(ids),
        };
        store.warn_about_legacy_segments();
        Ok(store)
    }

    /// `.leseg` files from a pre-DuckDB deployment cannot be read any more.
    /// Saying so once is better than silently reporting an empty dashboard.
    fn warn_about_legacy_segments(&self) {
        let mut found = 0usize;
        if let Ok(types) = std::fs::read_dir(&self.cfg.root) {
            for entry in types.flatten() {
                if !entry.path().is_dir() {
                    continue;
                }
                if let Ok(services) = std::fs::read_dir(entry.path()) {
                    for service in services.flatten() {
                        if let Ok(files) = std::fs::read_dir(service.path()) {
                            found += files
                                .flatten()
                                .filter(|f| f.path().to_string_lossy().ends_with(".leseg"))
                                .count();
                        }
                    }
                }
            }
        }
        if found > 0 {
            warn!(
                count = found,
                root = %self.cfg.root.display(),
                "found `.leseg` segments from an older version; they are not read by the \
                 DuckDB store — re-ingest the data or keep the old binary for it"
            );
        }
    }

    pub fn config(&self) -> &StoreConfig {
        &self.cfg
    }

    pub fn root(&self) -> &Path {
        &self.cfg.root
    }

    /// Path of the database file.
    pub fn path(&self) -> PathBuf {
        self.cfg.root.join(DB_FILE)
    }

    /// Kept for API compatibility; the schema is created on open.
    pub fn reload(&self) -> Result<usize> {
        self.warn_about_legacy_segments();
        Ok(self.stats().segment_count)
    }

    /// Every batch, oldest first.
    pub fn batches(&self) -> Vec<Arc<BatchMeta>> {
        self.query_batches(None, None, &Selector::default())
            .unwrap_or_default()
    }

    /// Batches of `profile_type` overlapping the window that satisfy the
    /// selector.
    pub fn select(
        &self,
        profile_type: &ProfileType,
        from_ns: u64,
        until_ns: u64,
        selector: &Selector,
    ) -> Vec<Arc<BatchMeta>> {
        self.query_batches(Some((profile_type, from_ns, until_ns)), None, selector)
            .unwrap_or_default()
    }

    fn query_batches(
        &self,
        window: Option<(&ProfileType, u64, u64)>,
        limit: Option<usize>,
        selector: &Selector,
    ) -> Result<Vec<Arc<BatchMeta>>> {
        let conn = self.conn.lock();
        let mut sql = String::from(
            "SELECT batch_id, profile_type, service_name, labels, start_ns, end_ns, \
                    total_value, sample_count, bytes, created_ns FROM batches",
        );
        let mut clauses: Vec<String> = Vec::new();
        let mut args: Vec<Box<dyn duckdb::ToSql>> = Vec::new();

        if let Some((profile_type, from_ns, until_ns)) = window {
            let types = profile_type.source_types();
            let placeholders = vec!["?"; types.len()].join(", ");
            clauses.push(format!("profile_type IN ({placeholders})"));
            for t in types {
                args.push(Box::new(t.as_str().to_string()));
            }
            clauses.push("end_ns > ?".into());
            args.push(Box::new(ts(from_ns)));
            clauses.push("start_ns < ?".into());
            args.push(Box::new(ts(until_ns)));
        }
        push_selector_sql(selector, &mut clauses, &mut args);

        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        sql.push_str(" ORDER BY start_ns ASC, batch_id ASC");
        if let Some(limit) = limit {
            sql.push_str(&format!(" LIMIT {limit}"));
        }

        let mut stmt = conn.prepare(&sql).map_err(db_err)?;
        let refs: Vec<&dyn duckdb::ToSql> = args.iter().map(|a| a.as_ref()).collect();
        let mut rows = stmt.query(refs.as_slice()).map_err(db_err)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(db_err)? {
            let labels: String = row.get(3).map_err(db_err)?;
            let meta = BatchMeta {
                batch_id: row.get::<_, i64>(0).map_err(db_err)?.max(0) as u64,
                profile_type: ProfileType::new(row.get::<_, String>(1).map_err(db_err)?),
                service_name: row.get(2).map_err(db_err)?,
                labels: decode_labels(&labels),
                start_ns: row.get::<_, i64>(4).map_err(db_err)?.max(0) as u64,
                end_ns: row.get::<_, i64>(5).map_err(db_err)?.max(0) as u64,
                total_value: row.get::<_, i64>(6).map_err(db_err)?.max(0) as u64,
                sample_count: row.get::<_, i64>(7).map_err(db_err)?.max(0) as u32,
                bytes: row.get::<_, i64>(8).map_err(db_err)?.max(0) as u64,
                created_ns: row.get::<_, i64>(9).map_err(db_err)?.max(0) as u64,
            };
            // SQL already rejected the profile type and the window; the label
            // matchers run here, where glob semantics live.
            if selector.matches_meta(&meta.service_name, &meta.labels) {
                out.push(Arc::new(meta));
            }
        }
        Ok(out)
    }

    /// Persist a report as one batch.
    pub fn append(&self, report: &Report) -> Result<Arc<BatchMeta>> {
        report.validate()?;

        let labels_text = encode_labels(&report.all_labels());
        let batch_id = {
            let mut ids = self.ids.lock();
            ids.next_batch += 1;
            ids.next_batch
        };

        // Intern frames and stacks before opening the transaction so a bad
        // report cannot leave a half-written batch behind.
        let pending = self.prepare_rows(report)?;
        let bytes = pending.samples.len() as u64 * 24
            + pending
                .stacks
                .iter()
                .map(|(_, frames)| frames.len() as u64 * 4)
                .sum::<u64>()
            + pending.frames.len() as u64 * 48;

        let created_ns = now_ns();
        let total_value: u64 = report.stacks.iter().map(|s| s.value).sum();

        {
            let conn = self.conn.lock();
            conn.execute_batch("BEGIN TRANSACTION").map_err(db_err)?;
            let result = (|| -> Result<()> {
                if !pending.frames.is_empty() {
                    let mut stmt = conn
                        .prepare(
                            "INSERT INTO frames (frame_id, module, symbol, file, line) \
                             VALUES (?, ?, ?, ?, ?)",
                        )
                        .map_err(db_err)?;
                    for (id, frame) in &pending.frames {
                        stmt.execute(params![
                            *id as i64,
                            frame.module,
                            frame.symbol,
                            frame.file,
                            frame.line as i64
                        ])
                        .map_err(db_err)?;
                    }
                }
                if !pending.stacks.is_empty() {
                    let mut stmt = conn
                        .prepare("INSERT INTO stacks (stack_id, frames) VALUES (?, ?)")
                        .map_err(db_err)?;
                    for (id, frames) in &pending.stacks {
                        stmt.execute(params![*id as i64, pack_frames(frames)])
                            .map_err(db_err)?;
                    }
                }
                let mut stmt = conn
                    .prepare(
                        "INSERT INTO samples (batch_id, profile_type, service_name, labels, \
                         start_ns, end_ns, stack_id, value) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                    )
                    .map_err(db_err)?;
                for (stack_id, value) in &pending.samples {
                    stmt.execute(params![
                        batch_id as i64,
                        report.profile_type.as_str(),
                        report.service_name,
                        labels_text,
                        ts(report.start_ns),
                        ts(report.end_ns),
                        *stack_id as i64,
                        *value as i64
                    ])
                    .map_err(db_err)?;
                }
                conn.execute(
                    "INSERT INTO batches (batch_id, profile_type, service_name, labels, start_ns, \
                     end_ns, total_value, sample_count, bytes, created_ns) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    params![
                        batch_id as i64,
                        report.profile_type.as_str(),
                        report.service_name,
                        labels_text,
                        ts(report.start_ns),
                        ts(report.end_ns),
                        total_value as i64,
                        pending.samples.len() as i64,
                        bytes as i64,
                        ts(created_ns)
                    ],
                )
                .map_err(db_err)?;
                Ok(())
            })();

            match result {
                Ok(()) => {
                    conn.execute_batch("COMMIT").map_err(db_err)?;
                }
                Err(err) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(err);
                }
            }
        }

        debug!(
            batch = batch_id,
            stacks = pending.samples.len(),
            frames = pending.frames.len(),
            "wrote batch"
        );

        Ok(Arc::new(BatchMeta {
            batch_id,
            profile_type: report.profile_type.clone(),
            service_name: report.service_name.clone(),
            labels: report.all_labels(),
            start_ns: report.start_ns,
            end_ns: report.end_ns,
            total_value,
            sample_count: pending.samples.len() as u32,
            bytes,
            created_ns,
        }))
    }

    /// Turn a report into the three row sets the transaction will insert.
    ///
    /// Deduplication happens twice and for two different reasons: frames are
    /// deduplicated against the whole database (so a function is stored once),
    /// stacks only inside the batch (a repeated stack inside one profile would
    /// otherwise be stored twice, while cross-batch deduplication would need a
    /// lookup of every stack on the ingest path).
    fn prepare_rows(&self, report: &Report) -> Result<PendingBatch> {
        let mut frame_rows: Vec<(u32, Frame)> = Vec::new();
        // Keyed by the whole frame reference `[module, symbol, file, line]`,
        // not by the module's string index: two functions in one binary share
        // a module string, and keying on it would merge them.
        let mut frame_ids: HashMap<[u32; 4], u32> = HashMap::new();
        {
            let mut cache = self.frame_cache.lock();
            let mut ids = self.ids.lock();
            for stack in &report.stacks {
                for frame_ref in &stack.frames {
                    if frame_ids.contains_key(frame_ref) {
                        continue;
                    }
                    let string = |i: u32| -> &str {
                        report
                            .strings
                            .get(i as usize)
                            .map(String::as_str)
                            .unwrap_or("")
                    };
                    let frame = Frame {
                        module: string(frame_ref[0]).to_string(),
                        symbol: string(frame_ref[1]).to_string(),
                        file: string(frame_ref[2]).to_string(),
                        line: frame_ref[3],
                        addr: 0,
                    };
                    let key: FrameKey = (
                        frame.module.clone(),
                        frame.symbol.clone(),
                        frame.file.clone(),
                        frame.line,
                    );
                    let id = match cache.get(&key) {
                        Some(&id) => id,
                        None => {
                            let id = ids.next_frame;
                            ids.next_frame = ids.next_frame.saturating_add(1);
                            cache.insert(key, id);
                            frame_rows.push((id, frame));
                            id
                        }
                    };
                    frame_ids.insert(*frame_ref, id);
                }
            }
        }

        let mut stack_rows: Vec<(u64, Vec<u32>)> = Vec::new();
        let mut stack_index: HashMap<Vec<u32>, u64> = HashMap::new();
        let mut sample_rows: Vec<(u64, u64)> = Vec::with_capacity(report.stacks.len());
        {
            let mut ids = self.ids.lock();
            for stack in &report.stacks {
                let frames: Vec<u32> = stack
                    .frames
                    .iter()
                    .filter_map(|frame_ref| frame_ids.get(frame_ref).copied())
                    .collect();
                let stack_id = match stack_index.get(&frames) {
                    Some(&id) => id,
                    None => {
                        let id = ids.next_stack;
                        ids.next_stack = ids.next_stack.saturating_add(1);
                        stack_index.insert(frames.clone(), id);
                        stack_rows.push((id, frames));
                        id
                    }
                };
                sample_rows.push((stack_id, stack.value));
            }
        }
        Ok(PendingBatch {
            frames: frame_rows,
            stacks: stack_rows,
            samples: sample_rows,
        })
    }

    /// Every `(stack, value)` pair matching the query, plus the frames they
    /// reference.
    pub fn scan(&self, query: &ScanQuery<'_>) -> Result<ScanResult> {
        let conn = self.conn.lock();
        let types = query.profile_type.source_types();
        let placeholders = vec!["?"; types.len()].join(", ");
        let mut sql = format!(
            "SELECT stack_id, service_name, labels, SUM(value) FROM samples \
             WHERE profile_type IN ({placeholders}) AND end_ns > ? AND start_ns < ?"
        );
        let mut args: Vec<Box<dyn duckdb::ToSql>> = Vec::new();
        for t in &types {
            args.push(Box::new(t.as_str().to_string()));
        }
        args.push(Box::new(ts(query.from_ns)));
        args.push(Box::new(ts(query.until_ns)));

        let mut clauses: Vec<String> = Vec::new();
        push_selector_sql(query.selector, &mut clauses, &mut args);
        if !clauses.is_empty() {
            sql.push_str(" AND ");
            sql.push_str(&clauses.join(" AND "));
        }
        // Grouping by the series as well as the stack keeps the label string
        // available for the exact matchers, and lets the same stack appear
        // once per series (which is what `group_by` needs).
        sql.push_str(" GROUP BY stack_id, service_name, labels");

        let mut stmt = conn.prepare(&sql).map_err(db_err)?;
        let refs: Vec<&dyn duckdb::ToSql> = args.iter().map(|a| a.as_ref()).collect();
        let mut raw = stmt.query(refs.as_slice()).map_err(db_err)?;

        let mut rows: Vec<SampleRow> = Vec::new();
        let mut truncated = false;
        while let Some(row) = raw.next().map_err(db_err)? {
            if rows.len() >= MAX_SCAN_ROWS {
                truncated = true;
                break;
            }
            let stack_id = row.get::<_, i64>(0).map_err(db_err)?.max(0) as u64;
            let service_name: String = row.get(1).map_err(db_err)?;
            let labels_text: String = row.get(2).map_err(db_err)?;
            let value = row.get::<_, i64>(3).map_err(db_err)?.max(0) as u64;

            let mut labels = decode_labels(&labels_text);
            // `all_labels` puts the service name into the label set before
            // encoding, but a report written by an older version may not have.
            if !labels.contains_key(SERVICE_NAME_LABEL) {
                labels.insert(SERVICE_NAME_LABEL.to_string(), service_name.clone());
            }
            // The label matchers (glob included) run here; SQL only pruned on
            // `service_name`. Dropping a whole (stack, series) group is safe
            // because the group is atomic: every row in it shares the series.
            if !query.selector.matches_meta(&service_name, &labels) {
                continue;
            }

            let group = query.group_by.and_then(|label| {
                let value = if label == SERVICE_NAME_LABEL {
                    Some(service_name.clone())
                } else {
                    labels.get(label).cloned()
                };
                value.map(|value| format!("{label}={value}"))
            });
            rows.push(SampleRow {
                stack_id,
                value,
                group,
            });
        }
        drop(raw);
        drop(stmt);

        let stack_ids: Vec<u64> = rows.iter().map(|r| r.stack_id).collect();
        let stacks = self.load_stacks(&conn, &stack_ids)?;
        let frame_ids: Vec<u32> = {
            let mut seen: std::collections::HashSet<u32> = Default::default();
            let mut out = Vec::new();
            for frames in stacks.values() {
                for &id in frames {
                    if seen.insert(id) {
                        out.push(id);
                    }
                }
            }
            out
        };
        let frames = self.load_frames(&conn, &frame_ids)?;

        Ok(ScanResult {
            rows,
            stacks,
            frames,
            truncated,
        })
    }

    fn load_stacks(&self, conn: &Connection, ids: &[u64]) -> Result<HashMap<u64, Vec<u32>>> {
        let mut out = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(1000) {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let sql =
                format!("SELECT stack_id, frames FROM stacks WHERE stack_id IN ({placeholders})");
            let mut stmt = conn.prepare(&sql).map_err(db_err)?;
            let args: Vec<i64> = chunk.iter().map(|&id| id as i64).collect();
            let param_refs = params_from(&args);
            let mut rows = stmt.query(param_refs.as_slice()).map_err(db_err)?;
            while let Some(row) = rows.next().map_err(db_err)? {
                let id = row.get::<_, i64>(0).map_err(db_err)?.max(0) as u64;
                let blob: Vec<u8> = row.get(1).map_err(db_err)?;
                out.insert(id, unpack_frames(&blob));
            }
        }
        Ok(out)
    }

    fn load_frames(&self, conn: &Connection, ids: &[u32]) -> Result<HashMap<u32, Frame>> {
        let mut out = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(4000) {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let sql = format!(
                "SELECT frame_id, module, symbol, file, line FROM frames \
                 WHERE frame_id IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql).map_err(db_err)?;
            let args: Vec<i64> = chunk.iter().map(|&id| id as i64).collect();
            let param_refs = params_from(&args);
            let mut rows = stmt.query(param_refs.as_slice()).map_err(db_err)?;
            while let Some(row) = rows.next().map_err(db_err)? {
                let id = row.get::<_, i64>(0).map_err(db_err)?.max(0) as u32;
                out.insert(
                    id,
                    Frame {
                        module: row.get(1).map_err(db_err)?,
                        symbol: row.get(2).map_err(db_err)?,
                        file: row.get(3).map_err(db_err)?,
                        line: row.get::<_, i64>(4).map_err(db_err)?.max(0) as u32,
                        addr: 0,
                    },
                );
            }
        }
        Ok(out)
    }

    pub fn stats(&self) -> StoreStats {
        let fallback = StoreStats::default();
        let conn = self.conn.lock();
        let row = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(total_value), 0), COALESCE(SUM(bytes), 0), \
                    MIN(start_ns), MAX(end_ns) FROM batches",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0).unwrap_or(0),
                    row.get::<_, i64>(1).unwrap_or(0),
                    row.get::<_, i64>(2).unwrap_or(0),
                    row.get::<_, Option<i64>>(3).ok().flatten(),
                    row.get::<_, Option<i64>>(4).ok().flatten(),
                ))
            },
        );
        let (count, value, logical, earliest, latest) = match row {
            Ok(v) => v,
            Err(err) => {
                warn!(error = %err, "stats query failed");
                return fallback;
            }
        };
        StoreStats {
            segment_count: count.max(0) as usize,
            total_bytes: self.disk_bytes(&conn).unwrap_or(0),
            logical_bytes: logical.max(0) as u64,
            total_value: value.max(0) as u64,
            earliest_ns: earliest.map(|v| v.max(0) as u64),
            latest_ns: latest.map(|v| v.max(0) as u64),
        }
    }

    fn disk_bytes(&self, conn: &Connection) -> Result<u64> {
        let size: i64 = conn
            .query_row(
                "SELECT database_size FROM pragma_database_size() \
                 WHERE database_name = current_database()",
                [],
                |row| row.get(0),
            )
            .map_err(db_err)?;
        Ok(size.max(0) as u64)
    }

    pub fn series(&self, from_ns: u64, until_ns: u64, selector: &Selector) -> Vec<SeriesInfo> {
        self.series_inner(from_ns, until_ns, selector)
            .unwrap_or_default()
    }

    fn series_inner(
        &self,
        from_ns: u64,
        until_ns: u64,
        selector: &Selector,
    ) -> Result<Vec<SeriesInfo>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT service_name, profile_type, labels, SUM(total_value), COUNT(*), \
                        MIN(start_ns), MAX(end_ns) FROM batches \
                 WHERE end_ns > ? AND start_ns < ? GROUP BY 1, 2, 3",
            )
            .map_err(db_err)?;
        let mut rows = stmt
            .query(params![ts(from_ns), ts(until_ns)])
            .map_err(db_err)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(db_err)? {
            let service_name: String = row.get(0).map_err(db_err)?;
            let labels = decode_labels(&row.get::<_, String>(2).map_err(db_err)?);
            if !selector.matches_meta(&service_name, &labels) {
                continue;
            }
            out.push(SeriesInfo {
                profile_type: row.get(1).map_err(db_err)?,
                total_value: row.get::<_, i64>(3).map_err(db_err)?.max(0) as u64,
                segment_count: row.get::<_, i64>(4).map_err(db_err)?.max(0) as usize,
                start_ns: row.get::<_, i64>(5).map_err(db_err)?.max(0) as u64,
                end_ns: row.get::<_, i64>(6).map_err(db_err)?.max(0) as u64,
                service_name,
                labels,
            });
        }
        out.sort_unstable_by(|a, b| {
            b.total_value
                .cmp(&a.total_value)
                .then_with(|| a.service_name.cmp(&b.service_name))
        });
        Ok(out)
    }

    pub fn label_values(
        &self,
        label: &str,
        from_ns: u64,
        until_ns: u64,
        selector: &Selector,
    ) -> Vec<(String, u64)> {
        let mut map: HashMap<String, u64> = HashMap::new();
        for series in self.series(from_ns, until_ns, selector) {
            let value = if label == SERVICE_NAME_LABEL {
                Some(series.service_name.as_str())
            } else {
                series.labels.get(label).map(String::as_str)
            };
            if let Some(value) = value {
                *map.entry(value.to_string()).or_default() += series.total_value;
            }
        }
        let mut out: Vec<(String, u64)> = map.into_iter().collect();
        out.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out
    }

    pub fn label_names(&self, from_ns: u64, until_ns: u64, selector: &Selector) -> Vec<String> {
        let mut set: std::collections::BTreeSet<String> = Default::default();
        for series in self.series(from_ns, until_ns, selector) {
            set.insert(SERVICE_NAME_LABEL.to_string());
            for key in series.labels.keys() {
                set.insert(key.clone());
            }
        }
        set.into_iter().collect()
    }

    pub fn series_count(&self) -> usize {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM (SELECT DISTINCT service_name, profile_type, labels FROM batches)",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|count| count.max(0) as usize)
        .unwrap_or(0)
    }

    /// Is this exact series already known? Used to let an existing series keep
    /// writing once a tenant has hit its series limit.
    pub fn contains_series(&self, report: &Report) -> bool {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT 1 FROM batches WHERE service_name = ? AND profile_type = ? AND labels = ? LIMIT 1",
            params![
                report.service_name,
                report.profile_type.as_str(),
                encode_labels(&report.all_labels())
            ],
            |_| Ok(()),
        )
        .is_ok()
    }

    /// Drop data older than the retention window and, if configured, evict the
    /// oldest batches until the database fits in `max_disk_bytes`.
    pub fn prune(&self, now_ns: u64) -> Result<PruneStats> {
        let mut stats = PruneStats::default();
        let before = self.stats();

        {
            let conn = self.conn.lock();
            if let Some(retention) = self.cfg.retention_ns {
                let cutoff = now_ns.saturating_sub(retention);
                let removed: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM batches WHERE end_ns < ?",
                        params![ts(cutoff)],
                        |row| row.get(0),
                    )
                    .unwrap_or(0);
                conn.execute("DELETE FROM samples WHERE end_ns < ?", params![ts(cutoff)])
                    .map_err(db_err)?;
                conn.execute("DELETE FROM batches WHERE end_ns < ?", params![ts(cutoff)])
                    .map_err(db_err)?;
                stats.removed_segments += removed.max(0) as usize;
            }

            if let Some(limit) = self.cfg.max_disk_bytes {
                // Delete whole batches, oldest first, until the *retained*
                // data fits. DuckDB does not return freed row-group space to
                // the filesystem, so the file size is not an eviction signal:
                // using it would evict everything and still never reach the
                // limit. `SUM(batches.bytes)` is, and the loop is bounded.
                let mut guard = 0;
                loop {
                    let size: i64 = conn
                        .query_row("SELECT COALESCE(SUM(bytes), 0) FROM batches", [], |row| {
                            row.get(0)
                        })
                        .unwrap_or(0);
                    if size.max(0) as u64 <= limit || guard >= 10_000 {
                        break;
                    }
                    guard += 1;
                    let oldest: Option<i64> = conn
                        .query_row(
                            "SELECT batch_id FROM batches ORDER BY start_ns ASC, batch_id ASC LIMIT 1",
                            [],
                            |row| row.get(0),
                        )
                        .ok();
                    let Some(batch_id) = oldest else { break };
                    conn.execute("DELETE FROM samples WHERE batch_id = ?", params![batch_id])
                        .map_err(db_err)?;
                    conn.execute("DELETE FROM batches WHERE batch_id = ?", params![batch_id])
                        .map_err(db_err)?;
                    stats.removed_segments += 1;
                }
            }

            conn.execute_batch("CHECKPOINT").map_err(db_err)?;
        }

        let after = self.stats();
        // Physical file size stays at its high-water mark after DELETE, so
        // report the logical size actually released instead.
        stats.removed_bytes = before.logical_bytes.saturating_sub(after.logical_bytes);
        Ok(stats)
    }

    /// Delete every row (used by tests and `fsck --fix`).
    pub fn clear(&self) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute_batch(
            "DELETE FROM samples; DELETE FROM stacks; DELETE FROM frames; DELETE FROM batches;",
        )
        .map_err(db_err)?;
        conn.execute_batch("CHECKPOINT").map_err(db_err)?;
        self.frame_cache.lock().clear();
        let mut ids = self.ids.lock();
        ids.next_frame = 0;
        ids.next_stack = 0;
        ids.next_batch = 0;
        Ok(())
    }

    /// Check that the database is readable and internally consistent.
    ///
    /// A real integrity check of the storage is DuckDB's job (`CHECKPOINT`
    /// fails loudly on a damaged file), so this verifies the invariants only
    /// this schema can violate: samples pointing at a missing stack, and stack
    /// blobs pointing at a missing frame.
    pub fn fsck(&self) -> Vec<(PathBuf, Result<()>)> {
        vec![(self.path(), self.fsck_inner())]
    }

    fn fsck_inner(&self) -> Result<()> {
        let path = self.path();
        let conn = self.conn.lock();
        conn.execute_batch("CHECKPOINT").map_err(db_err)?;

        let orphan_stacks: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM samples s LEFT JOIN stacks k USING (stack_id) \
                 WHERE k.stack_id IS NULL",
                [],
                |row| row.get(0),
            )
            .map_err(db_err)?;
        if orphan_stacks > 0 {
            return Err(Error::corrupt(
                &path,
                format!("{orphan_stacks} samples reference a missing stack"),
            ));
        }

        let (broken, orphan_frames) = self.dangling_stacks(&conn)?;
        if orphan_frames > 0 || !broken.is_empty() {
            return Err(Error::corrupt(
                &path,
                format!(
                    "{orphan_frames} stack frames reference a missing frame id; \
                     {} stack blobs cannot be decoded",
                    broken.len()
                ),
            ));
        }
        Ok(())
    }

    /// Remove rows that break the schema's referential invariants.
    ///
    /// The file-backed store could fix corruption by deleting one bad segment
    /// file. A tenant is a *single* database here, so deleting the file would
    /// destroy the whole tenant — `--fix` has to be surgical instead. Returns
    /// the number of rows removed.
    pub fn repair(&self) -> Result<usize> {
        let conn = self.conn.lock();
        let (broken, _) = self.dangling_stacks(&conn)?;
        conn.execute_batch("BEGIN TRANSACTION").map_err(db_err)?;

        let deleted = (|| -> Result<usize> {
            let mut removed = 0usize;
            for stack_id in broken {
                let id = stack_id as i64;
                removed += conn
                    .execute("DELETE FROM samples WHERE stack_id = ?", params![id])
                    .map_err(db_err)?;
                removed += conn
                    .execute("DELETE FROM stacks WHERE stack_id = ?", params![id])
                    .map_err(db_err)?;
            }
            // Samples whose stack is missing entirely (the other half of the
            // invariant checked by `fsck`).
            removed += conn
                .execute(
                    "DELETE FROM samples WHERE NOT EXISTS \
                     (SELECT 1 FROM stacks k WHERE k.stack_id = samples.stack_id)",
                    [],
                )
                .map_err(db_err)?;
            Ok(removed)
        })();

        match deleted {
            Ok(removed) => {
                conn.execute_batch("COMMIT; CHECKPOINT").map_err(db_err)?;
                Ok(removed)
            }
            Err(err) => {
                // Never leave the shared connection inside an open transaction.
                let _ = conn.execute_batch("ROLLBACK");
                Err(err)
            }
        }
    }

    /// Stacks that cannot be decoded against the frame dictionary, and the
    /// number of dangling frame references found inside them.
    ///
    /// Blobs are opaque to SQL, so the check is done here: walk every stack and
    /// compare its ids against `frames`. Frames are compact enough to hold in
    /// memory; a store with millions of distinct frames makes this slow.
    fn dangling_stacks(&self, conn: &Connection) -> Result<(HashSet<u64>, usize)> {
        let known: HashSet<i64> = {
            let mut stmt = conn
                .prepare("SELECT frame_id FROM frames")
                .map_err(db_err)?;
            let mut rows = stmt.query([]).map_err(db_err)?;
            let mut set = HashSet::new();
            while let Some(row) = rows.next().map_err(db_err)? {
                set.insert(row.get::<_, i64>(0).map_err(db_err)?);
            }
            set
        };

        let mut stmt = conn
            .prepare("SELECT stack_id, frames FROM stacks")
            .map_err(db_err)?;
        let mut rows = stmt.query([]).map_err(db_err)?;
        let mut broken = HashSet::new();
        let mut orphans = 0usize;
        while let Some(row) = rows.next().map_err(db_err)? {
            let stack_id: i64 = row.get(0).map_err(db_err)?;
            let blob: Vec<u8> = row.get(1).map_err(db_err)?;
            // Frame ids are 4 bytes each; a truncated blob cannot be trusted.
            let mut bad = blob.len() % 4 != 0;
            for id in unpack_frames(&blob) {
                if !known.contains(&(id as i64)) {
                    orphans += 1;
                    bad = true;
                }
            }
            if bad {
                broken.insert(stack_id as u64);
            }
        }
        Ok((broken, orphans))
    }
}

fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS batches (
            batch_id BIGINT PRIMARY KEY,
            profile_type VARCHAR NOT NULL,
            service_name VARCHAR NOT NULL,
            labels VARCHAR NOT NULL,
            start_ns BIGINT NOT NULL,
            end_ns BIGINT NOT NULL,
            total_value BIGINT NOT NULL,
            sample_count BIGINT NOT NULL,
            bytes BIGINT NOT NULL,
            created_ns BIGINT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS samples (
            batch_id BIGINT NOT NULL,
            profile_type VARCHAR NOT NULL,
            service_name VARCHAR NOT NULL,
            labels VARCHAR NOT NULL,
            start_ns BIGINT NOT NULL,
            end_ns BIGINT NOT NULL,
            stack_id BIGINT NOT NULL,
            value BIGINT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS stacks (
            stack_id BIGINT PRIMARY KEY,
            frames BLOB NOT NULL
        );
        CREATE TABLE IF NOT EXISTS frames (
            frame_id INTEGER PRIMARY KEY,
            module VARCHAR NOT NULL,
            symbol VARCHAR NOT NULL,
            file VARCHAR NOT NULL,
            line INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS meta (key VARCHAR PRIMARY KEY, value VARCHAR NOT NULL);
        INSERT OR REPLACE INTO meta VALUES ('schema_version', '{SCHEMA_VERSION}');"
    ))
    .map_err(db_err)
}

/// Render the parts of a selector that SQL can express.
///
/// Only `service_name` has a column of its own; the other matchers would need
/// substring matching on the encoded label string, which cannot express
/// `label != value` correctly (the label may simply be absent). Those run in
/// Rust after the scan. `service_name` is included because a dashboard almost
/// always filters on it and it is the column with the most distinct values.
fn push_selector_sql(
    selector: &Selector,
    clauses: &mut Vec<String>,
    args: &mut Vec<Box<dyn duckdb::ToSql>>,
) {
    for matcher in &selector.matchers {
        if matcher.label != SERVICE_NAME_LABEL {
            continue;
        }
        match matcher.op {
            MatchOp::Eq => {
                clauses.push("service_name = ?".into());
                args.push(Box::new(matcher.value.clone()));
            }
            MatchOp::NotEq => {
                clauses.push("service_name <> ?".into());
                args.push(Box::new(matcher.value.clone()));
            }
            MatchOp::Glob => {
                clauses.push("service_name LIKE ?".into());
                args.push(Box::new(glob_to_like(&matcher.value)));
            }
            MatchOp::NotGlob => {
                clauses.push("service_name NOT LIKE ?".into());
                args.push(Box::new(glob_to_like(&matcher.value)));
            }
        }
    }
}

/// `*` -> `%`, `?` -> `_`, with LIKE metacharacters escaped.
fn glob_to_like(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    for ch in pattern.chars() {
        match ch {
            '*' => out.push('%'),
            '?' => out.push('_'),
            '%' => out.push_str("\\%"),
            '_' => out.push_str("\\_"),
            c => out.push(c),
        }
    }
    out
}

fn params_from(values: &[i64]) -> Vec<&dyn duckdb::ToSql> {
    values.iter().map(|v| v as &dyn duckdb::ToSql).collect()
}

fn db_err(err: duckdb::Error) -> Error {
    Error::Db(err.to_string())
}

/// Clamp a nanosecond timestamp into DuckDB's `BIGINT`.
///
/// A query written as `until = u64::MAX` ("everything") would otherwise
/// become `-1` and match nothing at all.
fn ts(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

/// Version of the linked DuckDB engine, as the library reports it.
///
/// Asked of the library rather than hard-coded, so `lidlesseye info` cannot
/// claim a version the binary does not actually link.
pub fn duckdb_version() -> String {
    // SAFETY: `duckdb_library_version` returns a pointer to a static,
    // NUL-terminated string owned by the library.
    let ptr = unsafe { duckdb::ffi::duckdb_library_version() };
    if ptr.is_null() {
        return "unknown".to_string();
    }
    unsafe { std::ffi::CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

/// Pack frame ids into a little-endian blob.
fn pack_frames(frames: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(frames.len() * 4);
    for id in frames {
        out.extend_from_slice(&id.to_le_bytes());
    }
    out
}

fn unpack_frames(blob: &[u8]) -> Vec<u32> {
    blob.as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| u32::from_le_bytes(*chunk))
        .collect()
}

/// Encode a label set as `k=v,k2=v2`, sorted by key.
///
/// Values may contain `,`, `=` or `\`, so those are escaped. Encoding is
/// deterministic, which matters because the encoded string is part of the
/// series identity used by `contains_series` and by tenant series limits.
pub fn encode_labels(labels: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for (i, (key, value)) in labels.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        escape_into(&mut out, key);
        out.push('=');
        escape_into(&mut out, value);
    }
    out
}

fn escape_into(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ',' => out.push_str("\\,"),
            '=' => out.push_str("\\="),
            c => out.push(c),
        }
    }
}

/// Inverse of [`encode_labels`].
pub fn decode_labels(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut key = String::new();
    let mut value = String::new();
    let mut in_value = false;
    let mut escaped = false;
    for ch in text.chars() {
        if escaped {
            if in_value {
                value.push(ch);
            } else {
                key.push(ch);
            }
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '=' if !in_value => in_value = true,
            ',' => {
                if !key.is_empty() {
                    out.insert(std::mem::take(&mut key), std::mem::take(&mut value));
                }
                in_value = false;
            }
            c => {
                if in_value {
                    value.push(c);
                } else {
                    key.push(c);
                }
            }
        }
    }
    if !key.is_empty() {
        out.insert(key, value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Frame, ProfileType, ReportBuilder};

    fn temp_store(name: &str) -> (Store, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("lidlesseye-store-{name}-{}", uuid::Uuid::new_v4()));
        let store = Store::open(StoreConfig::new(&dir)).unwrap();
        (store, dir)
    }

    fn report(service: &str, start: u64, pod: &str, frames: &[&str], value: u64) -> Report {
        let mut b =
            ReportBuilder::new(service, "cpu", start, start + 10_000_000_000).label("pod", pod);
        b.add_stack(
            frames.iter().map(|name| Frame::new("/bin/app", *name)),
            value,
        );
        b.finish()
    }

    #[test]
    fn labels_round_trip_through_the_encoding() {
        let mut labels = BTreeMap::new();
        labels.insert("pod".to_string(), "a,b=c".to_string());
        labels.insert("path".to_string(), "C://x//y".to_string());
        labels.insert("empty".to_string(), String::new());

        let encoded = encode_labels(&labels);
        // Sorting by key makes the encoding a stable series identity.
        assert!(encoded.starts_with("empty=,path="), "{encoded}");
        assert_eq!(decode_labels(&encoded), labels);

        assert_eq!(decode_labels(""), BTreeMap::new());
        assert_eq!(decode_labels("a=1").get("a").map(String::as_str), Some("1"));
    }

    #[test]
    fn append_and_select() {
        let (store, dir) = temp_store("append");
        store
            .append(&report("checkout", 1_000, "a", &["hot", "main"], 1_000))
            .unwrap();
        store
            .append(&report("checkout", 2_000, "b", &["hot", "main"], 1_000))
            .unwrap();
        store
            .append(&report("cart", 3_000, "c", &["hot", "main"], 1_000))
            .unwrap();

        let all = store.select(&ProfileType::cpu(), 0, 10_000, &Selector::default());
        assert_eq!(all.len(), 3);

        let only_checkout = store.select(
            &ProfileType::cpu(),
            0,
            10_000,
            &Selector::parse(r#"{service_name="checkout"}"#).unwrap(),
        );
        assert_eq!(only_checkout.len(), 2);

        let narrowed = store.select(
            &ProfileType::cpu(),
            0,
            10_000,
            &Selector::parse(r#"{service_name="checkout",pod="b"}"#).unwrap(),
        );
        assert_eq!(narrowed.len(), 1);

        // Reopening reads the same rows out of the database file.
        let store2 = Store::open(StoreConfig::new(&dir)).unwrap();
        assert_eq!(store2.stats().segment_count, 3);
        assert_eq!(store2.stats().total_value, 3_000);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn scan_applies_label_matchers_not_just_service_name() {
        // The SQL pass can only filter on `service_name`; the label matchers
        // have to run afterwards or a `pod` filter would silently return
        // every pod of the service.
        let (store, dir) = temp_store("scan-labels");
        store
            .append(&report("api", 1_000, "a", &["work", "main"], 100))
            .unwrap();
        store
            .append(&report("api", 2_000, "b", &["work", "main"], 200))
            .unwrap();

        let selector = Selector::parse(r#"{pod="a"}"#).unwrap();
        let scan = store
            .scan(&ScanQuery::new(&ProfileType::cpu(), 0, 10_000, &selector))
            .unwrap();
        let total: u64 = scan.rows.iter().map(|r| r.value).sum();
        assert_eq!(total, 100, "only pod a should contribute");

        // A glob must be honoured too, and `!=` must exclude.
        let glob = Selector::parse(r#"{pod=~"b*"}"#).unwrap();
        let scan = store
            .scan(&ScanQuery::new(&ProfileType::cpu(), 0, 10_000, &glob))
            .unwrap();
        assert_eq!(scan.rows.iter().map(|r| r.value).sum::<u64>(), 200);

        let not_a = Selector::parse(r#"{pod!="a"}"#).unwrap();
        let scan = store
            .scan(&ScanQuery::new(&ProfileType::cpu(), 0, 10_000, &not_a))
            .unwrap();
        assert_eq!(scan.rows.iter().map(|r| r.value).sum::<u64>(), 200);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn scan_reports_the_group_label() {
        let (store, dir) = temp_store("scan-group");
        store
            .append(&report("api", 1_000, "a", &["work", "main"], 100))
            .unwrap();
        store
            .append(&report("api", 2_000, "b", &["work", "main"], 200))
            .unwrap();

        let scan = store
            .scan(
                &ScanQuery::new(&ProfileType::cpu(), 0, 10_000, &Selector::default())
                    .with_group_by(Some("pod")),
            )
            .unwrap();
        let mut groups: Vec<_> = scan.rows.iter().filter_map(|r| r.group.clone()).collect();
        groups.sort();
        assert_eq!(groups, vec!["pod=a", "pod=b"]);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn identical_frames_are_stored_once() {
        let (store, dir) = temp_store("frames");
        // Two functions in the same binary, and the same functions again in a
        // second batch: the dictionary must hold three frames, not six.
        store
            .append(&report("api", 1_000, "a", &["hot", "main"], 10))
            .unwrap();
        store
            .append(&report("api", 2_000, "a", &["hot", "main"], 10))
            .unwrap();

        let conn = store.conn.lock();
        let frames: i64 = conn
            .query_row("SELECT COUNT(*) FROM frames", [], |row| row.get(0))
            .unwrap();
        let stacks: i64 = conn
            .query_row("SELECT COUNT(*) FROM stacks", [], |row| row.get(0))
            .unwrap();
        assert_eq!(frames, 2, "a frame must be stored once per database");
        // Stacks are deduplicated per batch, so two batches hold two stacks.
        assert_eq!(stacks, 2);
        drop(conn);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn retention_and_the_disk_cap_remove_old_batches() {
        let hour = 3_600_000_000_000u64;
        let (store, dir) = temp_store("retention");
        store
            .append(&report("api", 1_000, "a", &["hot"], 10))
            .unwrap();
        store
            .append(&report("api", hour, "a", &["hot"], 10))
            .unwrap();
        // DuckDB locks the file, so a second `Store` on the same path is an
        // error rather than a second reader: reopen only after dropping.
        drop(store);

        let mut cfg = StoreConfig::new(&dir);
        cfg.retention_ns = Some(hour + hour / 2);
        let store = Store::open(cfg).unwrap();
        let stats = store.prune(2 * hour).unwrap();
        assert_eq!(stats.removed_segments, 1);
        assert_eq!(store.stats().segment_count, 1);
        drop(store);

        // The cap is measured on retained data, not on the file, which DuckDB
        // does not shrink.
        let mut cfg = StoreConfig::new(&dir);
        cfg.max_disk_bytes = Some(1);
        let store = Store::open(cfg).unwrap();
        store
            .append(&report("web", 3 * hour, "b", &["hot"], 10))
            .unwrap();
        assert!(store.stats().segment_count >= 1);
        store.prune(3 * hour).unwrap();
        assert_eq!(
            store.stats().segment_count,
            0,
            "everything exceeds a 1 byte cap"
        );

        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn series_and_label_values_aggregate() {
        let (store, dir) = temp_store("series");
        store
            .append(&report("api", 1_000, "a", &["hot"], 100))
            .unwrap();
        store
            .append(&report("api", 2_000, "a", &["hot"], 100))
            .unwrap();
        store
            .append(&report("web", 3_000, "b", &["hot"], 100))
            .unwrap();

        let series = store.series(0, 10_000, &Selector::default());
        assert_eq!(series.len(), 2);
        assert_eq!(series[0].service_name, "api");
        assert_eq!(series[0].segment_count, 2);
        assert_eq!(series[0].total_value, 200);

        let pods = store.label_values("pod", 0, 10_000, &Selector::default());
        assert_eq!(pods.len(), 2);
        assert_eq!(pods[0].0, "a");

        let names = store.label_names(0, 10_000, &Selector::default());
        assert!(names.contains(&"pod".to_string()));
        assert!(names.contains(&SERVICE_NAME_LABEL.to_string()));

        assert_eq!(store.series_count(), 2);
        assert!(store.contains_series(&report("api", 5_000, "a", &["hot"], 1)));
        assert!(!store.contains_series(&report("nope", 5_000, "a", &["hot"], 1)));

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn glob_patterns_become_like_patterns() {
        assert_eq!(glob_to_like("checkout-*"), "checkout-%");
        assert_eq!(glob_to_like("a?c"), "a_c");
        // LIKE metacharacters must not leak through as wildcards.
        assert_eq!(glob_to_like("100%"), "100\\%");
        assert_eq!(glob_to_like("a_b"), "a\\_b");
    }

    #[test]
    fn timestamps_do_not_wrap_at_the_top_of_u64() {
        assert_eq!(ts(0), 0);
        assert_eq!(ts(1_700_000_000_000_000_000), 1_700_000_000_000_000_000);
        assert_eq!(ts(u64::MAX), i64::MAX);
    }

    #[test]
    fn clearing_the_store_empties_every_table() {
        let (store, dir) = temp_store("clear");
        store
            .append(&report("api", 1_000, "a", &["hot"], 10))
            .unwrap();
        store.clear().unwrap();
        let stats = store.stats();
        assert_eq!(stats.segment_count, 0);
        assert_eq!(stats.total_value, 0);
        assert!(
            store
                .scan(&ScanQuery::new(
                    &ProfileType::cpu(),
                    0,
                    10_000,
                    &Selector::default()
                ))
                .unwrap()
                .rows
                .is_empty()
        );
        assert!(store.fsck().iter().all(|(_, r)| r.is_ok()));

        std::fs::remove_dir_all(dir).unwrap();
    }

    /// `--fix` used to delete a corrupt segment file. With one database per
    /// tenant that would delete everything, so it must drop only the bad rows.
    #[test]
    fn repair_removes_dangling_rows_and_keeps_good_batches() {
        let (store, dir) = temp_store("repair");
        store
            .append(&report("api", 1_000, "a", &["hot"], 10))
            .unwrap();
        let before = store.stats().segment_count;

        {
            let conn = store.conn.lock();
            conn.execute(
                "INSERT INTO samples VALUES (999, 'cpu', 'api', '', 1000, 2000, 999999, 7)",
                [],
            )
            .unwrap();
            // A truncated blob: not a whole number of 4-byte frame ids.
            conn.execute(
                "INSERT INTO stacks VALUES (999998, ?)",
                params![vec![1u8, 2, 3]],
            )
            .unwrap();
        }

        assert!(
            store.fsck()[0].1.is_err(),
            "fsck must report the dangling sample"
        );
        let removed = store.repair().unwrap();
        assert!(removed >= 2, "expected at least two rows, removed {removed}");
        assert!(store.fsck().iter().all(|(_, r)| r.is_ok()));
        assert_eq!(store.stats().segment_count, before, "good batch survives");
        assert!(store.path().is_file(), "database file survives");

        std::fs::remove_dir_all(dir).unwrap();
    }
}
