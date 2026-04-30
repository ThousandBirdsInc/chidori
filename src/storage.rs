//! Session and memory persistence.
//!
//! Provides two backends:
//!   * JSON files (default; same layout the framework has used since v0)
//!   * SQLite (enabled via CHIDORI_DB_PATH)
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
    Paused,
    /// Paused waiting for the operator to approve/deny a policy-gated call.
    AwaitingApproval,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSession {
    pub id: String,
    pub status: SessionStatus,
    pub input: Value,
    pub output: Option<Value>,
    pub call_log: Vec<CallRecord>,
    pub error: Option<String>,
    pub pending_seq: Option<u64>,
    pub pending_prompt: Option<String>,
    /// Set when status == AwaitingApproval. Carries the (target, args)
    /// the policy is asking about so the UI can render a prompt.
    #[serde(default)]
    pub pending_approval: Option<PendingApproval>,
    /// Per-session list of approvals the operator has already granted.
    /// Seeded into the PolicyCache on every re-run so the agent doesn't
    /// have to ask twice for the same (target, args) within a session.
    #[serde(default)]
    pub approvals: Vec<(String, Value)>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub trait SessionStore: Send + Sync {
    fn put(&self, session: &StoredSession) -> Result<()>;
    fn get(&self, id: &str) -> Result<Option<StoredSession>>;
    fn list(&self) -> Result<Vec<StoredSession>>;
    #[allow(dead_code)] // Exposed for cleanup tools; no current caller.
    fn delete(&self, id: &str) -> Result<()>;
}

/// In-memory store. Default when no persistence is configured; keeps the
/// pre-v1 behavior for dev loops.
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
        let mut stmt = conn
            .prepare("SELECT data FROM sessions ORDER BY created_at DESC LIMIT 200")?;
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

/// Build the SessionStore configured by env. CHIDORI_DB_PATH picks SQLite;
/// otherwise an in-memory store is returned.
pub fn build_session_store() -> std::sync::Arc<dyn SessionStore> {
    if let Ok(path) = std::env::var("CHIDORI_DB_PATH") {
        match SqliteStore::open(PathBuf::from(&path)) {
            Ok(store) => {
                tracing::info!("session store: sqlite at {}", path);
                return std::sync::Arc::new(store);
            }
            Err(e) => {
                tracing::warn!("sqlite session store failed ({}), falling back to memory", e);
            }
        }
    }
    std::sync::Arc::new(MemoryStore::new())
}
