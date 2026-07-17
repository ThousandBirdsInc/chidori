//! Session and memory persistence.
//!
//! Provides two backends:
//!   * SQLite (default: `.chidori/sessions.sqlite3` next to the agent's
//!     `.chidori/runs/`; override the path with CHIDORI_DB_PATH)
//!   * in-memory (opt-in via CHIDORI_DB_PATH=:memory:, for dev loops that
//!     should leave no state behind)
//!
//! The server holds a `SessionStore` trait object so it can switch backends
//! without touching the HTTP handlers. Sessions are serialized as a JSON blob
//! keyed by id; the call log is stored inline so a single SELECT retrieves
//! everything.

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::runtime::call_log::CallRecord;
use crate::runtime::context::PendingApproval;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
    Paused,
    /// Paused waiting for the operator to approve/deny a policy-gated call.
    AwaitingApproval,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSession {
    pub id: String,
    #[serde(default)]
    pub run_id: Option<String>,
    pub status: SessionStatus,
    pub input: Value,
    pub output: Option<Value>,
    pub call_log: Vec<CallRecord>,
    pub error: Option<String>,
    pub pending_seq: Option<u64>,
    pub pending_prompt: Option<String>,
    /// The artifact under review for an `input()` pause (`opts.details`) —
    /// surfaced alongside `pending_prompt` so an approval UI can show what it
    /// is approving. Defaulted for sessions stored before the field existed.
    #[serde(default)]
    pub pending_details: Option<String>,
    /// Set when status == Paused on a `chidori.signal(name)` listen point.
    /// Carries the listen-point name so the view advertises which signal the
    /// run is awaiting and the delivery endpoint can match `body.name` against
    /// it. A signal pause reuses `SessionStatus::Paused`; this field (not the
    /// status) distinguishes it from an `input()` pause. See `docs/signals.md`.
    #[serde(default)]
    pub pending_signal_name: Option<String>,
    /// The full awaited name set for a signal pause: `[name]` for
    /// `chidori.signal(name)`, the listen set for the fan-in `chidori.signal(names[])`.
    /// The delivery endpoint matches `body.name` against ANY of these. Empty
    /// when the session is not paused on a signal (sessions persisted before
    /// `signalAny` deserialize as empty; fall back to `pending_signal_name`).
    #[serde(default)]
    pub pending_signal_names: Vec<String>,
    /// Absolute deadline for a signal pause created with `timeoutMs`
    /// (`docs/signals.md` Phase 2). When it passes, the supervising server
    /// resolves the pause with the `{ timedOut: true }` sentinel. Persisted so
    /// a restarted server can re-arm the timer.
    #[serde(default)]
    pub pending_signal_deadline: Option<chrono::DateTime<chrono::Utc>>,
    /// Set when status == AwaitingApproval. Carries the (target, args)
    /// the policy is asking about so the UI can render a prompt.
    #[serde(default)]
    pub pending_approval: Option<PendingApproval>,
    /// Per-session list of approvals the operator has already granted.
    /// Seeded into the PolicyCache on every re-run so the agent doesn't
    /// have to ask twice for the same (target, args) within a session.
    #[serde(default)]
    pub approvals: Vec<(String, Value)>,
    /// Built-in policy profile selected at session creation (e.g. "untrusted",
    /// "supervised"). Layered on the server policy with stricter-wins
    /// semantics on every run of this session, including resume/approve
    /// replays — it can tighten the server policy but never relax it.
    #[serde(default)]
    pub policy_profile: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub trait SessionStore: Send + Sync {
    fn put(&self, session: &StoredSession) -> Result<()>;
    fn get(&self, id: &str) -> Result<Option<StoredSession>>;
    fn list(&self) -> Result<Vec<StoredSession>>;
    #[allow(dead_code)] // Exposed for cleanup tools; no current caller.
    fn delete(&self, id: &str) -> Result<()>;
}

/// In-memory store. Opt-in via `CHIDORI_DB_PATH=:memory:`, for dev loops that
/// should leave no state behind.
pub struct MemoryStore {
    inner: Mutex<std::collections::HashMap<String, StoredSession>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(std::collections::HashMap::new()),
        }
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore for MemoryStore {
    fn put(&self, s: &StoredSession) -> Result<()> {
        self.inner.lock().unwrap().insert(s.id.clone(), s.clone());
        Ok(())
    }
    fn get(&self, id: &str) -> Result<Option<StoredSession>> {
        Ok(self.inner.lock().unwrap().get(id).cloned())
    }
    fn list(&self) -> Result<Vec<StoredSession>> {
        Ok(self.inner.lock().unwrap().values().cloned().collect())
    }
    fn delete(&self, id: &str) -> Result<()> {
        self.inner.lock().unwrap().remove(id);
        Ok(())
    }
}

/// SQLite-backed store. One table, sessions are stored as a single JSON blob
/// per row. This is a deliberate shortcut: we don't need query-over-fields
/// yet, and a blob column is the cheapest thing that durably persists across
/// restarts.
pub struct SqliteStore {
    #[allow(dead_code)] // Retained so `path()` can surface it to tracing / debug.
    path: PathBuf,
    conn: Mutex<rusqlite::Connection>,
}

impl SqliteStore {
    pub fn open(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        let conn = rusqlite::Connection::open(&path)
            .with_context(|| format!("opening sqlite at {}", path.display()))?;
        // WAL + NORMAL, matching the run store (`runtime/store.rs`): every
        // session state transition rewrites the session row, and the default
        // rollback journal pays a full fsync per put while blocking readers.
        // In WAL a put is one log append and readers never block on the writer.
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL").ok();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                 id TEXT PRIMARY KEY,
                 created_at TEXT NOT NULL,
                 status TEXT NOT NULL,
                 data TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_sessions_created_at
                 ON sessions(created_at DESC);",
        )?;
        Ok(Self {
            path,
            conn: Mutex::new(conn),
        })
    }

    #[allow(dead_code)]
    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

impl SessionStore for SqliteStore {
    fn put(&self, s: &StoredSession) -> Result<()> {
        let data = serde_json::to_string(s)?;
        let status = serde_json::to_string(&s.status).unwrap_or_else(|_| "\"running\"".into());
        let created_at = s.created_at.to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions (id, created_at, status, data)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET status = excluded.status, data = excluded.data",
            rusqlite::params![s.id, created_at, status, data],
        )?;
        Ok(())
    }

    fn get(&self, id: &str) -> Result<Option<StoredSession>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT data FROM sessions WHERE id = ?1")?;
        let mut rows = stmt.query(rusqlite::params![id])?;
        if let Some(row) = rows.next()? {
            let data: String = row.get::<_, String>(0)?;
            let session: StoredSession = serde_json::from_str(&data)?;
            Ok(Some(session))
        } else {
            Ok(None)
        }
    }

    fn list(&self) -> Result<Vec<StoredSession>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT data FROM sessions ORDER BY created_at DESC LIMIT 200")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            let data: String = row?;
            if let Ok(s) = serde_json::from_str::<StoredSession>(&data) {
                out.push(s);
            }
        }
        Ok(out)
    }

    fn delete(&self, id: &str) -> Result<()> {
        self.conn
            .lock()
            .unwrap()
            .execute("DELETE FROM sessions WHERE id = ?1", rusqlite::params![id])?;
        Ok(())
    }
}

/// Build the SessionStore configured by env. Durable by default: sessions go
/// to SQLite at `CHIDORI_DB_PATH`, or `<base_dir>/.chidori/sessions.sqlite3`
/// when unset. `CHIDORI_DB_PATH=:memory:` (or `memory`) opts into the
/// non-durable in-memory store. A SQLite store that fails to open is a hard
/// startup error — a durability framework must not silently downgrade to a
/// store that loses every session on restart.
pub fn build_session_store(base_dir: &std::path::Path) -> Result<std::sync::Arc<dyn SessionStore>> {
    let path = match std::env::var("CHIDORI_DB_PATH") {
        Ok(v) if matches!(v.trim(), ":memory:" | "memory") => {
            tracing::info!("session store: in-memory (CHIDORI_DB_PATH=:memory:)");
            return Ok(std::sync::Arc::new(MemoryStore::new()));
        }
        Ok(v) => PathBuf::from(v),
        Err(_) => base_dir.join(".chidori").join("sessions.sqlite3"),
    };
    let store = SqliteStore::open(path.clone()).with_context(|| {
        format!(
            "opening the session store at {} — fix the path/permissions, point CHIDORI_DB_PATH \
             somewhere writable, or set CHIDORI_DB_PATH=:memory: to explicitly opt out of \
             durable sessions",
            path.display()
        )
    })?;
    tracing::info!("session store: sqlite at {}", path.display());
    Ok(std::sync::Arc::new(store))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_session_deserializes_without_run_id() {
        let raw = serde_json::json!({
            "id": "session-1",
            "status": "completed",
            "input": null,
            "output": {"ok": true},
            "call_log": [],
            "error": null,
            "pending_seq": null,
            "pending_prompt": null,
            "pending_approval": null,
            "approvals": [],
            "created_at": "2026-05-17T00:00:00Z"
        });

        let session: StoredSession = serde_json::from_value(raw).unwrap();
        assert_eq!(session.status, SessionStatus::Completed);
        assert_eq!(session.run_id, None);
    }

    fn sample_session(id: &str) -> StoredSession {
        StoredSession {
            id: id.to_string(),
            run_id: None,
            status: SessionStatus::Completed,
            pending_details: None,
            input: serde_json::json!({}),
            output: Some(serde_json::json!({"ok": true})),
            call_log: Vec::new(),
            error: None,
            pending_seq: None,
            pending_prompt: None,
            pending_signal_name: None,
            pending_signal_names: Vec::new(),
            pending_signal_deadline: None,
            pending_approval: None,
            approvals: Vec::new(),
            policy_profile: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn sqlite_store_persists_across_reopen() {
        // The durable default must actually be durable: a session written
        // through one handle is visible through a fresh one on the same path.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".chidori").join("sessions.sqlite3");

        let store = SqliteStore::open(path.clone()).unwrap();
        store.put(&sample_session("s-1")).unwrap();
        drop(store);

        let reopened = SqliteStore::open(path).unwrap();
        let got = reopened
            .get("s-1")
            .unwrap()
            .expect("session survives reopen");
        assert_eq!(got.status, SessionStatus::Completed);
    }

    #[test]
    fn sqlite_store_open_fails_loudly_on_unusable_path() {
        // The old behavior silently fell back to the in-memory store when
        // SQLite could not open. Failure must now surface to the caller.
        let dir = tempfile::tempdir().unwrap();
        let clash = dir.path().join("not-a-directory");
        std::fs::write(&clash, b"file, not dir").unwrap();
        let err = SqliteStore::open(clash.join("sessions.sqlite3"));
        assert!(err.is_err(), "opening under a file path must error");
    }
}
