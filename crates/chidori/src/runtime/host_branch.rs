//! `chidori.branch` — in-agent execution branching.
//!
//! An agent forks itself mid-run into N branches that each explore a strategy
//! from the same anchored state and return an outcome for comparison
//! (`docs/branching-execution.md`). A branch is a **separate continuation
//! source run once** — not a re-run of the parent (which would re-reach
//! `chidori.branch` and recurse, §8.2). The prefix is handed over as state:
//! each branch inherits the parent's VFS snapshot and receives an explicit
//! `input`, then runs live on its own [`RuntimeContext`] whose sequence
//! numbers come from a reserved, disjoint [`CallLogSequenceRange`].
//!
//! The whole fan-out is one recorded durable call on the parent, so a parent
//! replay returns the outcomes from cache and never re-runs the branches.
//! Variants run in waves of `options.concurrency` threads (default 1 —
//! sequential); outcome order always follows variant order.
//!
//! ## The branch store (Phase 2)
//!
//! When the parent run persists (`.chidori/runs/<run id>/`), every branch
//! sub-run is persisted under it:
//!
//! ```text
//! <run dir>/branches/op-<branch seq>/
//!   anchor.json              fork-time anchor: the parent VFS snapshot
//!   branch-<k>/
//!     source.ts              the branch's own EDITABLE source copy
//!     checkpoint.json        the branch's call log (same shape as a run's)
//!     branch.json            metadata: label, id, status, pending input,
//!                            reserved sequence range, input, output/error
//! ```
//!
//! That store makes a branch independently operable out-of-band, after the
//! parent has moved on:
//! - [`resume_branch`] answers a paused branch's pending `input()` by
//!   replaying its checkpoint with a synthetic `input` record (the same
//!   mechanism the server's `/resume` uses) and running to the next outcome.
//! - [`rerun_branch`] re-runs a branch **fresh from the parent anchor** with
//!   whatever `source.ts` now contains — edit the file, re-run, and only that
//!   strategy changes while the anchored state stays identical.
//!
//! A resumed or re-run branch updates its own store; the parent's recorded
//! `branch` outcome is immutable history (branches are independent referenced
//! sub-runs, compared — not merged — per the design's non-goals).

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::runtime::call_log::CallRecord;
use crate::runtime::context::{PendingInput, RuntimeContext};
use crate::runtime::errors::RunInterrupt;
use crate::runtime::host_core;
use crate::runtime::snapshot::{
    CallLogSequenceRange, HostOperationId, ParallelBranchManifest, BRANCHES_DIR,
    DEFAULT_BRANCH_SEQUENCE_RANGE_WIDTH,
};
use crate::runtime::typescript::bindings::HostBindingBackend;
use crate::runtime::vfs::Vfs;

/// Hard cap on the branch fan-out: every branch makes live host calls past the
/// fork (real LLM/tool spend), so an unbounded `variants` array is a cost
/// hazard before it is a correctness one.
const MAX_BRANCHES: usize = 16;

/// Stack size for branch worker threads. The JS interpreter recurses with the
/// agent's call depth, so give branch threads the same headroom the main
/// thread gets rather than the 2 MiB std default.
const BRANCH_THREAD_STACK_BYTES: usize = 16 * 1024 * 1024;

const BRANCH_META_FILE: &str = "branch.json";
const BRANCH_SOURCE_FILE: &str = "source.ts";
const BRANCH_CHECKPOINT_FILE: &str = "checkpoint.json";
const BRANCH_ANCHOR_FILE: &str = "anchor.json";
const BRANCH_STORE_VERSION: u32 = 1;

/// One validated `chidori.branch` variant: a label for outcomes/trace, the
/// branch's own source module (path + the text read at validation time, which
/// seeds the branch store's editable copy), and the state handed over as its
/// run input.
struct BranchVariant {
    label: String,
    source: String,
    source_text: String,
    input: Value,
}

/// Fork-time anchor persisted once per branch op: everything a branch needs to
/// re-run from the same shared state after the parent is gone.
#[derive(Serialize, Deserialize)]
struct BranchAnchor {
    version: u32,
    parent_run_id: String,
    branch_seq: u64,
    vfs: Vfs,
    created_at: DateTime<Utc>,
}

/// Per-branch metadata (`branch.json`): identity, anchor coordinates, the
/// reserved sequence range, and the latest outcome state.
#[derive(Serialize, Deserialize)]
struct BranchMeta {
    version: u32,
    branch_id: String,
    label: String,
    branch_index: u32,
    branch_seq: u64,
    parent_run_id: String,
    original_source: String,
    input: Value,
    sequence_range: CallLogSequenceRange,
    status: String,
    pending_input: Option<PendingInput>,
    pending_prompt: Option<String>,
    output: Option<Value>,
    error: Option<String>,
    updated_at: DateTime<Utc>,
}

/// How one branch run ended, normalized from `run_agent_file`'s result plus
/// the branch context's pending state. Shared by the in-agent fan-out and the
/// out-of-band resume/rerun paths so all three report identical outcomes.
struct SettledBranch {
    status: &'static str,
    output: Option<Value>,
    error: Option<String>,
    pending_input: Option<PendingInput>,
    pending_prompt: Option<String>,
}

fn settle_branch(result: anyhow::Result<Value>, branch_ctx: &RuntimeContext) -> SettledBranch {
    match result {
        Ok(output) => SettledBranch {
            status: "completed",
            output: Some(output),
            error: None,
            pending_input: None,
            pending_prompt: None,
        },
        Err(err) if RunInterrupt::from_error(&err).is_some() => {
            // A suspended branch is reported as paused; when it suspended on
            // `input()` the persisted pending op makes it resumable via
            // `resume_branch`. Approval/signal pauses are reported but not yet
            // resumable out-of-band.
            let pending_input = branch_ctx.take_pending_input();
            let pending_prompt = pending_input
                .as_ref()
                .map(|pending| pending.prompt.clone())
                .or_else(|| {
                    branch_ctx
                        .take_pending_approval()
                        .map(|pending| format!("approval required: {}", pending.target))
                })
                .or_else(|| {
                    branch_ctx
                        .take_pending_signal()
                        .map(|pending| format!("waiting on signal: {}", pending.name))
                });
            SettledBranch {
                status: "paused",
                output: None,
                error: None,
                pending_input,
                pending_prompt,
            }
        }
        Err(err) => SettledBranch {
            status: "failed",
            output: None,
            error: Some(err.to_string()),
            pending_input: None,
            pending_prompt: None,
        },
    }
}

fn branch_outcome_json(branch_id: &str, label: &str, settled: &SettledBranch) -> Value {
    let mut outcome = json!({
        "label": label,
        "branchId": branch_id,
        "status": settled.status,
    });
    if let Some(ref output) = settled.output {
        outcome["output"] = output.clone();
    }
    if let Some(ref prompt) = settled.pending_prompt {
        outcome["pendingPrompt"] = json!(prompt);
    }
    if let Some(ref error) = settled.error {
        outcome["error"] = json!(error);
    }
    outcome
}

/// Run `chidori.branch(variants, options)`: fork the agent into one sub-run
/// per variant from the parent's current state and return the
/// `BranchOutcome[]` JSON the agent awaits. The fan-out executes inside the
/// durable boundary as a single recorded `branch` call; variants run in waves
/// of `options.concurrency` worker threads (default 1 — sequential).
pub(crate) fn run_branches(
    backend: &HostBindingBackend,
    args: &Value,
) -> std::result::Result<Value, String> {
    let ctx = backend
        .runtime_ctx()
        .ok_or("chidori.branch requires the runtime host backend")?;
    if ctx.is_branch() {
        return Err(
            "nested chidori.branch is not supported: a branch cannot fork again (its records \
             must stay inside the reserved sequence range of the parent branch)"
                .to_string(),
        );
    }

    let variants = parse_variants(args)?;
    let concurrency = args
        .get("options")
        .and_then(|options| options.get("concurrency"))
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1)
        .min(variants.len() as u64) as usize;

    // Normalized args make the durable record self-describing (defaults
    // resolved), independent of the exact JS-side argument shape.
    let call_args = json!({
        "variants": variants
            .iter()
            .map(|v| json!({ "label": v.label, "source": v.source, "input": v.input }))
            .collect::<Vec<_>>(),
        "options": { "concurrency": concurrency },
    });

    // Allocate the seq explicitly so the fan-out below can seed each branch
    // context's call stack with it (`execute_durable_json_call` doesn't expose
    // the seq to its `live()` closure).
    let seq = ctx.next_seq();
    host_core::execute_durable_json_call_at_seq(ctx, seq, "branch", call_args, || {
        run_branches_live(backend, ctx, seq, &variants, concurrency)
            .map_err(|err| anyhow::anyhow!(err))
    })
    .map_err(|err| err.to_string())
}

/// The live fan-out: reserve disjoint sequence ranges, persist the fork
/// anchor, run the variants in concurrency-capped waves on worker threads,
/// validate range confinement, fold each branch's records into the parent
/// log, persist each branch store, and return the outcomes array.
fn run_branches_live(
    backend: &HostBindingBackend,
    ctx: &RuntimeContext,
    branch_seq: u64,
    variants: &[BranchVariant],
    concurrency: usize,
) -> std::result::Result<Value, String> {
    let count = variants.len() as u32;
    let width = DEFAULT_BRANCH_SEQUENCE_RANGE_WIDTH;
    // Reserve the next disjoint block of `count` ranges above every sequence
    // number used so far. The manifest derives `base = slot * width * count`,
    // so picking the first slot whose base clears the branch call's own seq
    // keeps successive branch ops' ranges monotonically increasing (linear,
    // not geometric, growth) and disjoint from all earlier records. The base
    // never needs to be re-derived on replay — the recorded branch records
    // keep their seqs and `absorb_replayed_subtree` realigns the counter.
    let block = width.saturating_mul(u64::from(count));
    let slot = branch_seq / block + 1;
    let parent_run_id = ctx.run_id();
    let manifest = ParallelBranchManifest::with_sequence_width(
        parent_run_id.clone(),
        HostOperationId(slot),
        count,
        concurrency as u32,
        width,
    );

    // Persist the fork anchor + each branch's editable source copy up front,
    // before any branch spends anything — so even a crash mid-fan-out leaves
    // re-runnable branch stores behind. Best-effort: the durable record of the
    // fan-out is the parent call log; the store enables out-of-band ops.
    let store_op_dir = ctx
        .persist_dir()
        .map(|run_dir| op_dir(&run_dir, branch_seq));
    if let Some(ref op_dir) = store_op_dir {
        let anchor = BranchAnchor {
            version: BRANCH_STORE_VERSION,
            parent_run_id: parent_run_id.clone(),
            branch_seq,
            vfs: ctx.vfs_snapshot(),
            created_at: Utc::now(),
        };
        if let Err(err) = persist_anchor_and_sources(op_dir, &anchor, variants) {
            tracing::warn!(error = %err, "failed to persist branch store anchor");
        }
    }

    let mut outcomes = Vec::with_capacity(variants.len());
    let indexed: Vec<(usize, &BranchVariant)> = variants.iter().enumerate().collect();
    for wave in indexed.chunks(concurrency.max(1)) {
        // One thread per branch in the wave; the chidori-js VM is built inside
        // the thread, host effects flow through the branch's own context, and
        // the disjoint sequence ranges keep concurrent records collision-free.
        // Settling, validation, merging, and persistence happen back on this
        // thread, in variant order, after the wave joins — so the merged log
        // and the outcomes array are deterministic regardless of completion
        // order.
        let mut wave_runs = Vec::with_capacity(wave.len());
        std::thread::scope(|scope| -> std::result::Result<(), String> {
            let mut handles = Vec::with_capacity(wave.len());
            for (index, variant) in wave {
                let branch = manifest
                    .branch(*index as u32)
                    .ok_or_else(|| format!("missing branch metadata for index {index}"))?;
                let range = branch.sequence_range.clone();
                let branch_id = format!("{parent_run_id}-op{branch_seq}-branch-{index}");
                let branch_ctx =
                    RuntimeContext::for_branch(ctx, branch_id.clone(), range.start - 1, branch_seq);
                // Stamp variant identity on the branch's OTEL spans so each
                // fan-out subtree is filterable by `chidori.branch_label`.
                branch_ctx.set_otel_branch(branch_id.clone(), variant.label.clone());
                let branch_backend = backend
                    .with_runtime_ctx(branch_ctx.clone())
                    .ok_or("chidori.branch requires the runtime host backend")?;
                let source = variant.source.clone();
                let input = variant.input.clone();
                let handle = std::thread::Builder::new()
                    .name(format!("chidori-branch-{index}"))
                    .stack_size(BRANCH_THREAD_STACK_BYTES)
                    .spawn_scoped(scope, move || {
                        crate::runtime::rust_engine::run_agent_file(
                            Path::new(&source),
                            &input,
                            &branch_backend,
                        )
                    })
                    .map_err(|err| format!("spawning branch thread: {err}"))?;
                handles.push((*index, *variant, branch_ctx, range, branch_id, handle));
            }
            for (index, variant, branch_ctx, range, branch_id, handle) in handles {
                let result = handle
                    .join()
                    .map_err(|_| format!("branch `{}` thread panicked", variant.label))?;
                wave_runs.push((index, variant, branch_ctx, range, branch_id, result));
            }
            Ok(())
        })?;

        for (index, variant, branch_ctx, range, branch_id, result) in wave_runs {
            let settled = settle_branch(result, &branch_ctx);

            // Disjointness is the determinism guarantee: every record the
            // branch produced must sit inside its reserved range before it may
            // join the parent's durable log. A violation is an invariant break
            // (e.g. a branch that outgrew its range width), not a comparable
            // outcome.
            let records = branch_ctx.call_log().into_records();
            for record in &records {
                if !range.contains(record.seq) {
                    return Err(format!(
                        "branch `{}` emitted call seq {} outside its reserved range {}..{}",
                        variant.label, record.seq, range.start, range.end_exclusive
                    ));
                }
            }

            if let Some(ref op_dir) = store_op_dir {
                let meta = BranchMeta {
                    version: BRANCH_STORE_VERSION,
                    branch_id: branch_id.clone(),
                    label: variant.label.clone(),
                    branch_index: index as u32,
                    branch_seq,
                    parent_run_id: parent_run_id.clone(),
                    original_source: variant.source.clone(),
                    input: variant.input.clone(),
                    sequence_range: range.clone(),
                    status: settled.status.to_string(),
                    pending_input: settled.pending_input.clone(),
                    pending_prompt: settled.pending_prompt.clone(),
                    output: settled.output.clone(),
                    error: settled.error.clone(),
                    updated_at: Utc::now(),
                };
                if let Err(err) =
                    persist_branch_state(&branch_dir(op_dir, index as u32), &meta, &records)
                {
                    tracing::warn!(error = %err, branch = %branch_id, "failed to persist branch store");
                }
            }

            ctx.merge_branch_records(records);
            outcomes.push(branch_outcome_json(&branch_id, &variant.label, &settled));
        }
    }

    Ok(Value::Array(outcomes))
}

/// Parse and validate the `variants` array: each needs a `source` (the
/// branch's own continuation module — reusing the parent source would re-reach
/// `chidori.branch` and recurse, §8.2); `label` defaults to `branch-<k>` and
/// `input` to `{}`. Sources are read here so a typo'd path fails the whole
/// call before any branch spends anything.
fn parse_variants(args: &Value) -> std::result::Result<Vec<BranchVariant>, String> {
    let variants = args
        .get("variants")
        .and_then(Value::as_array)
        .ok_or("chidori.branch requires an array of variants")?;
    if variants.is_empty() {
        return Err("chidori.branch requires at least one variant".to_string());
    }
    if variants.len() > MAX_BRANCHES {
        return Err(format!(
            "chidori.branch supports at most {MAX_BRANCHES} variants, got {}",
            variants.len()
        ));
    }
    // Validate every variant's shape before touching any file, so a missing
    // `source` on variant N surfaces even when variant 0's path is bad too.
    let mut parsed = Vec::with_capacity(variants.len());
    for (index, variant) in variants.iter().enumerate() {
        let label = variant
            .get("label")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("branch-{index}"));
        let source = variant
            .get("source")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| {
                format!(
                    "chidori.branch variant `{label}` requires a `source` module path (a \
                     branch runs its own continuation source, not a copy of the parent)"
                )
            })?;
        let input = variant
            .get("input")
            .cloned()
            .filter(|value| !value.is_null())
            .unwrap_or_else(|| json!({}));
        parsed.push((label, source, input));
    }
    parsed
        .into_iter()
        .map(|(label, source, input)| {
            let source_text = std::fs::read_to_string(&source).map_err(|err| {
                format!("chidori.branch variant `{label}`: reading source {source}: {err}")
            })?;
            Ok(BranchVariant {
                label,
                source,
                source_text,
                input,
            })
        })
        .collect()
}

// --- Branch store: out-of-band resume / edit-and-rerun ----------------------

/// List a persisted run's branch stores: one summary JSON per branch, ordered
/// by op seq then branch index.
pub(crate) fn list_branches(run_dir: &Path) -> std::result::Result<Vec<Value>, String> {
    let mut summaries = Vec::new();
    for (_, _, meta) in scan_branches(run_dir)? {
        summaries.push(json!({
            "branchId": meta.branch_id,
            "label": meta.label,
            "status": meta.status,
            "branchSeq": meta.branch_seq,
            "branchIndex": meta.branch_index,
            "pendingPrompt": meta.pending_prompt,
            "source": meta.original_source,
            "updatedAt": meta.updated_at.to_rfc3339(),
        }));
    }
    Ok(summaries)
}

/// Resume a persisted branch that paused on `chidori.input` by answering its
/// pending prompt: replay the branch's checkpoint with a synthetic `input`
/// record at the pending seq (the same mechanism the server's `/resume` uses)
/// and run the branch's `source.ts` to its next outcome. Updates the branch
/// store and returns the new `BranchOutcome` JSON.
pub(crate) fn resume_branch(
    backend: &HostBindingBackend,
    run_dir: &Path,
    branch_id: &str,
    response: &str,
) -> std::result::Result<Value, String> {
    let (op_dir, branch_dir, meta) = find_branch(run_dir, branch_id)?;
    if meta.status != "paused" {
        return Err(format!(
            "branch `{branch_id}` is {}, not paused",
            meta.status
        ));
    }
    let pending = meta.pending_input.clone().ok_or_else(|| {
        format!(
            "branch `{branch_id}` is paused on an operation that cannot be resumed with an \
             input response ({})",
            meta.pending_prompt.as_deref().unwrap_or("unknown")
        )
    })?;

    let mut replay_log = load_branch_checkpoint(&branch_dir)?;
    // Inject a synthetic `input` record at the pending seq so the replaying
    // branch returns the response to the agent's input() call and continues
    // live from there.
    replay_log.push(CallRecord {
        seq: pending.seq,
        parent_seq: None,
        function: "input".to_string(),
        args: json!({ "prompt": pending.prompt }),
        result: Value::String(response.to_string()),
        duration_ms: 0,
        token_usage: None,
        timestamp: Utc::now(),
        error: None,
    });

    run_persisted_branch(backend, &op_dir, &branch_dir, meta, replay_log)
}

/// Re-run a persisted branch **fresh from its parent anchor** with whatever
/// `source.ts` now contains — the edit-and-rerun flow. The previous checkpoint
/// is discarded (an edited source may diverge from it); the anchored state
/// (fork-time VFS + the variant's `input`) is identical to the original fork,
/// so only the branch's code is the variable. Updates the branch store and
/// returns the new `BranchOutcome` JSON.
pub(crate) fn rerun_branch(
    backend: &HostBindingBackend,
    run_dir: &Path,
    branch_id: &str,
) -> std::result::Result<Value, String> {
    let (op_dir, branch_dir, meta) = find_branch(run_dir, branch_id)?;
    run_persisted_branch(backend, &op_dir, &branch_dir, meta, Vec::new())
}

/// Shared core of [`resume_branch`] / [`rerun_branch`]: rebuild the branch
/// context from the persisted anchor, run the store's `source.ts`, validate
/// range confinement, and update the store with the new state.
fn run_persisted_branch(
    backend: &HostBindingBackend,
    op_dir: &Path,
    branch_dir: &Path,
    mut meta: BranchMeta,
    replay_log: Vec<CallRecord>,
) -> std::result::Result<Value, String> {
    let anchor = load_anchor(op_dir)?;
    let branch_ctx = RuntimeContext::for_branch_resume(
        replay_log,
        anchor.vfs,
        meta.sequence_range.start - 1,
        meta.branch_seq,
        meta.branch_id.clone(),
    );
    let branch_backend = backend
        .with_runtime_ctx(branch_ctx.clone())
        .ok_or("branch resume requires the runtime host backend")?;

    let source_path = branch_dir.join(BRANCH_SOURCE_FILE);
    let result =
        crate::runtime::rust_engine::run_agent_file(&source_path, &meta.input, &branch_backend);
    let settled = settle_branch(result, &branch_ctx);

    let records = branch_ctx.call_log().into_records();
    for record in &records {
        if !meta.sequence_range.contains(record.seq) {
            return Err(format!(
                "branch `{}` emitted call seq {} outside its reserved range {}..{}",
                meta.label,
                record.seq,
                meta.sequence_range.start,
                meta.sequence_range.end_exclusive
            ));
        }
    }

    meta.status = settled.status.to_string();
    meta.pending_input = settled.pending_input.clone();
    meta.pending_prompt = settled.pending_prompt.clone();
    meta.output = settled.output.clone();
    meta.error = settled.error.clone();
    meta.updated_at = Utc::now();
    persist_branch_state(branch_dir, &meta, &records)?;

    Ok(branch_outcome_json(&meta.branch_id, &meta.label, &settled))
}

fn op_dir(run_dir: &Path, branch_seq: u64) -> PathBuf {
    run_dir
        .join(BRANCHES_DIR)
        .join(format!("op-{branch_seq:020}"))
}

fn branch_dir(op_dir: &Path, branch_index: u32) -> PathBuf {
    op_dir.join(format!("branch-{branch_index:03}"))
}

fn persist_anchor_and_sources(
    op_dir: &Path,
    anchor: &BranchAnchor,
    variants: &[BranchVariant],
) -> std::result::Result<(), String> {
    std::fs::create_dir_all(op_dir)
        .map_err(|err| format!("creating {}: {err}", op_dir.display()))?;
    let anchor_path = op_dir.join(BRANCH_ANCHOR_FILE);
    let bytes = serde_json::to_vec_pretty(anchor).map_err(|err| err.to_string())?;
    std::fs::write(&anchor_path, bytes)
        .map_err(|err| format!("writing {}: {err}", anchor_path.display()))?;
    for (index, variant) in variants.iter().enumerate() {
        let dir = branch_dir(op_dir, index as u32);
        std::fs::create_dir_all(&dir)
            .map_err(|err| format!("creating {}: {err}", dir.display()))?;
        let source_path = dir.join(BRANCH_SOURCE_FILE);
        std::fs::write(&source_path, &variant.source_text)
            .map_err(|err| format!("writing {}: {err}", source_path.display()))?;
    }
    Ok(())
}

fn persist_branch_state(
    branch_dir: &Path,
    meta: &BranchMeta,
    records: &[CallRecord],
) -> std::result::Result<(), String> {
    std::fs::create_dir_all(branch_dir)
        .map_err(|err| format!("creating {}: {err}", branch_dir.display()))?;
    let meta_path = branch_dir.join(BRANCH_META_FILE);
    let bytes = serde_json::to_vec_pretty(meta).map_err(|err| err.to_string())?;
    std::fs::write(&meta_path, bytes)
        .map_err(|err| format!("writing {}: {err}", meta_path.display()))?;
    let checkpoint_path = branch_dir.join(BRANCH_CHECKPOINT_FILE);
    let bytes = serde_json::to_vec_pretty(records).map_err(|err| err.to_string())?;
    std::fs::write(&checkpoint_path, bytes)
        .map_err(|err| format!("writing {}: {err}", checkpoint_path.display()))?;
    Ok(())
}

fn load_anchor(op_dir: &Path) -> std::result::Result<BranchAnchor, String> {
    let path = op_dir.join(BRANCH_ANCHOR_FILE);
    let bytes = std::fs::read(&path).map_err(|err| format!("reading {}: {err}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|err| format!("parsing {}: {err}", path.display()))
}

fn load_branch_checkpoint(branch_dir: &Path) -> std::result::Result<Vec<CallRecord>, String> {
    let path = branch_dir.join(BRANCH_CHECKPOINT_FILE);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(&path).map_err(|err| format!("reading {}: {err}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|err| format!("parsing {}: {err}", path.display()))
}

/// Walk `<run dir>/branches/op-*/branch-*/branch.json`, ordered by op then
/// branch index.
fn scan_branches(
    run_dir: &Path,
) -> std::result::Result<Vec<(PathBuf, PathBuf, BranchMeta)>, String> {
    let branches_root = run_dir.join(BRANCHES_DIR);
    let mut found = Vec::new();
    if !branches_root.is_dir() {
        return Ok(found);
    }
    let mut op_dirs: Vec<PathBuf> = std::fs::read_dir(&branches_root)
        .map_err(|err| format!("reading {}: {err}", branches_root.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir())
        .collect();
    op_dirs.sort();
    for op_dir in op_dirs {
        let mut branch_dirs: Vec<PathBuf> = std::fs::read_dir(&op_dir)
            .map_err(|err| format!("reading {}: {err}", op_dir.display()))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_dir())
            .collect();
        branch_dirs.sort();
        for branch_dir in branch_dirs {
            let meta_path = branch_dir.join(BRANCH_META_FILE);
            if !meta_path.is_file() {
                continue;
            }
            let bytes = std::fs::read(&meta_path)
                .map_err(|err| format!("reading {}: {err}", meta_path.display()))?;
            let meta: BranchMeta = serde_json::from_slice(&bytes)
                .map_err(|err| format!("parsing {}: {err}", meta_path.display()))?;
            found.push((op_dir.clone(), branch_dir, meta));
        }
    }
    Ok(found)
}

fn find_branch(
    run_dir: &Path,
    branch_id: &str,
) -> std::result::Result<(PathBuf, PathBuf, BranchMeta), String> {
    for entry in scan_branches(run_dir)? {
        if entry.2.branch_id == branch_id {
            return Ok(entry);
        }
    }
    Err(format!(
        "no persisted branch `{branch_id}` under {} (use `chidori branches <run id>` to list)",
        run_dir.display()
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use serde_json::json;

    use crate::mcp::McpManager;
    use crate::policy::{PolicyCache, PolicyConfig};
    use crate::providers::ProviderRegistry;
    use crate::runtime::context::{InputMode, RuntimeContext};
    use crate::runtime::rust_engine::run_agent;
    use crate::runtime::snapshot::RuntimePolicy;
    use crate::runtime::template::TemplateEngine;
    use crate::runtime::typescript::bindings::HostBindingBackend;
    use crate::tools::ToolRegistry;

    /// A fully-wired runtime backend over `ctx`/`tools`, mirroring the
    /// rust_engine test harness.
    fn test_backend(ctx: RuntimeContext, tools: Arc<ToolRegistry>) -> HostBindingBackend {
        HostBindingBackend::for_runtime(
            ctx,
            Arc::new(ProviderRegistry::new()),
            Arc::new(TemplateEngine::new(".")),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
            PolicyConfig::from_env(),
            Arc::new(StdMutex::new(PolicyCache::default())),
            RuntimePolicy::durable_default("branch-test"),
            tools,
            Arc::new(McpManager::new()),
        )
    }

    /// A registry with a native `count` tool that increments `counter` and
    /// echoes its `value` argument — the live-execution probe: replayed or
    /// handed-over prefixes must not bump it.
    fn counting_registry(counter: Arc<AtomicUsize>) -> Arc<ToolRegistry> {
        let mut registry = ToolRegistry::new();
        registry.register_native(
            "count",
            "counts live invocations",
            Vec::new(),
            move |args| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(json!({ "value": args.get("value").cloned().unwrap_or(json!(0)) }))
            },
        );
        Arc::new(registry)
    }

    fn write_branch_sources(dir: &std::path::Path) {
        std::fs::create_dir_all(dir.join("branches")).unwrap();
        std::fs::write(
            dir.join("branches").join("double.ts"),
            r#"
            export async function agent(input: { base: number }) {
                await chidori.log("strategy double");
                return { strategy: "double", value: input.base * 2 };
            }
            "#,
        )
        .unwrap();
        std::fs::write(
            dir.join("branches").join("triple.ts"),
            r#"
            export async function agent(input: { base: number }) {
                await chidori.log("strategy triple");
                return { strategy: "triple", value: input.base * 3 };
            }
            "#,
        )
        .unwrap();
    }

    /// The shared parent agent: one live tool call (the prefix), a two-variant
    /// branch, and a post-branch host call (which proves live/replay sequence
    /// alignment after the fan-out).
    fn parent_agent_source(dir: &std::path::Path) -> String {
        r#"
            export async function agent(input: { base: number }) {
                const seed = await chidori.tool("count", { value: input.base });
                const outcomes = await chidori.branch([
                    { label: "double", source: "__DIR__/branches/double.ts", input: { base: seed.value } },
                    { label: "triple", source: "__DIR__/branches/triple.ts", input: { base: seed.value } },
                ]);
                await chidori.log("after branch");
                return { outcomes };
            }
        "#
        .replace("__DIR__", &dir.to_string_lossy())
    }

    #[test]
    fn branch_runs_variants_with_disjoint_ranges_and_nested_records() {
        let counter = Arc::new(AtomicUsize::new(0));
        let ctx = RuntimeContext::new();
        let dir = std::env::temp_dir().join(format!("chidori-branch-{}", uuid::Uuid::new_v4()));
        write_branch_sources(&dir);
        let path = dir.join("agent.ts");
        let src = parent_agent_source(&dir);
        std::fs::write(&path, &src).unwrap();

        let backend = test_backend(ctx.clone(), counting_registry(counter.clone()));
        let output = run_agent(&path, &src, &json!({ "value": 0, "base": 21 }), &backend).unwrap();

        // Two outcomes, completed, with each strategy's output.
        let outcomes = output["outcomes"].as_array().unwrap();
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0]["label"], json!("double"));
        assert_eq!(outcomes[0]["status"], json!("completed"));
        assert_eq!(
            outcomes[0]["output"],
            json!({ "strategy": "double", "value": 42 })
        );
        assert_eq!(outcomes[1]["label"], json!("triple"));
        assert_eq!(
            outcomes[1]["output"],
            json!({ "strategy": "triple", "value": 63 })
        );
        assert_eq!(
            outcomes[1]["branchId"],
            json!(format!("{}-op2-branch-1", ctx.run_id()))
        );

        // The prefix (the parent's `count` tool) fired exactly once: it was
        // handed over as state, not re-run per branch.
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        // Branch records nest under the `branch` call and live in disjoint
        // reserved ranges. With the parent's `tool` at seq 1 and `branch` at
        // seq 2 (block = 2 * 10_000), the slot-derived base is 20_000:
        // branch 0 owns [20_001, 30_001), branch 1 owns [30_001, 40_001).
        let records = ctx.call_log().into_records();
        let branch = records.iter().find(|r| r.function == "branch").unwrap();
        assert_eq!(branch.seq, 2);
        let log_double = records
            .iter()
            .find(|r| r.function == "log" && r.args["message"] == "strategy double")
            .unwrap();
        let log_triple = records
            .iter()
            .find(|r| r.function == "log" && r.args["message"] == "strategy triple")
            .unwrap();
        assert_eq!(log_double.parent_seq, Some(branch.seq));
        assert_eq!(log_triple.parent_seq, Some(branch.seq));
        assert!(
            (20_001..30_001).contains(&log_double.seq),
            "{}",
            log_double.seq
        );
        assert!(
            (30_001..40_001).contains(&log_triple.seq),
            "{}",
            log_triple.seq
        );

        // The post-branch parent call continues above the merged branch seqs.
        let log_after = records
            .iter()
            .find(|r| r.function == "log" && r.args["message"] == "after branch")
            .unwrap();
        assert!(log_after.seq > log_triple.seq);
        assert_eq!(log_after.parent_seq, None);

        // The branch record's durable result is the outcomes array itself.
        assert_eq!(branch.result.as_array().unwrap().len(), 2);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn branch_outcomes_replay_from_cache_without_rerunning_branches() {
        // Branches that each make their own live tool call. On replay of the
        // parent, the recorded `branch` outcome must come from cache: the
        // counter stays at its live value and the output is identical.
        let counter = Arc::new(AtomicUsize::new(0));
        let dir =
            std::env::temp_dir().join(format!("chidori-branch-replay-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("branches")).unwrap();
        for (name, factor) in [("double", 2), ("triple", 3)] {
            std::fs::write(
                dir.join("branches").join(format!("{name}.ts")),
                format!(
                    r#"
                    export async function agent(input: {{ base: number }}) {{
                        const counted = await chidori.tool("count", {{ value: input.base }});
                        return {{ strategy: "{name}", value: counted.value * {factor} }};
                    }}
                    "#
                ),
            )
            .unwrap();
        }
        let path = dir.join("agent.ts");
        let src = parent_agent_source(&dir);
        std::fs::write(&path, &src).unwrap();
        let input = json!({ "value": 0, "base": 10 });

        let live_ctx = RuntimeContext::new();
        let registry = counting_registry(counter.clone());
        let live_backend = test_backend(live_ctx.clone(), registry.clone());
        let live_output = run_agent(&path, &src, &input, &live_backend).unwrap();
        // One parent prefix call + one call per branch.
        assert_eq!(counter.load(Ordering::SeqCst), 3);

        let records = live_ctx.call_log().into_records();
        let replay_ctx = RuntimeContext::with_replay(records);
        let replay_backend = test_backend(replay_ctx, registry);
        let replay_output = run_agent(&path, &src, &input, &replay_backend).unwrap();

        assert_eq!(live_output, replay_output);
        assert_eq!(
            counter.load(Ordering::SeqCst),
            3,
            "replay must not re-run branches"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn paused_branch_surfaces_pending_prompt_outcome() {
        // A branch that suspends on `chidori.input` in Pause mode is reported
        // as a paused outcome; the parent run completes.
        let ctx = RuntimeContext::new();
        ctx.set_input_mode(InputMode::Pause);
        let dir =
            std::env::temp_dir().join(format!("chidori-branch-pause-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("branches")).unwrap();
        std::fs::write(
            dir.join("branches").join("ask.ts"),
            r#"
            export async function agent() {
                const answer = await chidori.input("Which option?");
                return { answer };
            }
            "#,
        )
        .unwrap();
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const outcomes = await chidori.branch([
                    { label: "ask", source: "__DIR__/branches/ask.ts" },
                ]);
                return { outcomes };
            }
        "#
        .replace("__DIR__", &dir.to_string_lossy());
        std::fs::write(&path, &src).unwrap();

        let backend = test_backend(ctx, Arc::new(ToolRegistry::new()));
        let output = run_agent(&path, &src, &json!({}), &backend).unwrap();

        let outcome = &output["outcomes"][0];
        assert_eq!(outcome["status"], json!("paused"));
        assert_eq!(outcome["pendingPrompt"], json!("Which option?"));
        assert!(outcome.get("output").is_none());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn nested_branch_is_rejected_inside_a_branch() {
        let ctx = RuntimeContext::new();
        let dir =
            std::env::temp_dir().join(format!("chidori-branch-nested-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("branches")).unwrap();
        std::fs::write(
            dir.join("branches").join("forker.ts"),
            r#"
            export async function agent() {
                return await chidori.branch([{ label: "inner", source: "anything.ts" }]);
            }
            "#,
        )
        .unwrap();
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const outcomes = await chidori.branch([
                    { label: "forker", source: "__DIR__/branches/forker.ts" },
                ]);
                return { outcomes };
            }
        "#
        .replace("__DIR__", &dir.to_string_lossy());
        std::fs::write(&path, &src).unwrap();

        let backend = test_backend(ctx, Arc::new(ToolRegistry::new()));
        let output = run_agent(&path, &src, &json!({}), &backend).unwrap();

        let outcome = &output["outcomes"][0];
        assert_eq!(outcome["status"], json!("failed"));
        assert!(
            outcome["error"]
                .as_str()
                .unwrap()
                .contains("nested chidori.branch"),
            "{}",
            outcome["error"]
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn branch_validates_variants_before_running_any() {
        let ctx = RuntimeContext::new();
        let dir =
            std::env::temp_dir().join(format!("chidori-branch-invalid-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        // Missing `source` on the second variant must fail the whole call —
        // before the first (valid-looking) variant runs anything.
        let src = r#"
            export async function agent() {
                return await chidori.branch([
                    { label: "a", source: "missing-on-purpose.ts" },
                    { label: "b" },
                ]);
            }
        "#;
        std::fs::write(&path, src).unwrap();

        let backend = test_backend(ctx.clone(), Arc::new(ToolRegistry::new()));
        let err = run_agent(&path, src, &json!({}), &backend).unwrap_err();
        assert!(
            err.to_string().contains("requires a `source` module path"),
            "{err}"
        );
        // Nothing ran, nothing recorded: validation precedes the durable call.
        assert!(ctx.call_log().into_records().is_empty());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn paused_branch_persists_and_resumes_with_value() {
        // Phase 2: a branch that pauses on input is persisted under the parent
        // run's branch store and can be resumed OUT-OF-BAND — after the parent
        // completed — by answering the pending prompt. The resumed branch
        // replays its checkpoint (with the synthetic input record) and runs to
        // completion, updating its store.
        let base = std::env::temp_dir().join(format!(
            "chidori-branch-resume-base-{}",
            uuid::Uuid::new_v4()
        ));
        let dir =
            std::env::temp_dir().join(format!("chidori-branch-resume-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("branches")).unwrap();
        std::fs::write(
            dir.join("branches").join("ask.ts"),
            r#"
            export async function agent(input: { question: string }) {
                await chidori.log("before input");
                const answer = await chidori.input(input.question);
                await chidori.log("after input: " + answer);
                return { answer };
            }
            "#,
        )
        .unwrap();
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const outcomes = await chidori.branch([
                    { label: "ask", source: "__DIR__/branches/ask.ts", input: { question: "Pick a color" } },
                ]);
                return { outcomes };
            }
        "#
        .replace("__DIR__", &dir.to_string_lossy());
        std::fs::write(&path, &src).unwrap();

        let ctx = RuntimeContext::new();
        ctx.set_input_mode(InputMode::Pause);
        let run_dir = ctx.enable_persistence(base.clone());
        let backend = test_backend(ctx.clone(), Arc::new(ToolRegistry::new()));
        let output = run_agent(&path, &src, &json!({}), &backend).unwrap();

        let outcome = &output["outcomes"][0];
        assert_eq!(outcome["status"], json!("paused"));
        let branch_id = outcome["branchId"].as_str().unwrap().to_string();

        // The branch store exists: anchor + source copy + checkpoint + meta.
        let listed = super::list_branches(&run_dir).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["branchId"], json!(branch_id));
        assert_eq!(listed[0]["status"], json!("paused"));
        assert_eq!(listed[0]["pendingPrompt"], json!("Pick a color"));
        let (op_dir, branch_dir, meta) = super::find_branch(&run_dir, &branch_id).unwrap();
        assert!(op_dir.join(super::BRANCH_ANCHOR_FILE).is_file());
        assert!(branch_dir.join(super::BRANCH_SOURCE_FILE).is_file());
        assert!(branch_dir.join(super::BRANCH_CHECKPOINT_FILE).is_file());
        let pending = meta.pending_input.as_ref().unwrap();
        assert_eq!(pending.prompt, "Pick a color");

        // Resume with a fresh backend (the parent context is history).
        let resume_backend = test_backend(RuntimeContext::new(), Arc::new(ToolRegistry::new()));
        let resumed = super::resume_branch(&resume_backend, &run_dir, &branch_id, "blue").unwrap();
        assert_eq!(resumed["status"], json!("completed"));
        assert_eq!(resumed["output"], json!({ "answer": "blue" }));

        // The store reflects the new state, and every record (replayed and
        // newly live) stayed inside the reserved range.
        let (_, _, meta) = super::find_branch(&run_dir, &branch_id).unwrap();
        assert_eq!(meta.status, "completed");
        assert!(meta.pending_input.is_none());
        let records = super::load_branch_checkpoint(&branch_dir).unwrap();
        let input = records.iter().find(|r| r.function == "input").unwrap();
        assert_eq!(input.result, json!("blue"));
        let after = records
            .iter()
            .find(|r| r.function == "log" && r.args["message"] == "after input: blue")
            .unwrap();
        assert!(meta.sequence_range.contains(after.seq));

        // Resuming a branch that isn't paused is rejected.
        let err = super::resume_branch(&resume_backend, &run_dir, &branch_id, "again").unwrap_err();
        assert!(err.contains("not paused"), "{err}");

        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn edited_branch_reruns_from_anchor() {
        // Phase 2 edit-and-rerun: edit the branch store's source.ts and re-run
        // it fresh from the parent anchor. The anchored state — the fork-time
        // VFS (a file the parent wrote before forking) and the variant input —
        // is identical; only the edited code diverges.
        let base = std::env::temp_dir().join(format!(
            "chidori-branch-rerun-base-{}",
            uuid::Uuid::new_v4()
        ));
        let dir =
            std::env::temp_dir().join(format!("chidori-branch-rerun-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("branches")).unwrap();
        std::fs::write(
            dir.join("branches").join("read.ts"),
            r#"
            import { readFileSync } from "node:fs";
            export async function agent(input: { suffix: string }) {
                const note = readFileSync("/notes/anchor.txt", "utf8");
                return { version: "v1", note, suffix: input.suffix };
            }
            "#,
        )
        .unwrap();
        let path = dir.join("agent.ts");
        let src = r#"
            import { mkdirSync, writeFileSync } from "node:fs";
            export async function agent() {
                mkdirSync("/notes", { recursive: true });
                writeFileSync("/notes/anchor.txt", "from-parent");
                const outcomes = await chidori.branch([
                    { label: "read", source: "__DIR__/branches/read.ts", input: { suffix: "s1" } },
                ]);
                return { outcomes };
            }
        "#
        .replace("__DIR__", &dir.to_string_lossy());
        std::fs::write(&path, &src).unwrap();

        let ctx = RuntimeContext::new();
        let run_dir = ctx.enable_persistence(base.clone());
        let backend = test_backend(ctx.clone(), Arc::new(ToolRegistry::new()));
        let output = run_agent(&path, &src, &json!({}), &backend).unwrap();
        let outcome = &output["outcomes"][0];
        assert_eq!(
            outcome["output"],
            json!({ "version": "v1", "note": "from-parent", "suffix": "s1" })
        );
        let branch_id = outcome["branchId"].as_str().unwrap().to_string();

        // Edit the branch's persisted source: a new strategy version.
        let (_, branch_dir, _) = super::find_branch(&run_dir, &branch_id).unwrap();
        let source_path = branch_dir.join(super::BRANCH_SOURCE_FILE);
        let edited = std::fs::read_to_string(&source_path)
            .unwrap()
            .replace("\"v1\"", "\"v2-edited\"");
        std::fs::write(&source_path, edited).unwrap();

        // Re-run from the anchor with a fresh backend: divergent output, same
        // anchored state (the parent-written VFS file and the input).
        let rerun_backend = test_backend(RuntimeContext::new(), Arc::new(ToolRegistry::new()));
        let rerun = super::rerun_branch(&rerun_backend, &run_dir, &branch_id).unwrap();
        assert_eq!(rerun["status"], json!("completed"));
        assert_eq!(
            rerun["output"],
            json!({ "version": "v2-edited", "note": "from-parent", "suffix": "s1" })
        );
        let (_, _, meta) = super::find_branch(&run_dir, &branch_id).unwrap();
        assert_eq!(meta.status, "completed");
        assert_eq!(
            meta.output,
            Some(json!({ "version": "v2-edited", "note": "from-parent", "suffix": "s1" }))
        );

        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn branches_run_concurrently_when_requested() {
        // With options.concurrency = 2, both branches run in the same wave on
        // worker threads. Each calls a rendezvous tool that waits (bounded)
        // until BOTH branches have arrived — which only ever happens when they
        // overlap in time. Outcome order still follows variant order, and the
        // merged records keep their disjoint reserved ranges.
        let arrivals = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        let tool_arrivals = arrivals.clone();
        registry.register_native(
            "rendezvous",
            "waits until both branches arrive",
            Vec::new(),
            move |_args| {
                tool_arrivals.fetch_add(1, Ordering::SeqCst);
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while tool_arrivals.load(Ordering::SeqCst) < 2 {
                    if std::time::Instant::now() > deadline {
                        return Ok(json!({ "overlapped": false }));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Ok(json!({ "overlapped": true }))
            },
        );

        let ctx = RuntimeContext::new();
        let dir = std::env::temp_dir().join(format!(
            "chidori-branch-concurrent-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(dir.join("branches")).unwrap();
        for name in ["a", "b"] {
            std::fs::write(
                dir.join("branches").join(format!("{name}.ts")),
                format!(
                    r#"
                    export async function agent() {{
                        const meet = await chidori.tool("rendezvous", {{}});
                        return {{ name: "{name}", overlapped: meet.overlapped }};
                    }}
                    "#
                ),
            )
            .unwrap();
        }
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const outcomes = await chidori.branch([
                    { label: "a", source: "__DIR__/branches/a.ts" },
                    { label: "b", source: "__DIR__/branches/b.ts" },
                ], { concurrency: 2 });
                return { outcomes };
            }
        "#
        .replace("__DIR__", &dir.to_string_lossy());
        std::fs::write(&path, &src).unwrap();

        let backend = test_backend(ctx.clone(), Arc::new(registry));
        let output = run_agent(&path, &src, &json!({}), &backend).unwrap();

        let outcomes = output["outcomes"].as_array().unwrap();
        assert_eq!(outcomes[0]["label"], json!("a"));
        assert_eq!(outcomes[1]["label"], json!("b"));
        for outcome in outcomes {
            assert_eq!(outcome["status"], json!("completed"));
            assert_eq!(
                outcome["output"]["overlapped"],
                json!(true),
                "branches must run concurrently under concurrency=2: {outcome}"
            );
        }

        // Concurrent execution still confines each branch to its reserved
        // range: branch 0 in [20_001, 30_001), branch 1 in [30_001, 40_001).
        let records = ctx.call_log().into_records();
        let tools: Vec<u64> = records
            .iter()
            .filter(|r| r.function == "tool")
            .map(|r| r.seq)
            .collect();
        assert_eq!(tools.len(), 2);
        assert!(tools.iter().any(|seq| (20_001..30_001).contains(seq)));
        assert!(tools.iter().any(|seq| (30_001..40_001).contains(seq)));

        let _ = std::fs::remove_dir_all(dir);
    }
}
