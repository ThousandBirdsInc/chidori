#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::runtime::call_log::CallRecord;

pub const SNAPSHOT_MANIFEST_FILE: &str = "runtime.snapshot.json";
pub const SNAPSHOT_BLOB_FILE: &str = "runtime.snapshot";
pub const PENDING_HOST_OPERATION_FILE: &str = "pending.json";
pub const HOST_PROMISE_TABLE_FILE: &str = "host_promises.json";
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimePolicy {
    pub typescript_imports: TypeScriptImportPolicy,
    pub date: DatePolicy,
    pub random: RandomPolicy,
    pub maps_sets: MapSetSnapshotPolicy,
    pub deterministic_seed: String,
}

impl RuntimePolicy {
    pub fn durable_default(run_id: &str) -> Self {
        Self {
            typescript_imports: TypeScriptImportPolicy::Relative,
            date: DatePolicy::Fixed,
            random: RandomPolicy::Seeded,
            maps_sets: MapSetSnapshotPolicy::Reject,
            deterministic_seed: stable_source_hash(run_id.as_bytes()),
        }
    }

    pub fn from_env_for_durable_run(run_id: &str) -> Result<Self> {
        let policy = Self {
            typescript_imports: parse_policy_env(
                "CHIDORI_TS_IMPORTS",
                TypeScriptImportPolicy::Relative,
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
    Sandbox,
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
                && record.operation.args == *args
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
            call_log_len,
            snapshot_file: SNAPSHOT_BLOB_FILE.to_string(),
            created_at: Utc::now(),
        }
    }

    pub fn with_host_promises(mut self, host_promises: Vec<HostPromiseRecord>) -> Self {
        self.host_promises = host_promises;
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
        snapshot: &chidori_quickjs::RuntimeSnapshot,
        call_log: &[CallRecord],
    ) -> Result<()> {
        validate_live_vm_runtime_snapshot(snapshot)?;
        let manifest = manifest
            .clone()
            .with_snapshot_kind(SnapshotBlobKind::LiveQuickJsVm);
        self.save(&manifest, &snapshot.0, call_log)
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
        validate_live_vm_runtime_snapshot(&chidori_quickjs::RuntimeSnapshot(blob.clone()))?;
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

fn validate_live_vm_runtime_snapshot(snapshot: &chidori_quickjs::RuntimeSnapshot) -> Result<()> {
    snapshot
        .ensure_restorable()
        .map_err(|err| anyhow::anyhow!("invalid live VM runtime snapshot: {}", err))?;
    Ok(())
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
    let snapshot = chidori_quickjs::RuntimeSnapshot(runtime.snapshot()?);
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

impl SnapshotCapableJsEngine for chidori_quickjs::SnapshotRuntime {
    fn snapshot(&mut self) -> Result<Vec<u8>> {
        Ok(chidori_quickjs::SnapshotRuntime::snapshot(self)
            .map_err(|err| anyhow::anyhow!(err))?
            .0)
    }

    fn restore(snapshot: &[u8]) -> Result<Self> {
        chidori_quickjs::SnapshotRuntime::restore(snapshot).map_err(|err| anyhow::anyhow!(err))
    }

    fn resolve_host_promise(&mut self, id: HostOperationId, value: Value) -> Result<()> {
        chidori_quickjs::SnapshotRuntime::resolve_host_promise(
            self,
            chidori_quickjs::HostPromiseId(id.0),
            value,
        )
        .map_err(|err| anyhow::anyhow!(err))
    }

    fn reject_host_promise(&mut self, id: HostOperationId, error: String) -> Result<()> {
        chidori_quickjs::SnapshotRuntime::reject_host_promise(
            self,
            chidori_quickjs::HostPromiseId(id.0),
            error,
        )
        .map_err(|err| anyhow::anyhow!(err))
    }

    fn run_jobs_until_blocked(&mut self) -> Result<JsRunState> {
        match chidori_quickjs::SnapshotRuntime::run_jobs_until_blocked(self)
            .map_err(|err| anyhow::anyhow!(err))?
        {
            chidori_quickjs::RunState::Completed(_) => Ok(JsRunState::Completed),
            chidori_quickjs::RunState::BlockedOnHostOperation(id) => {
                Ok(JsRunState::BlockedOnHostOperation(HostOperationId(id.0)))
            }
        }
    }
}

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
mod tests {
    use super::*;
    use crate::runtime::call_log::CallRecord;

    fn call_record(seq: u64, function: &str) -> CallRecord {
        CallRecord {
            seq,
            parent_seq: None,
            function: function.to_string(),
            args: serde_json::Value::Null,
            result: serde_json::Value::Null,
            duration_ms: 0,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        }
    }

    #[derive(Debug)]
    struct FakeLiveBranchEngine {
        snapshot: Vec<u8>,
        resolved: Vec<(HostOperationId, Value)>,
        rejected: Vec<(HostOperationId, String)>,
        run_count: usize,
    }

    impl FakeLiveBranchEngine {
        fn with_snapshot(snapshot: chidori_quickjs::RuntimeSnapshot) -> Self {
            Self {
                snapshot: snapshot.0,
                resolved: Vec::new(),
                rejected: Vec::new(),
                run_count: 0,
            }
        }
    }

    impl SnapshotCapableJsEngine for FakeLiveBranchEngine {
        fn snapshot(&mut self) -> Result<Vec<u8>> {
            Ok(self.snapshot.clone())
        }

        fn restore(snapshot: &[u8]) -> Result<Self> {
            Ok(Self {
                snapshot: snapshot.to_vec(),
                resolved: Vec::new(),
                rejected: Vec::new(),
                run_count: 0,
            })
        }

        fn resolve_host_promise(&mut self, id: HostOperationId, value: Value) -> Result<()> {
            self.resolved.push((id, value));
            Ok(())
        }

        fn reject_host_promise(&mut self, id: HostOperationId, error: String) -> Result<()> {
            self.rejected.push((id, error));
            Ok(())
        }

        fn run_jobs_until_blocked(&mut self) -> Result<JsRunState> {
            self.run_count += 1;
            Ok(JsRunState::Completed)
        }
    }

    #[test]
    fn source_fingerprint_is_stable() {
        let a = SourceFingerprint::from_source("agent.ts", "export async function agent() {}");
        let b = SourceFingerprint::from_source("agent.ts", "export async function agent() {}");
        let c = SourceFingerprint::from_source(
            "agent.ts",
            "export async function agent() { return 1 }",
        );

        assert_eq!(a, b);
        assert_ne!(a.hash, c.hash);
    }

    #[test]
    fn host_promise_table_tracks_pending_resolved_and_rejected_operations() {
        let mut table = HostPromiseTable::new();
        let prompt_id = table.create(
            1,
            PendingHostOperationKind::Prompt,
            serde_json::json!({ "text": "hello" }),
        );
        let input_id = table.create(
            2,
            PendingHostOperationKind::Input,
            serde_json::json!({ "prompt": "Proceed?" }),
        );
        let http_id = table.create(
            3,
            PendingHostOperationKind::Http,
            serde_json::json!({ "url": "https://example.com" }),
        );

        assert_eq!(prompt_id, HostOperationId(1));
        assert_eq!(input_id, HostOperationId(2));
        assert_eq!(http_id, HostOperationId(3));
        assert_eq!(table.pending_operations().len(), 3);

        table
            .resolve(prompt_id, serde_json::json!({ "answer": "ok" }))
            .unwrap();
        table.reject(http_id, "network failed").unwrap();

        assert!(table.pending_operation(prompt_id).is_none());
        assert_eq!(
            table.pending_operation(input_id).unwrap().kind,
            PendingHostOperationKind::Input
        );
        assert_eq!(table.pending_operations().len(), 1);

        let records = table.records();
        assert!(matches!(
            records[0].state,
            HostPromiseState::Resolved { .. }
        ));
        assert!(matches!(records[1].state, HostPromiseState::Pending));
        assert!(matches!(
            records[2].state,
            HostPromiseState::Rejected { .. }
        ));
        assert!(table
            .resolve(prompt_id, serde_json::json!(null))
            .unwrap_err()
            .to_string()
            .contains("already completed"));
    }

    #[test]
    fn host_promise_table_is_snapshot_serializable() {
        let mut table = HostPromiseTable::new();
        let id = table.create(
            1,
            PendingHostOperationKind::Tool,
            serde_json::json!({ "name": "search" }),
        );
        table
            .resolve(id, serde_json::json!({ "ok": true }))
            .unwrap();

        let encoded = serde_json::to_string(&table).unwrap();
        let decoded: HostPromiseTable = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded.records().len(), 1);
        assert!(matches!(
            decoded.records()[0].state,
            HostPromiseState::Resolved { .. }
        ));
    }

    #[test]
    fn snapshot_manifest_round_trips_host_promise_records() {
        let mut table = HostPromiseTable::new();
        let id = table.create(
            1,
            PendingHostOperationKind::Prompt,
            serde_json::json!({ "text": "hello" }),
        );
        table.resolve(id, serde_json::json!("answer")).unwrap();
        let manifest = SnapshotManifest::new(
            "run-1",
            SnapshotAbi::current("chidori-quickjs"),
            RuntimePolicy::durable_default("run-1"),
            SourceFingerprint::from_source("agent.ts", "export async function agent() {}"),
            Vec::new(),
            None,
            1,
        )
        .with_host_promises(table.records());

        let encoded = serde_json::to_vec(&manifest).unwrap();
        let decoded: SnapshotManifest = serde_json::from_slice(&encoded).unwrap();

        assert_eq!(
            decoded.snapshot_kind,
            SnapshotBlobKind::InitialTypeScriptStateScaffold
        );
        assert_eq!(decoded.host_promises.len(), 1);
        assert!(matches!(
            decoded.host_promises[0].state,
            HostPromiseState::Resolved { .. }
        ));
    }

    #[test]
    fn snapshot_manifest_deserializes_without_host_promises() {
        let raw = serde_json::json!({
            "run_id": "run-1",
            "abi": {
                "typescript_runtime": 1,
                "quickjs_snapshot": 1,
                "engine_fork": "chidori-quickjs"
            },
            "policy": RuntimePolicy::durable_default("run-1"),
            "entry": SourceFingerprint::from_source(
                "agent.ts",
                "export async function agent() {}"
            ),
            "modules": [],
            "pending": null,
            "call_log_len": 0,
            "snapshot_file": SNAPSHOT_BLOB_FILE,
            "created_at": Utc::now()
        });

        let decoded: SnapshotManifest = serde_json::from_value(raw).unwrap();
        assert!(decoded.host_promises.is_empty());
        assert_eq!(
            decoded.snapshot_kind,
            SnapshotBlobKind::InitialTypeScriptStateScaffold
        );
    }

    #[test]
    fn snapshot_manifest_round_trips_blob_kind() {
        let manifest = SnapshotManifest::new(
            "run-1",
            SnapshotAbi::current("chidori-quickjs"),
            RuntimePolicy::durable_default("run-1"),
            SourceFingerprint::from_source("agent.ts", "export async function agent() {}"),
            Vec::new(),
            None,
            0,
        )
        .with_snapshot_kind(SnapshotBlobKind::LiveQuickJsVm);

        let decoded: SnapshotManifest =
            serde_json::from_str(&serde_json::to_string(&manifest).unwrap()).unwrap();

        assert_eq!(decoded.snapshot_kind, SnapshotBlobKind::LiveQuickJsVm);
    }

    #[test]
    fn parallel_branch_manifest_assigns_operation_ids_and_sequence_ranges() {
        let manifest =
            ParallelBranchManifest::with_sequence_width("run-1", HostOperationId(5), 3, 2, 100);

        assert_eq!(manifest.parent_run_id, "run-1");
        assert_eq!(manifest.branch_count, 3);
        assert_eq!(manifest.requested_concurrency, 2);
        assert_eq!(manifest.branches.len(), 3);

        let first = manifest.branch(0).unwrap();
        let second = manifest.branch(1).unwrap();
        let third = manifest.branch(2).unwrap();

        assert_eq!(
            first.operation_id,
            BranchOperationId {
                parallel_op_id: HostOperationId(5),
                branch_index: 0,
            }
        );
        assert_eq!(
            second.operation_id,
            BranchOperationId {
                parallel_op_id: HostOperationId(5),
                branch_index: 1,
            }
        );
        assert_eq!(first.sequence_range.start, 1501);
        assert_eq!(first.sequence_range.end_exclusive, 1601);
        assert_eq!(second.sequence_range.start, 1601);
        assert_eq!(third.sequence_range.start, 1701);
        assert!(first.sequence_range.contains(1501));
        assert!(!first.sequence_range.contains(1601));
    }

    #[test]
    fn parallel_branch_manifest_is_snapshot_serializable() {
        let manifest = ParallelBranchManifest::new("run-1", HostOperationId(9), 2, 4);
        let encoded = serde_json::to_string(&manifest).unwrap();
        let decoded: ParallelBranchManifest = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded, manifest);
    }

    #[test]
    fn snapshot_manifest_round_trips_branch_metadata() {
        let branch_manifest = ParallelBranchManifest::new("parent-run", HostOperationId(9), 2, 4);
        let branch = branch_manifest.branch(1).unwrap();
        let manifest = SnapshotManifest::new(
            "parent-run-branch-1",
            SnapshotAbi::current("chidori-quickjs"),
            RuntimePolicy::durable_default("parent-run"),
            SourceFingerprint::from_source("agent.ts", "export async function agent() {}"),
            Vec::new(),
            None,
            0,
        )
        .with_branch_metadata(SnapshotBranchMetadata {
            parent_run_id: branch_manifest.parent_run_id.clone(),
            parallel_op_id: branch_manifest.parallel_op_id,
            branch_index: branch.branch_index,
            branch_operation_id: branch.operation_id.clone(),
        });

        let decoded: SnapshotManifest =
            serde_json::from_str(&serde_json::to_string(&manifest).unwrap()).unwrap();

        assert_eq!(decoded.branch, manifest.branch);
        assert_eq!(
            decoded.branch.unwrap().branch_operation_id,
            BranchOperationId {
                parallel_op_id: HostOperationId(9),
                branch_index: 1,
            }
        );
    }

    #[test]
    fn snapshot_store_persists_parallel_branch_manifest_and_branch_snapshot() {
        let run_dir = std::env::temp_dir().join(format!(
            "chidori-branch-snapshot-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SnapshotStore::new(&run_dir);
        let branch_manifest =
            ParallelBranchManifest::with_sequence_width("run-1", HostOperationId(4), 2, 2, 50);

        let branch_dir = store
            .save_parallel_branch_manifest(&branch_manifest)
            .unwrap();
        assert!(branch_dir.ends_with("op-00000000000000000004"));

        let loaded_branch_manifest = store
            .load_parallel_branch_manifest(HostOperationId(4))
            .unwrap();
        assert_eq!(loaded_branch_manifest, branch_manifest);

        let branch_store = store.branch_store(&branch_manifest, 1).unwrap();
        let snapshot_manifest = SnapshotManifest::new(
            "run-1",
            SnapshotAbi::current("chidori-quickjs"),
            RuntimePolicy::durable_default("run-1"),
            SourceFingerprint::from_source("agent.ts", "export async function agent() {}"),
            Vec::new(),
            None,
            1,
        )
        .with_branch_metadata(SnapshotBranchMetadata {
            parent_run_id: branch_manifest.parent_run_id.clone(),
            parallel_op_id: branch_manifest.parallel_op_id,
            branch_index: 1,
            branch_operation_id: branch_manifest.branch(1).unwrap().operation_id.clone(),
        });
        branch_store
            .save(&snapshot_manifest, b"branch-snapshot", &[])
            .unwrap();

        let loaded = branch_store.load().unwrap();
        assert_eq!(loaded.blob, b"branch-snapshot");
        assert_eq!(loaded.manifest.branch, snapshot_manifest.branch);
        assert!(branch_store.run_dir().ends_with("branch-001"));

        let _ = fs::remove_dir_all(run_dir);
    }

    #[test]
    fn merge_parallel_branch_outcomes_preserves_branch_order_and_merges_logs() {
        let manifest =
            ParallelBranchManifest::with_sequence_width("run-1", HostOperationId(4), 3, 3, 10);
        let outcomes = vec![
            ParallelBranchOutcome {
                branch_index: 2,
                output: Ok(serde_json::json!("third")),
                call_log: vec![call_record(
                    manifest.branch(2).unwrap().sequence_range.start,
                    "c",
                )],
            },
            ParallelBranchOutcome {
                branch_index: 0,
                output: Ok(serde_json::json!("first")),
                call_log: vec![call_record(
                    manifest.branch(0).unwrap().sequence_range.start,
                    "a",
                )],
            },
            ParallelBranchOutcome {
                branch_index: 1,
                output: Ok(serde_json::json!("second")),
                call_log: vec![call_record(
                    manifest.branch(1).unwrap().sequence_range.start,
                    "b",
                )],
            },
        ];

        let merged = merge_parallel_branch_outcomes(&manifest, &outcomes).unwrap();

        assert_eq!(
            merged.outputs,
            vec![
                serde_json::json!("first"),
                serde_json::json!("second"),
                serde_json::json!("third"),
            ]
        );
        assert_eq!(merged.call_log[0].function, "a");
        assert_eq!(merged.call_log[1].function, "b");
        assert_eq!(merged.call_log[2].function, "c");
    }

    #[test]
    fn merge_parallel_branch_outcomes_propagates_first_branch_error() {
        let manifest =
            ParallelBranchManifest::with_sequence_width("run-1", HostOperationId(4), 3, 3, 10);
        let outcomes = vec![
            ParallelBranchOutcome {
                branch_index: 0,
                output: Ok(serde_json::json!("first")),
                call_log: Vec::new(),
            },
            ParallelBranchOutcome {
                branch_index: 2,
                output: Err("later".to_string()),
                call_log: Vec::new(),
            },
            ParallelBranchOutcome {
                branch_index: 1,
                output: Err("earlier".to_string()),
                call_log: Vec::new(),
            },
        ];

        let err = merge_parallel_branch_outcomes(&manifest, &outcomes).unwrap_err();
        assert!(err
            .to_string()
            .contains("parallel branch 1 failed: earlier"));
    }

    #[test]
    fn merge_parallel_branch_outcomes_rejects_out_of_range_call_log() {
        let manifest =
            ParallelBranchManifest::with_sequence_width("run-1", HostOperationId(4), 1, 1, 10);
        let outcomes = vec![ParallelBranchOutcome {
            branch_index: 0,
            output: Ok(serde_json::json!("ok")),
            call_log: vec![call_record(
                manifest.branch(0).unwrap().sequence_range.end_exclusive,
                "bad",
            )],
        }];

        let err = merge_parallel_branch_outcomes(&manifest, &outcomes).unwrap_err();
        assert!(err.to_string().contains("outside reserved range"));
    }

    #[test]
    fn abi_mismatch_is_rejected() {
        let snapshot = SnapshotAbi::current("chidori-quickjs-a");
        let runtime = SnapshotAbi::current("chidori-quickjs-b");
        assert!(snapshot.ensure_compatible(&runtime).is_err());
    }

    #[test]
    fn snapshot_store_round_trips_manifest_and_blob() {
        let run_dir =
            std::env::temp_dir().join(format!("chidori-snapshot-test-{}", uuid::Uuid::new_v4()));
        let store = SnapshotStore::new(&run_dir);
        let pending = PendingHostOperation::new(
            HostOperationId(7),
            3,
            PendingHostOperationKind::Input,
            serde_json::json!({ "prompt": "Proceed?" }),
        );
        let manifest = SnapshotManifest::new(
            "run-1",
            SnapshotAbi::current("chidori-quickjs"),
            RuntimePolicy::durable_default("run-1"),
            SourceFingerprint::from_source("agent.ts", "export async function agent() {}"),
            Vec::new(),
            Some(pending),
            0,
        );
        let call_log: Vec<CallRecord> = Vec::new();

        store.save(&manifest, b"snapshot-bytes", &call_log).unwrap();
        let loaded = store.load().unwrap();

        assert_eq!(loaded.blob, b"snapshot-bytes");
        assert_eq!(loaded.manifest.run_id, "run-1");
        assert_eq!(loaded.manifest.pending.unwrap().id, HostOperationId(7));

        let _ = fs::remove_dir_all(run_dir);
    }

    #[test]
    fn snapshot_store_loads_manifest_without_blob() {
        let run_dir = std::env::temp_dir().join(format!(
            "chidori-snapshot-manifest-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SnapshotStore::new(&run_dir);
        let manifest = SnapshotManifest::new(
            "run-1",
            SnapshotAbi::current("chidori-quickjs"),
            RuntimePolicy::durable_default("run-1"),
            SourceFingerprint::from_source("agent.ts", "export async function agent() {}"),
            Vec::new(),
            None,
            0,
        );

        store.save(&manifest, b"snapshot-bytes", &[]).unwrap();
        fs::remove_file(run_dir.join(SNAPSHOT_BLOB_FILE)).unwrap();

        let loaded_manifest = store.load_manifest().unwrap();
        assert_eq!(loaded_manifest.run_id, "run-1");
        assert!(store.load().is_err());

        let _ = fs::remove_dir_all(run_dir);
    }

    #[test]
    fn host_date_and_random_are_rejected_for_durable_runs() {
        let mut policy = RuntimePolicy::durable_default("run-1");
        policy.date = DatePolicy::Host;
        assert!(policy.ensure_durable_safe().is_err());

        let mut policy = RuntimePolicy::durable_default("run-1");
        policy.random = RandomPolicy::Host;
        assert!(policy.ensure_durable_safe().is_err());
    }

    #[test]
    fn policy_mismatch_is_rejected() {
        let snapshot = RuntimePolicy::durable_default("run-1");
        let mut runtime = RuntimePolicy::durable_default("run-1");
        runtime.typescript_imports = TypeScriptImportPolicy::Project;

        assert!(snapshot.ensure_compatible(&runtime).is_err());
    }

    #[test]
    fn snapshot_capable_trait_targets_chidori_quickjs_wrapper() {
        let mut runtime =
            chidori_quickjs::SnapshotRuntime::new(chidori_quickjs::RuntimeLimits::default())
                .unwrap();

        let snapshot =
            <chidori_quickjs::SnapshotRuntime as SnapshotCapableJsEngine>::snapshot(&mut runtime)
                .unwrap();
        let snapshot = chidori_quickjs::RuntimeSnapshot(snapshot);

        assert_eq!(
            snapshot.payload().unwrap(),
            b"CHIDORI_QJS_RUNTIME_SNAPSHOT_V1"
        );
        assert_eq!(snapshot.context_payload().unwrap(), b"");
    }

    #[test]
    fn snapshot_capable_trait_drains_quickjs_jobs_when_idle() {
        let mut runtime =
            chidori_quickjs::SnapshotRuntime::new(chidori_quickjs::RuntimeLimits::default())
                .unwrap();

        let state =
            <chidori_quickjs::SnapshotRuntime as SnapshotCapableJsEngine>::run_jobs_until_blocked(
                &mut runtime,
            )
            .unwrap();

        assert_eq!(state, JsRunState::Completed);
    }

    #[test]
    fn load_for_resume_rejects_incompatible_snapshot_before_blob_use() {
        let run_dir = std::env::temp_dir().join(format!(
            "chidori-snapshot-resume-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SnapshotStore::new(&run_dir);
        let policy = RuntimePolicy::durable_default("run-1");
        let abi = SnapshotAbi::current("chidori-quickjs");
        let entry = SourceFingerprint::from_source("agent.ts", "export async function agent() {}");
        let modules = vec![SourceFingerprint::from_source(
            "child.ts",
            "export const value = 1;",
        )];
        let manifest = SnapshotManifest::new(
            "run-1",
            abi.clone(),
            policy.clone(),
            entry.clone(),
            modules.clone(),
            None,
            0,
        );

        store.save(&manifest, b"snapshot-bytes", &[]).unwrap();
        let loaded = store
            .load_for_resume(&abi, &policy, &entry, &modules)
            .unwrap();
        assert_eq!(loaded.blob, b"snapshot-bytes");
        fs::remove_file(run_dir.join(SNAPSHOT_BLOB_FILE)).unwrap();

        let wrong_abi = SnapshotAbi::current("other-fork");
        assert!(store
            .load_for_resume(&wrong_abi, &policy, &entry, &modules)
            .unwrap_err()
            .to_string()
            .contains("runtime snapshot ABI mismatch"));

        let wrong_entry = SourceFingerprint::from_source("agent.ts", "changed");
        assert!(store
            .load_for_resume(&abi, &policy, &wrong_entry, &modules)
            .unwrap_err()
            .to_string()
            .contains("runtime snapshot source mismatch"));

        let wrong_modules = vec![SourceFingerprint::from_source("child.ts", "changed")];
        assert!(store
            .load_for_resume(&abi, &policy, &entry, &wrong_modules)
            .unwrap_err()
            .to_string()
            .contains("runtime snapshot module mismatch"));

        let mut wrong_policy = policy.clone();
        wrong_policy.typescript_imports = TypeScriptImportPolicy::Project;
        assert!(store
            .load_for_resume(&abi, &wrong_policy, &entry, &modules)
            .unwrap_err()
            .to_string()
            .contains("runtime snapshot policy mismatch"));

        let _ = fs::remove_dir_all(run_dir);
    }

    #[test]
    fn load_live_vm_for_resume_rejects_scaffold_snapshot_kind_before_blob_use() {
        let run_dir = std::env::temp_dir().join(format!(
            "chidori-snapshot-live-kind-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SnapshotStore::new(&run_dir);
        let policy = RuntimePolicy::durable_default("run-1");
        let abi = SnapshotAbi::current("chidori-quickjs");
        let entry = SourceFingerprint::from_source("agent.ts", "export async function agent() {}");
        let manifest = SnapshotManifest::new(
            "run-1",
            abi.clone(),
            policy.clone(),
            entry.clone(),
            Vec::new(),
            None,
            0,
        );

        store.save(&manifest, b"snapshot-bytes", &[]).unwrap();
        fs::remove_file(run_dir.join(SNAPSHOT_BLOB_FILE)).unwrap();

        let err = store
            .load_live_vm_for_resume(&abi, &policy, &entry, &[], &[])
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("runtime snapshot blob kind mismatch"));

        let _ = fs::remove_dir_all(run_dir);
    }

    #[test]
    fn save_live_vm_snapshot_sets_kind_and_validates_envelope() {
        let run_dir = std::env::temp_dir().join(format!(
            "chidori-snapshot-live-save-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SnapshotStore::new(&run_dir);
        let policy = RuntimePolicy::durable_default("run-1");
        let abi = SnapshotAbi::current("chidori-quickjs");
        let entry = SourceFingerprint::from_source("agent.ts", "export async function agent() {}");
        let manifest = SnapshotManifest::new(
            "run-1",
            abi.clone(),
            policy.clone(),
            entry.clone(),
            Vec::new(),
            None,
            0,
        );
        let snapshot = chidori_quickjs::RuntimeSnapshot::from_payload(b"live-vm-payload");

        store
            .save_live_vm_snapshot(&manifest, &snapshot, &[])
            .unwrap();

        let loaded = store
            .load_live_vm_for_resume(&abi, &policy, &entry, &[], &[])
            .unwrap();
        assert_eq!(
            loaded.manifest.snapshot_kind,
            SnapshotBlobKind::LiveQuickJsVm
        );
        assert_eq!(loaded.blob, snapshot.0);

        let _ = fs::remove_dir_all(run_dir);
    }

    #[test]
    fn save_live_vm_snapshot_rejects_invalid_runtime_snapshot_envelope() {
        let run_dir = std::env::temp_dir().join(format!(
            "chidori-snapshot-live-save-invalid-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SnapshotStore::new(&run_dir);
        let manifest = SnapshotManifest::new(
            "run-1",
            SnapshotAbi::current("chidori-quickjs"),
            RuntimePolicy::durable_default("run-1"),
            SourceFingerprint::from_source("agent.ts", "export async function agent() {}"),
            Vec::new(),
            None,
            0,
        );
        let snapshot = chidori_quickjs::RuntimeSnapshot(b"not-enveloped".to_vec());

        let err = store
            .save_live_vm_snapshot(&manifest, &snapshot, &[])
            .unwrap_err();
        assert!(err.to_string().contains("invalid live VM runtime snapshot"));
        assert!(!run_dir.join(SNAPSHOT_MANIFEST_FILE).exists());

        let _ = fs::remove_dir_all(run_dir);
    }

    #[test]
    fn save_live_vm_snapshot_rejects_empty_context_payload() {
        let run_dir = std::env::temp_dir().join(format!(
            "chidori-snapshot-live-save-empty-context-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SnapshotStore::new(&run_dir);
        let manifest = SnapshotManifest::new(
            "run-1",
            SnapshotAbi::current("chidori-quickjs"),
            RuntimePolicy::durable_default("run-1"),
            SourceFingerprint::from_source("agent.ts", "export async function agent() {}"),
            Vec::new(),
            None,
            0,
        );
        let snapshot = chidori_quickjs::RuntimeSnapshot::from_parts(b"runtime-payload", b"");

        let err = store
            .save_live_vm_snapshot(&manifest, &snapshot, &[])
            .unwrap_err();
        assert!(err.to_string().contains("invalid live VM runtime snapshot"));
        assert!(err.to_string().contains("context payload is empty"));
        assert!(!run_dir.join(SNAPSHOT_MANIFEST_FILE).exists());

        let _ = fs::remove_dir_all(run_dir);
    }

    #[test]
    fn load_live_vm_for_resume_rejects_empty_context_payload() {
        let run_dir = std::env::temp_dir().join(format!(
            "chidori-snapshot-live-load-empty-context-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SnapshotStore::new(&run_dir);
        let policy = RuntimePolicy::durable_default("run-1");
        let abi = SnapshotAbi::current("chidori-quickjs");
        let entry = SourceFingerprint::from_source("agent.ts", "export async function agent() {}");
        let manifest = SnapshotManifest::new(
            "run-1",
            abi.clone(),
            policy.clone(),
            entry.clone(),
            Vec::new(),
            None,
            0,
        )
        .with_snapshot_kind(SnapshotBlobKind::LiveQuickJsVm);
        let snapshot = chidori_quickjs::RuntimeSnapshot::from_parts(b"runtime-payload", b"");
        store.save(&manifest, &snapshot.0, &[]).unwrap();

        let err = store
            .load_live_vm_for_resume(&abi, &policy, &entry, &[], &[])
            .unwrap_err();
        assert!(err.to_string().contains("invalid live VM runtime snapshot"));
        assert!(err.to_string().contains("context payload is empty"));

        let _ = fs::remove_dir_all(run_dir);
    }

    #[test]
    fn start_live_parallel_branch_runtimes_restores_each_branch_from_parent_snapshot() {
        let run_dir = std::env::temp_dir().join(format!(
            "chidori-live-branch-start-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SnapshotStore::new(&run_dir);
        let policy = RuntimePolicy::durable_default("run-1");
        let abi = SnapshotAbi::current("chidori-quickjs");
        let entry = SourceFingerprint::from_source("agent.ts", "export async function agent() {}");
        let parent_manifest = SnapshotManifest::new(
            "run-1",
            abi.clone(),
            policy.clone(),
            entry.clone(),
            Vec::new(),
            None,
            0,
        );
        let parent_snapshot = chidori_quickjs::RuntimeSnapshot::from_payload(b"parent-live-vm");
        store
            .save_live_vm_snapshot(&parent_manifest, &parent_snapshot, &[])
            .unwrap();
        let branch_manifest = ParallelBranchManifest::new("run-1", HostOperationId(7), 3, 2);

        let branches = start_live_parallel_branch_runtimes::<FakeLiveBranchEngine>(
            &store,
            &branch_manifest,
            &abi,
            &policy,
            &entry,
            &[],
            &[],
        )
        .unwrap();

        assert_eq!(branches.len(), 3);
        assert!(branches
            .iter()
            .all(|branch| branch.snapshot == parent_snapshot.0));
        assert_eq!(
            store
                .load_parallel_branch_manifest(HostOperationId(7))
                .unwrap(),
            branch_manifest
        );

        let _ = fs::remove_dir_all(run_dir);
    }

    #[test]
    fn save_live_parallel_branch_runtime_snapshot_persists_branch_metadata() {
        let run_dir = std::env::temp_dir().join(format!(
            "chidori-live-branch-save-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SnapshotStore::new(&run_dir);
        let policy = RuntimePolicy::durable_default("run-1");
        let entry = SourceFingerprint::from_source("agent.ts", "export async function agent() {}");
        let branch_manifest = ParallelBranchManifest::new("run-1", HostOperationId(8), 2, 2);
        store
            .save_parallel_branch_manifest(&branch_manifest)
            .unwrap();
        let mut runtime = FakeLiveBranchEngine::with_snapshot(
            chidori_quickjs::RuntimeSnapshot::from_payload(b"branch-live-vm"),
        );
        let call_log = vec![call_record(
            branch_manifest.branch(1).unwrap().sequence_range.start,
            "prompt",
        )];

        save_live_parallel_branch_runtime_snapshot(
            &store,
            &branch_manifest,
            1,
            &mut runtime,
            &policy,
            &entry,
            &[],
            &[],
            None,
            &call_log,
        )
        .unwrap();

        let branch_store = store.branch_store(&branch_manifest, 1).unwrap();
        let loaded = branch_store
            .load_live_vm_for_resume(
                &SnapshotAbi::current("chidori-quickjs"),
                &policy,
                &entry,
                &[],
                &[],
            )
            .unwrap();
        assert_eq!(
            loaded.manifest.snapshot_kind,
            SnapshotBlobKind::LiveQuickJsVm
        );
        assert_eq!(loaded.manifest.call_log_len, 1);
        assert_eq!(
            loaded.manifest.branch,
            Some(SnapshotBranchMetadata {
                parent_run_id: "run-1".to_string(),
                parallel_op_id: HostOperationId(8),
                branch_index: 1,
                branch_operation_id: branch_manifest.branch(1).unwrap().operation_id.clone(),
            })
        );
        assert_eq!(
            loaded.blob,
            chidori_quickjs::RuntimeSnapshot::from_payload(b"branch-live-vm").0
        );

        let _ = fs::remove_dir_all(run_dir);
    }

    #[test]
    fn resume_live_parallel_branch_from_store_restores_and_resolves_host_promise() {
        let run_dir = std::env::temp_dir().join(format!(
            "chidori-live-branch-resume-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SnapshotStore::new(&run_dir);
        let policy = RuntimePolicy::durable_default("run-1");
        let abi = SnapshotAbi::current("chidori-quickjs");
        let entry = SourceFingerprint::from_source("agent.ts", "export async function agent() {}");
        let branch_manifest = ParallelBranchManifest::new("run-1", HostOperationId(9), 2, 2);
        store
            .save_parallel_branch_manifest(&branch_manifest)
            .unwrap();
        let branch_snapshot = chidori_quickjs::RuntimeSnapshot::from_payload(b"paused-branch-live");
        let branch = branch_manifest.branch(1).unwrap();
        let branch_snapshot_manifest = SnapshotManifest::new(
            "run-1-branch-1",
            abi.clone(),
            policy.clone(),
            entry.clone(),
            Vec::new(),
            Some(PendingHostOperation::new(
                HostOperationId(77),
                branch.sequence_range.start,
                PendingHostOperationKind::Prompt,
                serde_json::json!({ "text": "continue" }),
            )),
            0,
        )
        .with_branch_metadata(SnapshotBranchMetadata {
            parent_run_id: "run-1".to_string(),
            parallel_op_id: HostOperationId(9),
            branch_index: 1,
            branch_operation_id: branch.operation_id.clone(),
        });
        store
            .branch_store(&branch_manifest, 1)
            .unwrap()
            .save_live_vm_snapshot(&branch_snapshot_manifest, &branch_snapshot, &[])
            .unwrap();

        let (runtime, state) = resume_live_parallel_branch_from_store::<FakeLiveBranchEngine>(
            &store,
            HostOperationId(9),
            1,
            &abi,
            &policy,
            &entry,
            &[],
            &[],
            HostOperationId(77),
            serde_json::json!("done"),
        )
        .unwrap();

        assert_eq!(state, JsRunState::Completed);
        assert_eq!(runtime.snapshot, branch_snapshot.0);
        assert_eq!(
            runtime.resolved,
            vec![(HostOperationId(77), serde_json::json!("done"))]
        );
        assert_eq!(runtime.run_count, 1);

        let _ = fs::remove_dir_all(run_dir);
    }

    #[test]
    fn reject_live_parallel_branch_from_store_restores_and_rejects_host_promise() {
        let run_dir = std::env::temp_dir().join(format!(
            "chidori-live-branch-reject-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SnapshotStore::new(&run_dir);
        let policy = RuntimePolicy::durable_default("run-1");
        let abi = SnapshotAbi::current("chidori-quickjs");
        let entry = SourceFingerprint::from_source("agent.ts", "export async function agent() {}");
        let branch_manifest = ParallelBranchManifest::new("run-1", HostOperationId(11), 2, 2);
        store
            .save_parallel_branch_manifest(&branch_manifest)
            .unwrap();
        let branch_snapshot = chidori_quickjs::RuntimeSnapshot::from_payload(b"failed-branch-live");
        let branch = branch_manifest.branch(1).unwrap();
        let branch_snapshot_manifest = SnapshotManifest::new(
            "run-1-branch-1",
            abi.clone(),
            policy.clone(),
            entry.clone(),
            Vec::new(),
            Some(PendingHostOperation::new(
                HostOperationId(88),
                branch.sequence_range.start,
                PendingHostOperationKind::Tool,
                serde_json::json!({ "name": "missing" }),
            )),
            0,
        )
        .with_branch_metadata(SnapshotBranchMetadata {
            parent_run_id: "run-1".to_string(),
            parallel_op_id: HostOperationId(11),
            branch_index: 1,
            branch_operation_id: branch.operation_id.clone(),
        });
        store
            .branch_store(&branch_manifest, 1)
            .unwrap()
            .save_live_vm_snapshot(&branch_snapshot_manifest, &branch_snapshot, &[])
            .unwrap();

        let (runtime, state) = reject_live_parallel_branch_from_store::<FakeLiveBranchEngine>(
            &store,
            HostOperationId(11),
            1,
            &abi,
            &policy,
            &entry,
            &[],
            &[],
            HostOperationId(88),
            "tool failed".to_string(),
        )
        .unwrap();

        assert_eq!(state, JsRunState::Completed);
        assert_eq!(runtime.snapshot, branch_snapshot.0);
        assert_eq!(
            runtime.rejected,
            vec![(HostOperationId(88), "tool failed".to_string())]
        );
        assert_eq!(runtime.run_count, 1);

        let _ = fs::remove_dir_all(run_dir);
    }

    #[test]
    fn resume_live_parallel_branch_from_store_rejects_branch_metadata_mismatch() {
        let run_dir = std::env::temp_dir().join(format!(
            "chidori-live-branch-resume-metadata-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SnapshotStore::new(&run_dir);
        let policy = RuntimePolicy::durable_default("run-1");
        let abi = SnapshotAbi::current("chidori-quickjs");
        let entry = SourceFingerprint::from_source("agent.ts", "export async function agent() {}");
        let branch_manifest = ParallelBranchManifest::new("run-1", HostOperationId(10), 2, 2);
        store
            .save_parallel_branch_manifest(&branch_manifest)
            .unwrap();
        let branch_snapshot = chidori_quickjs::RuntimeSnapshot::from_payload(b"wrong-branch-live");
        let wrong_snapshot_manifest = SnapshotManifest::new(
            "run-1-branch-0",
            abi.clone(),
            policy.clone(),
            entry.clone(),
            Vec::new(),
            None,
            0,
        )
        .with_branch_metadata(SnapshotBranchMetadata {
            parent_run_id: "run-1".to_string(),
            parallel_op_id: HostOperationId(10),
            branch_index: 0,
            branch_operation_id: branch_manifest.branch(0).unwrap().operation_id.clone(),
        });
        store
            .branch_store(&branch_manifest, 1)
            .unwrap()
            .save_live_vm_snapshot(&wrong_snapshot_manifest, &branch_snapshot, &[])
            .unwrap();

        let err = resume_live_parallel_branch_from_store::<FakeLiveBranchEngine>(
            &store,
            HostOperationId(10),
            1,
            &abi,
            &policy,
            &entry,
            &[],
            &[],
            HostOperationId(77),
            serde_json::json!("done"),
        )
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("runtime snapshot branch metadata mismatch"));

        let _ = fs::remove_dir_all(run_dir);
    }

    #[test]
    fn resume_live_parallel_branch_from_store_rejects_pending_host_operation_mismatch() {
        let run_dir = std::env::temp_dir().join(format!(
            "chidori-live-branch-resume-pending-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SnapshotStore::new(&run_dir);
        let policy = RuntimePolicy::durable_default("run-1");
        let abi = SnapshotAbi::current("chidori-quickjs");
        let entry = SourceFingerprint::from_source("agent.ts", "export async function agent() {}");
        let branch_manifest = ParallelBranchManifest::new("run-1", HostOperationId(12), 2, 2);
        store
            .save_parallel_branch_manifest(&branch_manifest)
            .unwrap();
        let branch_snapshot = chidori_quickjs::RuntimeSnapshot::from_payload(b"paused-branch-live");
        let branch = branch_manifest.branch(1).unwrap();
        let branch_snapshot_manifest = SnapshotManifest::new(
            "run-1-branch-1",
            abi.clone(),
            policy.clone(),
            entry.clone(),
            Vec::new(),
            Some(PendingHostOperation::new(
                HostOperationId(99),
                branch.sequence_range.start,
                PendingHostOperationKind::Prompt,
                serde_json::json!({ "text": "continue" }),
            )),
            0,
        )
        .with_branch_metadata(SnapshotBranchMetadata {
            parent_run_id: "run-1".to_string(),
            parallel_op_id: HostOperationId(12),
            branch_index: 1,
            branch_operation_id: branch.operation_id.clone(),
        });
        store
            .branch_store(&branch_manifest, 1)
            .unwrap()
            .save_live_vm_snapshot(&branch_snapshot_manifest, &branch_snapshot, &[])
            .unwrap();

        let err = resume_live_parallel_branch_from_store::<FakeLiveBranchEngine>(
            &store,
            HostOperationId(12),
            1,
            &abi,
            &policy,
            &entry,
            &[],
            &[],
            HostOperationId(77),
            serde_json::json!("done"),
        )
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("runtime snapshot pending host operation mismatch"));

        let _ = fs::remove_dir_all(run_dir);
    }

    #[test]
    fn reject_live_parallel_branch_from_store_rejects_missing_pending_host_operation() {
        let run_dir = std::env::temp_dir().join(format!(
            "chidori-live-branch-reject-pending-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SnapshotStore::new(&run_dir);
        let policy = RuntimePolicy::durable_default("run-1");
        let abi = SnapshotAbi::current("chidori-quickjs");
        let entry = SourceFingerprint::from_source("agent.ts", "export async function agent() {}");
        let branch_manifest = ParallelBranchManifest::new("run-1", HostOperationId(13), 2, 2);
        store
            .save_parallel_branch_manifest(&branch_manifest)
            .unwrap();
        let branch_snapshot = chidori_quickjs::RuntimeSnapshot::from_payload(b"failed-branch-live");
        let branch = branch_manifest.branch(1).unwrap();
        let branch_snapshot_manifest = SnapshotManifest::new(
            "run-1-branch-1",
            abi.clone(),
            policy.clone(),
            entry.clone(),
            Vec::new(),
            None,
            0,
        )
        .with_branch_metadata(SnapshotBranchMetadata {
            parent_run_id: "run-1".to_string(),
            parallel_op_id: HostOperationId(13),
            branch_index: 1,
            branch_operation_id: branch.operation_id.clone(),
        });
        store
            .branch_store(&branch_manifest, 1)
            .unwrap()
            .save_live_vm_snapshot(&branch_snapshot_manifest, &branch_snapshot, &[])
            .unwrap();

        let err = reject_live_parallel_branch_from_store::<FakeLiveBranchEngine>(
            &store,
            HostOperationId(13),
            1,
            &abi,
            &policy,
            &entry,
            &[],
            &[],
            HostOperationId(88),
            "tool failed".to_string(),
        )
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("runtime snapshot pending host operation missing"));

        let _ = fs::remove_dir_all(run_dir);
    }
}
