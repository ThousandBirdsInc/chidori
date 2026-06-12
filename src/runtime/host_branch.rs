//! `chidori.branch` — in-agent execution branching (Phase 1 MVP).
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

use std::path::Path;

use serde_json::{json, Value};

use crate::runtime::context::{RuntimeContext, PAUSE_MARKER};
use crate::runtime::host_core;
use crate::runtime::snapshot::{
    HostOperationId, ParallelBranchManifest, DEFAULT_BRANCH_SEQUENCE_RANGE_WIDTH,
};
use crate::runtime::typescript::bindings::HostBindingBackend;

/// Hard cap on the branch fan-out: every branch makes live host calls past the
/// fork (real LLM/tool spend), so an unbounded `variants` array is a cost
/// hazard before it is a correctness one.
const MAX_BRANCHES: usize = 16;

/// One validated `chidori.branch` variant: a label for outcomes/trace, the
/// branch's own source module, and the state handed over as its run input.
struct BranchVariant {
    label: String,
    source: String,
    input: Value,
}

/// Run `chidori.branch(variants, options)`: fork the agent into one sub-run per
/// variant from the parent's current state, sequentially (the MVP ignores
/// `options.concurrency` beyond validating it), and return the
/// `BranchOutcome[]` JSON the agent awaits. The fan-out executes inside the
/// durable boundary as a single recorded `branch` call.
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
        .max(1) as u32;

    // Validate every branch source up front: failing fast before any branch
    // runs means a typo'd path can't burn LLM spend on the variants before it.
    for variant in &variants {
        let path = Path::new(&variant.source);
        if !path.is_file() {
            return Err(format!(
                "chidori.branch variant `{}`: source not found: {}",
                variant.label, variant.source
            ));
        }
    }

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

/// The live fan-out: reserve disjoint sequence ranges, run each variant on a
/// fresh branch context, validate range confinement, fold the branch records
/// into the parent log, and return the outcomes array.
fn run_branches_live(
    backend: &HostBindingBackend,
    ctx: &RuntimeContext,
    branch_seq: u64,
    variants: &[BranchVariant],
    concurrency: u32,
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
        concurrency,
        width,
    );

    let mut outcomes = Vec::with_capacity(variants.len());
    for (index, variant) in variants.iter().enumerate() {
        let branch = manifest
            .branch(index as u32)
            .ok_or_else(|| format!("missing branch metadata for index {index}"))?;
        let range = branch.sequence_range.clone();
        let branch_id = format!("{parent_run_id}-branch-{index}");
        let branch_ctx =
            RuntimeContext::for_branch(ctx, branch_id.clone(), range.start - 1, branch_seq);
        let branch_backend = backend
            .with_runtime_ctx(branch_ctx.clone())
            .ok_or("chidori.branch requires the runtime host backend")?;

        let result = crate::runtime::rust_engine::run_agent_file(
            Path::new(&variant.source),
            &variant.input,
            &branch_backend,
        );

        let mut outcome = json!({
            "label": variant.label,
            "branchId": branch_id,
        });
        match result {
            Ok(output) => {
                outcome["status"] = json!("completed");
                outcome["output"] = output;
            }
            Err(err) if err.to_string().contains(PAUSE_MARKER) => {
                // Phase 1: a suspended branch is reported, not resumable — the
                // persisted-branch resume flow is Phase 2. Surface what the
                // branch is waiting on so the agent (or a human) can decide.
                outcome["status"] = json!("paused");
                if let Some(pending) = branch_ctx.take_pending_input() {
                    outcome["pendingPrompt"] = json!(pending.prompt);
                } else if let Some(pending) = branch_ctx.take_pending_approval() {
                    outcome["pendingPrompt"] =
                        json!(format!("approval required: {}", pending.target));
                } else if let Some(pending) = branch_ctx.take_pending_signal() {
                    outcome["pendingPrompt"] =
                        json!(format!("waiting on signal: {}", pending.name));
                }
            }
            Err(err) => {
                outcome["status"] = json!("failed");
                outcome["error"] = json!(err.to_string());
            }
        }

        // Disjointness is the determinism guarantee: every record the branch
        // produced must sit inside its reserved range before it may join the
        // parent's durable log. A violation is an invariant break (e.g. a
        // branch that outgrew its range width), not a comparable outcome.
        let records = branch_ctx.call_log().into_records();
        for record in &records {
            if !range.contains(record.seq) {
                return Err(format!(
                    "branch `{}` emitted call seq {} outside its reserved range {}..{}",
                    variant.label, record.seq, range.start, range.end_exclusive
                ));
            }
        }
        ctx.merge_branch_records(records);

        outcomes.push(outcome);
    }

    Ok(Value::Array(outcomes))
}

/// Parse and validate the `variants` array: each needs a `source` (the branch's
/// own continuation module — reusing the parent source would re-reach
/// `chidori.branch` and recurse, §8.2); `label` defaults to `branch-<k>` and
/// `input` to `{}`.
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
    variants
        .iter()
        .enumerate()
        .map(|(index, variant)| {
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
            Ok(BranchVariant {
                label,
                source,
                input,
            })
        })
        .collect()
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
            json!(format!("{}-branch-1", ctx.run_id()))
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
        // as a paused outcome (resume is Phase 2); the parent run completes.
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
}
