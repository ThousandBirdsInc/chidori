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

    pub fn completed_operation(
        &self,
        seq: u64,
        kind: PendingHostOperationKind,
        args: &Value,
    ) -> Option<HostPromiseRecord> {
        self.records.values().find_map(|record| {
            if record.operation.seq == seq
                && record.operation.kind == kind
                && completed_args_match(&record.operation.args, args)
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

/// Args comparison for completed-operation replay, ignoring derived request
/// metadata: `request_digest` describes the assembled prompt (it is recomputed
/// from the same inputs on resume) rather than identifying the operation, so a
/// digest-scheme change between record and resume must not force a completed
/// side effect to re-execute.
fn completed_args_match(recorded: &Value, rebuilt: &Value) -> bool {
    if recorded == rebuilt {
        return true;
    }
    let strip = |value: &Value| {
        let mut value = value.clone();
        if let Some(map) = value.as_object_mut() {
            map.remove("request_digest");
        }
        value
    };
    strip(recorded) == strip(rebuilt)
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
pub enum SnapshotBlobKind {
    /// Current scaffold: a serialized set of TypeScript context roots after
    /// initial module evaluation, not a suspended VM continuation.
    InitialTypeScriptStateScaffold,
    /// Future production path: a live QuickJS VM snapshot containing async
    /// continuations, job queues, module records, and heap roots.
    LiveQuickJsVm,
}

impl Default for SnapshotBlobKind {
    fn default() -> Self {
        Self::InitialTypeScriptStateScaffold
    }
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

#[derive(Debug, Clone)]
pub struct RuntimeSnapshot {
    pub manifest: SnapshotManifest,
    pub blob: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct SnapshotStore {
    run_dir: PathBuf,
}

impl SnapshotStore {
    pub fn new(run_dir: impl Into<PathBuf>) -> Self {
        Self {
            run_dir: run_dir.into(),
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
        fs::create_dir_all(&self.run_dir)
            .with_context(|| format!("creating snapshot dir {}", self.run_dir.display()))?;

        fs::write(self.run_dir.join(&manifest.snapshot_file), snapshot_blob).with_context(
            || {
                format!(
                    "writing {}",
                    self.run_dir.join(&manifest.snapshot_file).display()
                )
            },
        )?;

        fs::write(
            self.run_dir.join(SNAPSHOT_MANIFEST_FILE),
            serde_json::to_vec_pretty(manifest)?,
        )
        .with_context(|| {
            format!(
                "writing {}",
                self.run_dir.join(SNAPSHOT_MANIFEST_FILE).display()
            )
        })?;

        fs::write(
            self.run_dir.join("checkpoint.json"),
            serde_json::to_vec_pretty(call_log)?,
        )
        .with_context(|| format!("writing {}", self.run_dir.join("checkpoint.json").display()))?;

        match &manifest.pending {
            Some(pending) => fs::write(
                self.run_dir.join(PENDING_HOST_OPERATION_FILE),
                serde_json::to_vec_pretty(pending)?,
            )
            .with_context(|| {
                format!(
                    "writing {}",
                    self.run_dir.join(PENDING_HOST_OPERATION_FILE).display()
                )
            })?,
            None => {
                let pending_path = self.run_dir.join(PENDING_HOST_OPERATION_FILE);
                if pending_path.exists() {
                    fs::remove_file(&pending_path)
                        .with_context(|| format!("removing {}", pending_path.display()))?;
                }
            }
        }

        Ok(())
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
        fs::create_dir_all(&self.run_dir)
            .with_context(|| format!("creating snapshot dir {}", self.run_dir.display()))?;

        fs::write(
            self.run_dir.join(SNAPSHOT_MANIFEST_FILE),
            serde_json::to_vec_pretty(manifest)?,
        )
        .with_context(|| {
            format!(
                "writing {}",
                self.run_dir.join(SNAPSHOT_MANIFEST_FILE).display()
            )
        })?;

        fs::write(
            self.run_dir.join("checkpoint.json"),
            serde_json::to_vec_pretty(call_log)?,
        )
        .with_context(|| format!("writing {}", self.run_dir.join("checkpoint.json").display()))?;

        match &manifest.pending {
            Some(pending) => fs::write(
                self.run_dir.join(PENDING_HOST_OPERATION_FILE),
                serde_json::to_vec_pretty(pending)?,
            )
            .with_context(|| {
                format!(
                    "writing {}",
                    self.run_dir.join(PENDING_HOST_OPERATION_FILE).display()
                )
            })?,
            None => {
                let pending_path = self.run_dir.join(PENDING_HOST_OPERATION_FILE);
                if pending_path.exists() {
                    fs::remove_file(&pending_path)
                        .with_context(|| format!("removing {}", pending_path.display()))?;
                }
            }
        }

        Ok(())
    }

    pub fn load(&self) -> Result<RuntimeSnapshot> {
        let manifest = self.load_manifest()?;
        let blob_path = self.run_dir.join(&manifest.snapshot_file);
        let blob =
            fs::read(&blob_path).with_context(|| format!("reading {}", blob_path.display()))?;
        Ok(RuntimeSnapshot { manifest, blob })
    }

    pub fn load_manifest(&self) -> Result<SnapshotManifest> {
        let manifest_path = self.run_dir.join(SNAPSHOT_MANIFEST_FILE);
        serde_json::from_slice(
            &fs::read(&manifest_path)
                .with_context(|| format!("reading {}", manifest_path.display()))?,
        )
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
        fs::create_dir_all(&dir)
            .with_context(|| format!("creating branch manifest dir {}", dir.display()))?;
        fs::write(
            dir.join(PARALLEL_BRANCH_MANIFEST_FILE),
            serde_json::to_vec_pretty(manifest)?,
        )
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
        serde_json::from_slice(
            &fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
        )
        .with_context(|| format!("parsing {}", path.display()))
    }

    pub fn branch_store(
        &self,
        manifest: &ParallelBranchManifest,
        branch_index: u32,
    ) -> Result<SnapshotStore> {
        let branch = manifest
            .branch(branch_index)
            .ok_or_else(|| anyhow::anyhow!("unknown branch index {}", branch_index))?;
        Ok(SnapshotStore::new(
            self.parallel_branch_dir(manifest.parallel_op_id)
                .join(format!("branch-{:03}", branch.branch_index)),
        ))
    }

    fn parallel_branch_dir(&self, parallel_op_id: HostOperationId) -> PathBuf {
        self.run_dir
            .join(BRANCHES_DIR)
            .join(format!("op-{:020}", parallel_op_id.0))
    }
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
