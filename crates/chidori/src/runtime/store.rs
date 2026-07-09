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
//!     journal cross-datacenter replication and point-in-time recovery.
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

    /// Remove a named auxiliary artifact. Removing an absent key is Ok.
    fn delete_blob(&self, key: &str) -> Result<()>;

    /// Keys of every stored blob (relative paths). Used by hydration.
    fn list_blobs(&self) -> Result<Vec<String>>;

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
            fsync_writes: strict_durability(),
        }
    }

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
}

impl SqliteRunStore {
    pub fn new(shared: Arc<SqliteRunStoreShared>, run_id: impl Into<String>) -> Self {
        Self {
            shared,
            run_id: run_id.into(),
        }
    }
}

impl RunStore for SqliteRunStore {
    fn append_record(&self, record: &CallRecord) -> Result<()> {
        let data = serde_json::to_string(record)?;
        let conn = self.shared.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO run_records (run_id, seq, pos, data)
             VALUES (?1, ?2,
                     COALESCE((SELECT MAX(pos) FROM run_records WHERE run_id = ?1), 0) + 1,
                     ?3)
             ON CONFLICT(run_id, seq) DO UPDATE SET data = excluded.data",
            rusqlite::params![self.run_id, record.seq as i64, data],
        )?;
        Ok(())
    }

    fn write_call_log(&self, records: &[CallRecord]) -> Result<()> {
        let mut conn = self.shared.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM run_records WHERE run_id = ?1",
            rusqlite::params![self.run_id],
        )?;
        for (pos, record) in records.iter().enumerate() {
            tx.execute(
                "INSERT INTO run_records (run_id, seq, pos, data) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(run_id, seq) DO UPDATE SET
                     pos = excluded.pos, data = excluded.data",
                rusqlite::params![
                    self.run_id,
                    record.seq as i64,
                    pos as i64,
                    serde_json::to_string(record)?
                ],
            )?;
        }
        tx.commit()?;
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
}

impl HttpRunStore {
    pub fn new(relay: Arc<HttpRelay>, run_id: impl Into<String>) -> Self {
        Self {
            relay,
            run_id: run_id.into(),
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
    reply: std::sync::mpsc::Sender<Result<(u16, Vec<u8>)>>,
}

/// The dedicated request thread + its channel. Owning the blocking client on
/// a plain thread sidesteps every "blocking client inside an async runtime"
/// hazard without giving the store an async signature.
pub struct HttpRelay {
    base_url: String,
    sender: std::sync::mpsc::Sender<HttpRelayRequest>,
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
        std::thread::Builder::new()
            .name("chidori-run-store-relay".to_string())
            .spawn(move || {
                let client = reqwest::blocking::Client::builder()
                    .timeout(std::time::Duration::from_secs(30))
                    .build();
                for request in receiver {
                    let result = match &client {
                        Ok(client) => {
                            let mut builder = match request.method {
                                "GET" => client.get(&request.url),
                                "PUT" => client.put(&request.url),
                                "POST" => client.post(&request.url),
                                "DELETE" => client.delete(&request.url),
                                other => {
                                    let _ = request.reply.send(Err(anyhow::anyhow!(
                                        "unsupported relay method {other}"
                                    )));
                                    continue;
                                }
                            };
                            if let Some(ref token) = token {
                                builder = builder.bearer_auth(token);
                            }
                            for (name, value) in &request.headers {
                                builder = builder.header(name, value);
                            }
                            if let Some(body) = request.body {
                                builder = builder
                                    .header("content-type", request.content_type)
                                    .body(body);
                            }
                            builder
                                .send()
                                .map_err(anyhow::Error::from)
                                .and_then(|response| {
                                    let status = response.status().as_u16();
                                    let bytes = response.bytes().map_err(anyhow::Error::from)?;
                                    Ok((status, bytes.to_vec()))
                                })
                        }
                        Err(err) => Err(anyhow::anyhow!("building relay http client: {err}")),
                    };
                    let _ = request.reply.send(result);
                }
            })
            .expect("spawning run-store relay thread");
        Arc::new(Self { base_url, sender })
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
        self.request_full(method, url, body, content_type, Vec::new())
    }

    /// Full-control request used by the S3 backend: caller-supplied headers
    /// (the SigV4 signature set) ride verbatim.
    pub(crate) fn request_full(
        &self,
        method: &'static str,
        url: String,
        body: Option<Vec<u8>>,
        content_type: &'static str,
        headers: Vec<(String, String)>,
    ) -> Result<(u16, Vec<u8>)> {
        let (reply, receive) = std::sync::mpsc::channel();
        self.sender
            .send(HttpRelayRequest {
                method,
                url,
                body,
                content_type,
                headers,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("run-store relay thread is gone"))?;
        receive
            .recv()
            .map_err(|_| anyhow::anyhow!("run-store relay dropped the reply"))?
    }

    fn expect_ok(&self, method: &'static str, url: String, body: Option<Vec<u8>>) -> Result<()> {
        let (status, bytes) = self.request(method, url.clone(), body)?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            anyhow::bail!(
                "run store relay {method} {url} failed: HTTP {status} {}",
                String::from_utf8_lossy(&bytes)
            )
        }
    }

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
        let (status, body) = self.relay.request_typed(
            "PUT",
            self.blob_url(key),
            Some(bytes.to_vec()),
            "application/octet-stream",
        )?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            anyhow::bail!(
                "run store relay PUT blob {key} failed: HTTP {status} {}",
                String::from_utf8_lossy(&body)
            )
        }
    }

    fn get_blob(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let (status, bytes) = self.relay.request("GET", self.blob_url(key), None)?;
        match status {
            404 => Ok(None),
            s if (200..300).contains(&s) => Ok(Some(bytes)),
            s => anyhow::bail!("run store relay GET blob {key} failed: HTTP {s}"),
        }
    }

    fn delete_blob(&self, key: &str) -> Result<()> {
        let (status, _) = self.relay.request("DELETE", self.blob_url(key), None)?;
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
            s => anyhow::bail!("run store relay GET blobs failed: HTTP {s}"),
        }
    }

    fn flush(&self) -> Result<()> {
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

/// Try to acquire (or renew) the run's lease for `owner`. Succeeds when the
/// run has no lease, the lease is already `owner`'s, or the previous lease
/// expired — in which case ownership transfers (the takeover path). Returns
/// the granted lease, or the live holder's lease as the error.
///
/// The check-and-set runs through the store's blob interface: last-writer-wins
/// races are possible between two *processes* sharing only the filesystem
/// backend; the SQLite and HTTP backends serialize writers (single connection
/// / single Durable Object), which is where multi-writer deployments live.
pub fn acquire_lease(
    store: &dyn RunStore,
    owner: &str,
    ttl: chrono::Duration,
) -> Result<std::result::Result<RunLease, RunLease>> {
    let now = chrono::Utc::now();
    if let Some(bytes) = store.get_blob(LEASE_FILE)? {
        if let Ok(existing) = serde_json::from_slice::<RunLease>(&bytes) {
            if existing.owner != owner && existing.expires_at > now {
                return Ok(Err(existing));
            }
        }
    }
    let lease = RunLease {
        owner: owner.to_string(),
        expires_at: now + ttl,
    };
    store.put_blob(LEASE_FILE, &serde_json::to_vec_pretty(&lease)?)?;
    Ok(Ok(lease))
}

/// Release the run's lease if `owner` holds it.
pub fn release_lease(store: &dyn RunStore, owner: &str) -> Result<()> {
    if let Some(bytes) = store.get_blob(LEASE_FILE)? {
        if let Ok(existing) = serde_json::from_slice::<RunLease>(&bytes) {
            if existing.owner == owner {
                store.delete_blob(LEASE_FILE)?;
            }
        }
    }
    Ok(())
}

/// Whether `CHIDORI_DURABILITY=strict` is set: journal write failures poison
/// the run, and filesystem journal writes fsync before acknowledging.
pub fn strict_durability() -> bool {
    std::env::var("CHIDORI_DURABILITY")
        .map(|v| v.eq_ignore_ascii_case("strict"))
        .unwrap_or(false)
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

    /// An in-process HTTP server speaking the run-store relay protocol over
    /// memory — the same protocol the Cloudflare Durable Object shim
    /// (`integrations/cloudflare-durable-objects`) serves. Returns its base
    /// URL.
    fn spawn_protocol_server() -> String {
        use axum::extract::{Path as AxPath, State};
        use axum::http::StatusCode;
        use axum::routing::{get, post, put};
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
