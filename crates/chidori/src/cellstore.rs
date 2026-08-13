//! Self-hosted durable run store — the celld model applied to Chidori runs.
//!
//! `chidori cell-store` serves the same REST protocol as the Cloudflare
//! Durable Object relay (`integrations/cloudflare-durable-objects/`), so it is
//! a drop-in `CHIDORI_RUN_STORE=http(s)://…` target — but it runs on your own
//! machines. The design is the core of Deno's celld ("self-hosted, distributed
//! Durable Objects", <https://github.com/denoland/celld>):
//!
//!   * **Every run is its own SQLite database** (a *cell*), on node-local
//!     disk. Runs shard by construction — no shared database, small blast
//!     radius per cell.
//!   * **An S3-compatible bucket is the fleet's shared source of truth.**
//!     Each cell's database is replicated to the bucket as immutable
//!     snapshots; ownership records live next to them. Nodes are replaceable:
//!     everything a node holds can be rebuilt from the bucket.
//!   * **Object-storage compare-and-swap owns cells.** Exactly one node owns
//!     a cell at a time, without a membership protocol, failure detector, or
//!     consensus service: ownership epoch `N` is a create-only object PUT
//!     (`If-None-Match: *`) of `cells/<id>/meta/<N>.json` — atomic create
//!     guarantees exactly one winner per epoch. A cell's current owner is the
//!     highest-epoch meta object; leases expire, and takeover is winning the
//!     next epoch.
//!   * **Fencing by epoch.** A node that lost a cell (its lease expired and
//!     another node won a higher epoch) discovers this at its next renewal or
//!     publish, drops its local copy, and refuses the cell's requests. Stale
//!     snapshots it may still upload are never referenced: readers only follow
//!     the snapshot pointer in the *current* (highest-epoch) meta object.
//!   * **Idle cells hibernate to nearly nothing.** After a final replication
//!     the cell is published as unowned *without resetting its epoch* (the
//!     celld shedding rule), its database handle closes, and its memory is
//!     dropped. The next request — on any node — wins the next epoch and
//!     restores the database from the bucket.
//!
//! Beyond the plain protocol, blob writes honor `If-None-Match: *` and
//! `If-Match: "<sha256>"` and are evaluated inside the owning cell's lock —
//! one serialization point per run, since a cell has exactly one owning node.
//! That is what upgrades Chidori's run lease from advisory to enforced
//! (`crate::runtime::store::acquire_lease`, `docs/durable-storage.md`
//! §Leases): two processes racing for the same run cannot both win.
//!
//! What this deliberately does not implement from celld: the V8/Wrangler
//! application runtime (Chidori has its own engine; cells here hold run
//! journals, not application code) and inter-node request routing (a client
//! talks to one node; a cell owned by another live node answers 409 with the
//! owner's identity rather than proxying).
//!
//! Durability model: local disk is the fast primary — a node restart loses
//! nothing (the owner reclaims its own cells and their local databases).
//! The bucket is the copy that survives losing the machine; its freshness is
//! the replication cadence (`--sync-secs`, plus a final publish at hibernate
//! and shutdown), so a node that dies *with* its disk can lose at most the
//! last sync window of a cell's writes. This is the same local-fast /
//! remote-durable split as the rest of `docs/durable-storage.md`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use crate::runtime::store_blob::S3BlobStore;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// One node of the cell store fleet.
pub struct CellStoreConfig {
    /// Node-local state: `cells/<id>.sqlite3` databases, the run/registry
    /// index, and the persisted node identity.
    pub data_dir: PathBuf,
    /// Stable node identity, used in ownership records. Stable across
    /// restarts so a restarting node reclaims its own live leases (and their
    /// local databases) instead of waiting out its own TTL.
    pub node_id: String,
    /// The fleet's shared bucket. `None` runs a single-node store with no
    /// replication or ownership protocol — cells live on local disk only.
    pub bucket: Option<Arc<S3BlobStore>>,
    /// Ownership lease TTL. Leases renew at half-TTL cadence from the sync
    /// loop; takeover waits for expiry. Node clocks must be sane within a
    /// fraction of this (the usual lease assumption — celld calls its peer
    /// protocol "clock-bounded" for the same reason), so keep it in seconds,
    /// not milliseconds, in production.
    pub lease_ttl: chrono::Duration,
    /// Hibernate cells idle longer than this: final replication, published
    /// unowned (epoch kept), database closed, memory dropped.
    pub idle_hibernate: std::time::Duration,
}

// ---------------------------------------------------------------------------
// Bucket protocol
// ---------------------------------------------------------------------------

/// The ownership + snapshot record for one cell at one epoch:
/// `cells/<id>/meta/<epoch>.json` under the bucket prefix. The object for
/// epoch `N` is only ever *created* with a compare-and-swap (`If-None-Match:
/// *`), so exactly one node wins each epoch; after winning, only that node
/// overwrites it (lease renewals, snapshot pointer updates, the unowned
/// publish at hibernate). The cell's current state is the highest-epoch meta.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CellMeta {
    cell: String,
    epoch: u64,
    /// The node that won this epoch. A hibernated cell keeps its last owner
    /// here with an already-expired lease — "unowned without resetting the
    /// epoch"; claimability is `lease_expires_at`, not this field.
    owner: String,
    lease_expires_at: DateTime<Utc>,
    /// Bucket key of the newest published snapshot of the cell's database.
    /// Carried forward on takeover until the new owner publishes its own.
    snapshot: Option<String>,
    updated_at: DateTime<Utc>,
}

impl CellMeta {
    fn claimable(&self, now: DateTime<Utc>) -> bool {
        now >= self.lease_expires_at
    }
}

/// Errors the HTTP layer maps to responses: ownership conflicts are 409s that
/// name the live owner; everything else is a 500.
#[derive(Debug)]
pub enum CellError {
    OwnedElsewhere {
        owner: String,
        lease_expires_at: DateTime<Utc>,
    },
    Other(anyhow::Error),
}

impl std::fmt::Display for CellError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CellError::OwnedElsewhere {
                owner,
                lease_expires_at,
            } => write!(
                f,
                "cell is owned by node `{owner}` (lease expires {lease_expires_at})"
            ),
            CellError::Other(err) => write!(f, "{err:#}"),
        }
    }
}

impl From<anyhow::Error> for CellError {
    fn from(err: anyhow::Error) -> Self {
        CellError::Other(err)
    }
}

impl From<rusqlite::Error> for CellError {
    fn from(err: rusqlite::Error) -> Self {
        CellError::Other(err.into())
    }
}

impl From<serde_json::Error> for CellError {
    fn from(err: serde_json::Error) -> Self {
        CellError::Other(err.into())
    }
}

// ---------------------------------------------------------------------------
// Cells
// ---------------------------------------------------------------------------

/// An open, owned cell: the run's SQLite database plus this node's view of
/// its lease. Everything mutable lives behind the cell's slot mutex.
#[derive(Debug)]
pub struct Cell {
    id: String,
    conn: rusqlite::Connection,
    epoch: u64,
    lease_expires_at: DateTime<Utc>,
    /// Bumped on every mutation; `published` trails it. Equal ⇒ the bucket
    /// snapshot is current.
    dirty: u64,
    published: u64,
    /// Monotonic within the epoch; snapshot objects are immutable, so each
    /// publish writes a new generation and retires the previous object.
    snapshot_gen: u64,
    snapshot_key: Option<String>,
    last_access: Instant,
}

impl Cell {
    // --- Journal (mirrors the Durable Object worker's schema) -------------

    /// The journal as a JSON array string, `None` when empty. Records are
    /// stored as their JSON text, so the response is a splice, not a re-parse.
    fn records_json(&self) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT data FROM records ORDER BY pos, seq")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut parts = Vec::new();
        for row in rows {
            parts.push(row?);
        }
        Ok(if parts.is_empty() {
            None
        } else {
            Some(format!("[{}]", parts.join(",")))
        })
    }

    /// Append one record. Re-appending a seq replaces its data but keeps its
    /// position (resume paths re-record synthetic entries), matching the
    /// worker and the `RunStore::append_record` contract.
    fn record_append(&mut self, record: &serde_json::Value) -> Result<()> {
        let seq = record
            .get("seq")
            .and_then(|v| v.as_u64())
            .context("record body has no numeric `seq`")?;
        let next: i64 =
            self.conn
                .query_row("SELECT COALESCE(MAX(pos), 0) + 1 FROM records", [], |row| {
                    row.get(0)
                })?;
        self.conn.execute(
            "INSERT INTO records (seq, pos, data) VALUES (?1, ?2, ?3)
             ON CONFLICT(seq) DO UPDATE SET data = excluded.data",
            rusqlite::params![seq as i64, next, serde_json::to_string(record)?],
        )?;
        self.dirty += 1;
        Ok(())
    }

    /// Replace the whole journal (a compaction-point checkpoint write).
    fn records_replace(&mut self, records: &[serde_json::Value]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM records", [])?;
        {
            let mut stmt =
                tx.prepare_cached("INSERT INTO records (seq, pos, data) VALUES (?1, ?2, ?3)")?;
            for (pos, record) in records.iter().enumerate() {
                let seq = record
                    .get("seq")
                    .and_then(|v| v.as_u64())
                    .context("record body has no numeric `seq`")?;
                stmt.execute(rusqlite::params![
                    seq as i64,
                    pos as i64,
                    serde_json::to_string(record)?
                ])?;
            }
        }
        tx.commit()?;
        self.dirty += 1;
        Ok(())
    }

    // --- Blobs -------------------------------------------------------------

    fn blob_get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT data FROM blobs WHERE key = ?1")?;
        let mut rows = stmt.query(rusqlite::params![key])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get::<_, Vec<u8>>(0)?)),
            None => Ok(None),
        }
    }

    fn blob_put(&mut self, key: &str, bytes: &[u8]) -> Result<()> {
        self.conn.execute(
            "INSERT INTO blobs (key, data) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET data = excluded.data",
            rusqlite::params![key, bytes],
        )?;
        self.dirty += 1;
        Ok(())
    }

    fn blob_delete(&mut self, key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM blobs WHERE key = ?1", rusqlite::params![key])?;
        self.dirty += 1;
        Ok(())
    }

    /// Evaluate a conditional write's precondition against the stored blob.
    /// Called with the cell's slot mutex held, which is what makes the
    /// check-then-write atomic for the whole fleet: a cell has exactly one
    /// owning node, and within that node exactly one holder of this lock. This
    /// is what turns Chidori's run lease from advisory into enforced
    /// (`crate::runtime::store::acquire_lease`).
    fn precondition_holds(&self, key: &str, condition: &Precondition) -> Result<bool> {
        let current = self.blob_get(key)?;
        Ok(match condition {
            Precondition::None => true,
            Precondition::Absent => current.is_none(),
            Precondition::Matches(etag) => current.is_some_and(|bytes| blob_etag(&bytes) == *etag),
        })
    }

    fn blob_list(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT key FROM blobs ORDER BY key")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut keys = Vec::new();
        for row in rows {
            keys.push(row?);
        }
        Ok(keys)
    }
}

/// The precondition a conditional blob write carries, parsed from the request's
/// `If-None-Match` / `If-Match` headers.
#[derive(Debug, PartialEq, Eq)]
enum Precondition {
    /// Unconditional — an ordinary write.
    None,
    /// `If-None-Match: *` — the key must not exist.
    Absent,
    /// `If-Match: "<etag>"` — the key must exist with exactly this entity tag.
    Matches(String),
}

impl Precondition {
    fn from_headers(headers: &axum::http::HeaderMap) -> Self {
        if headers
            .get("if-none-match")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim() == "*")
        {
            return Precondition::Absent;
        }
        match headers.get("if-match").and_then(|v| v.to_str().ok()) {
            Some(etag) => Precondition::Matches(etag.trim().to_string()),
            None => Precondition::None,
        }
    }
}

/// The entity tag of a blob: quoted SHA-256 of its bytes. Content-derived, so
/// client and server compute it independently and never need to exchange
/// server-assigned tags (matches `runtime::store::blob_etag`).
fn blob_etag(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    format!("\"{}\"", hex::encode(sha2::Sha256::digest(bytes)))
}

/// A cell's slot in the node's map: `None` while hibernated or never opened.
/// The slot mutex serializes everything about the cell — SQLite access and
/// the bucket round-trips of acquire/renew/publish alike.
type CellSlot = Arc<Mutex<Option<Cell>>>;

fn open_cell_db(path: &Path) -> Result<rusqlite::Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let conn = rusqlite::Connection::open(path)
        .with_context(|| format!("opening cell database {}", path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL").ok();
    // FULL, not NORMAL: this node is somebody's durability tier. Under
    // CHIDORI_DURABILITY=strict the client treats our acknowledgement as "the
    // effect is recorded", and WAL+NORMAL acknowledges commits that a power
    // loss can still take back. The fsync costs microseconds against the tens
    // of milliseconds an S3 round-trip would have cost instead.
    conn.pragma_update(None, "synchronous", "FULL").ok();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS records (
             seq INTEGER PRIMARY KEY,
             pos INTEGER NOT NULL,
             data TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS blobs (
             key TEXT PRIMARY KEY,
             data BLOB NOT NULL
         );",
    )?;
    Ok(conn)
}

/// The epoch stamped into a local cell database (`PRAGMA user_version`), or
/// `None` when no local database exists. This is how a node knows whether a
/// local file is *its own* copy from a given ownership epoch — the reclaim
/// rule that makes node restarts lossless — versus a stale leftover from an
/// epoch another node has since owned.
fn local_epoch(path: &Path) -> Result<Option<u64>> {
    if !path.exists() {
        return Ok(None);
    }
    let conn = rusqlite::Connection::open(path)
        .with_context(|| format!("opening cell database {}", path.display()))?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    Ok(Some(version as u64))
}

fn remove_db_files(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut victim = path.as_os_str().to_owned();
        victim.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(victim));
    }
}

/// Cell ids and registry names become file names and bucket key segments, so
/// hold them to one safe path component. Chidori run ids (uuid-ish) and agent
/// names (the factory's `[A-Za-z0-9._-]`) all pass.
fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value != "."
        && value != ".."
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'))
}

// ---------------------------------------------------------------------------
// The node
// ---------------------------------------------------------------------------

/// One cell-store node: a map of open cells, the node-local run/registry
/// index, and the (optional) shared bucket. Multiple nodes pointed at the
/// same bucket form a fleet with no other coordination.
pub struct Node {
    config: CellStoreConfig,
    cells: Mutex<HashMap<String, CellSlot>>,
    /// Run index + detached-agent registry. Fleet-level rather than per-cell
    /// state, so it lives outside the ownership protocol: locally in one
    /// small SQLite database, mirrored to plain bucket objects
    /// (`registry/<name>.json`; run listing comes from the bucket's
    /// `cells/<id>/` prefixes). Keeping it out of a singleton cell avoids
    /// making every node's writes contend on one cell's ownership.
    index: Mutex<rusqlite::Connection>,
}

impl Node {
    pub fn new(config: CellStoreConfig) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&config.data_dir)
            .with_context(|| format!("creating {}", config.data_dir.display()))?;
        let index = rusqlite::Connection::open(config.data_dir.join("index.sqlite3"))?;
        index.pragma_update(None, "journal_mode", "WAL").ok();
        index.execute_batch(
            "CREATE TABLE IF NOT EXISTS runs (
                 run_id TEXT PRIMARY KEY
             );
             CREATE TABLE IF NOT EXISTS registry (
                 name TEXT PRIMARY KEY,
                 data TEXT NOT NULL
             );",
        )?;
        Ok(Arc::new(Self {
            config,
            cells: Mutex::new(HashMap::new()),
            index: Mutex::new(index),
        }))
    }

    pub fn node_id(&self) -> &str {
        &self.config.node_id
    }

    fn db_path(&self, id: &str) -> PathBuf {
        self.config
            .data_dir
            .join("cells")
            .join(format!("{id}.sqlite3"))
    }

    fn slot(&self, id: &str) -> CellSlot {
        self.cells
            .lock()
            .unwrap()
            .entry(id.to_string())
            .or_default()
            .clone()
    }

    /// How much lease must remain for this node to keep serving a cell
    /// without renewing first. The safety margin against clock skew and
    /// in-flight time: a request is only served while the lease has
    /// comfortably not expired anywhere in the fleet's view.
    fn lease_margin(&self) -> chrono::Duration {
        self.config.lease_ttl / 4
    }

    // --- Bucket keyspace ---------------------------------------------------

    fn cell_prefix(&self, bucket: &S3BlobStore, id: &str) -> String {
        format!("{}cells/{}/", bucket.key_prefix(), id)
    }

    fn meta_key(&self, bucket: &S3BlobStore, id: &str, epoch: u64) -> String {
        format!("{}meta/{epoch:020}.json", self.cell_prefix(bucket, id))
    }

    fn state_key(&self, bucket: &S3BlobStore, id: &str, epoch: u64, generation: u64) -> String {
        format!(
            "{}state/{epoch:020}-{generation:020}.db",
            self.cell_prefix(bucket, id)
        )
    }

    /// Every meta object of a cell, as `(epoch, key)` sorted ascending.
    fn list_meta(&self, bucket: &S3BlobStore, id: &str) -> Result<Vec<(u64, String)>> {
        let prefix = format!("{}meta/", self.cell_prefix(bucket, id));
        let (keys, _) = bucket.list(&prefix, None)?;
        let mut out: Vec<(u64, String)> = keys
            .into_iter()
            .filter_map(|key| {
                let epoch = key
                    .strip_prefix(&prefix)?
                    .strip_suffix(".json")?
                    .parse::<u64>()
                    .ok()?;
                Some((epoch, key))
            })
            .collect();
        out.sort();
        Ok(out)
    }

    /// The cell's current (highest-epoch) meta. A `NotFound` between LIST and
    /// GET means an old epoch was garbage-collected mid-look; the caller's
    /// retry loop re-reads.
    fn latest_meta(&self, bucket: &S3BlobStore, id: &str) -> Result<Option<CellMeta>> {
        let Some((_, key)) = self.list_meta(bucket, id)?.into_iter().last() else {
            return Ok(None);
        };
        match bucket.get_object(&key)? {
            Some(bytes) => Ok(Some(
                serde_json::from_slice(&bytes).context("parsing cell meta object")?,
            )),
            None => Ok(None),
        }
    }

    fn write_meta(&self, bucket: &S3BlobStore, meta: &CellMeta) -> Result<()> {
        bucket.put_object(
            &self.meta_key(bucket, &meta.cell, meta.epoch),
            &serde_json::to_vec_pretty(meta)?,
        )
    }

    // --- Ownership ----------------------------------------------------------

    /// Acquire the cell for this node and materialize its local database.
    /// The compare-and-swap loop of the celld protocol: read the highest
    /// epoch, decide (fresh cell / reclaim own live lease / take over an
    /// expired one / stand down from a live foreign one), and win the next
    /// epoch with a create-only PUT. Losing the create means another node
    /// raced us — re-read and re-decide.
    fn acquire(&self, id: &str) -> Result<Cell, CellError> {
        let db_path = self.db_path(id);
        let Some(bucket) = self.config.bucket.clone() else {
            // Single-node mode: no ownership protocol, cells are local-only.
            let conn = open_cell_db(&db_path)?;
            return Ok(Cell {
                id: id.to_string(),
                conn,
                epoch: 1,
                lease_expires_at: Utc::now() + chrono::Duration::days(365 * 100),
                dirty: 0,
                published: 0,
                snapshot_gen: 0,
                snapshot_key: None,
                last_access: Instant::now(),
            });
        };

        for _ in 0..8 {
            let now = Utc::now();
            let prior = self.latest_meta(&bucket, id)?;
            let (target_epoch, reclaim) = match &prior {
                None => (1, false),
                Some(m) if m.owner == self.config.node_id && !m.claimable(now) => {
                    // Our own live lease (a restart): renew in place. Only
                    // the epoch's winner writes its meta, so a plain
                    // overwrite is race-free.
                    (m.epoch, true)
                }
                Some(m) if m.claimable(now) => (m.epoch + 1, false),
                Some(m) => {
                    return Err(CellError::OwnedElsewhere {
                        owner: m.owner.clone(),
                        lease_expires_at: m.lease_expires_at,
                    })
                }
            };
            if target_epoch > i32::MAX as u64 {
                // The epoch is stamped into SQLite's 32-bit user_version.
                return Err(CellError::Other(anyhow::anyhow!(
                    "cell epoch overflow for `{id}`"
                )));
            }
            let meta = CellMeta {
                cell: id.to_string(),
                epoch: target_epoch,
                owner: self.config.node_id.clone(),
                lease_expires_at: now + self.config.lease_ttl,
                snapshot: prior.as_ref().and_then(|m| m.snapshot.clone()),
                updated_at: now,
            };
            let won = if reclaim {
                self.write_meta(&bucket, &meta)?;
                true
            } else {
                bucket.put_object_if_absent(
                    &self.meta_key(&bucket, id, target_epoch),
                    &serde_json::to_vec_pretty(&meta)?,
                )?
            };
            if !won {
                continue;
            }

            // Materialize the local database. Reuse the local file only when
            // it is provably this node's own copy from the prior epoch (we
            // were the prior owner and stamped the file with that epoch) —
            // it is then a superset of anything published, so a crash between
            // publishes loses nothing. Anything else is stale or foreign:
            // restore from the current snapshot, or start empty.
            let reuse_local = prior
                .as_ref()
                .is_some_and(|m| m.owner == self.config.node_id)
                && local_epoch(&db_path)? == prior.as_ref().map(|m| m.epoch);
            if !reuse_local {
                remove_db_files(&db_path);
                if let Some(key) = &meta.snapshot {
                    let bytes = bucket
                        .get_object(key)?
                        .with_context(|| format!("cell snapshot object `{key}` missing"))?;
                    if let Some(parent) = db_path.parent() {
                        std::fs::create_dir_all(parent)
                            .with_context(|| format!("creating {}", parent.display()))?;
                    }
                    std::fs::write(&db_path, bytes)
                        .with_context(|| format!("restoring {}", db_path.display()))?;
                }
            }
            let conn = open_cell_db(&db_path)?;
            conn.pragma_update(None, "user_version", target_epoch as i64)
                .context("stamping cell epoch")?;
            // Seed the snapshot generation past the carried-over pointer so a
            // reclaimed epoch keeps writing fresh object keys.
            let snapshot_gen = meta
                .snapshot
                .as_deref()
                .and_then(parse_snapshot_generation)
                .filter(|(epoch, _)| *epoch == target_epoch)
                .map(|(_, generation)| generation)
                .unwrap_or(0);
            tracing::info!(
                cell = id,
                epoch = target_epoch,
                restored = !reuse_local && meta.snapshot.is_some(),
                "cell acquired"
            );
            return Ok(Cell {
                id: id.to_string(),
                conn,
                epoch: target_epoch,
                lease_expires_at: meta.lease_expires_at,
                dirty: 0,
                published: 0,
                snapshot_gen,
                snapshot_key: meta.snapshot,
                last_access: Instant::now(),
            });
        }
        Err(CellError::Other(anyhow::anyhow!(
            "cell `{id}` ownership contention: lost the epoch compare-and-swap repeatedly"
        )))
    }

    /// Renew a still-live lease in place, fencing first: a higher epoch in
    /// the bucket means another node owns the cell now, and this node's copy
    /// is dead weight.
    fn renew(&self, cell: &mut Cell) -> Result<(), CellError> {
        let Some(bucket) = self.config.bucket.clone() else {
            return Ok(());
        };
        self.fence_check(&bucket, cell)?;
        let now = Utc::now();
        let meta = CellMeta {
            cell: cell.id.clone(),
            epoch: cell.epoch,
            owner: self.config.node_id.clone(),
            lease_expires_at: now + self.config.lease_ttl,
            snapshot: cell.snapshot_key.clone(),
            updated_at: now,
        };
        self.write_meta(&bucket, &meta)?;
        cell.lease_expires_at = meta.lease_expires_at;
        Ok(())
    }

    fn fence_check(&self, bucket: &S3BlobStore, cell: &Cell) -> Result<(), CellError> {
        if let Some(m) = self.latest_meta(bucket, &cell.id)? {
            if m.epoch > cell.epoch {
                return Err(CellError::OwnedElsewhere {
                    owner: m.owner,
                    lease_expires_at: m.lease_expires_at,
                });
            }
        }
        Ok(())
    }

    /// Replicate the cell to the bucket (when dirty) and renew its lease —
    /// the sync-loop workhorse. Snapshot objects are immutable: each publish
    /// writes a new generation, swings the meta pointer, then retires the
    /// superseded object and any pre-takeover epochs (best-effort GC; an
    /// interrupted GC leaves garbage, never a dangling pointer).
    fn publish(&self, cell: &mut Cell) -> Result<(), CellError> {
        let Some(bucket) = self.config.bucket.clone() else {
            return Ok(());
        };
        self.fence_check(&bucket, cell)?;
        if cell.dirty != cell.published {
            let tmp = self
                .config
                .data_dir
                .join("cells")
                .join(format!("{}.snapshot.tmp", cell.id));
            let _ = std::fs::remove_file(&tmp);
            // VACUUM INTO produces a compact, consistent, standalone copy
            // without blocking the WAL for readers.
            cell.conn
                .execute_batch(&format!(
                    "VACUUM INTO '{}'",
                    tmp.to_string_lossy().replace('\'', "''")
                ))
                .context("snapshotting cell database")?;
            let bytes = std::fs::read(&tmp).context("reading cell snapshot")?;
            let _ = std::fs::remove_file(&tmp);
            let generation = cell.snapshot_gen + 1;
            let key = self.state_key(&bucket, &cell.id, cell.epoch, generation);
            bucket.put_object(&key, &bytes)?;
            let superseded = cell.snapshot_key.replace(key);
            cell.snapshot_gen = generation;
            cell.published = cell.dirty;
            if let Some(old) = superseded {
                let _ = bucket.delete_object(&old);
            }
            for (epoch, key) in self.list_meta(&bucket, &cell.id)? {
                if epoch < cell.epoch {
                    let _ = bucket.delete_object(&key);
                }
            }
            tracing::debug!(cell = %cell.id, epoch = cell.epoch, generation, "cell replicated");
        }
        self.renew(cell)
    }

    /// Hibernate/shutdown path: final replication, then publish the cell as
    /// unowned — same epoch, expired lease — so any node's next request can
    /// claim the next epoch without waiting out a TTL.
    fn release(&self, mut cell: Cell) -> Result<(), CellError> {
        let Some(bucket) = self.config.bucket.clone() else {
            return Ok(()); // Dropping the connection closes the local cell.
        };
        self.publish(&mut cell)?;
        let now = Utc::now();
        let meta = CellMeta {
            cell: cell.id.clone(),
            epoch: cell.epoch,
            owner: self.config.node_id.clone(),
            lease_expires_at: now,
            snapshot: cell.snapshot_key.clone(),
            updated_at: now,
        };
        self.write_meta(&bucket, &meta)?;
        tracing::info!(cell = %cell.id, epoch = cell.epoch, "cell released (hibernated)");
        Ok(())
    }

    // --- Serving ------------------------------------------------------------

    /// Run `f` against the owned, open cell — acquiring, restoring, renewing,
    /// or fencing as needed. The write path: absent cells are created.
    pub fn with_cell<T>(
        &self,
        id: &str,
        f: impl FnOnce(&mut Cell) -> Result<T>,
    ) -> Result<T, CellError> {
        let slot = self.slot(id);
        let mut guard = slot.lock().unwrap();
        self.ensure_owned(id, &mut guard)?;
        let cell = guard.as_mut().expect("ensure_owned leaves an open cell");
        cell.last_access = Instant::now();
        f(cell).map_err(CellError::Other)
    }

    /// Read-path variant: `Ok(None)` when the cell exists nowhere (no open
    /// handle, no local file, no bucket record), without conjuring an empty
    /// cell — a read of an unknown run must not claim ownership records in
    /// the bucket.
    pub fn with_cell_opt<T>(
        &self,
        id: &str,
        f: impl FnOnce(&mut Cell) -> Result<T>,
    ) -> Result<Option<T>, CellError> {
        let slot = self.slot(id);
        let mut guard = slot.lock().unwrap();
        if guard.is_none() && !self.db_path(id).exists() {
            let known_to_bucket = match self.config.bucket.as_deref() {
                Some(bucket) => self.latest_meta(bucket, id)?.is_some(),
                None => false,
            };
            if !known_to_bucket {
                return Ok(None);
            }
        }
        self.ensure_owned(id, &mut guard)?;
        let cell = guard.as_mut().expect("ensure_owned leaves an open cell");
        cell.last_access = Instant::now();
        f(cell).map(Some).map_err(CellError::Other)
    }

    fn ensure_owned(&self, id: &str, guard: &mut Option<Cell>) -> Result<(), CellError> {
        if let Some(cell) = guard.as_mut() {
            if self.config.bucket.is_none() {
                return Ok(());
            }
            let now = Utc::now();
            if now + self.lease_margin() < cell.lease_expires_at {
                return Ok(());
            }
            if now < cell.lease_expires_at {
                // Inside the margin but still live: renew before serving.
                return match self.renew(cell) {
                    Ok(()) => Ok(()),
                    Err(err) => {
                        // Fenced (or the bucket is unreachable): the local
                        // copy may be stale — drop it rather than serve it.
                        *guard = None;
                        Err(err)
                    }
                };
            }
            // Expired outright (missed renewals — a long GC pause, a
            // partitioned bucket): the lease is gone, so ownership must be
            // re-won through the compare-and-swap, never by overwrite. The
            // local file stays on disk; if nobody else took the cell, the
            // reclaim rule reuses it losslessly.
            *guard = None;
        }
        *guard = Some(self.acquire(id)?);
        Ok(())
    }

    // --- Sync loop ----------------------------------------------------------

    /// One pass of the node's background loop: hibernate idle cells, publish
    /// dirty ones, renew leases due within half a TTL. Called on the
    /// `--sync-secs` cadence by `serve`, and directly by tests.
    pub fn tick(&self) {
        let slots: Vec<(String, CellSlot)> = self
            .cells
            .lock()
            .unwrap()
            .iter()
            .map(|(id, slot)| (id.clone(), slot.clone()))
            .collect();
        for (id, slot) in slots {
            let mut guard = slot.lock().unwrap();
            let Some(cell) = guard.as_mut() else { continue };
            if cell.last_access.elapsed() >= self.config.idle_hibernate {
                let cell = guard.take().expect("checked Some above");
                if let Err(err) = self.release(cell) {
                    tracing::warn!(cell = %id, error = %err, "cell hibernate failed");
                }
                continue;
            }
            if self.config.bucket.is_none() {
                continue;
            }
            let due_renew = cell.lease_expires_at - Utc::now() < self.config.lease_ttl / 2;
            if cell.dirty != cell.published || due_renew {
                if let Err(err) = self.publish(cell) {
                    // Fenced or bucket trouble: stop serving this copy.
                    tracing::warn!(cell = %id, error = %err, "cell publish failed; dropping local copy");
                    *guard = None;
                }
            }
        }
    }

    /// Release every open cell — the graceful-shutdown path, so a stopped
    /// node's cells are immediately claimable elsewhere instead of waiting
    /// out their leases.
    pub fn shutdown(&self) {
        let slots: Vec<(String, CellSlot)> = self.cells.lock().unwrap().drain().collect();
        for (id, slot) in slots {
            let Some(cell) = slot.lock().unwrap().take() else {
                continue;
            };
            if let Err(err) = self.release(cell) {
                tracing::warn!(cell = %id, error = %err, "cell release on shutdown failed");
            }
        }
    }

    // --- Run index + detached-agent registry --------------------------------

    fn run_register(&self, run_id: &str) -> Result<()> {
        self.index.lock().unwrap().execute(
            "INSERT OR IGNORE INTO runs (run_id) VALUES (?1)",
            rusqlite::params![run_id],
        )?;
        Ok(())
    }

    /// Runs this node has seen, unioned with every cell in the bucket — so a
    /// fresh node lists runs written through nodes that no longer exist.
    fn runs_list(&self) -> Result<Vec<String>> {
        let mut ids = BTreeSet::new();
        {
            let index = self.index.lock().unwrap();
            let mut stmt = index.prepare("SELECT run_id FROM runs")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            for row in rows {
                ids.insert(row?);
            }
        }
        if let Some(bucket) = self.config.bucket.as_deref() {
            let prefix = format!("{}cells/", bucket.key_prefix());
            let (_, common) = bucket.list(&prefix, Some("/"))?;
            for p in common {
                if let Some(rest) = p.strip_prefix(&prefix) {
                    let id = rest.trim_end_matches('/');
                    if !id.is_empty() {
                        ids.insert(id.to_string());
                    }
                }
            }
        }
        Ok(ids.into_iter().collect())
    }

    fn registry_key(&self, bucket: &S3BlobStore, name: &str) -> String {
        format!("{}registry/{name}.json", bucket.key_prefix())
    }

    fn registry_put(&self, name: &str, entry: &str) -> Result<()> {
        self.index.lock().unwrap().execute(
            "INSERT INTO registry (name, data) VALUES (?1, ?2)
             ON CONFLICT(name) DO UPDATE SET data = excluded.data",
            rusqlite::params![name, entry],
        )?;
        if let Some(bucket) = self.config.bucket.as_deref() {
            bucket.put_object(&self.registry_key(bucket, name), entry.as_bytes())?;
        }
        Ok(())
    }

    fn registry_get(&self, name: &str) -> Result<Option<String>> {
        // Bucket first: another node may have updated the entry more recently
        // than this node's local copy.
        if let Some(bucket) = self.config.bucket.as_deref() {
            if let Some(bytes) = bucket.get_object(&self.registry_key(bucket, name))? {
                return Ok(Some(String::from_utf8_lossy(&bytes).into_owned()));
            }
        }
        let index = self.index.lock().unwrap();
        let mut stmt = index.prepare("SELECT data FROM registry WHERE name = ?1")?;
        let mut rows = stmt.query(rusqlite::params![name])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get::<_, String>(0)?)),
            None => Ok(None),
        }
    }

    fn registry_entries(&self) -> Result<Vec<String>> {
        let mut by_name: BTreeMap<String, String> = BTreeMap::new();
        {
            let index = self.index.lock().unwrap();
            let mut stmt = index.prepare("SELECT name, data FROM registry")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (name, data) = row?;
                by_name.insert(name, data);
            }
        }
        if let Some(bucket) = self.config.bucket.as_deref() {
            let prefix = format!("{}registry/", bucket.key_prefix());
            let (keys, _) = bucket.list(&prefix, None)?;
            for key in keys {
                let Some(name) = key
                    .strip_prefix(&prefix)
                    .and_then(|rest| rest.strip_suffix(".json"))
                else {
                    continue;
                };
                if let Some(bytes) = bucket.get_object(&key)? {
                    by_name.insert(
                        name.to_string(),
                        String::from_utf8_lossy(&bytes).into_owned(),
                    );
                }
            }
        }
        Ok(by_name.into_values().collect())
    }

    /// Diagnostics: this node's identity and the cells it currently holds.
    fn status(&self) -> serde_json::Value {
        let cells: Vec<serde_json::Value> = self
            .cells
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(id, slot)| {
                let guard = slot.lock().unwrap();
                let cell = guard.as_ref()?;
                Some(serde_json::json!({
                    "cell": id,
                    "epoch": cell.epoch,
                    "lease_expires_at": cell.lease_expires_at,
                    "replicated": cell.dirty == cell.published,
                }))
            })
            .collect();
        serde_json::json!({
            "node": self.config.node_id,
            "bucket": self.config.bucket.is_some(),
            "cells": cells,
        })
    }
}

/// Parse `(epoch, generation)` back out of a snapshot key
/// (`…/state/<epoch>-<generation>.db`).
fn parse_snapshot_generation(key: &str) -> Option<(u64, u64)> {
    let name = key.rsplit('/').next()?.strip_suffix(".db")?;
    let (epoch, generation) = name.split_once('-')?;
    Some((epoch.parse().ok()?, generation.parse().ok()?))
}

// ---------------------------------------------------------------------------
// HTTP layer — the run-store REST protocol
// ---------------------------------------------------------------------------

use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

const JSON_CONTENT: [(&str, &str); 1] = [("content-type", "application/json")];

#[derive(Clone)]
struct AppState {
    node: Arc<Node>,
    /// Bearer token; requests must match when set (CHIDORI_RUN_STORE_TOKEN,
    /// the same knob the Durable Object worker uses).
    token: Option<String>,
}

fn router(state: AppState) -> axum::Router {
    use axum::routing::get;
    axum::Router::new()
        .route("/status", get(http_status))
        .route("/runs", get(http_runs))
        .route(
            "/runs/{id}/records",
            get(http_records_get)
                .post(http_records_post)
                .put(http_records_put),
        )
        .route("/runs/{id}/blobs", get(http_blobs_list))
        .route(
            "/runs/{id}/blobs/{*key}",
            get(http_blob_get)
                .put(http_blob_put)
                .delete(http_blob_delete),
        )
        .route("/registry", get(http_registry_list))
        .route(
            "/registry/{name}",
            get(http_registry_get).put(http_registry_put),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
}

async fn auth_middleware(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if let Some(expected) = &state.token {
        use subtle::ConstantTimeEq as _;
        let presented = request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        if presented.as_bytes().ct_eq(expected.as_bytes()).unwrap_u8() != 1 {
            return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        }
    }
    next.run(request).await
}

/// Cell operations do SQLite and bucket round-trips (the bucket client blocks
/// on a relay thread), so every handler hops to the blocking pool.
async fn blocking(f: impl FnOnce() -> Response + Send + 'static) -> Response {
    match tokio::task::spawn_blocking(f).await {
        Ok(response) => response,
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cell store worker panicked: {err}"),
        )
            .into_response(),
    }
}

fn error_response(err: CellError) -> Response {
    match err {
        CellError::OwnedElsewhere {
            owner,
            lease_expires_at,
        } => (
            StatusCode::CONFLICT,
            axum::Json(serde_json::json!({
                "error": "cell owned elsewhere",
                "owner": owner,
                "lease_expires_at": lease_expires_at,
            })),
        )
            .into_response(),
        CellError::Other(err) => {
            tracing::warn!(error = %format!("{err:#}"), "cell store request failed");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")).into_response()
        }
    }
}

/// `Some(rejection)` when the id/name is not a safe single path component.
fn check_component(value: &str, what: &str) -> Option<Response> {
    if valid_component(value) {
        None
    } else {
        Some((StatusCode::BAD_REQUEST, format!("invalid {what} `{value}`")).into_response())
    }
}

async fn http_status(State(state): State<AppState>) -> Response {
    blocking(move || axum::Json(state.node.status()).into_response()).await
}

async fn http_runs(State(state): State<AppState>) -> Response {
    blocking(move || match state.node.runs_list() {
        Ok(ids) => axum::Json(ids).into_response(),
        Err(err) => error_response(err.into()),
    })
    .await
}

async fn http_records_get(State(state): State<AppState>, AxPath(id): AxPath<String>) -> Response {
    blocking(move || {
        if let Some(response) = check_component(&id, "run id") {
            return response;
        }
        match state.node.with_cell_opt(&id, |cell| cell.records_json()) {
            Ok(Some(Some(body))) => (JSON_CONTENT, body).into_response(),
            Ok(Some(None)) | Ok(None) => (StatusCode::NOT_FOUND, "no journal").into_response(),
            Err(err) => error_response(err),
        }
    })
    .await
}

async fn http_records_post(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    body: axum::body::Bytes,
) -> Response {
    blocking(move || {
        if let Some(response) = check_component(&id, "run id") {
            return response;
        }
        let record: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(err) => {
                return (StatusCode::BAD_REQUEST, format!("invalid record: {err}")).into_response()
            }
        };
        if let Err(err) = state.node.run_register(&id) {
            return error_response(err.into());
        }
        match state
            .node
            .with_cell(&id, |cell| cell.record_append(&record))
        {
            Ok(()) => "ok".into_response(),
            Err(err) => error_response(err),
        }
    })
    .await
}

async fn http_records_put(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    body: axum::body::Bytes,
) -> Response {
    blocking(move || {
        if let Some(response) = check_component(&id, "run id") {
            return response;
        }
        let records: Vec<serde_json::Value> = match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(err) => {
                return (StatusCode::BAD_REQUEST, format!("invalid journal: {err}")).into_response()
            }
        };
        if let Err(err) = state.node.run_register(&id) {
            return error_response(err.into());
        }
        match state
            .node
            .with_cell(&id, |cell| cell.records_replace(&records))
        {
            Ok(()) => "ok".into_response(),
            Err(err) => error_response(err),
        }
    })
    .await
}

async fn http_blobs_list(State(state): State<AppState>, AxPath(id): AxPath<String>) -> Response {
    blocking(move || {
        if let Some(response) = check_component(&id, "run id") {
            return response;
        }
        match state.node.with_cell_opt(&id, |cell| cell.blob_list()) {
            Ok(Some(keys)) => axum::Json(keys).into_response(),
            Ok(None) => axum::Json(Vec::<String>::new()).into_response(),
            Err(err) => error_response(err),
        }
    })
    .await
}

async fn http_blob_get(
    State(state): State<AppState>,
    AxPath((id, key)): AxPath<(String, String)>,
) -> Response {
    blocking(move || {
        if let Some(response) = check_component(&id, "run id") {
            return response;
        }
        match state.node.with_cell_opt(&id, |cell| cell.blob_get(&key)) {
            Ok(Some(Some(bytes))) => bytes.into_response(),
            Ok(Some(None)) | Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
            Err(err) => error_response(err),
        }
    })
    .await
}

async fn http_blob_put(
    State(state): State<AppState>,
    AxPath((id, key)): AxPath<(String, String)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    blocking(move || {
        if let Some(response) = check_component(&id, "run id") {
            return response;
        }
        if let Err(err) = state.node.run_register(&id) {
            return error_response(err.into());
        }
        let condition = Precondition::from_headers(&headers);
        match state.node.with_cell(&id, |cell| {
            if !cell.precondition_holds(&key, &condition)? {
                return Ok(false);
            }
            cell.blob_put(&key, &body)?;
            Ok(true)
        }) {
            Ok(true) => "ok".into_response(),
            Ok(false) => precondition_failed(),
            Err(err) => error_response(err),
        }
    })
    .await
}

async fn http_blob_delete(
    State(state): State<AppState>,
    AxPath((id, key)): AxPath<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Response {
    blocking(move || {
        if let Some(response) = check_component(&id, "run id") {
            return response;
        }
        let condition = Precondition::from_headers(&headers);
        // If no cell exists anywhere, every key in it is absent — so an
        // `If-Match` can never be satisfied, while an unconditional or
        // `If-None-Match: *` delete is already in its requested state.
        let satisfied_by_absence = !matches!(condition, Precondition::Matches(_));
        // Deleting from a cell that exists nowhere is Ok without conjuring
        // one (mirrors the RunStore delete-absent contract).
        match state.node.with_cell_opt(&id, |cell| {
            if !cell.precondition_holds(&key, &condition)? {
                return Ok(false);
            }
            cell.blob_delete(&key)?;
            Ok(true)
        }) {
            Ok(Some(true)) => "ok".into_response(),
            Ok(Some(false)) => precondition_failed(),
            Ok(None) if satisfied_by_absence => "ok".into_response(),
            Ok(None) => precondition_failed(),
            Err(err) => error_response(err),
        }
    })
    .await
}

/// A conditional write whose precondition did not hold. 412 is the protocol's
/// "you lost the compare-and-swap" — a normal outcome the client retries after
/// re-reading, distinct from 409 (this node no longer owns the cell at all).
fn precondition_failed() -> Response {
    (StatusCode::PRECONDITION_FAILED, "precondition failed").into_response()
}

async fn http_registry_list(State(state): State<AppState>) -> Response {
    blocking(move || match state.node.registry_entries() {
        Ok(entries) => (JSON_CONTENT, format!("[{}]", entries.join(","))).into_response(),
        Err(err) => error_response(err.into()),
    })
    .await
}

async fn http_registry_get(
    State(state): State<AppState>,
    AxPath(name): AxPath<String>,
) -> Response {
    blocking(move || {
        if let Some(response) = check_component(&name, "registry name") {
            return response;
        }
        match state.node.registry_get(&name) {
            Ok(Some(entry)) => (JSON_CONTENT, entry).into_response(),
            Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
            Err(err) => error_response(err.into()),
        }
    })
    .await
}

async fn http_registry_put(
    State(state): State<AppState>,
    AxPath(name): AxPath<String>,
    body: String,
) -> Response {
    blocking(move || {
        if let Some(response) = check_component(&name, "registry name") {
            return response;
        }
        match state.node.registry_put(&name, &body) {
            Ok(()) => "ok".into_response(),
            Err(err) => error_response(err.into()),
        }
    })
    .await
}

// ---------------------------------------------------------------------------
// CLI entry
// ---------------------------------------------------------------------------

/// `chidori cell-store`: build the node from CLI flags + environment, run the
/// sync loop and the HTTP server, release every cell on shutdown.
pub fn cmd_cell_store(
    listen: &str,
    bucket: Option<&str>,
    data_dir: &Path,
    node_id: Option<String>,
    lease_secs: u64,
    sync_secs: u64,
    idle_secs: u64,
) -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();

    let bucket = match bucket {
        Some(value) => Some(
            S3BlobStore::from_env(value)
                .with_context(|| format!("configuring cell store bucket {value}"))?,
        ),
        None => None,
    };
    let node_id = match node_id {
        Some(id) => id,
        None => persisted_node_id(data_dir)?,
    };
    anyhow::ensure!(
        valid_component(&node_id),
        "invalid node id `{node_id}` (allowed: ASCII letters, digits, `-`, `_`, `.`, `@`)"
    );
    let node = Node::new(CellStoreConfig {
        data_dir: data_dir.to_path_buf(),
        node_id,
        bucket,
        lease_ttl: chrono::Duration::seconds(lease_secs.max(1) as i64),
        idle_hibernate: std::time::Duration::from_secs(idle_secs.max(1)),
    })?;

    match &node.config.bucket {
        Some(bucket) => eprintln!(
            "Cell store node `{}`: one SQLite cell per run, replicated to {:?}",
            node.node_id(),
            bucket
        ),
        None => eprintln!(
            "Cell store node `{}`: single-node mode (no bucket — cells are local-only)",
            node.node_id()
        ),
    }
    eprintln!("Point Chidori at it:  export CHIDORI_RUN_STORE=\"http://{listen}\"");

    // Sync loop: renewals must land well inside the lease TTL regardless of
    // how coarse --sync-secs is.
    let tick_every = std::time::Duration::from_secs(sync_secs.max(1)).min(
        std::time::Duration::from_secs((lease_secs.max(1)).div_ceil(3)),
    );
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let loop_node = node.clone();
    let loop_stop = stop.clone();
    let sync_thread = std::thread::Builder::new()
        .name("chidori-cell-sync".to_string())
        .spawn(move || {
            while !loop_stop.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(tick_every);
                loop_node.tick();
            }
        })
        .context("spawning cell-store sync thread")?;

    let state = AppState {
        node: node.clone(),
        token: std::env::var("CHIDORI_RUN_STORE_TOKEN").ok(),
    };
    if state.token.is_none() {
        eprintln!("Warning: CHIDORI_RUN_STORE_TOKEN not set — requests are unauthenticated");
    }
    let listen = listen.to_string();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building cell-store runtime")?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(&listen)
            .await
            .with_context(|| format!("binding {listen}"))?;
        eprintln!("Cell store listening on http://{}", listener.local_addr()?);
        axum::serve(listener, router(state))
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
                eprintln!("Shutting down: releasing cells…");
            })
            .await
            .context("cell store server failed")
    })?;

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = sync_thread.join();
    // Publish + release everything so the fleet can claim these cells now
    // rather than after a lease TTL.
    node.shutdown();
    Ok(())
}

/// A node identity generated once and persisted in the data directory, so
/// restarts reclaim their own leases instead of waiting out the TTL.
fn persisted_node_id(data_dir: &Path) -> Result<String> {
    let path = data_dir.join("node-id");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim().to_string();
        if valid_component(&existing) {
            return Ok(existing);
        }
    }
    let id = format!("node-{}", uuid::Uuid::new_v4());
    std::fs::create_dir_all(data_dir)?;
    std::fs::write(&path, &id).with_context(|| format!("writing {}", path.display()))?;
    Ok(id)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::call_log::CallRecord;
    use crate::runtime::store::{HttpRelay, HttpRunStore, RunStore};

    /// An in-process S3-compatible endpoint with the one primitive the
    /// ownership protocol needs beyond plain PUT/GET/DELETE/LIST: conditional
    /// create (`If-None-Match: *` → 412 when the key exists). Signatures are
    /// accepted but not validated.
    fn spawn_mock_bucket() -> String {
        use axum::extract::{Path as AxPath, Query, State};
        use std::collections::BTreeMap;

        type Objects = Arc<Mutex<BTreeMap<String, Vec<u8>>>>;
        let objects: Objects = Default::default();

        fn xml_escape(value: &str) -> String {
            value
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
        }

        async fn list_bucket(
            State(objects): State<Objects>,
            AxPath(bucket): AxPath<String>,
            Query(params): Query<std::collections::HashMap<String, String>>,
        ) -> Response {
            let prefix = params.get("prefix").cloned().unwrap_or_default();
            let delimiter = params.get("delimiter").cloned();
            let full_prefix = format!("{bucket}/{prefix}");
            let mut keys = Vec::new();
            let mut common = BTreeSet::new();
            for key in objects.lock().unwrap().keys() {
                let Some(rest) = key.strip_prefix(&full_prefix) else {
                    continue;
                };
                match delimiter.as_deref() {
                    Some(d) if rest.contains(d) => {
                        let head = &rest[..rest.find(d).unwrap() + d.len()];
                        common.insert(format!("{prefix}{head}"));
                    }
                    _ => keys.push(format!("{prefix}{rest}")),
                }
            }
            let mut xml = String::from(
                "<?xml version=\"1.0\"?><ListBucketResult><IsTruncated>false</IsTruncated>",
            );
            for key in keys {
                xml.push_str(&format!(
                    "<Contents><Key>{}</Key></Contents>",
                    xml_escape(&key)
                ));
            }
            for p in common {
                xml.push_str(&format!(
                    "<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>",
                    xml_escape(&p)
                ));
            }
            xml.push_str("</ListBucketResult>");
            (StatusCode::OK, xml).into_response()
        }

        async fn object(
            State(objects): State<Objects>,
            AxPath((bucket, key)): AxPath<(String, String)>,
            method: axum::http::Method,
            headers: axum::http::HeaderMap,
            body: axum::body::Bytes,
        ) -> Response {
            let full = format!("{bucket}/{key}");
            match method.as_str() {
                "PUT" => {
                    let mut objects = objects.lock().unwrap();
                    let create_only = headers
                        .get("if-none-match")
                        .is_some_and(|v| v.to_str().unwrap_or("") == "*");
                    if create_only && objects.contains_key(&full) {
                        return (StatusCode::PRECONDITION_FAILED, "exists").into_response();
                    }
                    objects.insert(full, body.to_vec());
                    StatusCode::OK.into_response()
                }
                "GET" => match objects.lock().unwrap().get(&full) {
                    Some(bytes) => (StatusCode::OK, bytes.clone()).into_response(),
                    None => StatusCode::NOT_FOUND.into_response(),
                },
                "DELETE" => {
                    objects.lock().unwrap().remove(&full);
                    StatusCode::NO_CONTENT.into_response()
                }
                _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
            }
        }

        let app = axum::Router::new()
            .route("/{bucket}", axum::routing::get(list_bucket))
            .route("/{bucket}/{*key}", axum::routing::any(object))
            .with_state(objects);
        spawn_axum(app)
    }

    /// Serve `app` on an ephemeral loopback port from a dedicated thread with
    /// its own runtime (callers use blocking clients), returning the base URL.
    fn spawn_axum(app: axum::Router) -> String {
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

    fn test_node(
        dir: &Path,
        node_id: &str,
        bucket: Option<Arc<S3BlobStore>>,
        lease_ms: i64,
        idle: std::time::Duration,
    ) -> Arc<Node> {
        Node::new(CellStoreConfig {
            data_dir: dir.to_path_buf(),
            node_id: node_id.to_string(),
            bucket,
            lease_ttl: chrono::Duration::milliseconds(lease_ms),
            idle_hibernate: idle,
        })
        .unwrap()
    }

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

    fn append(node: &Node, cell: &str, seq: u64, function: &str) -> Result<(), CellError> {
        let value = serde_json::to_value(record(seq, function)).unwrap();
        node.with_cell(cell, |c| c.record_append(&value))
    }

    fn record_seqs(node: &Node, cell: &str) -> Vec<u64> {
        let json = node
            .with_cell(cell, |c| c.records_json())
            .unwrap()
            .unwrap_or_else(|| "[]".to_string());
        serde_json::from_str::<Vec<CallRecord>>(&json)
            .unwrap()
            .iter()
            .map(|r| r.seq)
            .collect()
    }

    const LONG_IDLE: std::time::Duration = std::time::Duration::from_secs(3600);

    #[test]
    fn conditional_put_is_create_only() {
        let endpoint = spawn_mock_bucket();
        let bucket = S3BlobStore::for_tests(&endpoint, "cells", "");
        assert!(bucket.put_object_if_absent("k", b"first").unwrap());
        assert!(!bucket.put_object_if_absent("k", b"second").unwrap());
        assert_eq!(bucket.get_object("k").unwrap().unwrap(), b"first");
    }

    /// The full run-store protocol over HTTP against a single node — the same
    /// client (`HttpRunStore`) that talks to the Cloudflare worker.
    #[test]
    fn serves_the_run_store_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let node = test_node(dir.path(), "node-a", None, 60_000, LONG_IDLE);
        let base = spawn_axum(router(AppState {
            node: node.clone(),
            token: None,
        }));
        let relay = HttpRelay::new(base.clone(), None);
        let store = HttpRunStore::new(relay.clone(), "run-http");

        assert!(store.load_call_log().unwrap().is_none());
        store.append_record(&record(1, "prompt")).unwrap();
        store.append_record(&record(2, "tool")).unwrap();
        store.flush().unwrap();
        let loaded = store.load_call_log().unwrap().unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[1].function, "tool");

        // Checkpoint rewrite replaces; re-appending a seq replaces its record.
        store
            .write_call_log(&[record(1, "prompt"), record(2, "tool"), record(3, "signal")])
            .unwrap();
        store.append_record(&record(3, "signal_retry")).unwrap();
        store.flush().unwrap();
        let loaded = store.load_call_log().unwrap().unwrap();
        assert_eq!(
            loaded.iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(loaded[2].function, "signal_retry");

        // Blobs round-trip, list, delete; absent delete is Ok.
        store.put_blob("manifest.json", b"{\"a\":1}").unwrap();
        store.put_blob("signals/inbox.json", b"[]").unwrap();
        store.flush().unwrap();
        assert_eq!(
            store.get_blob("manifest.json").unwrap().unwrap(),
            b"{\"a\":1}"
        );
        let keys = store.list_blobs().unwrap();
        assert!(keys.contains(&"manifest.json".to_string()));
        assert!(keys.contains(&"signals/inbox.json".to_string()));
        store.delete_blob("manifest.json").unwrap();
        store.flush().unwrap();
        assert!(store.get_blob("manifest.json").unwrap().is_none());
        store.delete_blob("manifest.json").unwrap();
        store.flush().unwrap();

        // Run index + registry endpoints.
        let (status, body) = relay
            .request_full(
                "GET",
                format!("{base}/runs"),
                None,
                "application/json",
                vec![],
            )
            .unwrap();
        assert_eq!(status, 200);
        let runs: Vec<String> = serde_json::from_slice(&body).unwrap();
        assert_eq!(runs, vec!["run-http".to_string()]);
        let entry = br#"{"name":"triager","run_id":"run-http"}"#;
        let (status, _) = relay
            .request_full(
                "PUT",
                format!("{base}/registry/triager"),
                Some(entry.to_vec()),
                "application/json",
                vec![],
            )
            .unwrap();
        assert_eq!(status, 200);
        let (status, body) = relay
            .request_full(
                "GET",
                format!("{base}/registry/triager"),
                None,
                "application/json",
                vec![],
            )
            .unwrap();
        assert_eq!(status, 200);
        let entry: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(entry["run_id"], "run-http");
        let (status, body) = relay
            .request_full(
                "GET",
                format!("{base}/registry"),
                None,
                "application/json",
                vec![],
            )
            .unwrap();
        assert_eq!(status, 200);
        let entries: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(entries.len(), 1);

        // A read of an unknown run must not create a cell.
        assert!(HttpRunStore::new(relay.clone(), "run-never")
            .load_call_log()
            .unwrap()
            .is_none());
        assert!(!dir.path().join("cells/run-never.sqlite3").exists());
    }

    #[test]
    fn bearer_token_is_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let node = test_node(dir.path(), "node-a", None, 60_000, LONG_IDLE);
        let base = spawn_axum(router(AppState {
            node,
            token: Some("sekrit".to_string()),
        }));
        let anon = HttpRelay::new(base.clone(), None);
        let (status, _) = anon
            .request_full(
                "GET",
                format!("{base}/runs"),
                None,
                "application/json",
                vec![],
            )
            .unwrap();
        assert_eq!(status, 401);
        let authed = HttpRelay::new(base.clone(), Some("sekrit".to_string()));
        let (status, _) = authed
            .request_full(
                "GET",
                format!("{base}/runs"),
                None,
                "application/json",
                vec![],
            )
            .unwrap();
        assert_eq!(status, 200);
    }

    /// The celld ownership core: exactly one owner via create-only CAS, lease
    /// expiry hands the cell over with an epoch bump and a bucket restore,
    /// and the fenced previous owner is refused and drops its copy.
    #[test]
    fn single_owner_takeover_and_fencing() {
        let endpoint = spawn_mock_bucket();
        let bucket = S3BlobStore::for_tests(&endpoint, "cells", "fleet");
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let node_a = test_node(dir_a.path(), "node-a", Some(bucket.clone()), 400, LONG_IDLE);
        let node_b = test_node(dir_b.path(), "node-b", Some(bucket.clone()), 400, LONG_IDLE);

        append(&node_a, "run-x", 1, "prompt").unwrap();
        append(&node_a, "run-x", 2, "tool").unwrap();
        node_a.tick(); // replicate to the bucket

        // While A's lease is live, B is refused and told who owns the cell.
        match append(&node_b, "run-x", 99, "intruder") {
            Err(CellError::OwnedElsewhere { owner, .. }) => assert_eq!(owner, "node-a"),
            other => panic!("expected OwnedElsewhere, got {other:?}"),
        }

        // After expiry B takes over: next epoch, state restored from the
        // bucket snapshot.
        std::thread::sleep(std::time::Duration::from_millis(600));
        assert_eq!(record_seqs(&node_b, "run-x"), vec![1, 2]);
        append(&node_b, "run-x", 3, "signal").unwrap();

        // A is fenced: its stale copy is dropped and requests are refused.
        match append(&node_a, "run-x", 4, "stale") {
            Err(CellError::OwnedElsewhere { owner, .. }) => assert_eq!(owner, "node-b"),
            other => panic!("expected OwnedElsewhere, got {other:?}"),
        }
        assert_eq!(record_seqs(&node_b, "run-x"), vec![1, 2, 3]);
    }

    /// Hibernation: an idle cell replicates, is published unowned with its
    /// epoch intact, and closes. Another node claims it immediately (no TTL
    /// wait) and restores the database.
    #[test]
    fn hibernate_releases_and_restores_elsewhere() {
        let endpoint = spawn_mock_bucket();
        let bucket = S3BlobStore::for_tests(&endpoint, "cells", "");
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let node_a = test_node(
            dir_a.path(),
            "node-a",
            Some(bucket.clone()),
            60_000,
            std::time::Duration::ZERO, // hibernate at the first tick
        );
        let node_b = test_node(
            dir_b.path(),
            "node-b",
            Some(bucket.clone()),
            60_000,
            LONG_IDLE,
        );

        append(&node_a, "run-h", 1, "prompt").unwrap();
        node_a
            .with_cell("run-h", |c| c.blob_put("manifest.json", b"{}"))
            .unwrap();
        node_a.tick(); // hibernates: final publish + unowned meta
        assert_eq!(node_a.status()["cells"].as_array().unwrap().len(), 0);

        // The bucket shows epoch 1, unowned (lease expired), snapshot set.
        let meta = node_a
            .latest_meta(&bucket, "run-h")
            .unwrap()
            .expect("meta published");
        assert_eq!(meta.epoch, 1);
        assert!(meta.claimable(Utc::now()));
        assert!(meta.snapshot.is_some());

        // B claims the next epoch at once and sees the full cell.
        assert_eq!(record_seqs(&node_b, "run-h"), vec![1]);
        let manifest = node_b
            .with_cell("run-h", |c| c.blob_get("manifest.json"))
            .unwrap();
        assert_eq!(manifest.unwrap(), b"{}");
        let meta = node_b.latest_meta(&bucket, "run-h").unwrap().unwrap();
        assert_eq!(meta.epoch, 2);
        assert_eq!(meta.owner, "node-b");
    }

    /// A node restart reclaims its own live lease and reuses its local
    /// database — including writes that never reached the bucket. This is the
    /// local-disk-primary guarantee: a crash loses nothing unless the disk is
    /// lost too.
    #[test]
    fn restart_reclaims_own_cells_losslessly() {
        let endpoint = spawn_mock_bucket();
        let bucket = S3BlobStore::for_tests(&endpoint, "cells", "");
        let dir = tempfile::tempdir().unwrap();
        let node = test_node(
            dir.path(),
            "node-a",
            Some(bucket.clone()),
            60_000,
            LONG_IDLE,
        );

        append(&node, "run-r", 1, "prompt").unwrap();
        node.tick(); // published: seq 1 is in the bucket
        append(&node, "run-r", 2, "tool").unwrap(); // never published
        drop(node); // crash: no release, no final publish

        let reborn = test_node(
            dir.path(),
            "node-a",
            Some(bucket.clone()),
            60_000,
            LONG_IDLE,
        );
        assert_eq!(record_seqs(&reborn, "run-r"), vec![1, 2]);
        // Same epoch — a reclaim, not a takeover.
        let meta = reborn.latest_meta(&bucket, "run-r").unwrap().unwrap();
        assert_eq!(meta.epoch, 1);

        // A *different* node in the same situation must NOT see the
        // unpublished write (it restores from the snapshot), and only after
        // the lease expires.
        drop(reborn);
        let dir_b = tempfile::tempdir().unwrap();
        let node_b = test_node(
            dir_b.path(),
            "node-b",
            Some(bucket.clone()),
            60_000,
            LONG_IDLE,
        );
        match append(&node_b, "run-r", 9, "early") {
            Err(CellError::OwnedElsewhere { owner, .. }) => assert_eq!(owner, "node-a"),
            other => panic!("expected OwnedElsewhere, got {other:?}"),
        }
    }

    /// `GET /runs` unions this node's index with the bucket's cells, so a
    /// fresh node lists runs written through nodes that are gone.
    #[test]
    fn run_listing_spans_the_fleet() {
        let endpoint = spawn_mock_bucket();
        let bucket = S3BlobStore::for_tests(&endpoint, "cells", "");
        let dir_a = tempfile::tempdir().unwrap();
        let node_a = test_node(
            dir_a.path(),
            "node-a",
            Some(bucket.clone()),
            60_000,
            std::time::Duration::ZERO,
        );
        node_a.run_register("run-1").unwrap();
        append(&node_a, "run-1", 1, "prompt").unwrap();
        node_a.tick(); // replicate + hibernate
        drop(node_a);

        let dir_b = tempfile::tempdir().unwrap();
        let node_b = test_node(dir_b.path(), "node-b", Some(bucket), 60_000, LONG_IDLE);
        assert_eq!(node_b.runs_list().unwrap(), vec!["run-1".to_string()]);
    }

    /// Conditional blob writes are evaluated by the server, so the client's
    /// `compare_and_swap_blob` is atomic rather than a read-compare-write.
    #[test]
    fn conditional_writes_are_evaluated_server_side() {
        let dir = tempfile::tempdir().unwrap();
        let node = test_node(dir.path(), "node-a", None, 60_000, LONG_IDLE);
        let base = spawn_axum(router(AppState { node, token: None }));
        let store = HttpRunStore::new(HttpRelay::new(base.clone(), None), "run-cas");

        assert!(store
            .compare_and_swap_blob("lease.json", None, Some(b"one"))
            .unwrap());
        // The key now exists, so an absent-precondition write is refused…
        assert!(!store
            .compare_and_swap_blob("lease.json", None, Some(b"two"))
            .unwrap());
        // …a swap against stale bytes is refused…
        assert!(!store
            .compare_and_swap_blob("lease.json", Some(b"stale"), Some(b"two"))
            .unwrap());
        // …and one against the current bytes succeeds.
        assert!(store
            .compare_and_swap_blob("lease.json", Some(b"one"), Some(b"three"))
            .unwrap());
        assert_eq!(store.get_blob("lease.json").unwrap().unwrap(), b"three");

        assert!(!store
            .compare_and_swap_blob("lease.json", Some(b"one"), None)
            .unwrap());
        assert!(store
            .compare_and_swap_blob("lease.json", Some(b"three"), None)
            .unwrap());
        assert!(store.get_blob("lease.json").unwrap().is_none());

        // A run with no cell at all: absence satisfies an absent-precondition
        // delete, but never an `If-Match` for specific bytes.
        let absent = HttpRunStore::new(HttpRelay::new(base, None), "run-never-existed");
        assert!(absent.compare_and_swap_blob("k", None, None).unwrap());
        assert!(!absent
            .compare_and_swap_blob("k", Some(b"ghost"), None)
            .unwrap());
    }

    /// The headline claim: a run lease taken through the cell store is
    /// *enforced*, not advisory. Eight processes race for the same run with no
    /// coordination between them; exactly one may win, and every loser must be
    /// told who did. Each gets its own relay so the requests genuinely
    /// interleave at the server rather than serializing on one FIFO thread.
    #[test]
    fn lease_is_enforced_against_concurrent_processes() {
        let dir = tempfile::tempdir().unwrap();
        let node = test_node(dir.path(), "node-a", None, 60_000, LONG_IDLE);
        let base = spawn_axum(router(AppState { node, token: None }));
        let ttl = chrono::Duration::seconds(60);

        let winners: Vec<String> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|i| {
                    let base = base.clone();
                    scope.spawn(move || {
                        let store = HttpRunStore::new(HttpRelay::new(base, None), "run-contended");
                        let owner = format!("proc-{i}");
                        match crate::runtime::store::acquire_lease(&store, &owner, ttl).unwrap() {
                            Ok(lease) => Ok(lease.owner),
                            Err(holder) => Err(holder.owner),
                        }
                    })
                })
                .collect();
            let (won, lost): (Vec<_>, Vec<_>) = handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .partition(|r| r.is_ok());
            let winners: Vec<String> = won.into_iter().map(Result::unwrap).collect();
            // Every loser names the winner rather than erroring out.
            for holder in lost.into_iter().map(Result::unwrap_err) {
                assert_eq!(Some(&holder), winners.first(), "loser named a non-winner");
            }
            winners
        });
        assert_eq!(winners.len(), 1, "exactly one process may hold the lease");

        // The holder renews; a different process is still refused; a release
        // hands ownership over cleanly.
        let store = HttpRunStore::new(HttpRelay::new(base.clone(), None), "run-contended");
        let winner = winners.into_iter().next().unwrap();
        assert!(crate::runtime::store::acquire_lease(&store, &winner, ttl)
            .unwrap()
            .is_ok());
        assert_eq!(
            crate::runtime::store::acquire_lease(&store, "outsider", ttl)
                .unwrap()
                .unwrap_err()
                .owner,
            winner
        );
        crate::runtime::store::release_lease(&store, &winner).unwrap();
        assert!(
            crate::runtime::store::acquire_lease(&store, "outsider", ttl)
                .unwrap()
                .is_ok()
        );
    }

    /// A node fenced by a takeover answers 409, and the client turns that into
    /// an ordinary standdown — the run is someone else's, not broken.
    #[test]
    fn fenced_node_makes_the_client_stand_down() {
        let endpoint = spawn_mock_bucket();
        let bucket = S3BlobStore::for_tests(&endpoint, "cells", "");
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let node_a = test_node(dir_a.path(), "node-a", Some(bucket.clone()), 400, LONG_IDLE);
        let node_b = test_node(dir_b.path(), "node-b", Some(bucket), 60_000, LONG_IDLE);
        let base = spawn_axum(router(AppState {
            node: node_a.clone(),
            token: None,
        }));
        let store = HttpRunStore::new(HttpRelay::new(base, None), "run-fenced");

        append(&node_a, "run-fenced", 1, "prompt").unwrap();
        node_a.tick(); // replicate so B can restore
        assert!(store.get_blob("anything").unwrap().is_none());

        // A's lease lapses and B takes the cell; A is now fenced.
        std::thread::sleep(std::time::Duration::from_millis(600));
        assert_eq!(record_seqs(&node_b, "run-fenced"), vec![1]);

        // Synchronous requests through A are recognized as fencing.
        let err = store
            .get_blob("manifest.json")
            .expect_err("a fenced node must refuse the read");
        assert_eq!(
            crate::runtime::store::fenced_owner(&err)
                .expect("409 must be recognized as fencing")
                .owner,
            "node-b"
        );

        // Pipelined writes (the besteffort default) are enqueued without
        // waiting, so a fenced write is not refused inline — it lands at the
        // relay barrier, which is exactly why fencing is observed at lease
        // boundaries rather than per effect (docs/durable-storage.md §Known
        // limits). Assert the real behavior so the gap stays visible.
        store.put_blob("manifest.json", b"{}").unwrap();
        assert!(store.flush().is_ok(), "besteffort logs and continues");
        let holder =
            crate::runtime::store::acquire_lease(&store, "proc-a", chrono::Duration::seconds(60))
                .expect("fencing is a standdown, not an error")
                .expect_err("a fenced node must not believe it holds the lease");
        assert_eq!(holder.owner, "node-b");
    }

    #[test]
    fn rejects_hostile_ids() {
        assert!(!valid_component(""));
        assert!(!valid_component(".."));
        assert!(!valid_component("a/b"));
        assert!(!valid_component("a\\b"));
        assert!(!valid_component("a b"));
        assert!(valid_component("run-1_2.3@x"));
    }
}
