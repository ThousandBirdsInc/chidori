//! Pluggable persistence for the durable run artifact.
//!
//! Everything Chidori persists per run — the ordered call-record journal,
//! the snapshot manifest and blob, the pending host operation, the
//! host-promise table, the signal inbox, branch stores — flows through one
//! [`RunStore`] handle. Backends:
//!
//!   * [`FsRunStore`] — the `.chidori/runs/<run_id>/` file layout the
//!     framework has always written. Records additionally land in an
//!     append-only `records.jsonl` so a single record append is O(1) instead
//!     of an O(history) rewrite of `checkpoint.json`.
//!   * [`SqliteRunStore`] — records + blobs in a shared SQLite database
//!     (`CHIDORI_RUN_STORE=sqlite`, path from `CHIDORI_RUN_DB`).
//!   * [`HttpRunStore`] — records + blobs relayed to a remote store speaking
//!     the small REST protocol in `integrations/cloudflare-durable-objects/`
//!     (`CHIDORI_RUN_STORE=https://...`). One Durable Object per run gives the
//!     journal cross-datacenter replication and point-in-time recovery. The
//!     self-hosted equivalent is `chidori cell-store` (`crate::cellstore`):
//!     one SQLite cell per run on your own nodes, bucket-replicated, with
//!     compare-and-swap single ownership — the celld model.
//!   * [`TeeRunStore`] — the composition the runtime actually uses when a
//!     durable backend is configured: the filesystem layout stays the primary
//!     (every existing read path keeps working), the durable backend receives
//!     a mirrored copy of every write. [`RunStoreFactory::hydrate`]
//!     materializes a run directory back out of the durable backend after
//!     machine loss.
//!
//! Write-error policy: persistence failures are surfaced to the caller as
//! `Result`s instead of being silently dropped. How hard to fail is the
//! caller's policy — `CHIDORI_DURABILITY=strict` makes the runtime poison the
//! run on a failed journal write; the default (`besteffort`) logs and
//! continues, preserving the pre-store behavior.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::runtime::call_log::CallRecord;

/// Append-only journal file (one JSON `CallRecord` per line). Written by
/// per-record appends; superseded/compacted by every full `write_call_log`.
pub const RECORDS_FILE: &str = "records.jsonl";
/// Full-log checkpoint artifact (pretty JSON array). The long-standing on-disk
/// name; kept as the compaction target and for external readers.
pub const CHECKPOINT_FILE: &str = "checkpoint.json";
/// Lease blob for single-writer ownership of a run (`docs/durable-storage.md`).
pub const LEASE_FILE: &str = "lease.json";

/// One persistence handle for a single run. Implementations must be safe to
/// call from any thread; the runtime holds the handle behind the context lock.
pub trait RunStore: Send + Sync + std::fmt::Debug {
    /// Append one just-recorded call to the journal. O(1) per call — must not
    /// rewrite prior records. Records are keyed by `seq`; appending a seq that
    /// already exists replaces it (resume paths re-record synthetic entries).
    fn append_record(&self, record: &CallRecord) -> Result<()>;

    /// Replace the whole journal with `records` (order-preserving). Called at
    /// compaction points — run start after a resume replay, pause, settle,
    /// branch merges, and any safepoint where the in-memory log holds records
    /// the appends didn't cover (`RuntimeContext::call_log_checkpoint_dirty`).
    /// Steady-state per-effect persistence is `append_record` alone.
    fn write_call_log(&self, records: &[CallRecord]) -> Result<()>;

    /// Load the journal: the last full checkpoint unioned with any appended
    /// tail records a crash may have stranded after it. `Ok(None)` when the
    /// run has no journal at all.
    fn load_call_log(&self) -> Result<Option<Vec<CallRecord>>>;

    /// Write a named auxiliary artifact (manifest, snapshot blob, pending
    /// operation, host promises, signal inbox, branch files, ...). Keys are
    /// the artifact's relative path in the run directory, e.g.
    /// `"signals/inbox.json"`, so the filesystem backend maps them 1:1 onto
    /// the established layout.
    fn put_blob(&self, key: &str, bytes: &[u8]) -> Result<()>;

    /// Read a named auxiliary artifact. `Ok(None)` when absent.
    fn get_blob(&self, key: &str) -> Result<Option<Vec<u8>>>;

    /// Whether `key` exists, without reading its bytes. The source-history
    /// object writer calls this per file on every recording point, so the
    /// filesystem backend answers with one `stat` instead of a full read; the
    /// default is correct for every backend, just not as cheap.
    fn has_blob(&self, key: &str) -> Result<bool> {
        Ok(self.get_blob(key)?.is_some())
    }

    /// Append one line to a line-oriented blob (the source-history commit
    /// log). O(1) on the filesystem backend — must-not-rewrite is the same
    /// contract as [`RunStore::append_record`]. The default is a
    /// read-modify-write for backends with no native append; `line` must not
    /// contain a newline (one is added).
    fn append_blob_line(&self, key: &str, line: &[u8]) -> Result<()> {
        let mut bytes = self.get_blob(key)?.unwrap_or_default();
        bytes.extend_from_slice(line);
        bytes.push(b'\n');
        self.put_blob(key, &bytes)
    }

    /// The real filesystem path behind `key`, for stores that ARE a local
    /// directory. This is the hook for content-addressed object sharing —
    /// hardlink dedupe and copy-on-write clones between history stores —
    /// which only makes sense for immutable, content-addressed blobs.
    /// `None` — the default — disables sharing: the correct answer for
    /// remote backends, and deliberately for [`TeeRunStore`] (a hardlink
    /// into the primary would bypass the mirror write, leaving the durable
    /// copy without the object).
    fn blob_os_path(&self, _key: &str) -> Option<PathBuf> {
        None
    }

    /// Remove a named auxiliary artifact. Removing an absent key is Ok.
    fn delete_blob(&self, key: &str) -> Result<()>;

    /// Keys of every stored blob (relative paths). Used by hydration.
    fn list_blobs(&self) -> Result<Vec<String>>;

    /// Compare-and-swap one blob: apply `new` (`None` = delete) only when the
    /// stored value is byte-identical to `expected` (`None` = the key must be
    /// absent). `Ok(false)` means the precondition failed — another writer got
    /// there first — and is a normal outcome, not an error.
    ///
    /// Callers must pass bytes they actually read from this store rather than
    /// a re-serialization, so the comparison never turns on formatting.
    ///
    /// The default implementation is a read-compare-write and is **not**
    /// atomic: two callers can observe the same prior value and both write.
    /// That is the historical advisory behavior, correct for backends with no
    /// serialization point (`fs`, object stores). Backends that have one
    /// override this: [`SqliteRunStore`] wraps it in a transaction,
    /// [`HttpRunStore`] hands the precondition to the server as HTTP
    /// conditional headers. See `docs/durable-storage.md` §Leases.
    fn compare_and_swap_blob(
        &self,
        key: &str,
        expected: Option<&[u8]>,
        new: Option<&[u8]>,
    ) -> Result<bool> {
        if self.get_blob(key)?.as_deref() != expected {
            return Ok(false);
        }
        match new {
            Some(bytes) => self.put_blob(key, bytes)?,
            None => self.delete_blob(key)?,
        }
        Ok(true)
    }

    /// The store that coordination state (the run lease) must address.
    ///
    /// `None` — the default — means "this store". [`TeeRunStore`] overrides it
    /// with its durable secondary: a lease is *fleet* state, and the tee's
    /// local-primary-first reads would otherwise give every machine its own
    /// private lease, which is no lease at all.
    fn coordination_target(&self) -> Option<&dyn RunStore> {
        None
    }

    /// Durability barrier: every prior write on this handle is durable when
    /// this returns Ok. The runtime calls it before a run settles or pauses —
    /// the output-gate point — so backends may buffer between flushes.
    fn flush(&self) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Filesystem backend
// ---------------------------------------------------------------------------

/// The `.chidori/runs/<run_id>/` layout, unchanged, plus the append-only
/// `records.jsonl`. `fsync_writes` (set by `CHIDORI_DURABILITY=strict`) makes
/// journal writes call `sync_data` before returning so an acknowledged write
/// survives power loss, not just process death.
#[derive(Debug)]
pub struct FsRunStore {
    run_dir: PathBuf,
    fsync_writes: bool,
}

impl FsRunStore {
    pub fn new(run_dir: impl Into<PathBuf>) -> Self {
        Self {
            run_dir: run_dir.into(),
            fsync_writes: latching_durability(),
        }
    }

    #[allow(dead_code)] // Not yet wired into a call path; staged API.
    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    fn blob_path(&self, key: &str) -> Result<PathBuf> {
        // Keys are relative artifact paths; refuse traversal outside the run
        // directory so a hostile key cannot write elsewhere.
        let rel = Path::new(key);
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            anyhow::bail!("invalid run-store blob key `{key}`");
        }
        Ok(self.run_dir.join(rel))
    }

    fn write_file(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut file =
            std::fs::File::create(path).with_context(|| format!("writing {}", path.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("writing {}", path.display()))?;
        if self.fsync_writes {
            file.sync_data()
                .with_context(|| format!("syncing {}", path.display()))?;
        }
        Ok(())
    }
}

impl RunStore for FsRunStore {
    fn append_record(&self, record: &CallRecord) -> Result<()> {
        std::fs::create_dir_all(&self.run_dir)
            .with_context(|| format!("creating {}", self.run_dir.display()))?;
        let path = self.run_dir.join(RECORDS_FILE);
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("appending {}", path.display()))?;
        file.write_all(&line)
            .with_context(|| format!("appending {}", path.display()))?;
        if self.fsync_writes {
            file.sync_data()
                .with_context(|| format!("syncing {}", path.display()))?;
        }
        Ok(())
    }

    fn write_call_log(&self, records: &[CallRecord]) -> Result<()> {
        self.write_file(
            &self.run_dir.join(CHECKPOINT_FILE),
            &serde_json::to_vec_pretty(records)?,
        )?;
        // Compact the incremental artifact to match, so the two stay
        // consistent and the loader's union is exact.
        let mut lines = Vec::new();
        for record in records {
            lines.extend(serde_json::to_vec(record)?);
            lines.push(b'\n');
        }
        self.write_file(&self.run_dir.join(RECORDS_FILE), &lines)
    }

    fn load_call_log(&self) -> Result<Option<Vec<CallRecord>>> {
        let checkpoint: Option<Vec<CallRecord>> =
            match std::fs::read(self.run_dir.join(CHECKPOINT_FILE)) {
                Ok(bytes) => Some(serde_json::from_slice(&bytes).with_context(|| {
                    format!("parsing {}", self.run_dir.join(CHECKPOINT_FILE).display())
                })?),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!("reading {}", self.run_dir.join(CHECKPOINT_FILE).display())
                    })
                }
            };
        let tail: Vec<CallRecord> = match std::fs::read_to_string(self.run_dir.join(RECORDS_FILE)) {
            Ok(text) => text
                .lines()
                .filter(|line| !line.trim().is_empty())
                // A crash can truncate the final line mid-write; drop it and
                // keep every complete record before it.
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("reading {}", self.run_dir.join(RECORDS_FILE).display())
                })
            }
        };
        Ok(union_checkpoint_and_tail(checkpoint, tail))
    }

    fn put_blob(&self, key: &str, bytes: &[u8]) -> Result<()> {
        self.write_file(&self.blob_path(key)?, bytes)
    }

    fn get_blob(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match std::fs::read(self.blob_path(key)?) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).with_context(|| format!("reading blob {key}")),
        }
    }

    fn has_blob(&self, key: &str) -> Result<bool> {
        Ok(self.blob_path(key)?.is_file())
    }

    fn append_blob_line(&self, key: &str, line: &[u8]) -> Result<()> {
        let path = self.blob_path(key)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("appending {}", path.display()))?;
        file.write_all(line)
            .with_context(|| format!("appending {}", path.display()))?;
        file.write_all(b"\n")
            .with_context(|| format!("appending {}", path.display()))?;
        if self.fsync_writes {
            file.sync_data()
                .with_context(|| format!("syncing {}", path.display()))?;
        }
        Ok(())
    }

    fn blob_os_path(&self, key: &str) -> Option<PathBuf> {
        self.blob_path(key).ok()
    }

    fn delete_blob(&self, key: &str) -> Result<()> {
        match std::fs::remove_file(self.blob_path(key)?) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).with_context(|| format!("removing blob {key}")),
        }
    }

    fn list_blobs(&self) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        let mut stack = vec![self.run_dir.clone()];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err).with_context(|| format!("listing {}", dir.display())),
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(rel) = path.strip_prefix(&self.run_dir) {
                    let key = rel.to_string_lossy().replace('\\', "/");
                    if key != RECORDS_FILE && key != CHECKPOINT_FILE {
                        keys.push(key);
                    }
                }
            }
        }
        keys.sort();
        Ok(keys)
    }

    fn flush(&self) -> Result<()> {
        Ok(())
    }
}

/// Union a full checkpoint with the appended tail: checkpoint order wins;
/// tail records whose seq the checkpoint doesn't know (writes stranded after
/// the last safepoint by a crash) are appended in seq order.
pub(crate) fn union_checkpoint_and_tail(
    checkpoint: Option<Vec<CallRecord>>,
    tail: Vec<CallRecord>,
) -> Option<Vec<CallRecord>> {
    match checkpoint {
        Some(mut records) => {
            let known: BTreeSet<u64> = records.iter().map(|r| r.seq).collect();
            let mut extra: Vec<CallRecord> = tail
                .into_iter()
                .filter(|r| !known.contains(&r.seq))
                .collect();
            extra.sort_by_key(|r| r.seq);
            records.extend(dedup_keep_last(extra));
            Some(records)
        }
        None if tail.is_empty() => None,
        None => {
            let mut records = tail;
            records.sort_by_key(|r| r.seq);
            Some(dedup_keep_last(records))
        }
    }
}

/// Collapse repeated seqs to the LAST occurrence — a re-appended seq (a
/// synthetic resume record) replaces the earlier one. Input is seq-sorted with
/// stable order, so equal seqs are adjacent in append order.
fn dedup_keep_last(records: Vec<CallRecord>) -> Vec<CallRecord> {
    let mut deduped: Vec<CallRecord> = Vec::with_capacity(records.len());
    for record in records {
        if deduped.last().map(|r| r.seq) == Some(record.seq) {
            *deduped.last_mut().unwrap() = record;
        } else {
            deduped.push(record);
        }
    }
    deduped
}

// ---------------------------------------------------------------------------
// SQLite backend
// ---------------------------------------------------------------------------

/// Shared SQLite database holding every run's journal and blobs. One
/// connection (WAL mode) shared by all per-run handles; per-run tables keyed
/// by `run_id`. Unlike the session store's single-JSON-blob shortcut, records
/// are one row each, so an append writes O(1) bytes.
#[derive(Debug)]
pub struct SqliteRunStoreShared {
    conn: Mutex<rusqlite::Connection>,
}

impl SqliteRunStoreShared {
    pub fn open(path: &Path) -> Result<Arc<Self>> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        let conn = rusqlite::Connection::open(path)
            .with_context(|| format!("opening run store sqlite at {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL").ok();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS run_records (
                 run_id TEXT NOT NULL,
                 seq INTEGER NOT NULL,
                 pos INTEGER NOT NULL,
                 data TEXT NOT NULL,
                 PRIMARY KEY (run_id, seq)
             );
             -- append_record derives each new pos from MAX(pos) for the run;
             -- without this index that is a full scan of the run's rows, so
             -- appending the K-th record cost O(K) — against the trait's O(1)
             -- contract. With it, MAX(pos) is a B-tree seek.
             CREATE INDEX IF NOT EXISTS idx_run_records_pos
                 ON run_records(run_id, pos);
             CREATE TABLE IF NOT EXISTS run_blobs (
                 run_id TEXT NOT NULL,
                 key TEXT NOT NULL,
                 data BLOB NOT NULL,
                 PRIMARY KEY (run_id, key)
             );
             CREATE TABLE IF NOT EXISTS run_registry (
                 name TEXT PRIMARY KEY,
                 run_id TEXT NOT NULL,
                 data TEXT NOT NULL
             );",
        )?;
        Ok(Arc::new(Self {
            conn: Mutex::new(conn),
        }))
    }

    #[allow(dead_code)] // Not yet wired into a call path; staged API.
    fn list_runs(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT run_id FROM run_records
             UNION SELECT DISTINCT run_id FROM run_blobs",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        out.sort();
        Ok(out)
    }
}

#[derive(Debug)]
pub struct SqliteRunStore {
    shared: Arc<SqliteRunStoreShared>,
    run_id: String,
    /// Next `pos` ordinal for this handle's appends. Seeded from `MAX(pos)+1`
    /// on first use and tracked in memory afterwards: `pos` has no index, so
    /// the old per-append `MAX(pos)` subquery scanned every one of the run's
    /// rows — O(history) per append, O(history²) per run. Lock order is
    /// always `conn` before `next_pos`.
    next_pos: Mutex<Option<i64>>,
}

impl SqliteRunStore {
    pub fn new(shared: Arc<SqliteRunStoreShared>, run_id: impl Into<String>) -> Self {
        Self {
            shared,
            run_id: run_id.into(),
            next_pos: Mutex::new(None),
        }
    }
}

impl RunStore for SqliteRunStore {
    fn append_record(&self, record: &CallRecord) -> Result<()> {
        let data = serde_json::to_string(record)?;
        let conn = self.shared.conn.lock().unwrap();
        let mut next_pos = self.next_pos.lock().unwrap();
        let pos = match *next_pos {
            Some(pos) => pos,
            None => {
                let mut stmt = conn.prepare_cached(
                    "SELECT COALESCE(MAX(pos), 0) + 1 FROM run_records WHERE run_id = ?1",
                )?;
                stmt.query_row(rusqlite::params![self.run_id], |row| row.get(0))?
            }
        };
        // Re-appending an existing seq (a synthetic resume record) keeps its
        // original `pos` via the conflict clause; the skipped ordinal is a
        // harmless gap — loads order by (pos, seq).
        let mut stmt = conn.prepare_cached(
            "INSERT INTO run_records (run_id, seq, pos, data) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(run_id, seq) DO UPDATE SET data = excluded.data",
        )?;
        stmt.execute(rusqlite::params![self.run_id, record.seq as i64, pos, data])?;
        *next_pos = Some(pos + 1);
        Ok(())
    }

    fn write_call_log(&self, records: &[CallRecord]) -> Result<()> {
        let mut conn = self.shared.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM run_records WHERE run_id = ?1",
            rusqlite::params![self.run_id],
        )?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO run_records (run_id, seq, pos, data) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(run_id, seq) DO UPDATE SET
                     pos = excluded.pos, data = excluded.data",
            )?;
            for (pos, record) in records.iter().enumerate() {
                stmt.execute(rusqlite::params![
                    self.run_id,
                    record.seq as i64,
                    pos as i64,
                    serde_json::to_string(record)?
                ])?;
            }
        }
        tx.commit()?;
        // The rewrite renumbered `pos` to 0..len; continue appends from there.
        *self.next_pos.lock().unwrap() = Some(records.len() as i64);
        Ok(())
    }

    fn load_call_log(&self) -> Result<Option<Vec<CallRecord>>> {
        let conn = self.shared.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT data FROM run_records WHERE run_id = ?1 ORDER BY pos, seq")?;
        let rows = stmt.query_map(rusqlite::params![self.run_id], |row| {
            row.get::<_, String>(0)
        })?;
        let mut records = Vec::new();
        for row in rows {
            records.push(serde_json::from_str(&row?)?);
        }
        Ok(if records.is_empty() {
            None
        } else {
            Some(records)
        })
    }

    fn put_blob(&self, key: &str, bytes: &[u8]) -> Result<()> {
        let conn = self.shared.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO run_blobs (run_id, key, data) VALUES (?1, ?2, ?3)
             ON CONFLICT(run_id, key) DO UPDATE SET data = excluded.data",
            rusqlite::params![self.run_id, key, bytes],
        )?;
        Ok(())
    }

    fn get_blob(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let conn = self.shared.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT data FROM run_blobs WHERE run_id = ?1 AND key = ?2")?;
        let mut rows = stmt.query(rusqlite::params![self.run_id, key])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get::<_, Vec<u8>>(0)?)),
            None => Ok(None),
        }
    }

    fn delete_blob(&self, key: &str) -> Result<()> {
        let conn = self.shared.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM run_blobs WHERE run_id = ?1 AND key = ?2",
            rusqlite::params![self.run_id, key],
        )?;
        Ok(())
    }

    fn list_blobs(&self) -> Result<Vec<String>> {
        let conn = self.shared.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT key FROM run_blobs WHERE run_id = ?1 ORDER BY key")?;
        let rows = stmt.query_map(rusqlite::params![self.run_id], |row| {
            row.get::<_, String>(0)
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Atomic: the read and the conditional write share one `IMMEDIATE`
    /// transaction, which takes the database's write lock up front — so two
    /// processes sharing the file serialize here instead of interleaving a
    /// read-compare-write. (A deferred transaction would let both read before
    /// either wrote, and the loser would fail on upgrade rather than cleanly
    /// reporting a lost swap.)
    fn compare_and_swap_blob(
        &self,
        key: &str,
        expected: Option<&[u8]>,
        new: Option<&[u8]>,
    ) -> Result<bool> {
        let mut conn = self.shared.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        {
            let mut stmt =
                tx.prepare_cached("SELECT data FROM run_blobs WHERE run_id = ?1 AND key = ?2")?;
            let mut rows = stmt.query(rusqlite::params![self.run_id, key])?;
            let current: Option<Vec<u8>> = match rows.next()? {
                Some(row) => Some(row.get(0)?),
                None => None,
            };
            if current.as_deref() != expected {
                return Ok(false);
            }
        }
        match new {
            Some(bytes) => tx.execute(
                "INSERT INTO run_blobs (run_id, key, data) VALUES (?1, ?2, ?3)
                 ON CONFLICT(run_id, key) DO UPDATE SET data = excluded.data",
                rusqlite::params![self.run_id, key, bytes],
            )?,
            None => tx.execute(
                "DELETE FROM run_blobs WHERE run_id = ?1 AND key = ?2",
                rusqlite::params![self.run_id, key],
            )?,
        };
        tx.commit()?;
        Ok(true)
    }

    fn flush(&self) -> Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// HTTP backend (remote relay — Cloudflare Durable Object shim or compatible)
// ---------------------------------------------------------------------------

/// Remote run store speaking the REST protocol served by
/// `integrations/cloudflare-durable-objects/` (one Durable Object per run):
///
///   GET/PUT/DELETE {base}/runs/{run_id}/blobs/{key}
///   GET  {base}/runs/{run_id}/records          → JSON array of CallRecords
///   POST {base}/runs/{run_id}/records          → append one record
///   PUT  {base}/runs/{run_id}/records          → replace the journal
///   GET  {base}/runs                            → JSON array of run ids
///
/// Requests run on a dedicated plain thread owning a blocking HTTP client, so
/// store calls are safe from both sync and async-runtime-owned threads.
/// `CHIDORI_RUN_STORE_TOKEN` (optional) is sent as a bearer token.
#[derive(Debug)]
pub struct HttpRunStore {
    relay: Arc<HttpRelay>,
    run_id: String,
    /// Pipeline appends through the relay instead of blocking a network
    /// round-trip per record. Off under `CHIDORI_DURABILITY=strict`, which
    /// wants every append acknowledged before the next effect runs; in the
    /// default besteffort mode the `flush()` barrier at pause/settle is the
    /// durability gate (`RunStore::flush` contract).
    pipelined: bool,
}

impl HttpRunStore {
    pub fn new(relay: Arc<HttpRelay>, run_id: impl Into<String>) -> Self {
        Self {
            relay,
            run_id: run_id.into(),
            pipelined: !strict_durability(),
        }
    }

    fn records_url(&self) -> String {
        format!("{}/runs/{}/records", self.relay.base_url, self.run_id)
    }

    fn blob_url(&self, key: &str) -> String {
        format!(
            "{}/runs/{}/blobs/{}",
            self.relay.base_url,
            self.run_id,
            urlencode_path(key)
        )
    }

    /// Whether a 409 on this blob may be re-aimed at the owner the server
    /// names. Journal and artifact traffic may: the owner's store is the one
    /// that should have served it. Lease traffic may NOT — a 409 about
    /// `lease.json` IS the fencing verdict ([`acquire_lease`]), and following
    /// it would let a node that lost its cell keep arbitrating ownership
    /// through the node that took over.
    /// Matched on the key's last segment, so a scoped store's branch lease
    /// (`branches/…/lease.json`) is exempt too.
    fn follows_owner(key: &str) -> bool {
        key.rsplit('/').next() != Some(LEASE_FILE)
    }
}

/// The store refused an operation because this node no longer owns the run —
/// another node took its cell over (`chidori cell-store` reports this as HTTP
/// 409 naming the live owner). Distinct from an ordinary write failure: the
/// correct response is to stand down, not to retry or to poison the run.
/// Recognize it with [`fenced_owner`].
#[derive(Debug, Clone)]
pub struct FencedError {
    pub owner: String,
    /// The owner's advertised address, when the 409 named one (`--advertise`
    /// on the owning cell-store node). `None` against a server that does not
    /// send the field — an older node, or one with no advertised URL — which
    /// is why the relay only follows an owner it was actually given.
    pub owner_url: Option<String>,
    pub lease_expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl std::fmt::Display for FencedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "run is owned by `{}`", self.owner)?;
        if let Some(url) = &self.owner_url {
            write!(f, " at {url}")?;
        }
        if let Some(expires) = self.lease_expires_at {
            write!(f, " (lease expires {expires})")?;
        }
        Ok(())
    }
}

impl std::error::Error for FencedError {}

/// The fenced-store error anywhere in `err`'s cause chain, if any.
pub fn fenced_owner(err: &anyhow::Error) -> Option<&FencedError> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<FencedError>())
}

/// Build the error for a non-success relay status. A 409 always becomes a
/// [`FencedError`] — even when the body doesn't parse, since standing down
/// must not depend on being able to name the new owner.
fn relay_error(what: &str, status: u16, body: &[u8]) -> anyhow::Error {
    if status == 409 {
        let parsed: Option<serde_json::Value> = serde_json::from_slice(body).ok();
        return anyhow::Error::new(FencedError {
            owner: parsed
                .as_ref()
                .and_then(|v| v.get("owner"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
            owner_url: parsed
                .as_ref()
                .and_then(|v| v.get("owner_url"))
                .and_then(|v| v.as_str())
                .map(str::to_string),
            lease_expires_at: parsed
                .as_ref()
                .and_then(|v| v.get("lease_expires_at"))
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok()),
        });
    }
    anyhow::anyhow!(
        "run store relay {what} failed: HTTP {status} {}",
        String::from_utf8_lossy(body)
    )
}

/// The entity tag a conditional request compares against: the SHA-256 of the
/// blob's bytes, quoted per HTTP. Content-derived rather than server-assigned
/// so any implementation of the protocol computes the same value.
fn blob_etag(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    format!("\"{}\"", hex::encode(sha2::Sha256::digest(bytes)))
}

/// Percent-encode a blob key for use as a single path segment, keeping `/`
/// so nested keys stay readable server-side.
fn urlencode_path(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for byte in key.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'/' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

struct HttpRelayRequest {
    method: &'static str,
    url: String,
    body: Option<Vec<u8>>,
    /// Content type sent with a body; JSON for records/registry payloads,
    /// octet-stream for raw blobs.
    content_type: &'static str,
    /// Extra headers, verbatim — the S3 backend's SigV4 signature headers.
    headers: Vec<(String, String)>,
    /// Whether a 409 naming a reachable owner may be re-aimed at that owner
    /// (see [`follow_owner_url`]). Set for ordinary run-store traffic; clear
    /// for lease arbitration and for backends that sign their own absolute
    /// URLs.
    follow_owner: bool,
    reply: RelayReply,
}

enum RelayReply {
    /// Caller blocks on the outcome.
    Sync(std::sync::mpsc::Sender<Result<(u16, Vec<u8>)>>),
    /// Pipelined: the outcome lands in the relay's shared async state; the
    /// caller surfaces failures at the next [`HttpRelay::barrier`] (the
    /// `RunStore::flush` durability gate). `describe` labels the request in
    /// the collected error; `tolerate_not_found` treats a 404 as success
    /// (deleting an absent blob is Ok, matching the sync paths).
    Async {
        describe: String,
        tolerate_not_found: bool,
    },
}

/// Issue one relay request on the worker's blocking client.
fn send_relay_request(
    client: &reqwest::blocking::Client,
    token: Option<&str>,
    method: &'static str,
    url: &str,
    body: Option<Vec<u8>>,
    content_type: &'static str,
    headers: &[(String, String)],
) -> Result<(u16, Vec<u8>)> {
    let mut builder = match method {
        "GET" => client.get(url),
        "PUT" => client.put(url),
        "POST" => client.post(url),
        "DELETE" => client.delete(url),
        other => anyhow::bail!("unsupported relay method {other}"),
    };
    if let Some(token) = token {
        builder = builder.bearer_auth(token);
    }
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    if let Some(body) = body {
        builder = builder.header("content-type", content_type).body(body);
    }
    let response = builder.send()?;
    let status = response.status().as_u16();
    let bytes = response.bytes()?;
    Ok((status, bytes.to_vec()))
}

/// Where to retry a 409 that names the owner's address: the same path, on the
/// owner. `None` — meaning surface the fence — unless the body carries an
/// `owner_url` that is an http(s) address other than this relay's own base,
/// and the request went to that base in the first place (a backend signing
/// absolute URLs of its own is never re-aimed).
fn follow_owner_url(base_url: &str, url: &str, body: &[u8]) -> Option<String> {
    if base_url.is_empty() {
        return None;
    }
    let path = url.strip_prefix(base_url)?;
    let parsed: serde_json::Value = serde_json::from_slice(body).ok()?;
    let owner = parsed
        .get("owner_url")?
        .as_str()?
        .trim_end_matches('/')
        .to_string();
    if owner == base_url || !(owner.starts_with("http://") || owner.starts_with("https://")) {
        return None;
    }
    Some(format!("{owner}{path}"))
}

/// Bookkeeping for pipelined relay requests: how many are still in the
/// relay's queue/network, and the failures collected so far.
#[derive(Default)]
struct AsyncRelayState {
    in_flight: usize,
    errors: Vec<String>,
}

/// Backpressure bound on pipelined requests: an agent producing records
/// faster than the mirror's network drains them blocks here instead of
/// growing the relay queue without limit.
const RELAY_MAX_IN_FLIGHT: usize = 128;

/// The dedicated request thread + its channel. Owning the blocking client on
/// a plain thread sidesteps every "blocking client inside an async runtime"
/// hazard without giving the store an async signature.
pub struct HttpRelay {
    base_url: String,
    sender: std::sync::mpsc::Sender<HttpRelayRequest>,
    async_state: Arc<(Mutex<AsyncRelayState>, std::sync::Condvar)>,
}

impl std::fmt::Debug for HttpRelay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpRelay")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl HttpRelay {
    /// A relay with no base URL or bearer token — the S3 backend signs its
    /// own requests and always passes absolute URLs through `request_full`.
    pub(crate) fn new_headless() -> Arc<Self> {
        Self::new(String::new(), None)
    }

    pub fn new(base_url: impl Into<String>, token: Option<String>) -> Arc<Self> {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let (sender, receiver) = std::sync::mpsc::channel::<HttpRelayRequest>();
        let async_state: Arc<(Mutex<AsyncRelayState>, std::sync::Condvar)> = Arc::default();
        let worker_async_state = async_state.clone();
        let worker_base_url = base_url.clone();
        std::thread::Builder::new()
            .name("chidori-run-store-relay".to_string())
            .spawn(move || {
                let client = reqwest::blocking::Client::builder()
                    .timeout(std::time::Duration::from_secs(30))
                    .build();
                for request in receiver {
                    let result = match &client {
                        Ok(client) => {
                            // The retry needs the body again, so it is held
                            // back only for requests that may follow — the
                            // signed-URL backends keep moving theirs through
                            // without a copy.
                            let retry_body = if request.follow_owner {
                                request.body.clone()
                            } else {
                                None
                            };
                            let first = send_relay_request(
                                client,
                                token.as_deref(),
                                request.method,
                                &request.url,
                                request.body,
                                request.content_type,
                                &request.headers,
                            );
                            // Follow the owner, exactly one hop: a 409 that
                            // carries the owner's address is retried there
                            // with the same path, body and credentials. The
                            // second answer stands whatever it is — a 409
                            // from the owner is a genuine fence, and one hop
                            // cannot loop.
                            match first {
                                Ok((409, body)) if request.follow_owner => {
                                    match follow_owner_url(&worker_base_url, &request.url, &body) {
                                        Some(next) => send_relay_request(
                                            client,
                                            token.as_deref(),
                                            request.method,
                                            &next,
                                            retry_body,
                                            request.content_type,
                                            &request.headers,
                                        ),
                                        None => Ok((409, body)),
                                    }
                                }
                                other => other,
                            }
                        }
                        Err(err) => Err(anyhow::anyhow!("building relay http client: {err}")),
                    };
                    match request.reply {
                        RelayReply::Sync(tx) => {
                            let _ = tx.send(result);
                        }
                        RelayReply::Async {
                            describe,
                            tolerate_not_found,
                        } => {
                            let (lock, cvar) = &*worker_async_state;
                            let mut state = lock.lock().unwrap();
                            state.in_flight -= 1;
                            match result {
                                Ok((404, _)) if tolerate_not_found => {}
                                Ok((status, body)) if !(200..300).contains(&status) => {
                                    state.errors.push(format!(
                                        "{describe}: HTTP {status} {}",
                                        String::from_utf8_lossy(&body)
                                    ));
                                }
                                Err(err) => state.errors.push(format!("{describe}: {err}")),
                                Ok(_) => {}
                            }
                            cvar.notify_all();
                        }
                    }
                }
            })
            .expect("spawning run-store relay thread");
        Arc::new(Self {
            base_url,
            sender,
            async_state,
        })
    }

    fn request(
        &self,
        method: &'static str,
        url: String,
        body: Option<Vec<u8>>,
    ) -> Result<(u16, Vec<u8>)> {
        self.request_typed(method, url, body, "application/json")
    }

    fn request_typed(
        &self,
        method: &'static str,
        url: String,
        body: Option<Vec<u8>>,
        content_type: &'static str,
    ) -> Result<(u16, Vec<u8>)> {
        self.dispatch(method, url, body, content_type, Vec::new(), true)
    }

    /// Full-control request used by the S3 backend: caller-supplied headers
    /// (the SigV4 signature set) ride verbatim. Never follows an owner — these
    /// are absolute, individually signed URLs, not run-store paths.
    pub(crate) fn request_full(
        &self,
        method: &'static str,
        url: String,
        body: Option<Vec<u8>>,
        content_type: &'static str,
        headers: Vec<(String, String)>,
    ) -> Result<(u16, Vec<u8>)> {
        self.dispatch(method, url, body, content_type, headers, false)
    }

    /// Send one request on the relay thread and block for its outcome.
    /// `follow_owner` decides whether a 409 naming a reachable owner is
    /// retried once against that owner (see [`follow_owner_url`]).
    #[allow(clippy::too_many_arguments)]
    fn dispatch(
        &self,
        method: &'static str,
        url: String,
        body: Option<Vec<u8>>,
        content_type: &'static str,
        headers: Vec<(String, String)>,
        follow_owner: bool,
    ) -> Result<(u16, Vec<u8>)> {
        let (reply, receive) = std::sync::mpsc::channel();
        self.sender
            .send(HttpRelayRequest {
                method,
                url,
                body,
                content_type,
                headers,
                follow_owner,
                reply: RelayReply::Sync(reply),
            })
            .map_err(|_| anyhow::anyhow!("run-store relay thread is gone"))?;
        receive
            .recv()
            .map_err(|_| anyhow::anyhow!("run-store relay dropped the reply"))?
    }

    /// Pipelined request: enqueue and return immediately, without waiting for
    /// the network round-trip. The relay worker is a single FIFO thread, so
    /// ordering against later sync requests (checkpoint PUTs, loads) is
    /// preserved. Outcomes are collected in the relay's async state and
    /// surfaced by [`Self::barrier`]. Bounded by [`RELAY_MAX_IN_FLIGHT`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn request_async(
        &self,
        method: &'static str,
        url: String,
        body: Option<Vec<u8>>,
        content_type: &'static str,
        headers: Vec<(String, String)>,
        tolerate_not_found: bool,
        follow_owner: bool,
    ) -> Result<()> {
        {
            let (lock, cvar) = &*self.async_state;
            let mut state = lock.lock().unwrap();
            while state.in_flight >= RELAY_MAX_IN_FLIGHT {
                state = cvar.wait(state).unwrap();
            }
            state.in_flight += 1;
        }
        let describe = format!("{method} {url}");
        self.sender
            .send(HttpRelayRequest {
                method,
                url,
                body,
                content_type,
                headers,
                follow_owner,
                reply: RelayReply::Async {
                    describe,
                    tolerate_not_found,
                },
            })
            .map_err(|_| {
                let (lock, cvar) = &*self.async_state;
                lock.lock().unwrap().in_flight -= 1;
                cvar.notify_all();
                anyhow::anyhow!("run-store relay thread is gone")
            })
    }

    /// Durability barrier for pipelined requests: waits until every
    /// [`Self::request_async`] has completed, then surfaces any collected
    /// failures (draining them).
    pub(crate) fn barrier(&self) -> Result<()> {
        let (lock, cvar) = &*self.async_state;
        let mut state = lock.lock().unwrap();
        while state.in_flight > 0 {
            state = cvar.wait(state).unwrap();
        }
        if state.errors.is_empty() {
            Ok(())
        } else {
            let errors = std::mem::take(&mut state.errors);
            anyhow::bail!("run-store mirror writes failed: {}", errors.join("; "))
        }
    }

    fn expect_ok(&self, method: &'static str, url: String, body: Option<Vec<u8>>) -> Result<()> {
        let (status, bytes) = self.request(method, url.clone(), body)?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(relay_error(&format!("{method} {url}"), status, &bytes))
        }
    }

    #[allow(dead_code)] // Not yet wired into a call path; staged API.
    fn list_runs(&self) -> Result<Vec<String>> {
        let (status, bytes) = self.request("GET", format!("{}/runs", self.base_url), None)?;
        if !(200..300).contains(&status) {
            anyhow::bail!("run store relay GET /runs failed: HTTP {status}");
        }
        Ok(serde_json::from_slice(&bytes)?)
    }
}

impl RunStore for HttpRunStore {
    fn append_record(&self, record: &CallRecord) -> Result<()> {
        if self.pipelined {
            return self.relay.request_async(
                "POST",
                self.records_url(),
                Some(serde_json::to_vec(record)?),
                "application/json",
                Vec::new(),
                false,
                true,
            );
        }
        self.relay.expect_ok(
            "POST",
            self.records_url(),
            Some(serde_json::to_vec(record)?),
        )
    }

    fn write_call_log(&self, records: &[CallRecord]) -> Result<()> {
        self.relay.expect_ok(
            "PUT",
            self.records_url(),
            Some(serde_json::to_vec(records)?),
        )
    }

    fn load_call_log(&self) -> Result<Option<Vec<CallRecord>>> {
        let (status, bytes) = self.relay.request("GET", self.records_url(), None)?;
        match status {
            404 => Ok(None),
            s if (200..300).contains(&s) => {
                let records: Vec<CallRecord> = serde_json::from_slice(&bytes)?;
                Ok(if records.is_empty() {
                    None
                } else {
                    Some(records)
                })
            }
            s => anyhow::bail!("run store relay GET records failed: HTTP {s}"),
        }
    }

    fn put_blob(&self, key: &str, bytes: &[u8]) -> Result<()> {
        if self.pipelined {
            return self.relay.request_async(
                "PUT",
                self.blob_url(key),
                Some(bytes.to_vec()),
                "application/octet-stream",
                Vec::new(),
                false,
                Self::follows_owner(key),
            );
        }
        let (status, body) = self.relay.dispatch(
            "PUT",
            self.blob_url(key),
            Some(bytes.to_vec()),
            "application/octet-stream",
            Vec::new(),
            Self::follows_owner(key),
        )?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(relay_error(&format!("PUT blob {key}"), status, &body))
        }
    }

    fn get_blob(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let (status, bytes) = self.relay.dispatch(
            "GET",
            self.blob_url(key),
            None,
            "application/json",
            Vec::new(),
            Self::follows_owner(key),
        )?;
        match status {
            404 => Ok(None),
            s if (200..300).contains(&s) => Ok(Some(bytes)),
            s => Err(relay_error(&format!("GET blob {key}"), s, &bytes)),
        }
    }

    fn delete_blob(&self, key: &str) -> Result<()> {
        if self.pipelined {
            return self.relay.request_async(
                "DELETE",
                self.blob_url(key),
                None,
                "application/json",
                Vec::new(),
                true,
                Self::follows_owner(key),
            );
        }
        let (status, _) = self.relay.dispatch(
            "DELETE",
            self.blob_url(key),
            None,
            "application/json",
            Vec::new(),
            Self::follows_owner(key),
        )?;
        if status == 404 || (200..300).contains(&status) {
            Ok(())
        } else {
            anyhow::bail!("run store relay DELETE blob {key} failed: HTTP {status}")
        }
    }

    fn list_blobs(&self) -> Result<Vec<String>> {
        let (status, bytes) = self.relay.request(
            "GET",
            format!("{}/runs/{}/blobs", self.relay.base_url, self.run_id),
            None,
        )?;
        match status {
            404 => Ok(Vec::new()),
            s if (200..300).contains(&s) => Ok(serde_json::from_slice(&bytes)?),
            s => Err(relay_error("GET blobs", s, &bytes)),
        }
    }

    /// Atomic when the server is: the precondition rides as an HTTP
    /// conditional header and is evaluated server-side, inside whatever lock
    /// serializes that run (`chidori cell-store`: the cell's slot mutex; the
    /// Durable Object relay: the single instance per run id). A 412 is the
    /// precondition failing — a lost swap, not an error.
    ///
    /// Always synchronous, even in pipelined (besteffort) mode: the caller
    /// needs the verdict. The relay is a single FIFO thread, so appends
    /// enqueued earlier still reach the server first.
    fn compare_and_swap_blob(
        &self,
        key: &str,
        expected: Option<&[u8]>,
        new: Option<&[u8]>,
    ) -> Result<bool> {
        let precondition = match expected {
            Some(bytes) => ("if-match".to_string(), blob_etag(bytes)),
            None => ("if-none-match".to_string(), "*".to_string()),
        };
        let (method, body, content_type) = match new {
            Some(bytes) => ("PUT", Some(bytes.to_vec()), "application/octet-stream"),
            None => ("DELETE", None, "application/json"),
        };
        let (status, response) = self.relay.dispatch(
            method,
            self.blob_url(key),
            body,
            content_type,
            vec![precondition],
            Self::follows_owner(key),
        )?;
        match status {
            412 => Ok(false),
            // Deleting a key we required to be absent: already in the
            // requested state.
            404 if new.is_none() => Ok(true),
            s if (200..300).contains(&s) => Ok(true),
            s => Err(relay_error(&format!("CAS blob {key}"), s, &response)),
        }
    }

    fn flush(&self) -> Result<()> {
        // Surface pipelined-append failures at the durability gate. Only the
        // besteffort mode pipelines (strict appends stay synchronous), and
        // besteffort's contract is log-and-continue — matching what the
        // per-append error handling did before pipelining.
        if let Err(err) = self.relay.barrier() {
            tracing::warn!(run_id = %self.run_id, error = %err, "durable mirror pipelined writes failed");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tee composition
// ---------------------------------------------------------------------------

/// Filesystem primary + durable secondary. Reads come from the primary (every
/// existing consumer keeps its layout); writes go to both. A secondary write
/// failure is returned to the caller — the runtime's durability policy
/// decides whether that poisons the run (`strict`) or logs (`besteffort`).
#[derive(Debug)]
pub struct TeeRunStore {
    primary: FsRunStore,
    secondary: Arc<dyn RunStore>,
}

impl TeeRunStore {
    pub fn new(primary: FsRunStore, secondary: Arc<dyn RunStore>) -> Self {
        Self { primary, secondary }
    }
}

impl RunStore for TeeRunStore {
    fn append_record(&self, record: &CallRecord) -> Result<()> {
        self.primary.append_record(record)?;
        self.secondary.append_record(record)
    }

    fn write_call_log(&self, records: &[CallRecord]) -> Result<()> {
        self.primary.write_call_log(records)?;
        self.secondary.write_call_log(records)
    }

    fn load_call_log(&self) -> Result<Option<Vec<CallRecord>>> {
        match self.primary.load_call_log()? {
            Some(records) => Ok(Some(records)),
            None => self.secondary.load_call_log(),
        }
    }

    fn put_blob(&self, key: &str, bytes: &[u8]) -> Result<()> {
        self.primary.put_blob(key, bytes)?;
        self.secondary.put_blob(key, bytes)
    }

    fn get_blob(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match self.primary.get_blob(key)? {
            Some(bytes) => Ok(Some(bytes)),
            None => self.secondary.get_blob(key),
        }
    }

    // Same primary-then-secondary semantics as `get_blob`.
    fn has_blob(&self, key: &str) -> Result<bool> {
        Ok(self.primary.has_blob(key)? || self.secondary.has_blob(key)?)
    }

    fn append_blob_line(&self, key: &str, line: &[u8]) -> Result<()> {
        self.primary.append_blob_line(key, line)?;
        self.secondary.append_blob_line(key, line)
    }

    // Deliberately the default `None` (not the primary's path): a hardlinked
    // or cloned object would land only on local disk, and the mirror — the
    // copy that survives losing this machine — would never see its bytes.
    // Tee'd stores take the `put_blob` path for history objects instead.

    fn delete_blob(&self, key: &str) -> Result<()> {
        self.primary.delete_blob(key)?;
        self.secondary.delete_blob(key)
    }

    fn list_blobs(&self) -> Result<Vec<String>> {
        let mut keys = self.primary.list_blobs()?;
        for key in self.secondary.list_blobs()? {
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        keys.sort();
        Ok(keys)
    }

    /// The secondary is the authority — it is the copy every machine shares —
    /// so the swap is decided there and the primary is updated to match only
    /// once it succeeds. Deciding on the primary would make each machine's
    /// local file the arbiter of a fleet-wide question.
    fn compare_and_swap_blob(
        &self,
        key: &str,
        expected: Option<&[u8]>,
        new: Option<&[u8]>,
    ) -> Result<bool> {
        if !self.secondary.compare_and_swap_blob(key, expected, new)? {
            return Ok(false);
        }
        match new {
            Some(bytes) => self.primary.put_blob(key, bytes)?,
            None => self.primary.delete_blob(key)?,
        }
        Ok(true)
    }

    fn coordination_target(&self) -> Option<&dyn RunStore> {
        Some(self.secondary.as_ref())
    }

    fn flush(&self) -> Result<()> {
        self.primary.flush()?;
        self.secondary.flush()
    }
}

// ---------------------------------------------------------------------------
// Scoped view
// ---------------------------------------------------------------------------

/// A view of a parent store under a key prefix — how a branch sub-store
/// (`branches/op-N/branch-001/`) writes through the run's store (and any
/// durable mirror) while addressing its artifacts relatively. The journal
/// artifacts live at `<prefix>checkpoint.json`; appends are read-modify-write
/// since scoped journals are only written at branch persist points, never in
/// the per-record hot path.
#[derive(Debug)]
pub struct ScopedRunStore {
    inner: Arc<dyn RunStore>,
    prefix: String,
}

impl ScopedRunStore {
    /// `prefix` is a run-dir-relative directory path; a trailing `/` is added
    /// when missing.
    pub fn new(inner: Arc<dyn RunStore>, prefix: impl Into<String>) -> Self {
        let mut prefix = prefix.into();
        if !prefix.is_empty() && !prefix.ends_with('/') {
            prefix.push('/');
        }
        Self { inner, prefix }
    }

    fn key(&self, key: &str) -> String {
        format!("{}{key}", self.prefix)
    }
}

impl RunStore for ScopedRunStore {
    fn append_record(&self, record: &CallRecord) -> Result<()> {
        let mut records = self.load_call_log()?.unwrap_or_default();
        records.retain(|r| r.seq != record.seq);
        records.push(record.clone());
        self.write_call_log(&records)
    }

    fn write_call_log(&self, records: &[CallRecord]) -> Result<()> {
        self.inner.put_blob(
            &self.key(CHECKPOINT_FILE),
            &serde_json::to_vec_pretty(records)?,
        )
    }

    fn load_call_log(&self) -> Result<Option<Vec<CallRecord>>> {
        match self.inner.get_blob(&self.key(CHECKPOINT_FILE))? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }

    fn put_blob(&self, key: &str, bytes: &[u8]) -> Result<()> {
        self.inner.put_blob(&self.key(key), bytes)
    }

    fn get_blob(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.inner.get_blob(&self.key(key))
    }

    fn has_blob(&self, key: &str) -> Result<bool> {
        self.inner.has_blob(&self.key(key))
    }

    fn append_blob_line(&self, key: &str, line: &[u8]) -> Result<()> {
        self.inner.append_blob_line(&self.key(key), line)
    }

    fn blob_os_path(&self, key: &str) -> Option<PathBuf> {
        self.inner.blob_os_path(&self.key(key))
    }

    fn delete_blob(&self, key: &str) -> Result<()> {
        self.inner.delete_blob(&self.key(key))
    }

    fn list_blobs(&self) -> Result<Vec<String>> {
        Ok(self
            .inner
            .list_blobs()?
            .into_iter()
            .filter_map(|key| key.strip_prefix(&self.prefix).map(str::to_string))
            .collect())
    }

    fn compare_and_swap_blob(
        &self,
        key: &str,
        expected: Option<&[u8]>,
        new: Option<&[u8]>,
    ) -> Result<bool> {
        self.inner
            .compare_and_swap_blob(&self.key(key), expected, new)
    }

    fn flush(&self) -> Result<()> {
        self.inner.flush()
    }
}

// ---------------------------------------------------------------------------
// Factory + configuration
// ---------------------------------------------------------------------------

/// Which durable backend (if any) mirrors the filesystem layout.
#[derive(Debug, Clone)]
pub enum RunStoreBackend {
    /// Filesystem only — the default, byte-identical to the pre-store layout.
    Fs,
    /// Mirror to a shared SQLite database.
    Sqlite(Arc<SqliteRunStoreShared>),
    /// Mirror to a remote relay (Durable Object shim or compatible).
    Http(Arc<HttpRelay>),
    /// Mirror to an S3-compatible object store (S3, R2, GCS, MinIO, ...).
    Blob(Arc<crate::runtime::store_blob::S3BlobStore>),
}

/// Hands out per-run [`RunStore`] handles and owns backend-wide operations
/// (run listing, hydration, the detached-agent name registry).
#[derive(Debug, Clone)]
pub struct RunStoreFactory {
    run_base: PathBuf,
    backend: RunStoreBackend,
}

impl RunStoreFactory {
    #[allow(dead_code)] // Not yet wired into a call path; staged API.
    pub fn fs(run_base: impl Into<PathBuf>) -> Self {
        Self {
            run_base: run_base.into(),
            backend: RunStoreBackend::Fs,
        }
    }

    /// Build from the environment:
    ///   * unset / `fs` → filesystem only (the default)
    ///   * `sqlite` → mirror to `CHIDORI_RUN_DB` (default `<run_base>/runs.sqlite3`)
    ///   * `http(s)://...` → mirror to a remote relay
    pub fn from_env(run_base: impl Into<PathBuf>) -> Self {
        let run_base = run_base.into();
        let backend = match std::env::var("CHIDORI_RUN_STORE") {
            Ok(value) if value.eq_ignore_ascii_case("sqlite") => {
                let db_path = std::env::var("CHIDORI_RUN_DB")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| run_base.join("runs.sqlite3"));
                match SqliteRunStoreShared::open(&db_path) {
                    Ok(shared) => {
                        tracing::info!("run store: sqlite mirror at {}", db_path.display());
                        RunStoreBackend::Sqlite(shared)
                    }
                    Err(err) => {
                        tracing::warn!(
                            "run store sqlite mirror failed ({err}); falling back to fs only"
                        );
                        RunStoreBackend::Fs
                    }
                }
            }
            Ok(value) if value.starts_with("http://") || value.starts_with("https://") => {
                tracing::info!("run store: http mirror at {value}");
                RunStoreBackend::Http(HttpRelay::new(
                    value,
                    std::env::var("CHIDORI_RUN_STORE_TOKEN").ok(),
                ))
            }
            Ok(value) if value.starts_with("s3://") => {
                match crate::runtime::store_blob::S3BlobStore::from_env(&value) {
                    Ok(store) => {
                        tracing::info!("run store: s3-compatible mirror at {value}");
                        RunStoreBackend::Blob(store)
                    }
                    Err(err) => {
                        tracing::warn!(
                            "run store s3 mirror failed ({err}); falling back to fs only"
                        );
                        RunStoreBackend::Fs
                    }
                }
            }
            Ok(value) if !value.is_empty() && !value.eq_ignore_ascii_case("fs") => {
                tracing::warn!("unknown CHIDORI_RUN_STORE `{value}`; using fs only");
                RunStoreBackend::Fs
            }
            _ => RunStoreBackend::Fs,
        };
        Self { run_base, backend }
    }

    /// The process-wide factory for `run_base`, built from the environment on
    /// first use and memoized. This is how path-based persistence helpers
    /// (server session mutation, CLI resume) pick up the configured durable
    /// mirror without threading a factory through every signature — one
    /// factory per base means one shared SQLite connection / HTTP relay.
    pub fn shared(run_base: &Path) -> Self {
        static CACHE: std::sync::OnceLock<
            Mutex<std::collections::HashMap<PathBuf, RunStoreFactory>>,
        > = std::sync::OnceLock::new();
        let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
        let mut cache = cache.lock().unwrap();
        cache
            .entry(run_base.to_path_buf())
            .or_insert_with(|| Self::from_env(run_base))
            .clone()
    }

    pub fn run_base(&self) -> &Path {
        &self.run_base
    }

    /// Whether a durable mirror is configured (vs filesystem only).
    #[allow(dead_code)] // Not yet wired into a call path; staged API.
    pub fn has_durable_mirror(&self) -> bool {
        !matches!(self.backend, RunStoreBackend::Fs)
    }

    /// The per-run store handle: the filesystem layout, teed with the durable
    /// mirror when one is configured.
    pub fn store_for(&self, run_id: &str) -> Arc<dyn RunStore> {
        let primary = FsRunStore::new(self.run_base.join(run_id));
        match &self.backend {
            RunStoreBackend::Fs => Arc::new(primary),
            RunStoreBackend::Sqlite(shared) => Arc::new(TeeRunStore::new(
                primary,
                Arc::new(SqliteRunStore::new(shared.clone(), run_id)),
            )),
            RunStoreBackend::Http(relay) => Arc::new(TeeRunStore::new(
                primary,
                Arc::new(HttpRunStore::new(relay.clone(), run_id)),
            )),
            RunStoreBackend::Blob(store) => Arc::new(TeeRunStore::new(
                primary,
                Arc::new(crate::runtime::store_blob::BlobRunStore::new(
                    store.clone(),
                    run_id,
                )),
            )),
        }
    }

    /// The durable mirror's handle alone (no filesystem tee), when configured.
    fn mirror_for(&self, run_id: &str) -> Option<Arc<dyn RunStore>> {
        match &self.backend {
            RunStoreBackend::Fs => None,
            RunStoreBackend::Sqlite(shared) => {
                Some(Arc::new(SqliteRunStore::new(shared.clone(), run_id)))
            }
            RunStoreBackend::Http(relay) => {
                Some(Arc::new(HttpRunStore::new(relay.clone(), run_id)))
            }
            RunStoreBackend::Blob(store) => Some(Arc::new(
                crate::runtime::store_blob::BlobRunStore::new(store.clone(), run_id),
            )),
        }
    }

    /// Every run id the backend knows: local run directories, unioned with the
    /// durable mirror's runs (which may include runs from a lost machine).
    #[allow(dead_code)] // Not yet wired into a call path; staged API.
    pub fn list_runs(&self) -> Result<Vec<String>> {
        let mut ids = BTreeSet::new();
        match std::fs::read_dir(&self.run_base) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    if entry.path().is_dir() {
                        ids.insert(entry.file_name().to_string_lossy().to_string());
                    }
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| format!("listing {}", self.run_base.display()))
            }
        }
        match &self.backend {
            RunStoreBackend::Fs => {}
            RunStoreBackend::Sqlite(shared) => ids.extend(shared.list_runs()?),
            RunStoreBackend::Http(relay) => ids.extend(relay.list_runs()?),
            RunStoreBackend::Blob(store) => {
                ids.extend(crate::runtime::store_blob::list_runs(store)?)
            }
        }
        Ok(ids.into_iter().collect())
    }

    /// Materialize a run directory from the durable mirror — the recovery
    /// path after machine loss. No-op when the local journal already exists
    /// or no mirror is configured. Returns whether anything was hydrated.
    pub fn hydrate(&self, run_id: &str) -> Result<bool> {
        let run_dir = self.run_base.join(run_id);
        // Cheap presence check — hydration only kicks in when the local
        // journal is entirely absent (a fresh machine), so callers can invoke
        // this on every load without parsing anything.
        if run_dir.join(CHECKPOINT_FILE).exists() || run_dir.join(RECORDS_FILE).exists() {
            return Ok(false);
        }
        let local = FsRunStore::new(&run_dir);
        let Some(mirror) = self.mirror_for(run_id) else {
            return Ok(false);
        };
        let Some(records) = mirror.load_call_log()? else {
            return Ok(false);
        };
        local.write_call_log(&records)?;
        for key in mirror.list_blobs()? {
            if let Some(bytes) = mirror.get_blob(&key)? {
                local.put_blob(&key, &bytes)?;
            }
        }
        tracing::info!(run_id, "hydrated run directory from durable run store");
        Ok(true)
    }

    // --- Detached-agent name registry (docs/detached-agents.md) -----------
    //
    // Registry entries live OUTSIDE any single run: a name maps to the
    // detached agent's run id plus its descriptor JSON. Filesystem backend
    // keeps them under `<run_base>/agents/<name>.json`; the durable mirrors
    // keep them in their own keyspace (the `run_registry` table / the relay's
    // `/registry` resource) so a fresh machine can rediscover every agent.

    pub fn registry_put(
        &self,
        name: &str,
        run_id: &str,
        descriptor: &serde_json::Value,
    ) -> Result<()> {
        let entry = serde_json::json!({
            "name": name,
            "run_id": run_id,
            "descriptor": descriptor,
        });
        let bytes = serde_json::to_vec_pretty(&entry)?;
        let path = self.registry_path(name)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
        match &self.backend {
            RunStoreBackend::Fs => {}
            RunStoreBackend::Sqlite(shared) => {
                let conn = shared.conn.lock().unwrap();
                conn.execute(
                    "INSERT INTO run_registry (name, run_id, data) VALUES (?1, ?2, ?3)
                     ON CONFLICT(name) DO UPDATE SET
                         run_id = excluded.run_id, data = excluded.data",
                    rusqlite::params![name, run_id, serde_json::to_string(&entry)?],
                )?;
            }
            RunStoreBackend::Http(relay) => {
                relay.expect_ok(
                    "PUT",
                    format!("{}/registry/{}", relay.base_url, urlencode_path(name)),
                    Some(bytes),
                )?;
            }
            RunStoreBackend::Blob(store) => {
                crate::runtime::store_blob::registry_put(store, name, &entry)?;
            }
        }
        Ok(())
    }

    pub fn registry_get(&self, name: &str) -> Result<Option<serde_json::Value>> {
        match std::fs::read(self.registry_path(name)?) {
            Ok(bytes) => return Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        match &self.backend {
            RunStoreBackend::Fs => Ok(None),
            RunStoreBackend::Sqlite(shared) => {
                let conn = shared.conn.lock().unwrap();
                let mut stmt = conn.prepare("SELECT data FROM run_registry WHERE name = ?1")?;
                let mut rows = stmt.query(rusqlite::params![name])?;
                match rows.next()? {
                    Some(row) => Ok(Some(serde_json::from_str(&row.get::<_, String>(0)?)?)),
                    None => Ok(None),
                }
            }
            RunStoreBackend::Http(relay) => {
                let (status, bytes) = relay.request(
                    "GET",
                    format!("{}/registry/{}", relay.base_url, urlencode_path(name)),
                    None,
                )?;
                match status {
                    404 => Ok(None),
                    s if (200..300).contains(&s) => Ok(Some(serde_json::from_slice(&bytes)?)),
                    s => anyhow::bail!("run store relay GET registry failed: HTTP {s}"),
                }
            }
            RunStoreBackend::Blob(store) => crate::runtime::store_blob::registry_get(store, name),
        }
    }

    pub fn registry_list(&self) -> Result<Vec<serde_json::Value>> {
        let mut by_name: std::collections::BTreeMap<String, serde_json::Value> =
            std::collections::BTreeMap::new();
        match &self.backend {
            RunStoreBackend::Fs => {}
            RunStoreBackend::Sqlite(shared) => {
                let conn = shared.conn.lock().unwrap();
                let mut stmt = conn.prepare("SELECT name, data FROM run_registry")?;
                let rows = stmt.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                for row in rows {
                    let (name, data) = row?;
                    by_name.insert(name, serde_json::from_str(&data)?);
                }
            }
            RunStoreBackend::Http(relay) => {
                let (status, bytes) =
                    relay.request("GET", format!("{}/registry", relay.base_url), None)?;
                if (200..300).contains(&status) {
                    let entries: Vec<serde_json::Value> = serde_json::from_slice(&bytes)?;
                    for entry in entries {
                        if let Some(name) = entry.get("name").and_then(|v| v.as_str()) {
                            by_name.insert(name.to_string(), entry.clone());
                        }
                    }
                }
            }
            RunStoreBackend::Blob(store) => {
                for entry in crate::runtime::store_blob::registry_list(store)? {
                    if let Some(name) = entry.get("name").and_then(|v| v.as_str()) {
                        by_name.insert(name.to_string(), entry.clone());
                    }
                }
            }
        }
        // Local entries win: they reflect this machine's latest state.
        let agents_dir = self.run_base.join("agents");
        if let Ok(entries) = std::fs::read_dir(&agents_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                if let Ok(bytes) = std::fs::read(&path) {
                    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                        if let Some(name) = value.get("name").and_then(|v| v.as_str()) {
                            by_name.insert(name.to_string(), value.clone());
                        }
                    }
                }
            }
        }
        Ok(by_name.into_values().collect())
    }

    fn registry_path(&self, name: &str) -> Result<PathBuf> {
        if name.is_empty()
            || name
                .chars()
                .any(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'))
        {
            anyhow::bail!(
                "invalid detached agent name `{name}` \
                 (allowed: ASCII letters, digits, `-`, `_`, `.`)"
            );
        }
        Ok(self.run_base.join("agents").join(format!("{name}.json")))
    }
}

// ---------------------------------------------------------------------------
// Leases — single-writer ownership of a run (docs/durable-storage.md)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RunLease {
    pub owner: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

/// How many times [`acquire_lease`] re-reads and re-decides after losing a
/// compare-and-swap. Each loss means a concurrent writer won, so this bounds
/// livelock under contention rather than expressing a retry-on-failure policy.
const LEASE_CAS_ATTEMPTS: usize = 8;

/// Try to acquire (or renew) the run's lease for `owner`. Succeeds when the
/// run has no lease, the lease is already `owner`'s, or the previous lease
/// expired — in which case ownership transfers (the takeover path). Returns
/// the granted lease, or the live holder's lease as the error.
///
/// **Compare-and-swap, not read-then-write.** The decision is made against the
/// exact bytes read, and the write is conditional on those bytes still being
/// current ([`RunStore::compare_and_swap_blob`]); a lost swap re-reads and
/// re-decides instead of clobbering the winner. How atomic that is depends on
/// the backend — enforced on SQLite (one `IMMEDIATE` transaction) and on the
/// relay backends (server-side preconditions), advisory on the filesystem and
/// on object stores. `docs/durable-storage.md` §Leases has the table.
///
/// **Fleet state, so it addresses the shared store**
/// ([`RunStore::coordination_target`]): a tee's local-primary-first reads
/// would give every machine its own private lease.
///
/// A [`FencedError`] — this node's run was taken over — is reported as the
/// ordinary "held by someone else" outcome, so callers' existing standdown
/// paths handle it instead of it surfacing as a write failure.
pub fn acquire_lease(
    store: &dyn RunStore,
    owner: &str,
    ttl: chrono::Duration,
) -> Result<std::result::Result<RunLease, RunLease>> {
    let store = store.coordination_target().unwrap_or(store);

    // A fenced store means someone else owns the run; report them as the
    // holder. When the store couldn't name them, park the waiter for one TTL
    // rather than reporting an already-expired lease, which would spin.
    let standdown = |err: anyhow::Error| -> Result<std::result::Result<RunLease, RunLease>> {
        match fenced_owner(&err) {
            Some(fenced) => Ok(Err(RunLease {
                owner: fenced.owner.clone(),
                expires_at: fenced
                    .lease_expires_at
                    .unwrap_or_else(|| chrono::Utc::now() + ttl),
            })),
            None => Err(err),
        }
    };

    for _ in 0..LEASE_CAS_ATTEMPTS {
        let current = match store.get_blob(LEASE_FILE) {
            Ok(current) => current,
            Err(err) => return standdown(err),
        };
        let now = chrono::Utc::now();
        if let Some(bytes) = &current {
            if let Ok(existing) = serde_json::from_slice::<RunLease>(bytes) {
                if existing.owner != owner && existing.expires_at > now {
                    return Ok(Err(existing));
                }
            }
        }
        let lease = RunLease {
            owner: owner.to_string(),
            expires_at: now + ttl,
        };
        let bytes = serde_json::to_vec_pretty(&lease)?;
        match store.compare_and_swap_blob(LEASE_FILE, current.as_deref(), Some(&bytes)) {
            Ok(true) => return Ok(Ok(lease)),
            // Someone wrote between the read and the swap: re-read and
            // re-decide — they may now be a live holder we must yield to.
            Ok(false) => continue,
            Err(err) => return standdown(err),
        }
    }
    anyhow::bail!(
        "lease for this run is being contended: lost {LEASE_CAS_ATTEMPTS} \
         compare-and-swaps in a row"
    )
}

/// Release the run's lease if `owner` holds it.
///
/// The delete is itself conditional on the exact bytes read, so a lease that
/// was taken over between the read and the delete is left alone instead of
/// being deleted out from under its new owner.
pub fn release_lease(store: &dyn RunStore, owner: &str) -> Result<()> {
    let store = store.coordination_target().unwrap_or(store);
    let current = match store.get_blob(LEASE_FILE) {
        Ok(current) => current,
        // Already fenced: this node owns nothing here, so there is nothing to
        // release and nothing to report.
        Err(err) if fenced_owner(&err).is_some() => return Ok(()),
        Err(err) => return Err(err),
    };
    if let Some(bytes) = current {
        if let Ok(existing) = serde_json::from_slice::<RunLease>(&bytes) {
            if existing.owner == owner {
                match store.compare_and_swap_blob(LEASE_FILE, Some(&bytes), None) {
                    Ok(_) => {}
                    Err(err) if fenced_owner(&err).is_some() => {}
                    Err(err) => return Err(err),
                }
            }
        }
    }
    Ok(())
}

/// The run's durability posture, from `CHIDORI_DURABILITY`:
///
/// * `besteffort` (unset/default) — writes are buffered/pipelined; failures
///   are logged and tolerated; barriers only at settle/pause.
/// * `effect` — **durable at the effect, priced at the effect**: remote
///   appends stay pipelined (the thing that makes `strict` ~2× on a remote
///   store), but each effectful host call runs a durability barrier around
///   its pending-intent write BEFORE the effect executes and around its
///   result record AFTER — so a crash can never leave an executed effect
///   with no durable trace, and pure records never pay a round-trip.
///   Filesystem journal writes fsync, and write failures poison the run
///   exactly as under `strict`.
/// * `strict` — every append is synchronous and acknowledged; write
///   failures poison the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurabilityMode {
    BestEffort,
    Effect,
    Strict,
}

pub fn durability_mode() -> DurabilityMode {
    match std::env::var("CHIDORI_DURABILITY") {
        Ok(v) if v.eq_ignore_ascii_case("strict") => DurabilityMode::Strict,
        Ok(v) if v.eq_ignore_ascii_case("effect") => DurabilityMode::Effect,
        _ => DurabilityMode::BestEffort,
    }
}

/// Whether `CHIDORI_DURABILITY=strict` is set: journal write failures poison
/// the run, and filesystem journal writes fsync before acknowledging.
pub fn strict_durability() -> bool {
    durability_mode() == DurabilityMode::Strict
}

/// Failure latching + fsync posture: `effect` and `strict` both poison the
/// run on a failed journal write and fsync filesystem writes; only
/// `besteffort` tolerates and buffers.
pub fn latching_durability() -> bool {
    durability_mode() != DurabilityMode::BestEffort
}

/// Whether effectful host calls run explicit durability barriers around
/// their intent/result writes (the `effect` mode's mechanism; `strict`
/// doesn't need them — every write is already synchronous).
pub fn effect_barriers() -> bool {
    durability_mode() == DurabilityMode::Effect
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(seq: u64, function: &str) -> CallRecord {
        CallRecord {
            seq,
            parent_seq: None,
            function: function.to_string(),
            args: serde_json::json!({}),
            result: serde_json::json!({"ok": true}),
            duration_ms: 1,
            token_usage: None,
            timestamp: chrono::Utc::now(),
            error: None,
        }
    }

    /// Pipelined appends never block or fail the hot path on mirror trouble:
    /// the enqueue succeeds, the failure is collected at the relay barrier,
    /// and the besteffort `flush()` logs and continues (matching the
    /// pre-pipelining per-append warn-and-continue contract).
    #[test]
    fn pipelined_append_failures_surface_at_barrier_not_on_the_hot_path() {
        // Port 1 on loopback: nothing listens there, connections are refused.
        let relay = HttpRelay::new("http://127.0.0.1:1", None);
        let store = HttpRunStore::new(relay.clone(), "run-pipelined-test");
        if !store.pipelined {
            // CHIDORI_DURABILITY=strict in the environment disables
            // pipelining; the sync path is covered by the conformance tests.
            return;
        }
        store.append_record(&record(1, "prompt")).unwrap();
        store.append_record(&record(2, "tool")).unwrap();
        let err = relay.barrier().expect_err("both appends must have failed");
        assert!(err.to_string().contains("mirror writes failed"));
        // Errors were drained; a second barrier is clean, and flush() on a
        // fresh failure logs instead of failing.
        relay.barrier().unwrap();
        store.append_record(&record(3, "tool")).unwrap();
        store.flush().unwrap();
    }

    fn conformance(store: &dyn RunStore) {
        assert!(store.load_call_log().unwrap().is_none());
        store.append_record(&record(1, "prompt")).unwrap();
        store.append_record(&record(2, "tool")).unwrap();
        let loaded = store.load_call_log().unwrap().unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].seq, 1);
        assert_eq!(loaded[1].function, "tool");

        // Full rewrite compacts and replaces.
        store
            .write_call_log(&[record(1, "prompt"), record(2, "tool"), record(3, "signal")])
            .unwrap();
        // A stranded tail append after the checkpoint is recovered on load.
        store.append_record(&record(4, "http")).unwrap();
        let loaded = store.load_call_log().unwrap().unwrap();
        assert_eq!(
            loaded.iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );

        // Re-appending an existing seq replaces the record.
        store.append_record(&record(4, "http_retry")).unwrap();
        let loaded = store.load_call_log().unwrap().unwrap();
        assert_eq!(loaded.len(), 4);
        assert_eq!(loaded[3].function, "http_retry");

        // Blobs round-trip, list, and delete.
        assert!(store.get_blob("manifest.json").unwrap().is_none());
        store.put_blob("manifest.json", b"{\"a\":1}").unwrap();
        store.put_blob("signals/inbox.json", b"[]").unwrap();
        assert_eq!(
            store.get_blob("manifest.json").unwrap().unwrap(),
            b"{\"a\":1}"
        );
        let keys = store.list_blobs().unwrap();
        assert!(keys.contains(&"manifest.json".to_string()));
        assert!(keys.contains(&"signals/inbox.json".to_string()));
        store.delete_blob("manifest.json").unwrap();
        assert!(store.get_blob("manifest.json").unwrap().is_none());
        store.delete_blob("manifest.json").unwrap(); // absent delete is Ok
        store.flush().unwrap();
    }

    #[test]
    fn fs_run_store_conformance() {
        let dir = std::env::temp_dir().join(format!("chidori-store-fs-{}", uuid::Uuid::new_v4()));
        conformance(&FsRunStore::new(&dir));
        // The layout matches the established run dir shape.
        assert!(dir.join(CHECKPOINT_FILE).is_file());
        assert!(dir.join(RECORDS_FILE).is_file());
        assert!(dir.join("signals/inbox.json").is_file());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fs_run_store_rejects_traversal_keys() {
        let dir = std::env::temp_dir().join(format!("chidori-store-esc-{}", uuid::Uuid::new_v4()));
        let store = FsRunStore::new(&dir);
        assert!(store.put_blob("../escape.json", b"x").is_err());
        assert!(store.put_blob("/abs.json", b"x").is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sqlite_run_store_conformance() {
        let dir = std::env::temp_dir().join(format!("chidori-store-sq-{}", uuid::Uuid::new_v4()));
        let shared = SqliteRunStoreShared::open(&dir.join("runs.sqlite3")).unwrap();
        conformance(&SqliteRunStore::new(shared.clone(), "run-a"));
        // Runs are isolated per id.
        let other = SqliteRunStore::new(shared, "run-b");
        assert!(other.load_call_log().unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tee_run_store_mirrors_and_hydrates() {
        let base = std::env::temp_dir().join(format!("chidori-store-tee-{}", uuid::Uuid::new_v4()));
        let shared = SqliteRunStoreShared::open(&base.join("runs.sqlite3")).unwrap();
        let tee = TeeRunStore::new(
            FsRunStore::new(base.join("run-1")),
            Arc::new(SqliteRunStore::new(shared.clone(), "run-1")),
        );
        conformance(&tee);

        // Simulate machine loss: wipe the run dir, hydrate from the mirror.
        std::fs::remove_dir_all(base.join("run-1")).unwrap();
        let factory = RunStoreFactory {
            run_base: base.clone(),
            backend: RunStoreBackend::Sqlite(shared),
        };
        assert!(factory.hydrate("run-1").unwrap());
        let local = FsRunStore::new(base.join("run-1"));
        let records = local.load_call_log().unwrap().unwrap();
        assert_eq!(records.len(), 4);
        assert!(base.join("run-1").join("signals/inbox.json").is_file());
        // Second hydrate is a no-op (local journal exists).
        assert!(!factory.hydrate("run-1").unwrap());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn factory_lists_local_and_mirrored_runs() {
        let base = std::env::temp_dir().join(format!("chidori-store-ls-{}", uuid::Uuid::new_v4()));
        let shared = SqliteRunStoreShared::open(&base.join("runs.sqlite3")).unwrap();
        let factory = RunStoreFactory {
            run_base: base.clone(),
            backend: RunStoreBackend::Sqlite(shared),
        };
        factory
            .store_for("run-local")
            .append_record(&record(1, "log"))
            .unwrap();
        // A run that exists only in the mirror (e.g. written by a lost node).
        factory
            .mirror_for("run-remote")
            .unwrap()
            .append_record(&record(1, "log"))
            .unwrap();
        let runs = factory.list_runs().unwrap();
        assert!(runs.contains(&"run-local".to_string()));
        assert!(runs.contains(&"run-remote".to_string()));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn registry_round_trips() {
        let base = std::env::temp_dir().join(format!("chidori-store-reg-{}", uuid::Uuid::new_v4()));
        let factory = RunStoreFactory::fs(&base);
        assert!(factory.registry_get("triager").unwrap().is_none());
        factory
            .registry_put("triager", "run-9", &serde_json::json!({"source": "a.ts"}))
            .unwrap();
        let entry = factory.registry_get("triager").unwrap().unwrap();
        assert_eq!(entry["run_id"], "run-9");
        assert_eq!(factory.registry_list().unwrap().len(), 1);
        assert!(factory.registry_get("../evil").is_err());
        let _ = std::fs::remove_dir_all(base);
    }

    /// A stub node that refuses everything with the cell store's 409 — naming
    /// an owner, and optionally that owner's address — while counting the
    /// requests it received. The counter is what proves the follow is one hop
    /// and that lease traffic never takes it.
    struct FencingNode {
        url: String,
        hits: Arc<std::sync::atomic::AtomicUsize>,
        /// The address this node reports as the owner's, settable after both
        /// nodes exist so a test can point them at each other.
        owner_url: Arc<Mutex<Option<String>>>,
    }

    impl FencingNode {
        fn hits(&self) -> usize {
            self.hits.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn points_at(&self, other: &FencingNode) {
            *self.owner_url.lock().unwrap() = Some(other.url.clone());
        }
    }

    fn spawn_fencing_node(owner: &str) -> FencingNode {
        use axum::extract::State;
        use axum::http::StatusCode;

        #[derive(Clone)]
        struct Fence {
            owner: String,
            owner_url: Arc<Mutex<Option<String>>>,
            hits: Arc<std::sync::atomic::AtomicUsize>,
        }

        let fence = Fence {
            owner: owner.to_string(),
            owner_url: Arc::new(Mutex::new(None)),
            hits: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        let node = FencingNode {
            url: String::new(),
            hits: fence.hits.clone(),
            owner_url: fence.owner_url.clone(),
        };
        let app = axum::Router::new()
            .fallback(|State(fence): State<Fence>| async move {
                fence.hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut body = serde_json::json!({
                    "error": "cell owned elsewhere",
                    "owner": fence.owner,
                });
                if let Some(url) = fence.owner_url.lock().unwrap().clone() {
                    body["owner_url"] = serde_json::Value::String(url);
                }
                (StatusCode::CONFLICT, axum::Json(body))
            })
            .with_state(fence);
        FencingNode {
            url: serve_in_process(app),
            ..node
        }
    }

    /// Serve `app` on an ephemeral loopback port (the relay's client is
    /// blocking, so the server needs its own runtime on its own thread) and
    /// return its base URL.
    fn serve_in_process(app: axum::Router) -> String {
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                addr_tx.send(listener.local_addr().unwrap()).unwrap();
                axum::serve(listener, app).await.unwrap();
            });
        });
        format!("http://{}", addr_rx.recv().unwrap())
    }

    /// Following the owner is exactly ONE hop. Two nodes pointing at each
    /// other must not start a chase: the second 409 is the answer, and each
    /// node saw exactly one request.
    #[test]
    fn owner_follow_is_a_single_hop() {
        // The worst case for a follow rule: two nodes each naming the other.
        let first = spawn_fencing_node("node-1");
        let second = spawn_fencing_node("node-2");
        first.points_at(&second);
        second.points_at(&first);
        let store = HttpRunStore::new(HttpRelay::new(first.url.clone(), None), "run-x");

        let err = store.get_blob("manifest.json").unwrap_err();
        let fenced = fenced_owner(&err).expect("409 stays a fence");
        assert_eq!(fenced.owner, "node-2", "the second answer is the verdict");
        assert_eq!(first.hits(), 1);
        assert_eq!(second.hits(), 1);
    }

    /// Fencing is not weakened by routing: a 409 about the run LEASE is the
    /// ownership verdict itself, so it is never re-aimed at the owner — the
    /// node stands down exactly as before, and the owner is never asked.
    #[test]
    fn lease_traffic_never_follows_the_owner() {
        let owner = spawn_fencing_node("node-owner");
        let fenced = spawn_fencing_node("node-fenced");
        fenced.points_at(&owner);
        let store = HttpRunStore::new(HttpRelay::new(fenced.url.clone(), None), "run-x");

        let holder = acquire_lease(&store, "proc-a", chrono::Duration::seconds(60))
            .expect("fencing is a standdown, not an error")
            .expect_err("a fenced node must not believe it holds the lease");
        assert_eq!(holder.owner, "node-fenced");
        assert_eq!(fenced.hits(), 1);
        assert_eq!(
            owner.hits(),
            0,
            "lease arbitration must not be routed to the owner"
        );

        // The 409 still hands the address up to callers that want it.
        let err = store.get_blob(LEASE_FILE).unwrap_err();
        assert_eq!(
            fenced_owner(&err).unwrap().owner_url.as_deref(),
            Some(owner.url.as_str())
        );
    }

    /// An in-process HTTP server speaking the run-store relay protocol over
    /// memory — the same protocol the Cloudflare Durable Object shim
    /// (`integrations/cloudflare-durable-objects`) serves. Returns its base
    /// URL.
    fn spawn_protocol_server() -> String {
        use axum::extract::{Path as AxPath, State};
        use axum::http::StatusCode;
        use axum::routing::get;
        use std::collections::HashMap;

        #[derive(Clone, Default)]
        struct Mem {
            records: Arc<Mutex<HashMap<String, Vec<CallRecord>>>>,
            blobs: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        }

        let mem = Mem::default();
        let app = axum::Router::new()
            .route(
                "/runs",
                get(|State(mem): State<Mem>| async move {
                    let ids: Vec<String> = mem.records.lock().unwrap().keys().cloned().collect();
                    axum::Json(ids)
                }),
            )
            .route(
                "/runs/{id}/records",
                get(
                    |State(mem): State<Mem>, AxPath(id): AxPath<String>| async move {
                        match mem.records.lock().unwrap().get(&id) {
                            Some(records) => {
                                (StatusCode::OK, axum::Json(records.clone())).into_response()
                            }
                            None => StatusCode::NOT_FOUND.into_response(),
                        }
                    },
                )
                .post(
                    |State(mem): State<Mem>,
                     AxPath(id): AxPath<String>,
                     axum::Json(record): axum::Json<CallRecord>| async move {
                        let mut records = mem.records.lock().unwrap();
                        let entry = records.entry(id).or_default();
                        entry.retain(|r| r.seq != record.seq);
                        entry.push(record);
                        StatusCode::OK
                    },
                )
                .put(
                    |State(mem): State<Mem>,
                     AxPath(id): AxPath<String>,
                     axum::Json(records): axum::Json<Vec<CallRecord>>| async move {
                        mem.records.lock().unwrap().insert(id, records);
                        StatusCode::OK
                    },
                ),
            )
            .route(
                "/runs/{id}/blobs",
                get(
                    |State(mem): State<Mem>, AxPath(id): AxPath<String>| async move {
                        let prefix = format!("{id}/");
                        let keys: Vec<String> = mem
                            .blobs
                            .lock()
                            .unwrap()
                            .keys()
                            .filter_map(|k| k.strip_prefix(&prefix).map(str::to_string))
                            .collect();
                        axum::Json(keys)
                    },
                ),
            )
            .route(
                "/runs/{id}/blobs/{*key}",
                get(
                    |State(mem): State<Mem>,
                     AxPath((id, key)): AxPath<(String, String)>| async move {
                        match mem.blobs.lock().unwrap().get(&format!("{id}/{key}")) {
                            Some(bytes) => (StatusCode::OK, bytes.clone()).into_response(),
                            None => StatusCode::NOT_FOUND.into_response(),
                        }
                    },
                )
                .put(
                    |State(mem): State<Mem>,
                     AxPath((id, key)): AxPath<(String, String)>,
                     body: axum::body::Bytes| async move {
                        mem.blobs
                            .lock()
                            .unwrap()
                            .insert(format!("{id}/{key}"), body.to_vec());
                        StatusCode::OK
                    },
                )
                .delete(
                    |State(mem): State<Mem>,
                     AxPath((id, key)): AxPath<(String, String)>| async move {
                        mem.blobs.lock().unwrap().remove(&format!("{id}/{key}"));
                        StatusCode::OK
                    },
                ),
            )
            .with_state(mem);

        // The relay's dedicated request thread uses a blocking client, so the
        // server needs its own runtime on its own thread.
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                addr_tx.send(listener.local_addr().unwrap()).unwrap();
                axum::serve(listener, app).await.unwrap();
            });
        });
        use axum::response::IntoResponse as _;
        format!("http://{}", addr_rx.recv().unwrap())
    }

    #[test]
    fn http_run_store_conformance_over_relay_protocol() {
        let base = spawn_protocol_server();
        let relay = HttpRelay::new(base, None);
        conformance(&HttpRunStore::new(relay.clone(), "run-http"));
        // Runs are isolated per id, and the index lists what was written.
        assert!(HttpRunStore::new(relay.clone(), "run-other")
            .load_call_log()
            .unwrap()
            .is_none());
        assert!(relay.list_runs().unwrap().contains(&"run-http".to_string()));
    }

    /// The compare-and-swap contract every backend must satisfy, whether or
    /// not its implementation is atomic.
    fn cas_conformance(store: &dyn RunStore) {
        // Create-if-absent: the first wins, a second attempt sees the key.
        assert!(store
            .compare_and_swap_blob("cas.json", None, Some(b"one"))
            .unwrap());
        assert!(!store
            .compare_and_swap_blob("cas.json", None, Some(b"two"))
            .unwrap());
        assert_eq!(store.get_blob("cas.json").unwrap().unwrap(), b"one");

        // Swap against the current bytes succeeds; against stale bytes fails.
        assert!(store
            .compare_and_swap_blob("cas.json", Some(b"one"), Some(b"three"))
            .unwrap());
        assert!(!store
            .compare_and_swap_blob("cas.json", Some(b"one"), Some(b"four"))
            .unwrap());
        assert_eq!(store.get_blob("cas.json").unwrap().unwrap(), b"three");

        // Conditional delete: stale expectation refuses, current succeeds.
        assert!(!store
            .compare_and_swap_blob("cas.json", Some(b"one"), None)
            .unwrap());
        assert!(store
            .compare_and_swap_blob("cas.json", Some(b"three"), None)
            .unwrap());
        assert!(store.get_blob("cas.json").unwrap().is_none());
    }

    #[test]
    fn cas_conformance_across_backends() {
        let dir = std::env::temp_dir().join(format!("chidori-store-cas-{}", uuid::Uuid::new_v4()));
        cas_conformance(&FsRunStore::new(&dir));
        let shared = SqliteRunStoreShared::open(&dir.join("runs.sqlite3")).unwrap();
        cas_conformance(&SqliteRunStore::new(shared.clone(), "run-cas"));
        cas_conformance(&TeeRunStore::new(
            FsRunStore::new(dir.join("tee")),
            Arc::new(SqliteRunStore::new(shared, "run-tee")),
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Regression: the lease is fleet state, so it must be read from the
    /// shared backend. A tee reads its local primary first, so before
    /// `coordination_target` routed around it each machine saw its own
    /// private `lease.json` — and two machines would each believe they owned
    /// the run.
    #[test]
    fn lease_is_fleet_state_not_per_machine() {
        let base =
            std::env::temp_dir().join(format!("chidori-store-fleet-{}", uuid::Uuid::new_v4()));
        let shared = SqliteRunStoreShared::open(&base.join("runs.sqlite3")).unwrap();
        // Two machines: their own run directories, one shared mirror.
        let machine = |name: &str| {
            TeeRunStore::new(
                FsRunStore::new(base.join(name).join("run-1")),
                Arc::new(SqliteRunStore::new(shared.clone(), "run-1")),
            )
        };
        let (a, b) = (machine("a"), machine("b"));
        let ttl = chrono::Duration::seconds(60);

        assert!(acquire_lease(&a, "node-a", ttl).unwrap().is_ok());
        // B sees A's lease through the shared mirror and stands down.
        assert_eq!(
            acquire_lease(&b, "node-b", ttl).unwrap().unwrap_err().owner,
            "node-a"
        );

        // A's lease expires and B takes over. A's *local* copy still names A
        // with a live expiry — the stale view that used to fool it.
        let expired = RunLease {
            owner: "node-a".to_string(),
            expires_at: chrono::Utc::now() - chrono::Duration::seconds(1),
        };
        SqliteRunStore::new(shared.clone(), "run-1")
            .put_blob(LEASE_FILE, &serde_json::to_vec_pretty(&expired).unwrap())
            .unwrap();
        assert!(acquire_lease(&b, "node-b", ttl).unwrap().is_ok());
        assert_eq!(
            acquire_lease(&a, "node-a", ttl).unwrap().unwrap_err().owner,
            "node-b"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    /// A store that reports every read as fenced — what a node sees after
    /// another node takes its cell over.
    #[derive(Debug)]
    struct FencedStore;

    impl RunStore for FencedStore {
        fn append_record(&self, _: &CallRecord) -> Result<()> {
            Err(anyhow::Error::new(FencedError {
                owner: "node-live".to_string(),
                owner_url: None,
                lease_expires_at: None,
            }))
        }
        fn write_call_log(&self, _: &[CallRecord]) -> Result<()> {
            unimplemented!()
        }
        fn load_call_log(&self) -> Result<Option<Vec<CallRecord>>> {
            unimplemented!()
        }
        fn put_blob(&self, _: &str, _: &[u8]) -> Result<()> {
            unimplemented!()
        }
        fn get_blob(&self, _: &str) -> Result<Option<Vec<u8>>> {
            // Wrapped in context to prove the chain walk finds it.
            Err(anyhow::Error::new(FencedError {
                owner: "node-live".to_string(),
                owner_url: None,
                lease_expires_at: None,
            })
            .context("reading blob lease.json"))
        }
        fn delete_blob(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn list_blobs(&self) -> Result<Vec<String>> {
            unimplemented!()
        }
        fn flush(&self) -> Result<()> {
            Ok(())
        }
    }

    /// A fenced store means someone else owns the run: that must surface as
    /// the ordinary "held by another" outcome (which callers already stand
    /// down for), not as a write error that would poison the run under
    /// `CHIDORI_DURABILITY=strict`.
    #[test]
    fn fenced_store_reports_a_holder_instead_of_failing() {
        let ttl = chrono::Duration::seconds(60);
        let holder = acquire_lease(&FencedStore, "node-dead", ttl)
            .expect("a fenced store is a standdown, not an error")
            .expect_err("must not report ownership");
        assert_eq!(holder.owner, "node-live");
        // Unknown expiry parks the waiter for a TTL rather than reporting an
        // already-expired lease, which would spin.
        assert!(holder.expires_at > chrono::Utc::now());
        // Releasing a lease we no longer own is a no-op, not an error.
        release_lease(&FencedStore, "node-dead").unwrap();
        // The marker survives `.context()` wrapping.
        assert!(fenced_owner(&FencedStore.append_record(&record(1, "x")).unwrap_err()).is_some());
    }

    #[test]
    fn relay_409_becomes_a_fenced_error() {
        let body = br#"{"error":"cell owned elsewhere","owner":"node-b","lease_expires_at":"2030-01-01T00:00:00Z"}"#;
        let err = relay_error("PUT blob lease.json", 409, body);
        let fenced = fenced_owner(&err).expect("409 must be fenced");
        assert_eq!(fenced.owner, "node-b");
        assert!(fenced.lease_expires_at.is_some());
        // Still fenced when the body says nothing useful — standing down must
        // not depend on being able to name the winner.
        let err = relay_error("PUT blob lease.json", 409, b"nope");
        assert_eq!(fenced_owner(&err).unwrap().owner, "unknown");
        // Other failures stay ordinary errors.
        assert!(fenced_owner(&relay_error("PUT", 500, b"boom")).is_none());
    }

    #[test]
    fn lease_acquire_renew_expire() {
        let dir =
            std::env::temp_dir().join(format!("chidori-store-lease-{}", uuid::Uuid::new_v4()));
        let store = FsRunStore::new(&dir);
        // Fresh acquire.
        let granted = acquire_lease(&store, "node-a", chrono::Duration::seconds(60)).unwrap();
        assert!(granted.is_ok());
        // A different owner is refused while the lease is live.
        let refused = acquire_lease(&store, "node-b", chrono::Duration::seconds(60)).unwrap();
        assert_eq!(refused.unwrap_err().owner, "node-a");
        // The holder renews.
        assert!(
            acquire_lease(&store, "node-a", chrono::Duration::seconds(60))
                .unwrap()
                .is_ok()
        );
        // An expired lease transfers.
        let expired = RunLease {
            owner: "node-a".to_string(),
            expires_at: chrono::Utc::now() - chrono::Duration::seconds(1),
        };
        store
            .put_blob(LEASE_FILE, &serde_json::to_vec_pretty(&expired).unwrap())
            .unwrap();
        assert!(
            acquire_lease(&store, "node-b", chrono::Duration::seconds(60))
                .unwrap()
                .is_ok()
        );
        // Release by the holder clears it.
        release_lease(&store, "node-b").unwrap();
        assert!(store.get_blob(LEASE_FILE).unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }
}
