#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::runtime::call_log::CallRecord;
use crate::runtime::capability::CapabilityLedger;

pub const SNAPSHOT_MANIFEST_FILE: &str = "runtime.snapshot.json";
pub const SNAPSHOT_BLOB_FILE: &str = "runtime.snapshot";
pub const PENDING_HOST_OPERATION_FILE: &str = "pending.json";
pub const HOST_PROMISE_TABLE_FILE: &str = "host_promises.json";
/// Per-operation host-promise blobs (`host_promises/<id>.json`), one written
/// on every state change (begin/resolve/reject). This is the O(1) hot-path
/// alternative to rewriting the whole `host_promises.json` table per host
/// call (which cost O(history) per call, O(history²) per run). Compaction
/// points fold the blobs into the table file and delete them; readers union
/// both via [`load_host_promise_records`].
pub const HOST_PROMISE_EVENTS_PREFIX: &str = "host_promises/";
/// Durable per-run signal mailbox. Lives in a `signals/` subdirectory of the
/// run directory (unlike the flat `pending.json`/`host_promises.json`), so
/// writers must create the parent dir before writing. Holds the ordered
/// `Vec<QueuedSignal>` absorbing signals that arrive before the agent reaches a
/// matching `chidori.signal(name)` listen point.
pub const SIGNAL_INBOX_FILE: &str = "signals/inbox.json";
pub const BRANCHES_DIR: &str = "branches";
pub const PARALLEL_BRANCH_MANIFEST_FILE: &str = "manifest.json";
pub const TYPESCRIPT_RUNTIME_ABI_VERSION: u32 = 1;
pub const QUICKJS_SNAPSHOT_ABI_VERSION: u32 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypeScriptImportPolicy {
    None,
    Relative,
    Project,
    /// Node-style ESM resolution: relative imports work as before, and bare
    /// specifiers are resolved through `node_modules` walk-up, `package.json`
    /// `exports`/`main`, and the `node:` builtin shim allowlist. The
    /// behavioral target is bun/deno/node compatibility for the well-formed
    /// ESM subset described in `runtime::typescript::resolver`.
    Node,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatePolicy {
    Disabled,
    Fixed,
    Host,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RandomPolicy {
    Disabled,
    Seeded,
    Host,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MapSetSnapshotPolicy {
    Reject,
    Serialize,
}

/// How the runtime backs `node:fs` / `node:fs/promises`. See
/// `docs/captured-effects-vfs-crypto-timers.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsPolicy {
    /// Filesystem access throws — the pre-captured-effects behavior.
    Disabled,
    /// In-memory, snapshot-resident virtual filesystem. Reads/writes never
    /// touch the host disk; every operation raises an `Fs*` capability flag.
    Captured,
    /// Direct host-disk access (uncaptured). Rejected for durable runs.
    Host,
}

/// How the runtime backs `globalThis.crypto` / `node:crypto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CryptoPolicy {
    /// Crypto APIs throw.
    Disabled,
    /// Randomness is drawn from the deterministic seed PRNG — reproducible but
    /// not cryptographically strong. Hashing always runs inline.
    Seeded,
    /// Randomness is drawn from the host CSPRNG on first run and captured into
    /// the call log so resume replays the exact bytes. Durable default.
    Captured,
    /// Live host randomness with no capture. Rejected for durable runs.
    Host,
}

/// How the runtime backs timers (`setTimeout`/`setInterval`/...) and the clock
/// that drives `Date`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimerPolicy {
    /// Scheduling a timer throws.
    Disabled,
    /// Timers fire against a deterministic logical clock that fast-forwards to
    /// the next deadline; no real wall-clock sleeping occurs. Durable default.
    Virtual,
    /// Real wall-clock timers. Rejected for durable runs.
    Host,
}

fn default_fs_policy() -> FsPolicy {
    FsPolicy::Captured
}

fn default_crypto_policy() -> CryptoPolicy {
    CryptoPolicy::Captured
}

fn default_timer_policy() -> TimerPolicy {
    TimerPolicy::Virtual
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimePolicy {
    pub typescript_imports: TypeScriptImportPolicy,
    pub date: DatePolicy,
    pub random: RandomPolicy,
    pub maps_sets: MapSetSnapshotPolicy,
    /// Filesystem backing. Defaulted for snapshots written before captured
    /// effects existed so old manifests stay loadable.
    #[serde(default = "default_fs_policy")]
    pub fs: FsPolicy,
    /// Crypto backing. Defaulted for back-compat (see above).
    #[serde(default = "default_crypto_policy")]
    pub crypto: CryptoPolicy,
    /// Timer backing. Defaulted for back-compat (see above).
    #[serde(default = "default_timer_policy")]
    pub timers: TimerPolicy,
    pub deterministic_seed: String,
}

impl RuntimePolicy {
    pub fn durable_default(run_id: &str) -> Self {
        Self {
            // Node-style resolution by default so `node:` builtins (notably
            // `node:fs`, the only surface the captured VFS is exposed through)
            // resolve in the durable path. `Node` is a behavioral superset of
            // `Relative` — relative imports resolve identically — so this is
            // safe for relative-only agents.
            typescript_imports: TypeScriptImportPolicy::Node,
            date: DatePolicy::Fixed,
            random: RandomPolicy::Seeded,
            maps_sets: MapSetSnapshotPolicy::Reject,
            fs: FsPolicy::Captured,
            crypto: CryptoPolicy::Captured,
            timers: TimerPolicy::Virtual,
            deterministic_seed: stable_source_hash(run_id.as_bytes()),
        }
    }

    pub fn from_env_for_durable_run(run_id: &str) -> Result<Self> {
        let policy = Self {
            typescript_imports: parse_policy_env(
                "CHIDORI_TS_IMPORTS",
                TypeScriptImportPolicy::Node,
                parse_import_policy,
            )?,
            date: parse_policy_env("CHIDORI_TS_DATE", DatePolicy::Fixed, parse_date_policy)?,
            random: parse_policy_env(
                "CHIDORI_TS_RANDOM",
                RandomPolicy::Seeded,
                parse_random_policy,
            )?,
            maps_sets: parse_policy_env(
                "CHIDORI_SNAPSHOT_MAPS_SETS",
                MapSetSnapshotPolicy::Reject,
                parse_maps_sets_policy,
            )?,
            fs: parse_policy_env("CHIDORI_TS_FS", FsPolicy::Captured, parse_fs_policy)?,
            crypto: parse_policy_env(
                "CHIDORI_TS_CRYPTO",
                CryptoPolicy::Captured,
                parse_crypto_policy,
            )?,
            timers: parse_policy_env(
                "CHIDORI_TS_TIMERS",
                TimerPolicy::Virtual,
                parse_timer_policy,
            )?,
            deterministic_seed: stable_source_hash(run_id.as_bytes()),
        };
        policy.ensure_durable_safe()?;
        Ok(policy)
    }

    pub fn ensure_durable_safe(&self) -> Result<()> {
        if self.date == DatePolicy::Host {
            anyhow::bail!("runtime.date=host is not allowed for durable snapshot runs");
        }
        if self.random == RandomPolicy::Host {
            anyhow::bail!("runtime.random=host is not allowed for durable snapshot runs");
        }
        if self.fs == FsPolicy::Host {
            anyhow::bail!("runtime.fs=host is not allowed for durable snapshot runs");
        }
        if self.crypto == CryptoPolicy::Host {
            anyhow::bail!("runtime.crypto=host is not allowed for durable snapshot runs");
        }
        if self.timers == TimerPolicy::Host {
            anyhow::bail!("runtime.timers=host is not allowed for durable snapshot runs");
        }
        Ok(())
    }

    pub fn ensure_compatible(&self, expected: &RuntimePolicy) -> Result<()> {
        if self != expected {
            anyhow::bail!(
                "runtime snapshot policy mismatch: snapshot has {:?}, runtime expects {:?}",
                self,
                expected
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HostOperationId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingHostOperationKind {
    Prompt,
    Input,
    PolicyApproval,
    Tool,
    CallAgent,
    Http,
    Template,
    Memory,
    Checkpoint,
    Log,
    /// A virtual timer pending across a snapshot boundary. The in-run virtual
    /// timer queue fires inside the job drain; this kind reserves the slot for
    /// timers that must survive suspend → restore (see
    /// `docs/captured-effects-vfs-crypto-timers.md`).
    Timer,
    /// A `chidori.signal(name)` / `chidori.pollSignal(name)` listen point. The
    /// match key (the pending op's `args`) is `{ "name": <string> }` only; the
    /// delivered `{ name, payload, from }` rides in the resolved result. See
    /// `docs/signals.md`.
    Signal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingHostOperation {
    pub id: HostOperationId,
    pub seq: u64,
    pub kind: PendingHostOperationKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
    pub args: Value,
    pub created_at: DateTime<Utc>,
}

impl PendingHostOperation {
    pub fn new(id: HostOperationId, seq: u64, kind: PendingHostOperationKind, args: Value) -> Self {
        Self {
            id,
            seq,
            kind,
            function: None,
            args,
            created_at: Utc::now(),
        }
    }

    pub fn with_function(mut self, function: impl Into<String>) -> Self {
        self.function = Some(function.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostPromiseState {
    Pending,
    Resolved {
        value: Value,
        completed_at: DateTime<Utc>,
    },
    Rejected {
        error: String,
        completed_at: DateTime<Utc>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostPromiseRecord {
    pub operation: PendingHostOperation,
    pub state: HostPromiseState,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostPromiseTable {
    next_id: u64,
    records: BTreeMap<u64, HostPromiseRecord>,
}

impl HostPromiseTable {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            records: BTreeMap::new(),
        }
    }

    pub fn from_records(records: Vec<HostPromiseRecord>) -> Self {
        let next_id = records
            .iter()
            .map(|record| record.operation.id.0)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        Self {
            next_id,
            records: records
                .into_iter()
                .map(|record| (record.operation.id.0, record))
                .collect(),
        }
    }

    pub fn create(
        &mut self,
        seq: u64,
        kind: PendingHostOperationKind,
        args: Value,
    ) -> HostOperationId {
        self.create_with_function(seq, kind, None, args)
    }

    pub fn create_with_function(
        &mut self,
        seq: u64,
        kind: PendingHostOperationKind,
        function: Option<String>,
        args: Value,
    ) -> HostOperationId {
        let id = HostOperationId(self.next_id);
        self.next_id += 1;
        let mut operation = PendingHostOperation::new(id, seq, kind, args);
        operation.function = function;
        self.records.insert(
            id.0,
            HostPromiseRecord {
                operation,
                state: HostPromiseState::Pending,
            },
        );
        id
    }

    pub fn pending_operation(&self, id: HostOperationId) -> Option<&PendingHostOperation> {
        self.records
            .get(&id.0)
            .and_then(|record| match record.state {
                HostPromiseState::Pending => Some(&record.operation),
                HostPromiseState::Resolved { .. } | HostPromiseState::Rejected { .. } => None,
            })
    }

    pub fn record(&self, id: HostOperationId) -> Option<&HostPromiseRecord> {
        self.records.get(&id.0)
    }

    pub fn resolve(&mut self, id: HostOperationId, value: Value) -> Result<()> {
        let record = self
            .records
            .get_mut(&id.0)
            .ok_or_else(|| anyhow::anyhow!("unknown host promise id {}", id.0))?;
        match record.state {
            HostPromiseState::Pending => {
                record.state = HostPromiseState::Resolved {
                    value,
                    completed_at: Utc::now(),
                };
                Ok(())
            }
            HostPromiseState::Resolved { .. } | HostPromiseState::Rejected { .. } => {
                anyhow::bail!("host promise id {} is already completed", id.0)
            }
        }
    }

    pub fn reject(&mut self, id: HostOperationId, error: impl Into<String>) -> Result<()> {
        let record = self
            .records
            .get_mut(&id.0)
            .ok_or_else(|| anyhow::anyhow!("unknown host promise id {}", id.0))?;
        match record.state {
            HostPromiseState::Pending => {
                record.state = HostPromiseState::Rejected {
                    error: error.into(),
                    completed_at: Utc::now(),
                };
                Ok(())
            }
            HostPromiseState::Resolved { .. } | HostPromiseState::Rejected { .. } => {
                anyhow::bail!("host promise id {} is already completed", id.0)
            }
        }
    }

    pub fn pending_operations(&self) -> Vec<PendingHostOperation> {
        self.records
            .values()
            .filter_map(|record| match record.state {
                HostPromiseState::Pending => Some(record.operation.clone()),
                HostPromiseState::Resolved { .. } | HostPromiseState::Rejected { .. } => None,
            })
            .collect()
    }

    pub fn active_pending_operation(&self) -> Option<PendingHostOperation> {
        self.records
            .values()
            .rev()
            .find_map(|record| match record.state {
                HostPromiseState::Pending => Some(record.operation.clone()),
                HostPromiseState::Resolved { .. } | HostPromiseState::Rejected { .. } => None,
            })
    }

    /// The completed record at (seq, kind), if any. Args are NOT matched here:
    /// the caller compares them via [`completed_args_match`] so a mismatch can
    /// be surfaced as a replay divergence instead of silently discarding the
    /// recorded completion (and re-executing the side effect live).
    pub fn completed_operation(
        &self,
        seq: u64,
        kind: PendingHostOperationKind,
    ) -> Option<HostPromiseRecord> {
        self.records.values().find_map(|record| {
            if record.operation.seq == seq
                && record.operation.kind == kind
                && !matches!(record.state, HostPromiseState::Pending)
            {
                Some(record.clone())
            } else {
                None
            }
        })
    }

    pub fn records(&self) -> Vec<HostPromiseRecord> {
        self.records.values().cloned().collect()
    }
}

/// Blob key for one operation's durable host-promise state.
pub fn host_promise_blob_key(id: HostOperationId) -> String {
    format!("{HOST_PROMISE_EVENTS_PREFIX}{}.json", id.0)
}

/// Load the durable host-promise table: the compacted `host_promises.json`
/// base unioned with any per-operation blobs written since the last
/// compaction. A per-op blob wins over the base entry for its id — it is
/// strictly newer (compaction deletes the blobs it folds in, and a blob that
/// survives a crashed compaction carries the same state the table already
/// folded). Returns an empty vec when the run has no table at all.
pub fn load_host_promise_records(
    store: &dyn crate::runtime::store::RunStore,
) -> Result<Vec<HostPromiseRecord>> {
    let mut records: Vec<HostPromiseRecord> = match store.get_blob(HOST_PROMISE_TABLE_FILE)? {
        Some(bytes) => serde_json::from_slice(&bytes).context("parsing host promise table")?,
        None => Vec::new(),
    };
    let mut tail: Vec<HostPromiseRecord> = Vec::new();
    for key in store.list_blobs()? {
        if !key.starts_with(HOST_PROMISE_EVENTS_PREFIX) {
            continue;
        }
        let Some(bytes) = store.get_blob(&key)? else {
            continue;
        };
        match serde_json::from_slice::<HostPromiseRecord>(&bytes) {
            Ok(record) => tail.push(record),
            // A crash can truncate a blob mid-write; the compacted base (or
            // the pending re-execution path) covers that operation.
            Err(_) => continue,
        }
    }
    tail.sort_by_key(|record| record.operation.id.0);
    for record in tail {
        match records
            .iter_mut()
            .find(|existing| existing.operation.id == record.operation.id)
        {
            Some(existing) => *existing = record,
            None => records.push(record),
        }
    }
    Ok(records)
}

/// Args comparison for completed-operation replay, ignoring derived request
/// metadata: `request_digest` describes the assembled prompt (it is recomputed
/// from the same inputs on resume) rather than identifying the operation, so a
/// digest-scheme change between record and resume must not force a completed
/// side effect to re-execute.
pub(crate) fn completed_args_match(recorded: &Value, rebuilt: &Value) -> bool {
    if recorded == rebuilt {
        return true;
    }
    // `request_digest` is derived metadata, never compared. Beyond that the
    // comparison is key-tolerant at the top level: a key present on only one
    // side is metadata evolution (e.g. newer runtimes journal `max_tokens` /
    // `temperature`, older checkpoints don't carry them) and must not fail
    // replay of existing runs. A key present on BOTH sides must match
    // exactly — so an edit that changes a journaled argument still fails
    // loudly as a divergence.
    match (recorded, rebuilt) {
        (Value::Object(a), Value::Object(b)) => a
            .iter()
            .filter(|(k, _)| k.as_str() != "request_digest")
            .all(|(k, av)| b.get(k).map(|bv| av == bv).unwrap_or(true)),
        _ => false,
    }
}

/// One signal sitting in the durable per-run mailbox (`signals/inbox.json`),
/// absorbed before the agent reached a matching `chidori.signal(name)` listen
/// point. `delivery_seq` is a monotonic counter assigned by the delivery
/// endpoint, freezing global arrival order across all senders so same-name
/// signals are consumed lowest-first and that choice replays deterministically.
/// `from` stays an opaque JSON value carrying `{ kind, id, runId? }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueuedSignal {
    pub name: String,
    pub payload: Value,
    pub from: Value,
    pub delivery_seq: u64,
    pub enqueued_at: DateTime<Utc>,
}

pub const DEFAULT_BRANCH_SEQUENCE_RANGE_WIDTH: u64 = 10_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchOperationId {
    pub parallel_op_id: HostOperationId,
    pub branch_index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallLogSequenceRange {
    pub start: u64,
    pub end_exclusive: u64,
}

impl CallLogSequenceRange {
    pub fn contains(&self, seq: u64) -> bool {
        seq >= self.start && seq < self.end_exclusive
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParallelBranchRecord {
    pub branch_index: u32,
    pub operation_id: BranchOperationId,
    pub sequence_range: CallLogSequenceRange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParallelBranchManifest {
    pub parent_run_id: String,
    pub parallel_op_id: HostOperationId,
    pub branch_count: u32,
    pub requested_concurrency: u32,
    pub branch_sequence_width: u64,
    pub branches: Vec<ParallelBranchRecord>,
}

impl ParallelBranchManifest {
    pub fn new(
        parent_run_id: impl Into<String>,
        parallel_op_id: HostOperationId,
        branch_count: u32,
        requested_concurrency: u32,
    ) -> Self {
        Self::with_sequence_width(
            parent_run_id,
            parallel_op_id,
            branch_count,
            requested_concurrency,
            DEFAULT_BRANCH_SEQUENCE_RANGE_WIDTH,
        )
    }

    pub fn with_sequence_width(
        parent_run_id: impl Into<String>,
        parallel_op_id: HostOperationId,
        branch_count: u32,
        requested_concurrency: u32,
        branch_sequence_width: u64,
    ) -> Self {
        let width = branch_sequence_width.max(1);
        let base = parallel_op_id
            .0
            .saturating_mul(width)
            .saturating_mul(u64::from(branch_count.max(1)));
        let branches = (0..branch_count)
            .map(|branch_index| {
                let start = base + u64::from(branch_index).saturating_mul(width) + 1;
                ParallelBranchRecord {
                    branch_index,
                    operation_id: BranchOperationId {
                        parallel_op_id,
                        branch_index,
                    },
                    sequence_range: CallLogSequenceRange {
                        start,
                        end_exclusive: start.saturating_add(width),
                    },
                }
            })
            .collect();

        Self {
            parent_run_id: parent_run_id.into(),
            parallel_op_id,
            branch_count,
            requested_concurrency: requested_concurrency.max(1),
            branch_sequence_width: width,
            branches,
        }
    }

    pub fn branch(&self, branch_index: u32) -> Option<&ParallelBranchRecord> {
        self.branches
            .iter()
            .find(|branch| branch.branch_index == branch_index)
    }
}

#[derive(Debug, Clone)]
pub struct ParallelBranchOutcome {
    pub branch_index: u32,
    pub output: Result<Value, String>,
    pub call_log: Vec<CallRecord>,
}

#[derive(Debug, Clone)]
pub struct ParallelMergeResult {
    pub outputs: Vec<Value>,
    pub call_log: Vec<CallRecord>,
}

pub fn merge_parallel_branch_outcomes(
    manifest: &ParallelBranchManifest,
    outcomes: &[ParallelBranchOutcome],
) -> Result<ParallelMergeResult> {
    let mut outputs = Vec::with_capacity(manifest.branch_count as usize);
    let mut call_log = Vec::new();

    for branch_index in 0..manifest.branch_count {
        let branch = manifest
            .branch(branch_index)
            .ok_or_else(|| anyhow::anyhow!("missing branch metadata for index {branch_index}"))?;
        let outcome = outcomes
            .iter()
            .find(|outcome| outcome.branch_index == branch_index)
            .ok_or_else(|| anyhow::anyhow!("missing branch outcome for index {branch_index}"))?;

        let output = outcome
            .output
            .clone()
            .map_err(|err| anyhow::anyhow!("parallel branch {} failed: {}", branch_index, err))?;

        for record in &outcome.call_log {
            if !branch.sequence_range.contains(record.seq) {
                anyhow::bail!(
                    "parallel branch {} emitted call seq {} outside reserved range {}..{}",
                    branch_index,
                    record.seq,
                    branch.sequence_range.start,
                    branch.sequence_range.end_exclusive
                );
            }
            call_log.push(record.clone());
        }
        outputs.push(output);
    }

    Ok(ParallelMergeResult { outputs, call_log })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFingerprint {
    pub path: PathBuf,
    pub hash: String,
}

impl SourceFingerprint {
    pub fn from_source(path: impl Into<PathBuf>, source: &str) -> Self {
        Self {
            path: path.into(),
            hash: stable_source_hash(source.as_bytes()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotModuleImport {
    pub specifier: String,
    pub resolved_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotModuleGraphEntry {
    pub path: PathBuf,
    pub imports: Vec<SnapshotModuleImport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotAbi {
    pub typescript_runtime: u32,
    pub quickjs_snapshot: u32,
    pub engine_fork: String,
}

impl SnapshotAbi {
    pub fn current(engine_fork: impl Into<String>) -> Self {
        Self {
            typescript_runtime: TYPESCRIPT_RUNTIME_ABI_VERSION,
            quickjs_snapshot: QUICKJS_SNAPSHOT_ABI_VERSION,
            engine_fork: engine_fork.into(),
        }
    }

    pub fn ensure_compatible(&self, expected: &SnapshotAbi) -> Result<()> {
        if self != expected {
            anyhow::bail!(
                "runtime snapshot ABI mismatch: snapshot has {:?}, runtime expects {:?}",
                self,
                expected
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotBranchMetadata {
    pub parent_run_id: String,
    pub parallel_op_id: HostOperationId,
    pub branch_index: u32,
    pub branch_operation_id: BranchOperationId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum SnapshotBlobKind {
    /// Current scaffold: a serialized set of TypeScript context roots after
    /// initial module evaluation, not a suspended VM continuation.
    #[default]
    InitialTypeScriptStateScaffold,
    /// Legacy blob kind for a live VM-image snapshot (async continuations, job
    /// queues, module records, and heap roots). VM-image snapshots are descoped
    /// — durability is the deterministic-replay journal — but the variant is
    /// retained for manifest compatibility (serialized as `live_quick_js_vm`).
    LiveQuickJsVm,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotManifest {
    pub run_id: String,
    pub abi: SnapshotAbi,
    #[serde(default)]
    pub snapshot_kind: SnapshotBlobKind,
    pub policy: RuntimePolicy,
    pub entry: SourceFingerprint,
    pub modules: Vec<SourceFingerprint>,
    #[serde(default)]
    pub module_graph: Vec<SnapshotModuleGraphEntry>,
    pub pending: Option<PendingHostOperation>,
    #[serde(default)]
    pub host_promises: Vec<HostPromiseRecord>,
    #[serde(default)]
    pub branch: Option<SnapshotBranchMetadata>,
    /// Capability flags the run touched (filesystem, crypto, timers). Defaulted
    /// (empty) for manifests written before captured effects existed.
    #[serde(default)]
    pub capabilities: CapabilityLedger,
    /// The snapshot-resident virtual filesystem state. Defaulted (empty) for
    /// manifests written before the VFS existed; restored into the runtime
    /// context on resume so reads/writes survive suspend identically.
    #[serde(default)]
    pub vfs: crate::runtime::vfs::Vfs,
    pub call_log_len: usize,
    pub snapshot_file: String,
    pub created_at: DateTime<Utc>,
}

impl SnapshotManifest {
    pub fn new(
        run_id: impl Into<String>,
        abi: SnapshotAbi,
        policy: RuntimePolicy,
        entry: SourceFingerprint,
        modules: Vec<SourceFingerprint>,
        pending: Option<PendingHostOperation>,
        call_log_len: usize,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            abi,
            snapshot_kind: SnapshotBlobKind::default(),
            policy,
            entry,
            modules,
            module_graph: Vec::new(),
            pending,
            host_promises: Vec::new(),
            branch: None,
            capabilities: CapabilityLedger::new(),
            vfs: crate::runtime::vfs::Vfs::new(),
            call_log_len,
            snapshot_file: SNAPSHOT_BLOB_FILE.to_string(),
            created_at: Utc::now(),
        }
    }

    pub fn with_host_promises(mut self, host_promises: Vec<HostPromiseRecord>) -> Self {
        self.host_promises = host_promises;
        self
    }

    pub fn with_capabilities(mut self, capabilities: CapabilityLedger) -> Self {
        self.capabilities = capabilities;
        self
    }

    pub fn with_vfs(mut self, vfs: crate::runtime::vfs::Vfs) -> Self {
        self.vfs = vfs;
        self
    }

    pub fn with_module_graph(mut self, module_graph: Vec<SnapshotModuleGraphEntry>) -> Self {
        self.module_graph = module_graph;
        self
    }

    pub fn with_branch_metadata(mut self, branch: SnapshotBranchMetadata) -> Self {
        self.branch = Some(branch);
        self
    }

    pub fn with_snapshot_kind(mut self, snapshot_kind: SnapshotBlobKind) -> Self {
        self.snapshot_kind = snapshot_kind;
        self
    }

    pub fn ensure_live_vm_snapshot(&self) -> Result<()> {
        if self.snapshot_kind != SnapshotBlobKind::LiveQuickJsVm {
            anyhow::bail!(
                "runtime snapshot blob kind mismatch: snapshot has {:?}, live VM resume requires {:?}",
                self.snapshot_kind,
                SnapshotBlobKind::LiveQuickJsVm
            );
        }
        Ok(())
    }

    pub fn ensure_branch_metadata(&self, expected: &SnapshotBranchMetadata) -> Result<()> {
        match &self.branch {
            Some(actual) if actual == expected => Ok(()),
            Some(actual) => anyhow::bail!(
                "runtime snapshot branch metadata mismatch: snapshot has {:?}, expected {:?}",
                actual,
                expected
            ),
            None => anyhow::bail!(
                "runtime snapshot branch metadata missing: expected {:?}",
                expected
            ),
        }
    }

    pub fn ensure_pending_host_operation(&self, expected_id: HostOperationId) -> Result<()> {
        match &self.pending {
            Some(pending) if pending.id == expected_id => Ok(()),
            Some(pending) => anyhow::bail!(
                "runtime snapshot pending host operation mismatch: snapshot has {:?}, expected {:?}",
                pending.id,
                expected_id
            ),
            None => anyhow::bail!(
                "runtime snapshot pending host operation missing: expected {:?}",
                expected_id
            ),
        }
    }

    pub fn ensure_sources_match(&self, current_entry: &SourceFingerprint) -> Result<()> {
        if self.entry != *current_entry {
            anyhow::bail!(
                "runtime snapshot source mismatch: snapshot has {} {}, runtime has {} {}",
                self.entry.path.display(),
                self.entry.hash,
                current_entry.path.display(),
                current_entry.hash
            );
        }
        Ok(())
    }

    pub fn ensure_modules_match(&self, current_modules: &[SourceFingerprint]) -> Result<()> {
        if self.modules != current_modules {
            anyhow::bail!(
                "runtime snapshot module mismatch: snapshot has {:?}, runtime has {:?}",
                self.modules,
                current_modules
            );
        }
        Ok(())
    }

    pub fn ensure_module_graph_matches(
        &self,
        current_module_graph: &[SnapshotModuleGraphEntry],
    ) -> Result<()> {
        if !self.module_graph.is_empty() && self.module_graph != current_module_graph {
            anyhow::bail!(
                "runtime snapshot module graph mismatch: snapshot has {:?}, runtime has {:?}",
                self.module_graph,
                current_module_graph
            );
        }
        Ok(())
    }

    pub fn ensure_resume_compatible(
        &self,
        expected_abi: &SnapshotAbi,
        expected_policy: &RuntimePolicy,
        current_entry: &SourceFingerprint,
        current_modules: &[SourceFingerprint],
        current_module_graph: &[SnapshotModuleGraphEntry],
    ) -> Result<()> {
        self.abi.ensure_compatible(expected_abi)?;
        self.policy.ensure_compatible(expected_policy)?;
        self.ensure_sources_match(current_entry)?;
        self.ensure_modules_match(current_modules)?;
        self.ensure_module_graph_matches(current_module_graph)?;
        Ok(())
    }
}

/// Verify the agent code on disk against the snapshot manifest recorded for
/// `run_id` before a resume replays its journal. Replay is positional — a
/// changed source file could silently pair cached results with different code
/// — so every resume surface (the server's resume/approve routes AND the
/// `chidori resume` CLI) must call this first. Runs persisted before manifests
/// existed (no readable manifest) are tolerated with a warning; every other
/// mismatch is an error the caller should surface.
///
/// `allow_source_change` is the edit-and-resume opt-in (`--allow-source-change`
/// on the CLI, `"allow_source_change": true` on the server routes): source and
/// module fingerprint mismatches downgrade to a warning and the resume
/// proceeds, relying on the replay engine's own edit-conflict policy — an edit
/// that touches already-journaled calls is a fail-loud divergence error, while
/// an edit past the replay frontier resumes cleanly (see
/// `chidori_js::replay`). ABI and policy mismatches stay fatal either way:
/// those are environment drift, not a deliberate edit.
pub fn validate_manifest_for_resume(
    run_base: &Path,
    run_id: Option<&str>,
    agent_path: &Path,
    allow_source_change: bool,
) -> Result<()> {
    let Some(run_id) = run_id else {
        return Ok(());
    };
    let store = SnapshotStore::new(run_base.join(run_id));
    let manifest = match store.load_manifest() {
        Ok(manifest) => manifest,
        Err(err) => {
            tracing::warn!(
                "resume: no readable snapshot manifest for run {run_id} ({err}); \
                 skipping source verification"
            );
            return Ok(());
        }
    };
    let entry_source = std::fs::read_to_string(agent_path).map_err(|err| {
        anyhow::anyhow!("reading resume source {}: {}", agent_path.display(), err)
    })?;
    let current_entry = SourceFingerprint::from_source(agent_path, &entry_source);
    let expected_abi = SnapshotAbi::current("chidori-quickjs");
    let expected_policy = RuntimePolicy::from_env_for_durable_run(run_id)?;
    // One module walk yields both manifest views (fingerprints + graph), so
    // each imported module is read from disk once per resume instead of twice
    // (once for its fingerprint, again inside the graph walk). Manifests
    // written before the graph existed fall back to fingerprinting exactly
    // the paths they list.
    let (current_modules, current_module_graph) = if manifest.module_graph.is_empty() {
        let mut current_modules = Vec::with_capacity(manifest.modules.len());
        for module in &manifest.modules {
            let source = std::fs::read_to_string(&module.path).map_err(|err| {
                anyhow::anyhow!(
                    "reading resume module source {}: {}",
                    module.path.display(),
                    err
                )
            })?;
            current_modules.push(SourceFingerprint::from_source(&module.path, &source));
        }
        (current_modules, Vec::new())
    } else {
        crate::runtime::typescript::module_graph::snapshot_modules(
            agent_path,
            &entry_source,
            &expected_policy,
        )?
    };
    manifest.abi.ensure_compatible(&expected_abi)?;
    manifest.policy.ensure_compatible(&expected_policy)?;
    let sources = manifest
        .ensure_sources_match(&current_entry)
        .and_then(|()| manifest.ensure_modules_match(&current_modules))
        .and_then(|()| manifest.ensure_module_graph_matches(&current_module_graph));
    match sources {
        Ok(()) => Ok(()),
        Err(err) if allow_source_change => {
            tracing::warn!(
                "resume: agent source changed since run {run_id} was recorded; proceeding \
                 because the caller opted in (allow_source_change) — replay will fail loudly \
                 if the edit diverges from already-journaled calls: {err}"
            );
            Ok(())
        }
        Err(err) => Err(err.context(
            "the agent source changed since this run was recorded; edit-and-resume is \
             opt-in — pass --allow-source-change (CLI) or \"allow_source_change\": true \
             (server) to replay the recorded calls against the edited code",
        )),
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeSnapshot {
    pub manifest: SnapshotManifest,
    pub blob: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct SnapshotStore {
    run_dir: PathBuf,
    /// Every write goes through this handle: the plain filesystem layout for
    /// `SnapshotStore::new`, or the filesystem teed with a durable mirror for
    /// `SnapshotStore::with_store` (`docs/durable-storage.md`). The on-disk
    /// shape is identical either way.
    store: std::sync::Arc<dyn crate::runtime::store::RunStore>,
}

impl SnapshotStore {
    pub fn new(run_dir: impl Into<PathBuf>) -> Self {
        let run_dir = run_dir.into();
        let store = std::sync::Arc::new(crate::runtime::store::FsRunStore::new(&run_dir));
        Self { run_dir, store }
    }

    /// A snapshot store writing through an explicit [`RunStore`] handle
    /// (e.g. the run's tee to a durable mirror). `run_dir` stays the
    /// filesystem address of the artifacts for path-based consumers.
    pub fn with_store(
        run_dir: impl Into<PathBuf>,
        store: std::sync::Arc<dyn crate::runtime::store::RunStore>,
    ) -> Self {
        Self {
            run_dir: run_dir.into(),
            store,
        }
    }

    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    pub fn save(
        &self,
        manifest: &SnapshotManifest,
        snapshot_blob: &[u8],
        call_log: &[CallRecord],
    ) -> Result<()> {
        self.store
            .put_blob(&manifest.snapshot_file, snapshot_blob)
            .with_context(|| format!("writing snapshot blob {}", manifest.snapshot_file))?;
        self.save_manifest_only(manifest, call_log)
    }

    pub fn save_live_vm_snapshot(
        &self,
        manifest: &SnapshotManifest,
        snapshot: &[u8],
        call_log: &[CallRecord],
    ) -> Result<()> {
        let manifest = manifest
            .clone()
            .with_snapshot_kind(SnapshotBlobKind::LiveQuickJsVm);
        self.save(&manifest, snapshot, call_log)
    }

    pub fn save_manifest_only(
        &self,
        manifest: &SnapshotManifest,
        call_log: &[CallRecord],
    ) -> Result<()> {
        self.put_manifest(manifest)?;
        self.write_call_log(call_log)?;
        self.put_pending(manifest.pending.as_ref())
    }

    /// Write just the snapshot blob under the manifest's blob file name.
    /// The blob is run-invariant (the durable code bundle), so per-safepoint
    /// persisters write it once per run instead of on every safepoint.
    pub fn put_snapshot_blob(&self, manifest: &SnapshotManifest, blob: &[u8]) -> Result<()> {
        self.store
            .put_blob(&manifest.snapshot_file, blob)
            .with_context(|| format!("writing snapshot blob {}", manifest.snapshot_file))
    }

    /// Write just the manifest artifact.
    pub fn put_manifest(&self, manifest: &SnapshotManifest) -> Result<()> {
        self.store
            .put_blob(
                SNAPSHOT_MANIFEST_FILE,
                &serde_json::to_vec_pretty(manifest)?,
            )
            .with_context(|| format!("writing {SNAPSHOT_MANIFEST_FILE}"))
    }

    /// Write the full-log checkpoint artifact (compacting the append-only
    /// journal to match). O(history) — callers on per-effect paths should
    /// reach for this only when the in-memory log holds records the O(1)
    /// journal appends did not cover (see `RuntimeContext::record_call` /
    /// `try_replay`), or at explicit compaction points (run start, pause,
    /// settle).
    pub fn write_call_log(&self, call_log: &[CallRecord]) -> Result<()> {
        self.store
            .write_call_log(call_log)
            .context("writing checkpoint")
    }

    /// Fold the host-promise table into its compacted artifact: write the
    /// full `host_promises.json` and delete the per-operation blobs it now
    /// covers. Write-then-delete ordering keeps a crash in between harmless —
    /// a surviving blob carries the same state the table just folded, and the
    /// union loader lets it win by id.
    pub fn compact_host_promises(&self, records: &[HostPromiseRecord]) -> Result<()> {
        self.store
            .put_blob(
                HOST_PROMISE_TABLE_FILE,
                &serde_json::to_vec_pretty(records)?,
            )
            .with_context(|| format!("writing {HOST_PROMISE_TABLE_FILE}"))?;
        for key in self.store.list_blobs()? {
            if key.starts_with(HOST_PROMISE_EVENTS_PREFIX) {
                self.store.delete_blob(&key)?;
            }
        }
        Ok(())
    }

    /// Write (or, when `None`, remove) the pending-host-operation artifact.
    pub fn put_pending(&self, pending: Option<&PendingHostOperation>) -> Result<()> {
        match pending {
            Some(pending) => self
                .store
                .put_blob(
                    PENDING_HOST_OPERATION_FILE,
                    &serde_json::to_vec_pretty(pending)?,
                )
                .with_context(|| format!("writing {PENDING_HOST_OPERATION_FILE}")),
            None => self
                .store
                .delete_blob(PENDING_HOST_OPERATION_FILE)
                .with_context(|| format!("removing {PENDING_HOST_OPERATION_FILE}")),
        }
    }

    pub fn load(&self) -> Result<RuntimeSnapshot> {
        let manifest = self.load_manifest()?;
        let blob = self
            .store
            .get_blob(&manifest.snapshot_file)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "reading {}: not found",
                    self.run_dir.join(&manifest.snapshot_file).display()
                )
            })?;
        Ok(RuntimeSnapshot { manifest, blob })
    }

    pub fn load_manifest(&self) -> Result<SnapshotManifest> {
        let manifest_path = self.run_dir.join(SNAPSHOT_MANIFEST_FILE);
        let bytes = self
            .store
            .get_blob(SNAPSHOT_MANIFEST_FILE)?
            .ok_or_else(|| anyhow::anyhow!("reading {}: not found", manifest_path.display()))?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", manifest_path.display()))
    }

    pub fn load_for_resume(
        &self,
        expected_abi: &SnapshotAbi,
        expected_policy: &RuntimePolicy,
        current_entry: &SourceFingerprint,
        current_modules: &[SourceFingerprint],
    ) -> Result<RuntimeSnapshot> {
        let manifest = self.load_manifest()?;
        manifest.ensure_resume_compatible(
            expected_abi,
            expected_policy,
            current_entry,
            current_modules,
            &manifest.module_graph,
        )?;
        let blob_path = self.run_dir.join(&manifest.snapshot_file);
        let blob =
            fs::read(&blob_path).with_context(|| format!("reading {}", blob_path.display()))?;
        Ok(RuntimeSnapshot { manifest, blob })
    }

    pub fn load_live_vm_for_resume(
        &self,
        expected_abi: &SnapshotAbi,
        expected_policy: &RuntimePolicy,
        current_entry: &SourceFingerprint,
        current_modules: &[SourceFingerprint],
        current_module_graph: &[SnapshotModuleGraphEntry],
    ) -> Result<RuntimeSnapshot> {
        let manifest = self.load_manifest()?;
        manifest.ensure_live_vm_snapshot()?;
        manifest.ensure_resume_compatible(
            expected_abi,
            expected_policy,
            current_entry,
            current_modules,
            current_module_graph,
        )?;
        let blob_path = self.run_dir.join(&manifest.snapshot_file);
        let blob =
            fs::read(&blob_path).with_context(|| format!("reading {}", blob_path.display()))?;
        Ok(RuntimeSnapshot { manifest, blob })
    }

    pub fn save_parallel_branch_manifest(
        &self,
        manifest: &ParallelBranchManifest,
    ) -> Result<PathBuf> {
        let dir = self.parallel_branch_dir(manifest.parallel_op_id);
        let key = format!(
            "{}/{PARALLEL_BRANCH_MANIFEST_FILE}",
            parallel_branch_prefix(manifest.parallel_op_id)
        );
        self.store
            .put_blob(&key, &serde_json::to_vec_pretty(manifest)?)
            .with_context(|| {
                format!(
                    "writing {}",
                    dir.join(PARALLEL_BRANCH_MANIFEST_FILE).display()
                )
            })?;
        Ok(dir)
    }

    pub fn load_parallel_branch_manifest(
        &self,
        parallel_op_id: HostOperationId,
    ) -> Result<ParallelBranchManifest> {
        let path = self
            .parallel_branch_dir(parallel_op_id)
            .join(PARALLEL_BRANCH_MANIFEST_FILE);
        let key = format!(
            "{}/{PARALLEL_BRANCH_MANIFEST_FILE}",
            parallel_branch_prefix(parallel_op_id)
        );
        let bytes = self
            .store
            .get_blob(&key)?
            .ok_or_else(|| anyhow::anyhow!("reading {}: not found", path.display()))?;
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn branch_store(
        &self,
        manifest: &ParallelBranchManifest,
        branch_index: u32,
    ) -> Result<SnapshotStore> {
        let branch = manifest
            .branch(branch_index)
            .ok_or_else(|| anyhow::anyhow!("unknown branch index {}", branch_index))?;
        let prefix = format!(
            "{}/branch-{:03}",
            parallel_branch_prefix(manifest.parallel_op_id),
            branch.branch_index
        );
        // A scoped view of the run's store, so branch artifacts flow through
        // the same handle (and any durable mirror) under their established
        // relative paths.
        Ok(SnapshotStore::with_store(
            self.parallel_branch_dir(manifest.parallel_op_id)
                .join(format!("branch-{:03}", branch.branch_index)),
            std::sync::Arc::new(crate::runtime::store::ScopedRunStore::new(
                self.store.clone(),
                prefix,
            )),
        ))
    }

    fn parallel_branch_dir(&self, parallel_op_id: HostOperationId) -> PathBuf {
        self.run_dir
            .join(BRANCHES_DIR)
            .join(format!("op-{:020}", parallel_op_id.0))
    }
}

/// The run-dir-relative key prefix of a parallel branch op's artifacts.
fn parallel_branch_prefix(parallel_op_id: HostOperationId) -> String {
    format!("{BRANCHES_DIR}/op-{:020}", parallel_op_id.0)
}

pub fn start_live_parallel_branch_runtimes<E: SnapshotCapableJsEngine>(
    store: &SnapshotStore,
    manifest: &ParallelBranchManifest,
    expected_abi: &SnapshotAbi,
    expected_policy: &RuntimePolicy,
    current_entry: &SourceFingerprint,
    current_modules: &[SourceFingerprint],
    current_module_graph: &[SnapshotModuleGraphEntry],
) -> Result<Vec<E>> {
    store.save_parallel_branch_manifest(manifest)?;
    let parent_snapshot = store.load_live_vm_for_resume(
        expected_abi,
        expected_policy,
        current_entry,
        current_modules,
        current_module_graph,
    )?;

    (0..manifest.branch_count)
        .map(|_| E::restore(&parent_snapshot.blob))
        .collect()
}

pub fn save_live_parallel_branch_runtime_snapshot<E: SnapshotCapableJsEngine>(
    store: &SnapshotStore,
    manifest: &ParallelBranchManifest,
    branch_index: u32,
    runtime: &mut E,
    policy: &RuntimePolicy,
    current_entry: &SourceFingerprint,
    current_modules: &[SourceFingerprint],
    current_module_graph: &[SnapshotModuleGraphEntry],
    pending: Option<PendingHostOperation>,
    call_log: &[CallRecord],
) -> Result<()> {
    let branch_record = manifest
        .branch(branch_index)
        .ok_or_else(|| anyhow::anyhow!("unknown branch index {}", branch_index))?;
    let snapshot = runtime.snapshot()?;
    let branch_manifest = SnapshotManifest::new(
        format!("{}-branch-{}", manifest.parent_run_id, branch_index),
        SnapshotAbi::current("chidori-quickjs"),
        policy.clone(),
        current_entry.clone(),
        current_modules.to_vec(),
        pending,
        call_log.len(),
    )
    .with_module_graph(current_module_graph.to_vec())
    .with_branch_metadata(SnapshotBranchMetadata {
        parent_run_id: manifest.parent_run_id.clone(),
        parallel_op_id: manifest.parallel_op_id,
        branch_index,
        branch_operation_id: branch_record.operation_id.clone(),
    });

    store
        .branch_store(manifest, branch_index)?
        .save_live_vm_snapshot(&branch_manifest, &snapshot, call_log)
}

pub fn resume_live_parallel_branch_from_store<E: SnapshotCapableJsEngine>(
    store: &SnapshotStore,
    parallel_op_id: HostOperationId,
    branch_index: u32,
    expected_abi: &SnapshotAbi,
    expected_policy: &RuntimePolicy,
    current_entry: &SourceFingerprint,
    current_modules: &[SourceFingerprint],
    current_module_graph: &[SnapshotModuleGraphEntry],
    host_operation_id: HostOperationId,
    value: Value,
) -> Result<(E, JsRunState)> {
    let manifest = store.load_parallel_branch_manifest(parallel_op_id)?;
    let branch_record = manifest
        .branch(branch_index)
        .ok_or_else(|| anyhow::anyhow!("unknown branch index {}", branch_index))?;
    let branch_store = store.branch_store(&manifest, branch_index)?;
    let branch_snapshot = branch_store.load_live_vm_for_resume(
        expected_abi,
        expected_policy,
        current_entry,
        current_modules,
        current_module_graph,
    )?;
    branch_snapshot
        .manifest
        .ensure_branch_metadata(&SnapshotBranchMetadata {
            parent_run_id: manifest.parent_run_id.clone(),
            parallel_op_id: manifest.parallel_op_id,
            branch_index,
            branch_operation_id: branch_record.operation_id.clone(),
        })?;
    branch_snapshot
        .manifest
        .ensure_pending_host_operation(host_operation_id)?;
    let mut runtime = E::restore(&branch_snapshot.blob)?;
    runtime.resolve_host_promise(host_operation_id, value)?;
    let state = runtime.run_jobs_until_blocked()?;
    Ok((runtime, state))
}

pub fn reject_live_parallel_branch_from_store<E: SnapshotCapableJsEngine>(
    store: &SnapshotStore,
    parallel_op_id: HostOperationId,
    branch_index: u32,
    expected_abi: &SnapshotAbi,
    expected_policy: &RuntimePolicy,
    current_entry: &SourceFingerprint,
    current_modules: &[SourceFingerprint],
    current_module_graph: &[SnapshotModuleGraphEntry],
    host_operation_id: HostOperationId,
    error: String,
) -> Result<(E, JsRunState)> {
    let manifest = store.load_parallel_branch_manifest(parallel_op_id)?;
    let branch_record = manifest
        .branch(branch_index)
        .ok_or_else(|| anyhow::anyhow!("unknown branch index {}", branch_index))?;
    let branch_store = store.branch_store(&manifest, branch_index)?;
    let branch_snapshot = branch_store.load_live_vm_for_resume(
        expected_abi,
        expected_policy,
        current_entry,
        current_modules,
        current_module_graph,
    )?;
    branch_snapshot
        .manifest
        .ensure_branch_metadata(&SnapshotBranchMetadata {
            parent_run_id: manifest.parent_run_id.clone(),
            parallel_op_id: manifest.parallel_op_id,
            branch_index,
            branch_operation_id: branch_record.operation_id.clone(),
        })?;
    branch_snapshot
        .manifest
        .ensure_pending_host_operation(host_operation_id)?;
    let mut runtime = E::restore(&branch_snapshot.blob)?;
    runtime.reject_host_promise(host_operation_id, error)?;
    let state = runtime.run_jobs_until_blocked()?;
    Ok((runtime, state))
}

pub trait SnapshotCapableJsEngine: Sized {
    fn snapshot(&mut self) -> Result<Vec<u8>>;
    fn restore(snapshot: &[u8]) -> Result<Self>;
    fn resolve_host_promise(&mut self, id: HostOperationId, value: Value) -> Result<()>;
    fn reject_host_promise(&mut self, id: HostOperationId, error: String) -> Result<()>;
    fn run_jobs_until_blocked(&mut self) -> Result<JsRunState>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsRunState {
    Completed,
    BlockedOnHostOperation(HostOperationId),
}

pub struct UnsupportedSnapshotEngine;

impl SnapshotCapableJsEngine for UnsupportedSnapshotEngine {
    fn snapshot(&mut self) -> Result<Vec<u8>> {
        unsupported_snapshot_engine()
    }

    fn restore(_snapshot: &[u8]) -> Result<Self> {
        unsupported_snapshot_engine()
    }

    fn resolve_host_promise(&mut self, _id: HostOperationId, _value: Value) -> Result<()> {
        unsupported_snapshot_engine()
    }

    fn reject_host_promise(&mut self, _id: HostOperationId, _error: String) -> Result<()> {
        unsupported_snapshot_engine()
    }

    fn run_jobs_until_blocked(&mut self) -> Result<JsRunState> {
        unsupported_snapshot_engine()
    }
}

fn unsupported_snapshot_engine<T>() -> Result<T> {
    anyhow::bail!(
        "durable TypeScript VM snapshots require the Chidori QuickJS fork; \
         stock rquickjs/Boa engines do not expose heap, async frame, promise, \
         and job queue serialization"
    )
}

fn parse_policy_env<T: Copy>(name: &str, default: T, parse: fn(&str) -> Option<T>) -> Result<T> {
    let Ok(raw) = std::env::var(name) else {
        return Ok(default);
    };
    parse(raw.trim()).ok_or_else(|| anyhow::anyhow!("invalid {} value: {}", name, raw))
}

fn parse_import_policy(value: &str) -> Option<TypeScriptImportPolicy> {
    match value {
        "none" => Some(TypeScriptImportPolicy::None),
        "relative" => Some(TypeScriptImportPolicy::Relative),
        "project" => Some(TypeScriptImportPolicy::Project),
        "node" => Some(TypeScriptImportPolicy::Node),
        _ => None,
    }
}

fn parse_date_policy(value: &str) -> Option<DatePolicy> {
    match value {
        "disabled" => Some(DatePolicy::Disabled),
        "fixed" => Some(DatePolicy::Fixed),
        "host" => Some(DatePolicy::Host),
        _ => None,
    }
}

fn parse_random_policy(value: &str) -> Option<RandomPolicy> {
    match value {
        "disabled" => Some(RandomPolicy::Disabled),
        "seeded" => Some(RandomPolicy::Seeded),
        "host" => Some(RandomPolicy::Host),
        _ => None,
    }
}

fn parse_maps_sets_policy(value: &str) -> Option<MapSetSnapshotPolicy> {
    match value {
        "reject" => Some(MapSetSnapshotPolicy::Reject),
        "serialize" => Some(MapSetSnapshotPolicy::Serialize),
        _ => None,
    }
}

fn parse_fs_policy(value: &str) -> Option<FsPolicy> {
    match value {
        "disabled" => Some(FsPolicy::Disabled),
        "captured" => Some(FsPolicy::Captured),
        "host" => Some(FsPolicy::Host),
        _ => None,
    }
}

fn parse_crypto_policy(value: &str) -> Option<CryptoPolicy> {
    match value {
        "disabled" => Some(CryptoPolicy::Disabled),
        "seeded" => Some(CryptoPolicy::Seeded),
        "captured" => Some(CryptoPolicy::Captured),
        "host" => Some(CryptoPolicy::Host),
        _ => None,
    }
}

fn parse_timer_policy(value: &str) -> Option<TimerPolicy> {
    match value {
        "disabled" => Some(TimerPolicy::Disabled),
        "virtual" => Some(TimerPolicy::Virtual),
        "host" => Some(TimerPolicy::Host),
        _ => None,
    }
}

fn stable_source_hash(bytes: &[u8]) -> String {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("fnv1a64:{hash:016x}")
}

#[cfg(test)]
mod completed_args_match_tests {
    use super::completed_args_match;
    use serde_json::json;

    #[test]
    fn identical_args_match() {
        let args = json!({"text": "hi", "model": "m"});
        assert!(completed_args_match(&args, &args));
    }

    #[test]
    fn request_digest_is_ignored() {
        assert!(completed_args_match(
            &json!({"text": "hi", "request_digest": "aaa"}),
            &json!({"text": "hi", "request_digest": "bbb"}),
        ));
    }

    /// Metadata evolution: newer runtimes journal keys (e.g. `max_tokens`)
    /// that older checkpoints don't carry — those must still replay.
    #[test]
    fn key_missing_from_recorded_side_is_tolerated() {
        assert!(completed_args_match(
            &json!({"text": "hi"}),
            &json!({"text": "hi", "max_tokens": 1200, "temperature": 0.7}),
        ));
    }

    /// But a key present on BOTH sides must match: editing a journaled
    /// argument is a loud divergence, not a silent cache hit.
    #[test]
    fn differing_shared_key_is_a_divergence() {
        assert!(!completed_args_match(
            &json!({"text": "hi", "max_tokens": 1200}),
            &json!({"text": "hi", "max_tokens": 800}),
        ));
        assert!(!completed_args_match(
            &json!({"text": "hi"}),
            &json!({"text": "CHANGED"}),
        ));
    }
}

#[cfg(test)]
mod host_promise_union_tests {
    use super::*;
    use crate::runtime::store::{FsRunStore, RunStore as _};

    fn record(id: u64, state: HostPromiseState) -> HostPromiseRecord {
        HostPromiseRecord {
            operation: PendingHostOperation::new(
                HostOperationId(id),
                id,
                PendingHostOperationKind::Prompt,
                serde_json::json!({ "n": id }),
            ),
            state,
        }
    }

    /// Pins the per-op blob ∪ compacted table protocol: a per-op blob wins
    /// over the table entry for its id (it is strictly newer), new ids from
    /// blobs are unioned in, and compaction folds everything into the table
    /// file and retires the blobs.
    #[test]
    fn per_op_blobs_union_with_table_and_compact_away() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-host-promise-union-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = FsRunStore::new(&dir);

        // Compacted base: op 1 still Pending.
        store
            .put_blob(
                HOST_PROMISE_TABLE_FILE,
                &serde_json::to_vec_pretty(&vec![record(1, HostPromiseState::Pending)]).unwrap(),
            )
            .unwrap();
        // Per-op blobs written since: op 1 resolved, op 2 begun.
        let resolved = record(
            1,
            HostPromiseState::Resolved {
                value: serde_json::json!("done"),
                completed_at: chrono::Utc::now(),
            },
        );
        store
            .put_blob(
                &host_promise_blob_key(HostOperationId(1)),
                &serde_json::to_vec_pretty(&resolved).unwrap(),
            )
            .unwrap();
        store
            .put_blob(
                &host_promise_blob_key(HostOperationId(2)),
                &serde_json::to_vec_pretty(&record(2, HostPromiseState::Pending)).unwrap(),
            )
            .unwrap();

        let records = load_host_promise_records(&store).unwrap();
        assert_eq!(records.len(), 2);
        assert!(matches!(
            records[0].state,
            HostPromiseState::Resolved { .. }
        ));
        assert!(matches!(records[1].state, HostPromiseState::Pending));

        // Compaction folds the union into the table file and retires the blobs.
        SnapshotStore::new(&dir)
            .compact_host_promises(&records)
            .unwrap();
        assert!(!store
            .list_blobs()
            .unwrap()
            .iter()
            .any(|key| key.starts_with(HOST_PROMISE_EVENTS_PREFIX)));
        let reloaded = load_host_promise_records(&store).unwrap();
        assert_eq!(reloaded.len(), 2);
        assert!(matches!(
            reloaded[0].state,
            HostPromiseState::Resolved { .. }
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
