#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{Context, Result};
use base64::Engine as _;

use crate::mcp::McpManager;
use crate::policy::{Decision, PolicyCache, PolicyConfig};
use crate::providers::{
    ContentBlock, LlmRequest, Message as LlmMessage, ProviderRegistry, ToolSchema,
};
use crate::runtime::call_log::CallRecord;
use crate::runtime::capability::Capability;
use crate::runtime::context::{InputMode, PendingApproval, RuntimeContext, PAUSE_MARKER};
use crate::runtime::host_core;
use crate::runtime::snapshot::{
    merge_parallel_branch_outcomes, DatePolicy, HostOperationId, HostPromiseRecord,
    HostPromiseState, MapSetSnapshotPolicy, ParallelBranchManifest, ParallelBranchOutcome,
    ParallelMergeResult, PendingHostOperation, PendingHostOperationKind, RandomPolicy,
    RuntimePolicy, SnapshotAbi, SnapshotBranchMetadata, SnapshotManifest, SnapshotModuleGraphEntry,
    SnapshotModuleImport, SnapshotStore, SourceFingerprint,
};
use crate::runtime::template::TemplateEngine;
use crate::runtime::typescript::engine::TypeScriptVmRuntime;
use crate::runtime::typescript::transpile::{transpile_module, validate_imports, TranspileOptions};
use crate::tools::{ToolBackend, ToolDef, ToolRegistry};

pub const DEFAULT_TS_SNAPSHOT_ROOTS: &[&str] = &[
    "__chidori_exports",
    "__chidori_modules",
    "__chidori_call_result",
    "__chidori_call_error",
    "__chidori_active_host_operation_id",
    "__chidori_host_promises",
    "__chidori_host_calls",
    "__chidori_host_method_queues",
];

/// JS-level `chidori` SDK sugar shared by both engines: the `tryCall`/`retry`/
/// `parallel` control-flow helpers and the `memory.set/get/delete/clear` wrappers
/// (and a workspace shim, guarded so it no-ops when the host already installed a
/// native `chidori.workspace`). The QuickJS path evals this in
/// `eval_agent_source`; the rust engine evals the same const after
/// `install_chidori_effects` so the two engines never diverge on these APIs.
pub(crate) const CHIDORI_JS_HELPERS_SCRIPT: &str = r#"
(() => {
    globalThis.chidori = globalThis.chidori || {};

    globalThis.chidori.tryCall = async function tryCall(fn) {
        try {
            return { ok: true, value: await fn() };
        } catch (err) {
            return {
                ok: false,
                error: String(err && err.message ? err.message : err),
            };
        }
    };

    globalThis.chidori.retry = async function retry(fn, options) {
        const attempts = Math.max(1, Number(options && options.attempts) || 3);
        let lastErr;
        for (let i = 0; i < attempts; i += 1) {
            try {
                return await fn();
            } catch (err) {
                lastErr = err;
            }
        }
        throw lastErr;
    };

    globalThis.chidori.parallel = async function parallel(tasks, options) {
        if (!Array.isArray(tasks)) {
            throw new Error("chidori.parallel expects an array of task functions");
        }
        for (const [index, task] of tasks.entries()) {
            if (typeof task !== "function") {
                throw new Error(`chidori.parallel task ${index} must be a function`);
            }
        }
        const concurrency = Math.max(
            1,
            Math.min(
                tasks.length || 1,
                Number(options && options.concurrency) || tasks.length || 1,
            ),
        );
        const results = new Array(tasks.length);
        let next = 0;

        async function worker() {
            while (next < tasks.length) {
                const index = next;
                next += 1;
                results[index] = await tasks[index]();
            }
        }

        await Promise.all(Array.from({ length: concurrency }, () => worker()));
        return results;
    };

    globalThis.__chidori_install_memory_helpers = function installMemoryHelpers() {
        const current = globalThis.chidori && globalThis.chidori.memory;
        if (typeof current !== "function") {
            return null;
        }
        const memoryCall = current.__chidori_call || current;
        function memory(...args) {
            return memoryCall.call(globalThis.chidori, ...args);
        }
        memory.__chidori_call = memoryCall;
        memory.set = memory.set || function set(key, value, options) {
            return memory("set", key, value, options);
        };
        memory.get = memory.get || function get(key, options) {
            return memory("get", key, null, options);
        };
        memory.delete = memory.delete || function deleteKey(key, options) {
            return memory("delete", key, null, options);
        };
        memory.clear = memory.clear || function clear(options) {
            return memory("clear", null, null, options);
        };
        globalThis.chidori.memory = memory;
        return null;
    };
    globalThis.__chidori_install_memory_helpers();

    if (typeof globalThis.__chidori_workspace_write === "function") {
        globalThis.chidori.workspace = {
            list(options) {
                return globalThis.__chidori_workspace_list(options || {});
            },
            read(path) {
                return globalThis.__chidori_workspace_read(path);
            },
            write(path, content, options) {
                return globalThis.__chidori_workspace_write(path, content, options || {});
            },
            delete(path, reason) {
                return globalThis.__chidori_workspace_delete(path, reason || null);
            },
            remove(path, reason) {
                return globalThis.__chidori_workspace_delete(path, reason || null);
            },
            manifest() {
                return globalThis.__chidori_workspace_manifest();
            },
        };
    }

    return null;
})()
"#;

const FUTURE_HOST_PROMISE_SLOTS: u64 = 8;

pub fn snapshot_initial_agent_state(
    path: &Path,
    source: &str,
    policy: RuntimePolicy,
) -> Result<Vec<u8>> {
    let runtime = TypeScriptSnapshotRuntime::new(policy)?;
    let mut context = runtime.eval_agent_source(path, source)?;
    context.snapshot()
}

pub fn snapshot_live_agent_state(
    path: &Path,
    source: &str,
    input: serde_json::Value,
    policy: RuntimePolicy,
    host_promises: &[HostPromiseRecord],
    expected_pending: Option<&PendingHostOperation>,
) -> Result<chidori_quickjs::RuntimeSnapshot> {
    let runtime = TypeScriptSnapshotRuntime::new(policy)?;
    let mut context = runtime.eval_agent_source(path, source)?;
    context.install_host_promise_records(host_promises)?;
    context.install_future_host_promises(
        host_promises,
        &[
            ("input", FUTURE_HOST_PROMISE_SLOTS),
            ("prompt", FUTURE_HOST_PROMISE_SLOTS),
            ("log", FUTURE_HOST_PROMISE_SLOTS),
            ("template", FUTURE_HOST_PROMISE_SLOTS),
            ("memory", FUTURE_HOST_PROMISE_SLOTS),
            ("checkpoint", FUTURE_HOST_PROMISE_SLOTS),
            ("http", FUTURE_HOST_PROMISE_SLOTS),
            ("tool", FUTURE_HOST_PROMISE_SLOTS),
            ("callAgent", FUTURE_HOST_PROMISE_SLOTS),
            ("execJs", FUTURE_HOST_PROMISE_SLOTS),
            ("execPython", FUTURE_HOST_PROMISE_SLOTS),
            ("execWasm", FUTURE_HOST_PROMISE_SLOTS),
        ],
    )?;
    let state = context.call_agent(input)?;
    match expected_pending {
        Some(expected) => match state {
            chidori_quickjs::RunState::BlockedOnHostOperation(actual)
                if actual.0 == expected.id.0 => {}
            chidori_quickjs::RunState::BlockedOnHostOperation(actual) => {
                anyhow::bail!(
                    "snapshot live agent state blocked on host operation {}, expected {}",
                    actual.0,
                    expected.id.0
                );
            }
            chidori_quickjs::RunState::Completed(value) => {
                anyhow::bail!(
                    "snapshot live agent state completed with {} before expected host operation {}",
                    value,
                    expected.id.0
                );
            }
        },
        None => {
            if let chidori_quickjs::RunState::BlockedOnHostOperation(actual) = state {
                anyhow::bail!(
                    "snapshot live agent state blocked on unexpected host operation {}",
                    actual.0
                );
            }
        }
    }
    context.snapshot_runtime()
}

pub fn snapshot_module_fingerprints(
    path: &Path,
    source: &str,
    policy: &RuntimePolicy,
) -> Result<Vec<SourceFingerprint>> {
    let entry_path = stable_path(path);
    let mut builder = SnapshotModuleBuilder::new(policy);
    builder.collect(path, source)?;
    Ok(builder
        .modules
        .iter()
        .filter(|module| module.path != entry_path)
        .map(|module| SourceFingerprint::from_source(module.path.clone(), &module.source))
        .collect())
}

pub fn snapshot_module_graph(
    path: &Path,
    source: &str,
    policy: &RuntimePolicy,
) -> Result<Vec<SnapshotModuleGraphEntry>> {
    let mut builder = SnapshotModuleBuilder::new(policy);
    builder.collect(path, source)?;
    Ok(builder
        .modules
        .iter()
        .map(|module| SnapshotModuleGraphEntry {
            path: module.path.clone(),
            imports: module
                .imports
                .iter()
                .map(|import| SnapshotModuleImport {
                    specifier: import.specifier.clone(),
                    resolved_path: import.resolved_path.clone(),
                })
                .collect(),
        })
        .collect())
}

pub struct TypeScriptSnapshotRuntime {
    runtime: chidori_quickjs::SnapshotRuntime,
    policy: RuntimePolicy,
}

impl TypeScriptSnapshotRuntime {
    pub fn new(policy: RuntimePolicy) -> Result<Self> {
        policy.ensure_durable_safe()?;
        Ok(Self {
            runtime: chidori_quickjs::SnapshotRuntime::new(
                chidori_quickjs::RuntimeLimits::default(),
            )
            .map_err(|err| anyhow::anyhow!(err))?,
            policy,
        })
    }

    pub fn eval_agent_source(
        &self,
        path: &Path,
        source: &str,
    ) -> Result<TypeScriptSnapshotContext<'_>> {
        let javascript = build_snapshot_bundle(path, source, &self.policy)?;
        let mut context = self
            .runtime
            .new_context()
            .map_err(|err| anyhow::anyhow!(err))?;
        if let Err(err) = context.eval_module(&path.display().to_string(), &javascript) {
            let bundle_note = write_eval_failure_bundle(path, &javascript);
            return Err(anyhow::anyhow!(err))
                .with_context(|| format!("evaluating {}{}", path.display(), bundle_note));
        }
        Ok(TypeScriptSnapshotContext { context })
    }

    pub fn restore_context(&self, snapshot: &[u8]) -> Result<TypeScriptSnapshotContext<'_>> {
        let mut context = self
            .runtime
            .new_context()
            .map_err(|err| anyhow::anyhow!(err))?;
        context
            .restore_globals(snapshot)
            .map_err(|err| anyhow::anyhow!(err))?;
        Ok(TypeScriptSnapshotContext { context })
    }

    pub fn fork_context(&self, parent_snapshot: &[u8]) -> Result<TypeScriptSnapshotContext<'_>> {
        self.restore_context(parent_snapshot)
    }

    pub fn resolve_host_promise_from_snapshot(
        &self,
        snapshot: &[u8],
        host_promise_id: chidori_quickjs::HostPromiseId,
        value: serde_json::Value,
    ) -> Result<(TypeScriptSnapshotContext<'_>, chidori_quickjs::RunState)> {
        let mut context = self.restore_context(snapshot)?;
        let state = context.resolve_host_promise_and_run(host_promise_id, value)?;
        Ok((context, state))
    }

    pub fn reject_host_promise_from_snapshot(
        &self,
        snapshot: &[u8],
        host_promise_id: chidori_quickjs::HostPromiseId,
        error: String,
    ) -> Result<(TypeScriptSnapshotContext<'_>, chidori_quickjs::RunState)> {
        let mut context = self.restore_context(snapshot)?;
        let state = context.reject_host_promise_and_run(host_promise_id, error)?;
        Ok((context, state))
    }

    pub fn restore_live_vm_from_store(
        store: &SnapshotStore,
        expected_policy: &RuntimePolicy,
        current_entry: &SourceFingerprint,
        current_modules: &[SourceFingerprint],
        current_module_graph: &[SnapshotModuleGraphEntry],
    ) -> Result<chidori_quickjs::SnapshotRuntime> {
        let snapshot = store.load_live_vm_for_resume(
            &SnapshotAbi::current("chidori-quickjs"),
            expected_policy,
            current_entry,
            current_modules,
            current_module_graph,
        )?;
        chidori_quickjs::SnapshotRuntime::restore(&snapshot.blob)
            .map_err(|err| anyhow::anyhow!(err))
    }

    pub fn save_live_vm_to_store(
        &mut self,
        store: &SnapshotStore,
        manifest: &SnapshotManifest,
        call_log: &[CallRecord],
    ) -> Result<()> {
        let snapshot = self
            .runtime
            .snapshot()
            .map_err(|err| anyhow::anyhow!(err))?;
        store.save_live_vm_snapshot(manifest, &snapshot, call_log)
    }

    pub fn run_parallel_branches_from_snapshot(
        &self,
        manifest: &ParallelBranchManifest,
        parent_snapshot: &[u8],
        inputs: &[serde_json::Value],
    ) -> Result<ParallelMergeResult> {
        if inputs.len() != manifest.branch_count as usize {
            anyhow::bail!(
                "parallel branch input count {} does not match manifest branch count {}",
                inputs.len(),
                manifest.branch_count
            );
        }

        let mut outcomes = Vec::with_capacity(inputs.len());
        for branch_index in 0..manifest.branch_count {
            let input = inputs
                .get(branch_index as usize)
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let output = match self.fork_context(parent_snapshot) {
                Ok(mut context) => match context.call_agent(input) {
                    Ok(chidori_quickjs::RunState::Completed(value)) => Ok(value),
                    Ok(chidori_quickjs::RunState::BlockedOnHostOperation(id)) => {
                        Err(format!("branch blocked on host operation {}", id.0))
                    }
                    Err(err) => Err(err.to_string()),
                },
                Err(err) => Err(err.to_string()),
            };
            outcomes.push(ParallelBranchOutcome {
                branch_index,
                output,
                call_log: Vec::new(),
            });
        }

        merge_parallel_branch_outcomes(manifest, &outcomes)
    }

    pub fn run_parallel_branches_from_store(
        &self,
        store: &SnapshotStore,
        manifest: &ParallelBranchManifest,
        inputs: &[serde_json::Value],
        current_entry: &SourceFingerprint,
        current_modules: &[SourceFingerprint],
    ) -> Result<ParallelMergeResult> {
        if inputs.len() != manifest.branch_count as usize {
            anyhow::bail!(
                "parallel branch input count {} does not match manifest branch count {}",
                inputs.len(),
                manifest.branch_count
            );
        }

        store.save_parallel_branch_manifest(manifest)?;
        let parent_snapshot = store.load_for_resume(
            &SnapshotAbi::current("chidori-quickjs"),
            &self.policy,
            current_entry,
            current_modules,
        )?;

        let mut outcomes = Vec::with_capacity(inputs.len());
        for branch_index in 0..manifest.branch_count {
            let input = inputs
                .get(branch_index as usize)
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let mut context = self.fork_context(&parent_snapshot.blob)?;
            let output = match context.call_agent(input) {
                Ok(chidori_quickjs::RunState::Completed(value)) => Ok(value),
                Ok(chidori_quickjs::RunState::BlockedOnHostOperation(id)) => {
                    Err(format!("branch blocked on host operation {}", id.0))
                }
                Err(err) => Err(err.to_string()),
            };

            let branch_snapshot_blob = context.snapshot()?;
            let branch_record = manifest
                .branch(branch_index)
                .ok_or_else(|| anyhow::anyhow!("unknown branch index {}", branch_index))?;
            let branch_manifest = SnapshotManifest::new(
                format!("{}-branch-{}", manifest.parent_run_id, branch_index),
                SnapshotAbi::current("chidori-quickjs"),
                self.policy.clone(),
                current_entry.clone(),
                current_modules.to_vec(),
                None,
                0,
            )
            .with_branch_metadata(SnapshotBranchMetadata {
                parent_run_id: manifest.parent_run_id.clone(),
                parallel_op_id: manifest.parallel_op_id,
                branch_index,
                branch_operation_id: branch_record.operation_id.clone(),
            });
            store.branch_store(manifest, branch_index)?.save(
                &branch_manifest,
                &branch_snapshot_blob,
                &[],
            )?;

            outcomes.push(ParallelBranchOutcome {
                branch_index,
                output,
                call_log: Vec::new(),
            });
        }

        merge_parallel_branch_outcomes(manifest, &outcomes)
    }

    pub fn resume_paused_branch_from_snapshot(
        &self,
        manifest: &ParallelBranchManifest,
        branch_index: u32,
        branch_snapshot: &[u8],
        host_promise_id: chidori_quickjs::HostPromiseId,
        value: serde_json::Value,
        result_expression: &str,
    ) -> Result<ParallelBranchOutcome> {
        if manifest.branch(branch_index).is_none() {
            anyhow::bail!("unknown branch index {}", branch_index);
        }
        let output = match self.restore_context(branch_snapshot) {
            Ok(mut context) => {
                if let Err(err) = context.resolve_host_promise(host_promise_id, value) {
                    Err(err.to_string())
                } else if let Err(err) = context.run_jobs_until_blocked() {
                    Err(err.to_string())
                } else {
                    context
                        .eval_json_expression("branch-result.js", result_expression)
                        .map_err(|err| err.to_string())
                }
            }
            Err(err) => Err(err.to_string()),
        };
        Ok(ParallelBranchOutcome {
            branch_index,
            output,
            call_log: Vec::new(),
        })
    }

    pub fn resume_paused_branch_from_store(
        &self,
        store: &SnapshotStore,
        parallel_op_id: HostOperationId,
        branch_index: u32,
        current_entry: &SourceFingerprint,
        current_modules: &[SourceFingerprint],
        host_promise_id: chidori_quickjs::HostPromiseId,
        value: serde_json::Value,
        result_expression: &str,
    ) -> Result<ParallelBranchOutcome> {
        let manifest = store.load_parallel_branch_manifest(parallel_op_id)?;
        let branch_record = manifest
            .branch(branch_index)
            .ok_or_else(|| anyhow::anyhow!("unknown branch index {}", branch_index))?;
        let branch_store = store.branch_store(&manifest, branch_index)?;
        let branch_snapshot = branch_store.load_for_resume(
            &SnapshotAbi::current("chidori-quickjs"),
            &self.policy,
            current_entry,
            current_modules,
        )?;
        branch_snapshot
            .manifest
            .ensure_branch_metadata(&SnapshotBranchMetadata {
                parent_run_id: manifest.parent_run_id.clone(),
                parallel_op_id: manifest.parallel_op_id,
                branch_index,
                branch_operation_id: branch_record.operation_id.clone(),
            })?;

        self.resume_paused_branch_from_snapshot(
            &manifest,
            branch_index,
            &branch_snapshot.blob,
            host_promise_id,
            value,
            result_expression,
        )
    }
}

fn write_eval_failure_bundle(path: &Path, javascript: &str) -> String {
    let file_stem = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("agent")
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let debug_path = std::env::temp_dir().join(format!(
        "chidori-eval-failed-{}-{file_stem}-{nanos}.js",
        std::process::id()
    ));

    match std::fs::write(&debug_path, javascript) {
        Ok(()) => format!("; transpiled bundle written to {}", debug_path.display()),
        Err(err) => format!("; failed to write transpiled bundle: {err}"),
    }
}

fn build_snapshot_bundle(path: &Path, source: &str, policy: &RuntimePolicy) -> Result<String> {
    let mut builder = SnapshotModuleBuilder::new(policy);
    builder.collect(path, source)?;
    builder.finish(path, source)
}

struct SnapshotModuleBuilder<'policy> {
    policy: &'policy RuntimePolicy,
    modules: Vec<SnapshotModule>,
    module_keys: HashMap<PathBuf, String>,
    seen: HashSet<PathBuf>,
    visiting: HashSet<PathBuf>,
}

struct SnapshotModule {
    path: PathBuf,
    key: String,
    source: String,
    imports: Vec<ResolvedSnapshotImport>,
}

#[derive(Clone)]
struct ResolvedSnapshotImport {
    specifier: String,
    resolved_path: Option<PathBuf>,
}

impl<'policy> SnapshotModuleBuilder<'policy> {
    fn new(policy: &'policy RuntimePolicy) -> Self {
        Self {
            policy,
            modules: Vec::new(),
            module_keys: HashMap::new(),
            seen: HashSet::new(),
            visiting: HashSet::new(),
        }
    }

    fn collect(&mut self, path: &Path, source: &str) -> Result<()> {
        let path = stable_path(path);
        if self.seen.contains(&path) {
            return Ok(());
        }
        if !self.visiting.insert(path.clone()) {
            anyhow::bail!(
                "{}: cyclic TypeScript imports are not supported by the snapshot scaffold",
                path.display()
            );
        }

        let imports = resolved_snapshot_imports(&path, source, self.policy)?;
        for module_path in imports
            .iter()
            .filter_map(|import| import.resolved_path.as_ref())
        {
            // node:* builtins resolve to synthetic paths under
            // `__node_builtins__/`; their bodies come from the shim registry,
            // not the filesystem.
            let module_source = if let Some(shim) =
                crate::runtime::typescript::builtins::source_for(module_path)
            {
                shim.to_string()
            } else {
                std::fs::read_to_string(module_path)
                    .with_context(|| format!("Failed to read {}", module_path.display()))?
            };
            self.collect(module_path, &module_source)?;
        }

        self.visiting.remove(&path);
        self.seen.insert(path.clone());
        let key = snapshot_module_key(&path);
        self.module_keys.insert(path.clone(), key.clone());
        self.modules.push(SnapshotModule {
            path,
            key,
            source: source.to_string(),
            imports,
        });
        Ok(())
    }

    fn finish(&self, entry_path: &Path, entry_source: &str) -> Result<String> {
        let entry_path = stable_path(entry_path);
        let mut out = snapshot_policy_prelude(self.policy)?;
        out.push_str("globalThis.__chidori_modules = globalThis.__chidori_modules || {};\n");

        for module in self
            .modules
            .iter()
            .filter(|module| module.path != entry_path)
        {
            out.push_str(&self.dependency_module_source(module)?);
        }

        let entry_imports = resolved_snapshot_imports(&entry_path, entry_source, self.policy)?;
        let entry_javascript = transpile_module(
            &entry_path,
            entry_source,
            &TranspileOptions {
                import_policy: self.policy.typescript_imports,
            },
        )?;
        out.push_str(&self.entry_module_source(&entry_javascript, &entry_imports)?);
        Ok(out)
    }

    fn dependency_module_source(&self, module: &SnapshotModule) -> Result<String> {
        let javascript = transpile_module(
            &module.path,
            &module.source,
            &TranspileOptions {
                import_policy: self.policy.typescript_imports,
            },
        )?;
        let key_json = serde_json::to_string(&module.key)?;
        let mut out = format!(
            "\n// {}\nglobalThis.__chidori_modules[{key_json}] = globalThis.__chidori_modules[{key_json}] || {{}};\n{{\nconst __chidori_module = globalThis.__chidori_modules[{key_json}];\n",
            module.path.display()
        );
        for (line_no, line) in javascript.lines().enumerate() {
            if let Some(statement) =
                self.import_statement(&module.path, line_no + 1, line, &module.imports, "const")?
            {
                out.push_str(&statement);
            } else if let Some(statement) = export_statement(line, "__chidori_module")? {
                out.push_str(&statement);
            } else {
                out.push_str(line);
                out.push('\n');
            }
        }
        out.push_str("}\n");
        Ok(out)
    }

    fn entry_module_source(
        &self,
        javascript: &str,
        imports: &[ResolvedSnapshotImport],
    ) -> Result<String> {
        let mut out = String::from("(() => {\n");
        for (line_no, line) in javascript.lines().enumerate() {
            if let Some(statement) =
                self.import_statement(Path::new("<entry>"), line_no + 1, line, imports, "const")?
            {
                out.push_str(&statement);
            } else if let Some(statement) =
                export_statement(line, "globalThis.__chidori_exports")?
            {
                out.push_str(&statement);
            } else {
                out.push_str(line);
                out.push('\n');
            }
        }
        out.push_str("})();\n");
        Ok(out)
    }

    fn import_statement(
        &self,
        path: &Path,
        line_no: usize,
        line: &str,
        imports: &[ResolvedSnapshotImport],
        declaration: &str,
    ) -> Result<Option<String>> {
        let Some(parsed) = parse_import_line(line) else {
            return Ok(None);
        };
        if parsed.is_type_only {
            return Ok(Some("\n".to_string()));
        }
        let Some(import) = imports
            .iter()
            .find(|import| import.specifier == parsed.specifier)
        else {
            anyhow::bail!(
                "{}:{}: unsupported TypeScript import syntax for snapshot scaffold",
                path.display(),
                line_no
            );
        };
        let Some(resolved_path) = import.resolved_path.as_ref() else {
            anyhow::bail!(
                "{}:{}: runtime imports from `{}` are not supported by the snapshot scaffold",
                path.display(),
                line_no,
                import.specifier
            );
        };
        let key = self.module_keys.get(resolved_path).ok_or_else(|| {
            anyhow::anyhow!(
                "{}:{}: unresolved TypeScript snapshot import {}",
                path.display(),
                line_no,
                import.specifier
            )
        })?;
        let namespace = format!(
            "globalThis.__chidori_modules[{}]",
            serde_json::to_string(key)?
        );
        parsed
            .binding_statement(path, line_no, &namespace, declaration)
            .map(Some)
    }
}

/// Read the host-provided agent environment (a JSON object of allowlisted
/// vars) and return it as a JS-safe object literal. Returns `{}` when unset or
/// malformed. Values are re-serialized through `serde_json` so they cannot
/// inject JS into the prelude.
pub(crate) fn chidori_agent_env_json() -> String {
    match std::env::var("CHIDORI_AGENT_ENV") {
        Ok(raw) if !raw.trim().is_empty() => {
            match serde_json::from_str::<serde_json::Value>(&raw) {
                Ok(value) if value.is_object() => value.to_string(),
                _ => "{}".to_string(),
            }
        }
        _ => "{}".to_string(),
    }
}

/// UTF-8 + base64 text primitives. The QuickJS runtime ships no Web encoding
/// APIs, but `node:buffer` and `node:fs` shims (and lots of real packages) need
/// `TextEncoder`/`TextDecoder`/`atob`/`btoa`. Pure-JS, deterministic, no host
/// access — safe to install unconditionally like `URLSearchParams`.
pub(crate) const TEXT_ENCODING_POLYFILL: &str = r#"
(function () {
    const B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    if (typeof globalThis.btoa !== "function") {
        globalThis.btoa = function (bin) {
            bin = String(bin);
            let out = "";
            for (let i = 0; i < bin.length; i += 3) {
                const a = bin.charCodeAt(i);
                const b = bin.charCodeAt(i + 1);
                const c = bin.charCodeAt(i + 2);
                const e1 = a >> 2;
                const e2 = ((a & 3) << 4) | (b >> 4);
                const e3 = isNaN(b) ? 64 : (((b & 15) << 2) | (c >> 6));
                const e4 = isNaN(c) ? 64 : (c & 63);
                out += B64[e1] + B64[e2] + (e3 === 64 ? "=" : B64[e3]) + (e4 === 64 ? "=" : B64[e4]);
            }
            return out;
        };
    }
    if (typeof globalThis.atob !== "function") {
        globalThis.atob = function (b64) {
            b64 = String(b64).replace(/[^A-Za-z0-9+/]/g, "");
            let out = "";
            let buffer = 0;
            let bits = 0;
            for (let i = 0; i < b64.length; i++) {
                const idx = B64.indexOf(b64[i]);
                if (idx < 0) continue;
                buffer = (buffer << 6) | idx;
                bits += 6;
                if (bits >= 8) {
                    bits -= 8;
                    out += String.fromCharCode((buffer >> bits) & 0xff);
                }
            }
            return out;
        };
    }
    if (typeof globalThis.TextEncoder !== "function") {
        globalThis.TextEncoder = class TextEncoder {
            get encoding() { return "utf-8"; }
            encode(str) {
                str = String(str === undefined ? "" : str);
                const out = [];
                for (let i = 0; i < str.length; i++) {
                    let c = str.charCodeAt(i);
                    if (c < 0x80) {
                        out.push(c);
                    } else if (c < 0x800) {
                        out.push(0xc0 | (c >> 6), 0x80 | (c & 0x3f));
                    } else if (c >= 0xd800 && c <= 0xdbff) {
                        const c2 = str.charCodeAt(++i);
                        const cp = 0x10000 + ((c - 0xd800) << 10) + (c2 - 0xdc00);
                        out.push(
                            0xf0 | (cp >> 18),
                            0x80 | ((cp >> 12) & 0x3f),
                            0x80 | ((cp >> 6) & 0x3f),
                            0x80 | (cp & 0x3f)
                        );
                    } else {
                        out.push(0xe0 | (c >> 12), 0x80 | ((c >> 6) & 0x3f), 0x80 | (c & 0x3f));
                    }
                }
                return new Uint8Array(out);
            }
        };
    }
    if (typeof globalThis.TextDecoder !== "function") {
        globalThis.TextDecoder = class TextDecoder {
            get encoding() { return "utf-8"; }
            decode(buf) {
                if (buf === undefined) return "";
                const bytes = buf instanceof Uint8Array
                    ? buf
                    : new Uint8Array(buf.buffer ? buf.buffer : buf);
                let out = "";
                let i = 0;
                while (i < bytes.length) {
                    const c = bytes[i++];
                    if (c < 0x80) {
                        out += String.fromCharCode(c);
                    } else if (c < 0xe0) {
                        out += String.fromCharCode(((c & 0x1f) << 6) | (bytes[i++] & 0x3f));
                    } else if (c < 0xf0) {
                        out += String.fromCharCode(
                            ((c & 0x0f) << 12) | ((bytes[i++] & 0x3f) << 6) | (bytes[i++] & 0x3f)
                        );
                    } else {
                        const cp =
                            ((c & 0x07) << 18) |
                            ((bytes[i++] & 0x3f) << 12) |
                            ((bytes[i++] & 0x3f) << 6) |
                            (bytes[i++] & 0x3f);
                        const u = cp - 0x10000;
                        out += String.fromCharCode(0xd800 + (u >> 10), 0xdc00 + (u & 0x3ff));
                    }
                }
                return out;
            }
        };
    }
})();
"#;

/// `globalThis.crypto` (Web Crypto subset): `getRandomValues`, `randomUUID`,
/// and `subtle.digest`. Randomness routes through the captured native, so it is
/// flagged and replayed like `node:crypto`. Installed unconditionally; the
/// native throws if the crypto policy is `disabled`.
pub(crate) const WEB_CRYPTO_POLYFILL: &str = r#"
(function () {
    if (globalThis.crypto && typeof globalThis.crypto.getRandomValues === "function") return;
    function base64ToBytes(b64) {
        const bin = atob(b64);
        const out = new Uint8Array(bin.length);
        for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
        return out;
    }
    function bytesToBase64(bytes) {
        let s = "";
        for (let i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i]);
        return btoa(s);
    }
    const cryptoObj = {
        getRandomValues(typedArray) {
            if (!ArrayBuffer.isView(typedArray)) {
                throw new TypeError("crypto.getRandomValues expects a typed array");
            }
            const view = new Uint8Array(
                typedArray.buffer,
                typedArray.byteOffset,
                typedArray.byteLength
            );
            const bytes = base64ToBytes(globalThis.__chidori_crypto_random(view.length));
            view.set(bytes.subarray(0, view.length));
            return typedArray;
        },
        randomUUID() {
            const b = base64ToBytes(globalThis.__chidori_crypto_random(16));
            b[6] = (b[6] & 0x0f) | 0x40;
            b[8] = (b[8] & 0x3f) | 0x80;
            const h = [];
            for (let i = 0; i < 16; i++) h.push(b[i].toString(16).padStart(2, "0"));
            return `${h[0]}${h[1]}${h[2]}${h[3]}-${h[4]}${h[5]}-${h[6]}${h[7]}-${h[8]}${h[9]}-${h[10]}${h[11]}${h[12]}${h[13]}${h[14]}${h[15]}`;
        },
        subtle: {
            async digest(algorithm, data) {
                const alg = typeof algorithm === "string" ? algorithm : (algorithm && algorithm.name);
                let bytes;
                if (typeof data === "string") bytes = new TextEncoder().encode(data);
                else if (ArrayBuffer.isView(data)) bytes = new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
                else bytes = new Uint8Array(data);
                const out = base64ToBytes(globalThis.__chidori_crypto_hash(alg, bytesToBase64(bytes)));
                return out.buffer;
            },
        },
    };
    Object.defineProperty(globalThis, "crypto", {
        value: cryptoObj,
        writable: true,
        configurable: true,
    });
})();
"#;

pub(crate) const URL_SEARCH_PARAMS_POLYFILL: &str = r#"
globalThis.URLSearchParams = class URLSearchParams {
    constructor(init) {
        this._p = [];
        if (typeof init === "string") {
            const s = init.charAt(0) === "?" ? init.slice(1) : init;
            if (s.length) {
                for (const pair of s.split("&")) {
                    const i = pair.indexOf("=");
                    const k = i === -1 ? pair : pair.slice(0, i);
                    const v = i === -1 ? "" : pair.slice(i + 1);
                    this._p.push([decodeURIComponent(k), decodeURIComponent(v.replace(/\+/g, " "))]);
                }
            }
        } else if (init && typeof init === "object") {
            const entries = typeof init.forEach === "function" && !Array.isArray(init)
                ? Array.from(init)
                : (Array.isArray(init) ? init : Object.entries(init));
            for (const [k, v] of entries) this._p.push([String(k), String(v)]);
        }
    }
    append(k, v) { this._p.push([String(k), String(v)]); }
    set(k, v) { this.delete(k); this._p.push([String(k), String(v)]); }
    get(k) { const e = this._p.find((p) => p[0] === k); return e ? e[1] : null; }
    getAll(k) { return this._p.filter((p) => p[0] === k).map((p) => p[1]); }
    has(k) { return this._p.some((p) => p[0] === k); }
    delete(k) { this._p = this._p.filter((p) => p[0] !== k); }
    forEach(cb) { for (const [k, v] of this._p) cb(v, k, this); }
    toString() {
        return this._p
            .map(([k, v]) => encodeURIComponent(k) + "=" + encodeURIComponent(v))
            .join("&");
    }
};
"#;

/// WHATWG `fetch` + `Headers`/`Request`/`Response`, implemented on top of the
/// captured `chidori.http` host op. Routing through `chidori.http` is what makes
/// the stdlib net surface honor the security policy and the approval-pause path:
/// `chidori.http` runs `enforce_policy` (allow / ask / deny) and, on an
/// AskBefore rule, records a pending approval and throws the pause sentinel
/// before any bytes leave the process — exactly as a direct `chidori.http` call
/// would. The synchronous host call below is therefore deliberately *not*
/// wrapped in a Promise executor or a try/catch: the pause sentinel must
/// propagate the same way it does for `await chidori.http(...)` so the engine
/// can pause and resume the run after approval.
pub(crate) const FETCH_POLYFILL: &str = r#"
(function () {
    class Headers {
        constructor(init) {
            this._h = [];
            if (!init) return;
            if (init instanceof Headers) {
                for (const [k, v] of init._h) this._h.push([k, v]);
                return;
            }
            if (Array.isArray(init)) {
                for (const pair of init) this.append(pair[0], pair[1]);
                return;
            }
            for (const k of Object.keys(init)) this.append(k, init[k]);
        }
        append(k, v) { this._h.push([String(k).toLowerCase(), String(v)]); }
        set(k, v) { this.delete(k); this._h.push([String(k).toLowerCase(), String(v)]); }
        get(k) {
            k = String(k).toLowerCase();
            const vals = this._h.filter((p) => p[0] === k).map((p) => p[1]);
            return vals.length ? vals.join(", ") : null;
        }
        has(k) { k = String(k).toLowerCase(); return this._h.some((p) => p[0] === k); }
        delete(k) { k = String(k).toLowerCase(); this._h = this._h.filter((p) => p[0] !== k); }
        forEach(cb, thisArg) { for (const [k, v] of this._h) cb.call(thisArg, v, k, this); }
        keys() { return this._h.map((p) => p[0])[Symbol.iterator](); }
        values() { return this._h.map((p) => p[1])[Symbol.iterator](); }
        entries() { return this._h.map((p) => [p[0], p[1]])[Symbol.iterator](); }
        [Symbol.iterator]() { return this.entries(); }
    }

    function headersToObject(h) {
        const obj = {};
        if (!h) return obj;
        if (h instanceof Headers) { h.forEach((v, k) => { obj[k] = v; }); return obj; }
        if (Array.isArray(h)) {
            for (const pair of h) obj[String(pair[0]).toLowerCase()] = String(pair[1]);
            return obj;
        }
        for (const k of Object.keys(h)) obj[k.toLowerCase()] = String(h[k]);
        return obj;
    }

    function bodyToString(body) {
        if (body === undefined || body === null) return undefined;
        if (typeof body === "string") return body;
        if (globalThis.URLSearchParams && body instanceof globalThis.URLSearchParams) {
            return body.toString();
        }
        if (ArrayBuffer.isView(body) || body instanceof ArrayBuffer) {
            return new TextDecoder().decode(body);
        }
        // Plain objects are forwarded as-is; the host JSON-encodes them.
        return body;
    }

    function responseBodyText(res) {
        if (!res) return null;
        const b = res.body;
        if (b === undefined || b === null) return null;
        return typeof b === "string" ? b : JSON.stringify(b);
    }

    class Request {
        constructor(input, init) {
            init = init || {};
            if (input instanceof Request) {
                this.url = input.url;
                this.method = String(init.method || input.method || "GET").toUpperCase();
                this.headers = new Headers(init.headers || input.headers);
                this._bodyInit = "body" in init ? init.body : input._bodyInit;
            } else {
                this.url = String(input);
                this.method = String(init.method || "GET").toUpperCase();
                this.headers = new Headers(init.headers);
                this._bodyInit = init.body;
            }
        }
    }

    class Response {
        constructor(body, init) {
            init = init || {};
            this._bodyText =
                body === undefined || body === null
                    ? null
                    : typeof body === "string"
                        ? body
                        : JSON.stringify(body);
            this.status = init.status !== undefined ? init.status : 200;
            this.statusText = init.statusText || "";
            this.ok = this.status >= 200 && this.status < 300;
            this.headers = init.headers instanceof Headers ? init.headers : new Headers(init.headers);
            this.url = init.url || "";
            this.redirected = false;
            this.bodyUsed = false;
        }
        text() { this.bodyUsed = true; return Promise.resolve(this._bodyText == null ? "" : this._bodyText); }
        json() {
            this.bodyUsed = true;
            return Promise.resolve(this._bodyText == null ? null : JSON.parse(this._bodyText));
        }
        arrayBuffer() {
            this.bodyUsed = true;
            return Promise.resolve(new TextEncoder().encode(this._bodyText || "").buffer);
        }
        clone() {
            return new Response(this._bodyText, {
                status: this.status,
                statusText: this.statusText,
                headers: this.headers,
                url: this.url,
            });
        }
    }

    function buildRequestArgs(input, init) {
        init = init || {};
        let url;
        let method = "GET";
        let headers = null;
        let bodyInit;
        if (input instanceof Request) {
            url = input.url;
            method = input.method;
            headers = input.headers;
            bodyInit = input._bodyInit;
        } else {
            url = String(input);
        }
        if (init.method) method = String(init.method).toUpperCase();
        const headerObj = headersToObject(headers);
        Object.assign(headerObj, headersToObject(init.headers));
        if ("body" in init) bodyInit = init.body;
        const options = { method: method.toUpperCase(), headers: headerObj };
        const body = bodyToString(bodyInit);
        if (body !== undefined) options.body = body;
        return { url, options };
    }

    function fetch(input, init) {
        if (!globalThis.chidori || typeof globalThis.chidori.http !== "function") {
            return Promise.reject(new TypeError("fetch is unavailable: chidori.http host is not installed"));
        }
        const { url, options } = buildRequestArgs(input, init);
        // Synchronous, blocking host call. If the security policy requires
        // approval this throws the pause sentinel synchronously; letting it
        // escape (no surrounding try/catch) preserves the engine's pause path.
        const res = globalThis.chidori.http(url, options);
        if (res && res.status === 0 && res.error) {
            return Promise.reject(new TypeError("fetch failed: " + res.error));
        }
        return Promise.resolve(
            new Response(responseBodyText(res), {
                status: res ? res.status : 0,
                headers: (res && res.headers) || {},
                url,
            })
        );
    }

    globalThis.Headers = Headers;
    globalThis.Request = Request;
    globalThis.Response = Response;
    globalThis.fetch = fetch;
})();
"#;

fn snapshot_policy_prelude(policy: &RuntimePolicy) -> Result<String> {
    let mut out = String::from(
        r#"
globalThis.WeakRef = function WeakRef() {
    throw new Error("WeakRef is disabled by Chidori snapshot policy");
};
globalThis.FinalizationRegistry = function FinalizationRegistry() {
    throw new Error("FinalizationRegistry is disabled by Chidori snapshot policy");
};
globalThis.SharedArrayBuffer = function SharedArrayBuffer() {
    throw new Error("SharedArrayBuffer is disabled by Chidori snapshot policy");
};
globalThis.Atomics = undefined;
// Logical clock (milliseconds). Driven forward by the virtual timer queue and
// read by the fixed-Date shim, so `Date.now()` advances as timers fire while
// staying fully deterministic. See docs/captured-effects-vfs-crypto-timers.md.
if (typeof globalThis.__chidori_now !== "number") globalThis.__chidori_now = 0;
"#,
    );

    // `process.env` is populated only from an explicit, allowlisted channel
    // (the `CHIDORI_AGENT_ENV` JSON blob the host sets) — never the raw OS
    // environment, which would leak host secrets into agent code. Absent the
    // blob this stays an empty frozen object, preserving determinism.
    let env_json = chidori_agent_env_json();
    out.push_str(&format!(
        "globalThis.process = Object.freeze({{ env: Object.freeze({env_json}) }});\n"
    ));

    // Minimal `URLSearchParams` — the QuickJS runtime ships no Web APIs, and
    // generated agents commonly build query strings with it.
    out.push_str(URL_SEARCH_PARAMS_POLYFILL);

    // UTF-8 + base64 text primitives, needed by `node:buffer`/`node:fs` shims
    // and common packages.
    out.push_str(TEXT_ENCODING_POLYFILL);

    // `globalThis.crypto` (Web Crypto subset). Routes randomness through the
    // captured native so it is flagged and replayable.
    out.push_str(WEB_CRYPTO_POLYFILL);

    // `globalThis.fetch` + `Headers`/`Request`/`Response`. Implemented over the
    // captured `chidori.http` host op so every network call from the stdlib net
    // surface honors the security policy and the approval-pause path. Installed
    // after `URLSearchParams`/`TextEncoder` since the polyfill relies on them.
    out.push_str(FETCH_POLYFILL);

    match policy.date {
        DatePolicy::Disabled => out.push_str(
            r#"
globalThis.Date = function Date() {
    throw new Error("Date is disabled by Chidori runtime policy");
};
globalThis.Date.now = function now() {
    throw new Error("Date.now is disabled by Chidori runtime policy");
};
globalThis.Date.parse = function parse() {
    throw new Error("Date.parse is disabled by Chidori runtime policy");
};
globalThis.Date.UTC = function UTC() {
    throw new Error("Date.UTC is disabled by Chidori runtime policy");
};
"#,
        ),
        DatePolicy::Fixed => out.push_str(
            r#"
const ChidoriHostDate = globalThis.Date;
function ChidoriFixedDate(...args) {
    if (this instanceof ChidoriFixedDate) {
        return args.length === 0
            ? new ChidoriHostDate(globalThis.__chidori_now)
            : new ChidoriHostDate(...args);
    }
    return args.length === 0
        ? new ChidoriHostDate(globalThis.__chidori_now).toString()
        : ChidoriHostDate(...args);
}
ChidoriFixedDate.now = function now() { return globalThis.__chidori_now; };
ChidoriFixedDate.parse = ChidoriHostDate.parse;
ChidoriFixedDate.UTC = ChidoriHostDate.UTC;
ChidoriFixedDate.prototype = ChidoriHostDate.prototype;
globalThis.Date = ChidoriFixedDate;
"#,
        ),
        DatePolicy::Host => {}
    }

    match policy.random {
        RandomPolicy::Disabled => out.push_str(
            r#"
Math.random = function random() {
    throw new Error("Math.random is disabled by Chidori runtime policy");
};
"#,
        ),
        RandomPolicy::Seeded => {
            let seed = u64::from_str_radix(&policy.deterministic_seed[..16], 16).unwrap_or(1);
            out.push_str(&format!(
                r#"
Math.random = (function() {{
    let state = {seed}n;
    return function random() {{
        state = (state * 6364136223846793005n + 1442695040888963407n) & ((1n << 64n) - 1n);
        return Number(state >> 11n) / 9007199254740992;
    }};
}})();
"#
            ));
        }
        RandomPolicy::Host => {}
    }

    match policy.maps_sets {
        MapSetSnapshotPolicy::Reject => out.push_str(
            r#"
globalThis.Map = function Map() {
    throw new Error("Map is disabled by Chidori snapshot policy");
};
globalThis.Set = function Set() {
    throw new Error("Set is disabled by Chidori snapshot policy");
};
"#,
        ),
        MapSetSnapshotPolicy::Serialize => {}
    }

    match policy.timers {
        crate::runtime::snapshot::TimerPolicy::Disabled => out.push_str(TIMER_DISABLED_POLYFILL),
        // Host is rejected for durable runs; absent a real OS event loop we run
        // it through the same deterministic virtual queue as Virtual.
        crate::runtime::snapshot::TimerPolicy::Virtual
        | crate::runtime::snapshot::TimerPolicy::Host => out.push_str(TIMER_VIRTUAL_POLYFILL),
    }

    Ok(out)
}

/// Virtual timer queue: deterministic, driven by the logical clock. Timers fire
/// in `(deadline, id)` order via a self-rescheduling microtask pump, so they
/// run inside the engine's normal job drain without any real wall-clock sleep.
/// Uses plain arrays (not `Map`/`Set`, which the snapshot policy may disable).
pub(crate) const TIMER_VIRTUAL_POLYFILL: &str = r#"
(function () {
    const tasks = [];
    let nextId = 1;
    let pumping = false;
    let fired = 0;
    const MAX_FIRES = 1000000;
    function schedule(cb, delay, args, repeat) {
        if (typeof cb !== "function") {
            throw new TypeError("timer callback must be a function");
        }
        const d = Math.max(0, Math.floor(Number(delay) || 0));
        const id = nextId++;
        tasks.push({ id, deadline: globalThis.__chidori_now + d, interval: repeat ? d : null, cb, args });
        if (typeof globalThis.__chidori_note_capability === "function") {
            globalThis.__chidori_note_capability("timer");
        }
        if (!pumping) {
            pumping = true;
            Promise.resolve().then(pump);
        }
        return id;
    }
    function earliestIndex() {
        let best = -1;
        for (let i = 0; i < tasks.length; i++) {
            if (best === -1 ||
                tasks[i].deadline < tasks[best].deadline ||
                (tasks[i].deadline === tasks[best].deadline && tasks[i].id < tasks[best].id)) {
                best = i;
            }
        }
        return best;
    }
    function pump() {
        if (tasks.length === 0) { pumping = false; return; }
        if (fired++ > MAX_FIRES) {
            pumping = false;
            tasks.length = 0;
            throw new Error("Chidori timer pump exceeded " + MAX_FIRES + " firings (runaway setInterval?)");
        }
        const idx = earliestIndex();
        const task = tasks[idx];
        if (task.deadline > globalThis.__chidori_now) {
            globalThis.__chidori_now = task.deadline;
        }
        if (task.interval != null) {
            task.deadline = globalThis.__chidori_now + task.interval;
        } else {
            tasks.splice(idx, 1);
        }
        // Reschedule before invoking so the pump survives a throwing callback.
        Promise.resolve().then(pump);
        task.cb.apply(undefined, task.args);
    }
    globalThis.setTimeout = function setTimeout(cb, delay, ...args) {
        return schedule(cb, delay, args, false);
    };
    globalThis.setInterval = function setInterval(cb, delay, ...args) {
        return schedule(cb, delay, args, true);
    };
    globalThis.setImmediate = function setImmediate(cb, ...args) {
        return schedule(cb, 0, args, false);
    };
    function clear(id) {
        for (let i = 0; i < tasks.length; i++) {
            if (tasks[i].id === id) { tasks.splice(i, 1); return; }
        }
    }
    globalThis.clearTimeout = clear;
    globalThis.clearInterval = clear;
    globalThis.clearImmediate = clear;
    if (typeof globalThis.queueMicrotask !== "function") {
        globalThis.queueMicrotask = function queueMicrotask(cb) {
            if (typeof cb !== "function") throw new TypeError("queueMicrotask callback must be a function");
            if (typeof globalThis.__chidori_note_capability === "function") {
                globalThis.__chidori_note_capability("microtask");
            }
            Promise.resolve().then(cb);
        };
    }
})();
"#;

/// Timer surface under `timers=disabled`: scheduling throws, so an agent that
/// must not schedule fails loudly rather than silently no-op'ing.
pub(crate) const TIMER_DISABLED_POLYFILL: &str = r#"
(function () {
    const blocked = function () {
        throw new Error("timers are disabled by Chidori runtime policy (timers=disabled)");
    };
    globalThis.setTimeout = blocked;
    globalThis.setInterval = blocked;
    globalThis.setImmediate = blocked;
    globalThis.clearTimeout = function () {};
    globalThis.clearInterval = function () {};
    globalThis.clearImmediate = function () {};
})();
"#;

fn resolved_snapshot_imports(
    path: &Path,
    source: &str,
    policy: &RuntimePolicy,
) -> Result<Vec<ResolvedSnapshotImport>> {
    validate_imports(path, source, policy.typescript_imports).map(|imports| {
        imports
            .into_iter()
            .map(|import| ResolvedSnapshotImport {
                specifier: import.specifier,
                resolved_path: import.resolved_path.map(|path| stable_path(&path)),
            })
            .collect()
    })
}

fn stable_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn snapshot_module_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

struct ParsedImport {
    specifier: String,
    clause: Option<String>,
    is_type_only: bool,
}

impl ParsedImport {
    fn binding_statement(
        &self,
        path: &Path,
        line_no: usize,
        namespace: &str,
        declaration: &str,
    ) -> Result<String> {
        let Some(clause) = self.clause.as_deref() else {
            return Ok("\n".to_string());
        };
        let clause = clause.trim();
        if let Some(rest) = clause.strip_prefix("* as ") {
            let name = rest.trim();
            validate_identifier(path, line_no, name)?;
            return Ok(format!("{declaration} {name} = {namespace};\n"));
        }
        if clause.starts_with('{') && clause.ends_with('}') {
            let bindings = import_named_bindings(path, line_no, &clause[1..clause.len() - 1])?;
            return Ok(format!("{declaration} {{ {bindings} }} = {namespace};\n"));
        }
        // A default import, optionally combined with a namespace or named
        // clause: `import D from`, `import D, * as ns from`, `import D, { a } from`.
        // The module namespace object exposes the default export under
        // `.default` (see `export_statement`).
        let (default_name, remainder) = match clause.split_once(',') {
            Some((default_name, remainder)) => (default_name.trim(), Some(remainder.trim())),
            None => (clause, None),
        };
        validate_identifier(path, line_no, default_name)?;
        let mut out = format!("{declaration} {default_name} = {namespace}.default;\n");
        if let Some(remainder) = remainder {
            if let Some(rest) = remainder.strip_prefix("* as ") {
                let name = rest.trim();
                validate_identifier(path, line_no, name)?;
                out.push_str(&format!("{declaration} {name} = {namespace};\n"));
            } else if remainder.starts_with('{') && remainder.ends_with('}') {
                let bindings =
                    import_named_bindings(path, line_no, &remainder[1..remainder.len() - 1])?;
                out.push_str(&format!("{declaration} {{ {bindings} }} = {namespace};\n"));
            } else {
                anyhow::bail!(
                    "{}:{}: unsupported TypeScript import clause `{}` for snapshot scaffold",
                    path.display(),
                    line_no,
                    clause
                );
            }
        }
        Ok(out)
    }
}

fn parse_import_line(line: &str) -> Option<ParsedImport> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("import ")?;
    if let Some(type_rest) = rest.strip_prefix("type ") {
        return quoted_specifier(type_rest).map(|specifier| ParsedImport {
            specifier,
            clause: None,
            is_type_only: true,
        });
    }
    if let Some(specifier) = quoted_specifier(rest) {
        return Some(ParsedImport {
            specifier,
            clause: None,
            is_type_only: false,
        });
    }
    let from_index = rest.find(" from ")?;
    let clause = rest[..from_index].trim().to_string();
    let specifier = quoted_specifier(&rest[from_index + 6..])?;
    Some(ParsedImport {
        specifier,
        clause: Some(clause),
        is_type_only: false,
    })
}

fn quoted_specifier(input: &str) -> Option<String> {
    let input = input.trim_start();
    let quote = input.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let end = input[1..].find(quote)?;
    Some(input[1..1 + end].to_string())
}

fn import_named_bindings(path: &Path, line_no: usize, source: &str) -> Result<String> {
    let mut out = Vec::new();
    for binding in source
        .split(',')
        .map(str::trim)
        .filter(|binding| !binding.is_empty())
    {
        let mut parts = binding.split_whitespace();
        let imported = parts.next().unwrap_or_default();
        validate_identifier(path, line_no, imported)?;
        match (parts.next(), parts.next(), parts.next()) {
            (None, None, None) => out.push(imported.to_string()),
            (Some("as"), Some(local), None) => {
                validate_identifier(path, line_no, local)?;
                out.push(format!("{imported}: {local}"));
            }
            _ => anyhow::bail!(
                "{}:{}: unsupported named TypeScript import binding `{}`",
                path.display(),
                line_no,
                binding
            ),
        }
    }
    Ok(out.join(", "))
}

fn export_statement(line: &str, namespace: &str) -> Result<Option<String>> {
    let trimmed = line.trim_start();
    let prefix = &line[..line.len() - trimmed.len()];
    if let Some(rest) = trimmed.strip_prefix("export async function ") {
        let name = export_name_before_paren(rest);
        return Ok(Some(format!(
            "{prefix}{namespace}.{name} = {name};\n{prefix}async function {rest}\n"
        )));
    }
    if let Some(rest) = trimmed.strip_prefix("export function ") {
        let name = export_name_before_paren(rest);
        return Ok(Some(format!(
            "{prefix}{namespace}.{name} = {name};\n{prefix}function {rest}\n"
        )));
    }
    for keyword in ["const", "let", "var"] {
        let export_prefix = format!("export {keyword} ");
        if let Some(rest) = trimmed.strip_prefix(&export_prefix) {
            return Ok(Some(exported_binding_statement(
                keyword, rest, namespace, prefix,
            )));
        }
    }
    if let Some(rest) = trimmed.strip_prefix("export {") {
        let Some(bindings) = rest.strip_suffix("};").or_else(|| rest.strip_suffix('}')) else {
            anyhow::bail!("unsupported named export statement `{}`", trimmed);
        };
        let mut out = String::new();
        for binding in bindings
            .split(',')
            .map(str::trim)
            .filter(|binding| !binding.is_empty())
        {
            let mut parts = binding.split_whitespace();
            let local = parts.next().unwrap_or_default();
            match (parts.next(), parts.next(), parts.next()) {
                (None, None, None) => {
                    out.push_str(&format!("{prefix}{namespace}.{local} = {local};\n"))
                }
                (Some("as"), Some(exported), None) => {
                    out.push_str(&format!("{prefix}{namespace}.{exported} = {local};\n"));
                }
                _ => anyhow::bail!("unsupported named export binding `{}`", binding),
            }
        }
        return Ok(Some(out));
    }
    if let Some(rest) = trimmed.strip_prefix("export default ") {
        // Assign the default export onto the module namespace under `.default`,
        // which `binding_statement` reads for default imports. `rest` keeps the
        // expression and its trailing semicolon, so `export default fs;` becomes
        // `<namespace>.default = fs;` and `export default function f(){}` becomes
        // a function-expression assignment.
        return Ok(Some(format!("{prefix}{namespace}.default = {rest}\n")));
    }
    Ok(None)
}

fn exported_binding_statement(keyword: &str, rest: &str, namespace: &str, prefix: &str) -> String {
    let name_end = rest
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(rest.len());
    let name = &rest[..name_end];
    let rhs = rest[name_end..].trim_start();
    format!("{prefix}{keyword} {name} {rhs}\n{prefix}{namespace}.{name} = {name};\n")
}

fn export_name_before_paren(rest: &str) -> &str {
    rest.find('(')
        .map(|idx| rest[..idx].trim())
        .unwrap_or_else(|| rest.trim())
}

fn validate_identifier(path: &Path, line_no: usize, value: &str) -> Result<()> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        anyhow::bail!("{}:{}: empty identifier", path.display(), line_no);
    };
    if !(first == '_' || first == '$' || first.is_ascii_alphabetic()) {
        anyhow::bail!(
            "{}:{}: invalid identifier `{}`",
            path.display(),
            line_no,
            value
        );
    }
    if !chars.all(|ch| ch == '_' || ch == '$' || ch.is_ascii_alphanumeric()) {
        anyhow::bail!(
            "{}:{}: invalid identifier `{}`",
            path.display(),
            line_no,
            value
        );
    }
    Ok(())
}

pub struct TypeScriptSnapshotContext<'runtime> {
    context: chidori_quickjs::SnapshotContext<'runtime>,
}

pub struct TypeScriptSnapshotHostState {
    runtime_ctx: RuntimeContext,
    template_engine: Option<Arc<TemplateEngine>>,
    tokio_rt: Option<Arc<tokio::runtime::Runtime>>,
    providers: Option<Arc<ProviderRegistry>>,
    policy: Arc<PolicyConfig>,
    policy_cache: Arc<StdMutex<PolicyCache>>,
    runtime_policy: RuntimePolicy,
    tools: Option<Arc<ToolRegistry>>,
    mcp: Option<Arc<McpManager>>,
}

impl TypeScriptSnapshotHostState {
    pub fn new(runtime_ctx: RuntimeContext) -> Self {
        Self {
            runtime_ctx,
            template_engine: None,
            tokio_rt: None,
            providers: None,
            policy: Arc::new(PolicyConfig::default()),
            policy_cache: Arc::new(StdMutex::new(PolicyCache::default())),
            runtime_policy: RuntimePolicy::durable_default("snapshot-host"),
            tools: None,
            mcp: None,
        }
    }

    pub fn with_template_engine(
        runtime_ctx: RuntimeContext,
        template_engine: Arc<TemplateEngine>,
    ) -> Self {
        Self {
            runtime_ctx: runtime_ctx.clone(),
            template_engine: Some(template_engine),
            ..Self::new(runtime_ctx)
        }
    }

    pub fn with_http(
        runtime_ctx: RuntimeContext,
        tokio_rt: Arc<tokio::runtime::Runtime>,
        policy: Arc<PolicyConfig>,
        policy_cache: Arc<StdMutex<PolicyCache>>,
    ) -> Self {
        Self {
            runtime_ctx,
            template_engine: None,
            tokio_rt: Some(tokio_rt),
            providers: None,
            policy,
            policy_cache,
            runtime_policy: RuntimePolicy::durable_default("snapshot-host"),
            tools: None,
            mcp: None,
        }
    }

    pub fn with_prompt(
        runtime_ctx: RuntimeContext,
        providers: Arc<ProviderRegistry>,
        tokio_rt: Arc<tokio::runtime::Runtime>,
    ) -> Self {
        Self {
            runtime_ctx,
            template_engine: None,
            tokio_rt: Some(tokio_rt),
            providers: Some(providers),
            policy: Arc::new(PolicyConfig::default()),
            policy_cache: Arc::new(StdMutex::new(PolicyCache::default())),
            runtime_policy: RuntimePolicy::durable_default("snapshot-host"),
            tools: None,
            mcp: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_tools(
        runtime_ctx: RuntimeContext,
        providers: Arc<ProviderRegistry>,
        template_engine: Arc<TemplateEngine>,
        tokio_rt: Arc<tokio::runtime::Runtime>,
        policy: Arc<PolicyConfig>,
        policy_cache: Arc<StdMutex<PolicyCache>>,
        runtime_policy: RuntimePolicy,
        tools: Arc<ToolRegistry>,
        mcp: Arc<McpManager>,
    ) -> Self {
        Self {
            runtime_ctx,
            template_engine: Some(template_engine),
            tokio_rt: Some(tokio_rt),
            providers: Some(providers),
            policy,
            policy_cache,
            runtime_policy,
            tools: Some(tools),
            mcp: Some(mcp),
        }
    }
}

fn execute_snapshot_workspace_call(
    runtime_ctx: &RuntimeContext,
    action: &str,
    args: serde_json::Value,
    live: impl FnOnce() -> Result<serde_json::Value>,
) -> Result<serde_json::Value> {
    let call_args = serde_json::json!({
        "action": action,
        "args": args,
    });
    let seq = runtime_ctx.next_seq();
    let started = chrono::Utc::now();
    let result = live();
    let duration_ms = chrono::Utc::now()
        .signed_duration_since(started)
        .num_milliseconds()
        .max(0) as u64;
    match result {
        Ok(result) => {
            runtime_ctx.record_call(CallRecord {
                seq,
                parent_seq: None,
                function: "workspace".to_string(),
                args: call_args,
                result: result.clone(),
                duration_ms,
                token_usage: None,
                timestamp: started,
                error: None,
            });
            Ok(result)
        }
        Err(err) => {
            let message = err.to_string();
            runtime_ctx.record_call(CallRecord {
                seq,
                parent_seq: None,
                function: "workspace".to_string(),
                args: call_args,
                result: serde_json::Value::Null,
                duration_ms,
                token_usage: None,
                timestamp: started,
                error: Some(message),
            });
            Err(err)
        }
    }
}

fn snapshot_workspace_root(runtime_ctx: &RuntimeContext) -> Result<PathBuf> {
    runtime_ctx.workspace_root().ok_or_else(|| {
        anyhow::anyhow!(
            "chidori.workspace requires CHIDORI_WORKSPACE_ROOT or a runtime workspace root"
        )
    })
}

unsafe extern "C" fn native_runtime_workspace_list(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };

    let result = (|| -> Result<serde_json::Value> {
        let options = if argc > 0 {
            unsafe { chidori_quickjs::callback_arg_to_json(ctx, argc, argv, 0) }
                .map_err(|err| anyhow::anyhow!(err))?
        } else {
            serde_json::json!({})
        };
        let complete_only = options
            .get("completeOnly")
            .or_else(|| options.get("complete_only"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        execute_snapshot_workspace_call(&state.runtime_ctx, "list", options, || {
            let root = snapshot_workspace_root(&state.runtime_ctx)?;
            crate::runtime::workspace::list(&root, complete_only)
                .map_err(|err| anyhow::anyhow!(err))
        })
    })();

    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

unsafe extern "C" fn native_runtime_workspace_read(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };

    let result = (|| -> Result<serde_json::Value> {
        let path = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 0) }
            .map_err(|err| anyhow::anyhow!(err))?;
        execute_snapshot_workspace_call(
            &state.runtime_ctx,
            "read",
            serde_json::json!({ "path": path }),
            || {
                let root = snapshot_workspace_root(&state.runtime_ctx)?;
                crate::runtime::workspace::read(&root, &path)
                    .map(serde_json::Value::String)
                    .map_err(|err| anyhow::anyhow!(err))
            },
        )
    })();

    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

unsafe extern "C" fn native_runtime_workspace_write(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };

    let result = (|| -> Result<serde_json::Value> {
        let path = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 0) }
            .map_err(|err| anyhow::anyhow!(err))?;
        let content = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 1) }
            .map_err(|err| anyhow::anyhow!(err))?;
        let options = if argc > 2 {
            unsafe { chidori_quickjs::callback_arg_to_json(ctx, argc, argv, 2) }
                .map_err(|err| anyhow::anyhow!(err))?
        } else {
            serde_json::json!({})
        };
        execute_snapshot_workspace_call(
            &state.runtime_ctx,
            "write",
            serde_json::json!({
                "path": path,
                "bytes": content.len(),
                "options": options,
            }),
            || {
                let root = snapshot_workspace_root(&state.runtime_ctx)?;
                crate::runtime::workspace::write(&root, &path, &content, &options)
                    .map_err(|err| anyhow::anyhow!(err))
            },
        )
    })();

    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

unsafe extern "C" fn native_runtime_workspace_delete(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };

    let result = (|| -> Result<serde_json::Value> {
        let path = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 0) }
            .map_err(|err| anyhow::anyhow!(err))?;
        let reason_value = if argc > 1 {
            unsafe { chidori_quickjs::callback_arg_to_json(ctx, argc, argv, 1) }
                .map_err(|err| anyhow::anyhow!(err))?
        } else {
            serde_json::Value::Null
        };
        let reason = reason_value.as_str().map(ToOwned::to_owned);
        execute_snapshot_workspace_call(
            &state.runtime_ctx,
            "delete",
            serde_json::json!({
                "path": path,
                "reason": reason,
            }),
            || {
                let root = snapshot_workspace_root(&state.runtime_ctx)?;
                crate::runtime::workspace::delete(&root, &path, reason.as_deref())
                    .map_err(|err| anyhow::anyhow!(err))
            },
        )
    })();

    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

unsafe extern "C" fn native_runtime_workspace_manifest(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    _argc: std::ffi::c_int,
    _argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };

    let result = execute_snapshot_workspace_call(
        &state.runtime_ctx,
        "manifest",
        serde_json::json!({}),
        || {
            let root = snapshot_workspace_root(&state.runtime_ctx)?;
            crate::runtime::workspace::manifest(&root).map_err(|err| anyhow::anyhow!(err))
        },
    );

    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

/// Reject filesystem access unless the policy permits the captured VFS. Host
/// (uncaptured disk) mode is intentionally unimplemented here — it is rejected
/// for durable runs and out of scope for the captured-effects model.
fn fs_policy_guard(state: &TypeScriptSnapshotHostState) -> Result<()> {
    match state.runtime_policy.fs {
        crate::runtime::snapshot::FsPolicy::Captured => Ok(()),
        crate::runtime::snapshot::FsPolicy::Disabled => {
            anyhow::bail!("node:fs is disabled by Chidori runtime policy (fs=disabled)")
        }
        crate::runtime::snapshot::FsPolicy::Host => {
            anyhow::bail!("node:fs host-disk mode (fs=host) is not implemented in this runtime")
        }
    }
}

/// Bytes cross the JS↔Rust boundary base64-encoded so binary content survives
/// the JSON value bridge intact.
fn b64_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn b64_decode(s: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(s.as_bytes())
        .map_err(|err| anyhow::anyhow!("invalid base64 payload: {err}"))
}

unsafe extern "C" fn native_runtime_fs_read(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };
    let result = (|| -> Result<serde_json::Value> {
        fs_policy_guard(state)?;
        let path = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 0) }
            .map_err(|err| anyhow::anyhow!(err))?;
        let bytes = state
            .runtime_ctx
            .vfs_read(&path)
            .map_err(|err| anyhow::anyhow!(err))?;
        Ok(serde_json::Value::String(b64_encode(&bytes)))
    })();
    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

unsafe extern "C" fn native_runtime_fs_write(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };
    let result = (|| -> Result<serde_json::Value> {
        fs_policy_guard(state)?;
        let path = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 0) }
            .map_err(|err| anyhow::anyhow!(err))?;
        let data_b64 = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 1) }
            .map_err(|err| anyhow::anyhow!(err))?;
        let bytes = b64_decode(&data_b64)?;
        state
            .runtime_ctx
            .vfs_write(&path, bytes)
            .map_err(|err| anyhow::anyhow!(err))?;
        Ok(serde_json::Value::Null)
    })();
    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

unsafe extern "C" fn native_runtime_fs_append(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };
    let result = (|| -> Result<serde_json::Value> {
        fs_policy_guard(state)?;
        let path = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 0) }
            .map_err(|err| anyhow::anyhow!(err))?;
        let data_b64 = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 1) }
            .map_err(|err| anyhow::anyhow!(err))?;
        let bytes = b64_decode(&data_b64)?;
        state
            .runtime_ctx
            .vfs_append(&path, &bytes)
            .map_err(|err| anyhow::anyhow!(err))?;
        Ok(serde_json::Value::Null)
    })();
    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

unsafe extern "C" fn native_runtime_fs_readdir(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };
    let result = (|| -> Result<serde_json::Value> {
        fs_policy_guard(state)?;
        let path = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 0) }
            .map_err(|err| anyhow::anyhow!(err))?;
        let names = state
            .runtime_ctx
            .vfs_readdir(&path)
            .map_err(|err| anyhow::anyhow!(err))?;
        Ok(serde_json::Value::Array(
            names.into_iter().map(serde_json::Value::String).collect(),
        ))
    })();
    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

unsafe extern "C" fn native_runtime_fs_mkdir(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };
    let result = (|| -> Result<serde_json::Value> {
        fs_policy_guard(state)?;
        let path = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 0) }
            .map_err(|err| anyhow::anyhow!(err))?;
        let recursive = if argc > 1 {
            unsafe { chidori_quickjs::callback_arg_to_json(ctx, argc, argv, 1) }
                .map_err(|err| anyhow::anyhow!(err))?
                .as_bool()
                .unwrap_or(false)
        } else {
            false
        };
        state
            .runtime_ctx
            .vfs_mkdir(&path, recursive)
            .map_err(|err| anyhow::anyhow!(err))?;
        Ok(serde_json::Value::Null)
    })();
    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

unsafe extern "C" fn native_runtime_fs_rm(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };
    let result = (|| -> Result<serde_json::Value> {
        fs_policy_guard(state)?;
        let path = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 0) }
            .map_err(|err| anyhow::anyhow!(err))?;
        let recursive = if argc > 1 {
            unsafe { chidori_quickjs::callback_arg_to_json(ctx, argc, argv, 1) }
                .map_err(|err| anyhow::anyhow!(err))?
                .as_bool()
                .unwrap_or(false)
        } else {
            false
        };
        let force = if argc > 2 {
            unsafe { chidori_quickjs::callback_arg_to_json(ctx, argc, argv, 2) }
                .map_err(|err| anyhow::anyhow!(err))?
                .as_bool()
                .unwrap_or(false)
        } else {
            false
        };
        state
            .runtime_ctx
            .vfs_remove(&path, recursive, force)
            .map_err(|err| anyhow::anyhow!(err))?;
        Ok(serde_json::Value::Null)
    })();
    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

unsafe extern "C" fn native_runtime_fs_stat(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };
    let result = (|| -> Result<serde_json::Value> {
        fs_policy_guard(state)?;
        let path = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 0) }
            .map_err(|err| anyhow::anyhow!(err))?;
        state
            .runtime_ctx
            .vfs_stat(&path)
            .map_err(|err| anyhow::anyhow!(err))
    })();
    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

unsafe extern "C" fn native_runtime_fs_exists(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };
    let result = (|| -> Result<serde_json::Value> {
        fs_policy_guard(state)?;
        let path = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 0) }
            .map_err(|err| anyhow::anyhow!(err))?;
        Ok(serde_json::Value::Bool(state.runtime_ctx.vfs_exists(&path)))
    })();
    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

unsafe extern "C" fn native_runtime_fs_rename(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };
    let result = (|| -> Result<serde_json::Value> {
        fs_policy_guard(state)?;
        let from = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 0) }
            .map_err(|err| anyhow::anyhow!(err))?;
        let to = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 1) }
            .map_err(|err| anyhow::anyhow!(err))?;
        state
            .runtime_ctx
            .vfs_rename(&from, &to)
            .map_err(|err| anyhow::anyhow!(err))?;
        Ok(serde_json::Value::Null)
    })();
    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

/// Reject crypto access when the policy disables it.
fn crypto_policy_guard(state: &TypeScriptSnapshotHostState) -> Result<()> {
    if state.runtime_policy.crypto == crate::runtime::snapshot::CryptoPolicy::Disabled {
        anyhow::bail!("node:crypto is disabled by Chidori runtime policy (crypto=disabled)");
    }
    Ok(())
}

/// Draw `n` random bytes honoring the crypto policy, with capture/replay so a
/// resume reproduces the exact bytes. `Captured`/`Host` draw from the host
/// CSPRNG (only `Captured` records); `Seeded` derives deterministically from
/// the run seed (and records, so replay is exact). Raises `CryptoRandom`.
fn execute_captured_random(state: &TypeScriptSnapshotHostState, n: usize) -> Result<Vec<u8>> {
    use crate::runtime::snapshot::CryptoPolicy;
    let ctx = &state.runtime_ctx;
    let seq = ctx.next_seq();
    match ctx.try_replay_checked(seq, "crypto.random") {
        Ok(Some(record)) => {
            ctx.note_capability(Capability::CryptoRandom, seq);
            let b64 = record
                .result
                .get("bytes")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("crypto replay record is missing bytes"))?;
            return b64_decode(b64);
        }
        Ok(None) => {}
        Err(message) => anyhow::bail!(message),
    }
    let bytes = match state.runtime_policy.crypto {
        CryptoPolicy::Seeded => crate::runtime::crypto::seeded_bytes(
            &state.runtime_policy.deterministic_seed,
            seq,
            n,
        ),
        CryptoPolicy::Captured | CryptoPolicy::Host => crate::runtime::crypto::random_bytes(n),
        CryptoPolicy::Disabled => unreachable!("guarded by crypto_policy_guard"),
    };
    if state.runtime_policy.crypto != CryptoPolicy::Host {
        ctx.record_call(CallRecord {
            seq,
            parent_seq: None,
            function: "crypto.random".to_string(),
            args: serde_json::json!({ "n": n }),
            result: serde_json::json!({ "bytes": b64_encode(&bytes) }),
            duration_ms: 0,
            token_usage: None,
            timestamp: chrono::Utc::now(),
            error: None,
        });
    }
    ctx.note_capability(Capability::CryptoRandom, seq);
    Ok(bytes)
}

unsafe extern "C" fn native_runtime_crypto_random(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };
    let result = (|| -> Result<serde_json::Value> {
        crypto_policy_guard(state)?;
        let n = unsafe { chidori_quickjs::callback_arg_to_json(ctx, argc, argv, 0) }
            .map_err(|err| anyhow::anyhow!(err))?
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("crypto random length must be a non-negative integer"))?
            as usize;
        if n > 1_048_576 {
            anyhow::bail!("crypto random length {n} exceeds the 1MiB cap");
        }
        let bytes = execute_captured_random(state, n)?;
        Ok(serde_json::Value::String(b64_encode(&bytes)))
    })();
    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

unsafe extern "C" fn native_runtime_crypto_hash(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };
    let result = (|| -> Result<serde_json::Value> {
        crypto_policy_guard(state)?;
        let alg = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 0) }
            .map_err(|err| anyhow::anyhow!(err))?;
        let data_b64 = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 1) }
            .map_err(|err| anyhow::anyhow!(err))?;
        let data = b64_decode(&data_b64)?;
        let digest = crate::runtime::crypto::hash(&alg, &data)?;
        // Hashing is a pure function of its input: flag for visibility but do
        // not call-log (nothing nondeterministic to replay).
        state
            .runtime_ctx
            .note_capability(Capability::CryptoHash, state.runtime_ctx.current_seq());
        Ok(serde_json::Value::String(b64_encode(&digest)))
    })();
    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

unsafe extern "C" fn native_runtime_crypto_hmac(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };
    let result = (|| -> Result<serde_json::Value> {
        crypto_policy_guard(state)?;
        let alg = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 0) }
            .map_err(|err| anyhow::anyhow!(err))?;
        let key_b64 = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 1) }
            .map_err(|err| anyhow::anyhow!(err))?;
        let data_b64 = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 2) }
            .map_err(|err| anyhow::anyhow!(err))?;
        let key = b64_decode(&key_b64)?;
        let data = b64_decode(&data_b64)?;
        let digest = crate::runtime::crypto::hmac(&alg, &key, &data)?;
        state
            .runtime_ctx
            .note_capability(Capability::CryptoHash, state.runtime_ctx.current_seq());
        Ok(serde_json::Value::String(b64_encode(&digest)))
    })();
    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

/// Raise a capability flag from JS (used by the timer polyfill for
/// `Timer`/`Microtask`). The flag name is the snake_case `Capability::as_str`
/// form; unknown names are ignored so a forward-compatible prelude can't crash.
unsafe extern "C" fn native_runtime_note_capability(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };
    if let Ok(name) = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 0) } {
        if let Some(cap) = Capability::from_str(&name) {
            state
                .runtime_ctx
                .note_capability(cap, state.runtime_ctx.current_seq());
        }
    }
    unsafe { chidori_quickjs::json_to_js_value(ctx, serde_json::Value::Null) }
        .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) })
}

unsafe extern "C" fn native_runtime_log(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };

    let result = (|| -> Result<serde_json::Value> {
        let message = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 0) }
            .map_err(|err| anyhow::anyhow!(err))?;
        let fields = if argc > 1 {
            unsafe { chidori_quickjs::callback_arg_to_json(ctx, argc, argv, 1) }
                .map_err(|err| anyhow::anyhow!(err))?
        } else {
            serde_json::Value::Null
        };

        let mut args = serde_json::Map::new();
        args.insert("message".to_string(), serde_json::Value::String(message));
        if !fields.is_null() {
            args.insert("fields".to_string(), fields);
        }
        let args = serde_json::Value::Object(args);
        host_core::execute_durable_json_call(&state.runtime_ctx, "log", args.clone(), || {
            host_core::execute_log(&args)
        })
    })();

    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

unsafe extern "C" fn native_runtime_checkpoint(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };

    let result = (|| -> Result<serde_json::Value> {
        let label = if argc > 0 {
            unsafe { chidori_quickjs::callback_arg_to_json(ctx, argc, argv, 0) }
                .map_err(|err| anyhow::anyhow!(err))?
        } else {
            serde_json::Value::Null
        };
        let data = if argc > 1 {
            unsafe { chidori_quickjs::callback_arg_to_json(ctx, argc, argv, 1) }
                .map_err(|err| anyhow::anyhow!(err))?
        } else {
            serde_json::Value::Null
        };
        let args = serde_json::json!({
            "label": label,
            "data": data,
        });
        host_core::execute_durable_json_call(&state.runtime_ctx, "checkpoint", args, || {
            Ok(serde_json::Value::Null)
        })
    })();

    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

unsafe extern "C" fn native_runtime_memory(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };

    let result = (|| -> Result<serde_json::Value> {
        let action = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 0) }
            .map_err(|err| anyhow::anyhow!(err))?;
        let key = optional_json_string_arg(ctx, argc, argv, 1)?;
        let value = if argc > 2 {
            unsafe { chidori_quickjs::callback_arg_to_json(ctx, argc, argv, 2) }
                .map_err(|err| anyhow::anyhow!(err))?
        } else {
            serde_json::Value::Null
        };
        let options = if argc > 3 {
            unsafe { chidori_quickjs::callback_arg_to_json(ctx, argc, argv, 3) }
                .map_err(|err| anyhow::anyhow!(err))?
        } else {
            serde_json::Value::Null
        };
        let namespace = options
            .get("namespace")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("default");
        let prefix = options
            .get("prefix")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let args = serde_json::json!({
            "action": action,
            "key": key,
            "namespace": namespace,
            "prefix": prefix,
            "value": value,
        });
        host_core::execute_durable_json_call(&state.runtime_ctx, "memory", args.clone(), || {
            host_core::execute_memory(&args)
        })
    })();

    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

unsafe extern "C" fn native_runtime_template(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };
    let Some(template_engine) = state.template_engine.clone() else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot template engine")
        };
    };

    let result = (|| -> Result<serde_json::Value> {
        let template = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 0) }
            .map_err(|err| anyhow::anyhow!(err))?;
        let vars = if argc > 1 {
            unsafe { chidori_quickjs::callback_arg_to_json(ctx, argc, argv, 1) }
                .map_err(|err| anyhow::anyhow!(err))?
        } else {
            serde_json::json!({})
        };
        let args = serde_json::json!({
            "template": template,
            "vars": vars,
        });
        host_core::execute_durable_json_call(&state.runtime_ctx, "template", args.clone(), || {
            host_core::execute_template(&template_engine, &args)
        })
    })();

    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

unsafe extern "C" fn native_runtime_exec_js(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    native_runtime_sandbox_string(ctx, argc, argv, "exec_js")
}

unsafe extern "C" fn native_runtime_exec_python(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    native_runtime_sandbox_string(ctx, argc, argv, "exec_python")
}

unsafe extern "C" fn native_runtime_exec_wasm(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };

    let result = (|| -> Result<serde_json::Value> {
        let source = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 0) }
            .map_err(|err| anyhow::anyhow!(err))?;
        let options = if argc > 1 {
            unsafe { chidori_quickjs::callback_arg_to_json(ctx, argc, argv, 1) }
                .map_err(|err| anyhow::anyhow!(err))?
        } else {
            serde_json::Value::Null
        };
        let function = options
            .get("function")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("main");
        let args = options
            .get("args")
            .cloned()
            .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
        let fuel = options
            .get("fuel")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1_000_000)
            .max(1);
        let memory_pages = options
            .get("memoryPages")
            .or_else(|| options.get("memory_pages"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(16)
            .max(1);
        let args = serde_json::json!({
            "source": source,
            "function": function,
            "args": args,
            "fuel": fuel,
            "memory_pages": memory_pages,
        });
        host_core::execute_durable_json_call(&state.runtime_ctx, "exec", args.clone(), || {
            host_core::execute_sandbox_wasm(&args)
        })
    })();

    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

unsafe extern "C" fn native_runtime_http(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };

    let result = (|| -> Result<serde_json::Value> {
        let first = unsafe { chidori_quickjs::callback_arg_to_json(ctx, argc, argv, 0) }
            .map_err(|err| anyhow::anyhow!(err))?;
        let (url, options) = if let Some(url) = first.as_str() {
            let options = if argc > 1 {
                unsafe { chidori_quickjs::callback_arg_to_json(ctx, argc, argv, 1) }
                    .map_err(|err| anyhow::anyhow!(err))?
            } else {
                serde_json::Value::Object(Default::default())
            };
            (url.to_string(), options)
        } else if let serde_json::Value::Object(map) = first {
            let url = map
                .get("url")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("chidori.http options must include string url"))?
                .to_string();
            (url, serde_json::Value::Object(map))
        } else {
            anyhow::bail!("chidori.http requires a URL string or options object")
        };
        let mut method = options
            .get("method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("GET")
            .to_uppercase();
        if method.is_empty() {
            method = "GET".to_string();
        }
        let headers = options.get("headers").and_then(|value| match value {
            serde_json::Value::Object(map) => Some(map.clone()),
            _ => None,
        });
        let body = options.get("body").cloned();
        let params = options
            .get("params")
            .or_else(|| options.get("query"))
            .and_then(|value| match value {
                serde_json::Value::Object(map) => Some(map.clone()),
                _ => None,
            });
        let args = serde_json::json!({
            "url": url,
            "method": method,
            "headers": headers,
            "body": body,
            "params": params,
        });
        host_core::execute_durable_json_call(&state.runtime_ctx, "http", args.clone(), || {
            snapshot_enforce_policy(
                state,
                "http",
                &serde_json::json!({
                    "url": args.get("url").cloned().unwrap_or_default(),
                    "method": args.get("method").cloned().unwrap_or_default(),
                }),
            )?;
            let tokio_rt = state
                .tokio_rt
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("missing TypeScript snapshot tokio runtime"))?;
            host_core::execute_http(tokio_rt, &args)
        })
    })();

    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

unsafe extern "C" fn native_runtime_input(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };

    let result = (|| -> Result<serde_json::Value> {
        let prompt = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 0) }
            .map_err(|err| anyhow::anyhow!(err))?;
        host_core::execute_input(&state.runtime_ctx, &serde_json::json!({ "prompt": prompt }))
    })();

    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

fn snapshot_execute_tool(
    state: &TypeScriptSnapshotHostState,
    name: &str,
    kwargs_value: serde_json::Value,
) -> Result<serde_json::Value> {
    let kwargs_value = match kwargs_value {
        serde_json::Value::Object(_) => kwargs_value,
        serde_json::Value::Null => serde_json::Value::Object(serde_json::Map::new()),
        other => anyhow::bail!("chidori.tool args must be an object, got {other}"),
    };

    host_core::execute_tool_call(&state.runtime_ctx, name, kwargs_value.clone(), || {
        let tools = state
            .tools
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("missing TypeScript snapshot tool registry"))?;
        let providers = state
            .providers
            .clone()
            .ok_or_else(|| anyhow::anyhow!("missing TypeScript snapshot provider registry"))?;
        let template_engine = state
            .template_engine
            .clone()
            .ok_or_else(|| anyhow::anyhow!("missing TypeScript snapshot template engine"))?;
        let tokio_rt = state
            .tokio_rt
            .clone()
            .ok_or_else(|| anyhow::anyhow!("missing TypeScript snapshot tokio runtime"))?;
        let mcp = state
            .mcp
            .clone()
            .ok_or_else(|| anyhow::anyhow!("missing TypeScript snapshot MCP manager"))?;
        let tool_def = tools
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!(tools.describe_miss(name)))?;
        snapshot_enforce_policy(state, &format!("tool:{name}"), &kwargs_value)?;

        match &tool_def.backend {
            ToolBackend::Mcp {
                server_id,
                remote_name,
            } => tokio_rt
                .block_on(async { mcp.call_tool(server_id, remote_name, &kwargs_value).await }),
            ToolBackend::TypeScript => TypeScriptVmRuntime::new(state.runtime_policy.clone())?
                .run_tool_file_with_context(
                    &tool_def.source_path,
                    &kwargs_value,
                    state.runtime_ctx.clone(),
                    providers,
                    template_engine,
                    tokio_rt,
                    state.policy.clone(),
                    state.policy_cache.clone(),
                    state.tools.clone().ok_or_else(|| {
                        anyhow::anyhow!("missing TypeScript snapshot tool registry")
                    })?,
                    mcp,
                ),
            ToolBackend::Native => tools.dispatch_native(name, kwargs_value),
        }
    })
}

fn tool_def_to_schema(def: &ToolDef) -> ToolSchema {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for param in &def.params {
        let mut prop = serde_json::Map::new();
        prop.insert(
            "type".to_string(),
            serde_json::Value::String(param.param_type.clone()),
        );
        if let Some(description) = &param.description {
            prop.insert(
                "description".to_string(),
                serde_json::Value::String(description.clone()),
            );
        }
        properties.insert(param.name.clone(), serde_json::Value::Object(prop));
        if param.required {
            required.push(serde_json::Value::String(param.name.clone()));
        }
    }
    ToolSchema {
        name: def.name.clone(),
        description: def.description.clone(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": required,
        }),
    }
}

unsafe extern "C" fn native_runtime_prompt(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };

    let result = (|| -> Result<serde_json::Value> {
        let text = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 0) }
            .map_err(|err| anyhow::anyhow!(err))?;
        let options = if argc > 1 {
            unsafe { chidori_quickjs::callback_arg_to_json(ctx, argc, argv, 1) }
                .map_err(|err| anyhow::anyhow!(err))?
        } else {
            serde_json::Value::Object(Default::default())
        };
        let providers = state
            .providers
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("missing TypeScript snapshot provider registry"))?;
        let tokio_rt = state
            .tokio_rt
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("missing TypeScript snapshot tokio runtime"))?;
        let config = state.runtime_ctx.config();
        let model = options
            .get("model")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(&config.model)
            .to_string();
        let temperature = options
            .get("temperature")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(config.temperature);
        let max_tokens = options
            .get("maxTokens")
            .or_else(|| options.get("max_tokens"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(config.max_tokens);
        let max_turns = options
            .get("maxTurns")
            .or_else(|| options.get("max_turns"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(config.max_turns)
            .max(1);
        let system = options
            .get("system")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned);
        let format = options
            .get("format")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned);
        let prompt_type = options
            .get("type")
            .or_else(|| options.get("streamType"))
            .or_else(|| options.get("stream_type"))
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned);
        let tool_names: Vec<String> = options
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let mut tool_schemas = Vec::new();
        for name in &tool_names {
            let tools = state
                .tools
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("missing TypeScript snapshot tool registry"))?;
            let tool_def = tools
                .get(name)
                .ok_or_else(|| anyhow::anyhow!("Unknown tool in prompt tools: {name}"))?;
            tool_schemas.push(tool_def_to_schema(tool_def));
        }

        if !tool_schemas.is_empty() {
            let mut messages = vec![LlmMessage::user_text(text.clone())];
            let mut final_text = String::new();
            for turn in 0..max_turns {
                let request = LlmRequest {
                    model: model.clone(),
                    messages: messages.clone(),
                    system: system.clone(),
                    temperature,
                    max_tokens,
                    tools: tool_schemas.clone(),
                };
                let response = host_core::execute_prompt_response(
                    &state.runtime_ctx,
                    providers,
                    tokio_rt,
                    request,
                    serde_json::json!({
                        "text": text,
                        "model": model,
                        "type": prompt_type,
                        "tools": tool_names,
                        "turn": turn,
                        "max_turns": max_turns,
                    }),
                    prompt_type.clone(),
                )?;

                final_text = response.content.clone();
                if response.tool_calls.is_empty() {
                    break;
                }
                messages.push(LlmMessage::assistant_blocks(response.blocks.clone()));
                let mut result_blocks = Vec::new();
                for call in response.tool_calls {
                    match snapshot_execute_tool(state, &call.name, call.input.clone()) {
                        Ok(value) => result_blocks.push(ContentBlock::ToolResult {
                            tool_use_id: call.id,
                            content: serde_json::to_string(&value)
                                .unwrap_or_else(|_| value.to_string()),
                            is_error: false,
                        }),
                        Err(err) => result_blocks.push(ContentBlock::ToolResult {
                            tool_use_id: call.id,
                            content: err.to_string(),
                            is_error: true,
                        }),
                    }
                }
                messages.push(LlmMessage {
                    role: "user".to_string(),
                    content: result_blocks,
                });
            }
            if format.as_deref() == Some("json") {
                return serde_json::from_str::<serde_json::Value>(&final_text)
                    .or(Ok(serde_json::Value::String(final_text)));
            }
            return Ok(serde_json::Value::String(final_text));
        }

        let request = LlmRequest {
            model: model.clone(),
            messages: vec![LlmMessage::user_text(text.clone())],
            system,
            temperature,
            max_tokens,
            tools: Vec::new(),
        };
        let result = host_core::execute_prompt_text(
            &state.runtime_ctx,
            providers,
            tokio_rt,
            request,
            serde_json::json!({ "text": text, "model": model, "type": prompt_type }),
            prompt_type,
        )?;

        if format.as_deref() == Some("json") {
            if let Some(content) = result.as_str() {
                serde_json::from_str::<serde_json::Value>(content)
                    .or(Ok(serde_json::Value::String(content.to_string())))
            } else {
                Ok(result)
            }
        } else {
            Ok(result)
        }
    })();

    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

unsafe extern "C" fn native_runtime_tool(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };

    let result = (|| -> Result<serde_json::Value> {
        let name = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 0) }
            .map_err(|err| anyhow::anyhow!(err))?;
        let kwargs = if argc > 1 {
            unsafe { chidori_quickjs::callback_arg_to_json(ctx, argc, argv, 1) }
                .map_err(|err| anyhow::anyhow!(err))?
        } else {
            serde_json::Value::Object(Default::default())
        };
        snapshot_execute_tool(state, &name, kwargs)
    })();

    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

unsafe extern "C" fn native_runtime_call_agent(
    ctx: *mut chidori_quickjs::sys::JSContext,
    _this_val: chidori_quickjs::sys::JSValue,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };

    let result = (|| -> Result<serde_json::Value> {
        let path = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 0) }
            .map_err(|err| anyhow::anyhow!(err))?;
        let input = if argc > 1 {
            unsafe { chidori_quickjs::callback_arg_to_json(ctx, argc, argv, 1) }
                .map_err(|err| anyhow::anyhow!(err))?
        } else {
            serde_json::Value::Object(Default::default())
        };
        let args = serde_json::json!({
            "path": path,
            "input": input,
        });
        host_core::execute_call_agent(&state.runtime_ctx, args.clone(), || {
            let path = args
                .get("path")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let input = args.get("input").unwrap_or(&serde_json::Value::Null);
            match Path::new(path).extension().and_then(|ext| ext.to_str()) {
                Some("ts") => {
                    let providers = state.providers.clone().ok_or_else(|| {
                        anyhow::anyhow!("missing TypeScript snapshot provider registry")
                    })?;
                    let template_engine = state.template_engine.clone().ok_or_else(|| {
                        anyhow::anyhow!("missing TypeScript snapshot template engine")
                    })?;
                    let tokio_rt = state.tokio_rt.clone().ok_or_else(|| {
                        anyhow::anyhow!("missing TypeScript snapshot tokio runtime")
                    })?;
                    let tools = state.tools.clone().ok_or_else(|| {
                        anyhow::anyhow!("missing TypeScript snapshot tool registry")
                    })?;
                    let mcp = state.mcp.clone().ok_or_else(|| {
                        anyhow::anyhow!("missing TypeScript snapshot MCP manager")
                    })?;
                    TypeScriptVmRuntime::new(state.runtime_policy.clone())?
                        .run_agent_file_with_context(
                            Path::new(path),
                            input,
                            state.runtime_ctx.clone(),
                            providers,
                            template_engine,
                            tokio_rt,
                            state.policy.clone(),
                            state.policy_cache.clone(),
                            tools,
                            mcp,
                        )
                }
                _ => Err(anyhow::anyhow!("chidori.callAgent supports .ts agents")),
            }
        })
    })();

    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

fn snapshot_enforce_policy(
    state: &TypeScriptSnapshotHostState,
    target: &str,
    args: &serde_json::Value,
) -> Result<()> {
    let (decision, reason) = state.policy.decide(target, args);
    match decision {
        Decision::AlwaysAllow => Ok(()),
        Decision::NeverAllow => anyhow::bail!(
            "policy: `{}` denied{}",
            target,
            reason.map(|r| format!(" ({})", r)).unwrap_or_default()
        ),
        Decision::AskBefore => {
            {
                let cache = state.policy_cache.lock().unwrap();
                if cache.is_approved(target, args) {
                    return Ok(());
                }
            }
            if std::env::var("CHIDORI_POLICY_AUTO_APPROVE").ok().as_deref() == Some("1") {
                state.policy_cache.lock().unwrap().approve(target, args);
                return Ok(());
            }
            if state.runtime_ctx.input_mode() == InputMode::Pause {
                state.runtime_ctx.set_pending_approval(PendingApproval {
                    target: target.to_string(),
                    args: args.clone(),
                    reason,
                });
                anyhow::bail!(PAUSE_MARKER.to_string());
            }
            anyhow::bail!(
                "policy: `{}` requires approval{}. Set CHIDORI_POLICY_AUTO_APPROVE=1 to \
                 auto-approve, or run through the server so the approval flow can pause.",
                target,
                reason.map(|r| format!(" - {}", r)).unwrap_or_default()
            );
        }
    }
}

unsafe fn native_runtime_sandbox_string(
    ctx: *mut chidori_quickjs::sys::JSContext,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
    function: &'static str,
) -> chidori_quickjs::sys::JSValue {
    let Some(state) =
        (unsafe { chidori_quickjs::context_opaque_mut::<TypeScriptSnapshotHostState>(ctx) })
    else {
        return unsafe {
            chidori_quickjs::throw_string(ctx, "missing TypeScript snapshot host state")
        };
    };

    let result = (|| -> Result<serde_json::Value> {
        let source = unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 0) }
            .map_err(|err| anyhow::anyhow!(err))?;
        let options = if argc > 1 {
            unsafe { chidori_quickjs::callback_arg_to_json(ctx, argc, argv, 1) }
                .map_err(|err| anyhow::anyhow!(err))?
        } else {
            serde_json::Value::Null
        };
        let fuel = options
            .get("fuel")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(200_000_000)
            .max(1);
        let args = serde_json::json!({
            "source": source,
            "fuel": fuel,
        });
        host_core::execute_durable_json_call(&state.runtime_ctx, function, args.clone(), || {
            host_core::execute_sandbox_string(function, &args)
        })
    })();

    match result {
        Ok(value) => unsafe { chidori_quickjs::json_to_js_value(ctx, value) }
            .unwrap_or_else(|err| unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) }),
        Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
    }
}

unsafe fn optional_json_string_arg(
    ctx: *mut chidori_quickjs::sys::JSContext,
    argc: std::ffi::c_int,
    argv: *mut chidori_quickjs::sys::JSValue,
    index: usize,
) -> Result<Option<String>> {
    if index >= usize::try_from(argc).unwrap_or(0) {
        return Ok(None);
    }
    match unsafe { chidori_quickjs::callback_arg_to_json(ctx, argc, argv, index) }
        .map_err(|err| anyhow::anyhow!(err))?
    {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::String(value) => Ok(Some(value)),
        other => anyhow::bail!("callback argument {index} must be a string or null, got {other}"),
    }
}

impl TypeScriptSnapshotContext<'_> {
    pub fn call_export(
        &mut self,
        export_name: &str,
        input: serde_json::Value,
    ) -> Result<chidori_quickjs::RunState> {
        self.context
            .call_export_json(export_name, input)
            .map_err(|err| anyhow::anyhow!(err))
    }

    pub fn call_agent(&mut self, input: serde_json::Value) -> Result<chidori_quickjs::RunState> {
        self.call_export("agent", input)
    }

    pub fn snapshot(&mut self) -> Result<Vec<u8>> {
        self.snapshot_roots(DEFAULT_TS_SNAPSHOT_ROOTS)
    }

    pub fn snapshot_roots(&mut self, roots: &[&str]) -> Result<Vec<u8>> {
        self.context
            .snapshot_globals(roots)
            .map_err(|err| anyhow::anyhow!(err))
    }

    /// Sets the opaque native callback state pointer for this QuickJS context.
    ///
    /// # Safety
    ///
    /// The caller must ensure the pointer is either null or remains valid for
    /// every native callback that may read it.
    pub unsafe fn set_context_opaque(&mut self, opaque: *mut std::ffi::c_void) {
        unsafe {
            self.context.set_context_opaque(opaque);
        }
    }

    pub fn install_global_native_function(
        &mut self,
        name: &str,
        function: chidori_quickjs::sys::JSCFunction,
        arity: i32,
    ) -> Result<()> {
        self.context
            .install_global_native_function(name, function, arity)
            .map_err(|err| anyhow::anyhow!(err))
    }

    pub fn install_global_object_native_function(
        &mut self,
        object_name: &str,
        function_name: &str,
        function: chidori_quickjs::sys::JSCFunction,
        arity: i32,
    ) -> Result<()> {
        self.context
            .install_global_object_native_function(object_name, function_name, function, arity)
            .map_err(|err| anyhow::anyhow!(err))
    }

    /// Installs the Rust-backed `chidori.log` method for the snapshot runtime.
    ///
    /// # Safety
    ///
    /// `state` must outlive this context or the context opaque pointer must be
    /// cleared before `state` is dropped.
    pub unsafe fn install_runtime_log_host(
        &mut self,
        state: &mut TypeScriptSnapshotHostState,
    ) -> Result<()> {
        unsafe {
            self.set_context_opaque((state as *mut TypeScriptSnapshotHostState).cast());
        }
        self.install_global_object_native_function("chidori", "log", Some(native_runtime_log), 2)
    }

    /// Installs the Rust-backed `chidori.checkpoint` method for the snapshot
    /// runtime.
    ///
    /// # Safety
    ///
    /// `state` must outlive this context or the context opaque pointer must be
    /// cleared before `state` is dropped.
    pub unsafe fn install_runtime_checkpoint_host(
        &mut self,
        state: &mut TypeScriptSnapshotHostState,
    ) -> Result<()> {
        unsafe {
            self.set_context_opaque((state as *mut TypeScriptSnapshotHostState).cast());
        }
        self.install_global_object_native_function(
            "chidori",
            "checkpoint",
            Some(native_runtime_checkpoint),
            2,
        )
    }

    /// Installs the Rust-backed `chidori.memory` method for the snapshot
    /// runtime.
    ///
    /// # Safety
    ///
    /// `state` must outlive this context or the context opaque pointer must be
    /// cleared before `state` is dropped.
    pub unsafe fn install_runtime_memory_host(
        &mut self,
        state: &mut TypeScriptSnapshotHostState,
    ) -> Result<()> {
        unsafe {
            self.set_context_opaque((state as *mut TypeScriptSnapshotHostState).cast());
        }
        self.install_global_object_native_function(
            "chidori",
            "memory",
            Some(native_runtime_memory),
            4,
        )?;
        self.eval_json_expression(
            "install-memory-helpers.js",
            r#"
            (() => {
                if (typeof globalThis.__chidori_install_memory_helpers === "function") {
                    return globalThis.__chidori_install_memory_helpers();
                }
                const current = globalThis.chidori && globalThis.chidori.memory;
                if (typeof current !== "function") {
                    return null;
                }
                const memoryCall = current.__chidori_call || current;
                function memory(...args) {
                    return memoryCall.call(globalThis.chidori, ...args);
                }
                memory.__chidori_call = memoryCall;
                memory.set = function set(key, value, options) {
                    return memory("set", key, value, options);
                };
                memory.get = function get(key, options) {
                    return memory("get", key, null, options);
                };
                memory.delete = function deleteKey(key, options) {
                    return memory("delete", key, null, options);
                };
                memory.clear = function clear(options) {
                    return memory("clear", null, null, options);
                };
                globalThis.chidori.memory = memory;
                return null;
            })()
            "#,
        )?;
        Ok(())
    }

    /// Installs the Rust-backed `chidori.template` method for the snapshot
    /// runtime.
    ///
    /// # Safety
    ///
    /// `state` must outlive this context or the context opaque pointer must be
    /// cleared before `state` is dropped.
    pub unsafe fn install_runtime_template_host(
        &mut self,
        state: &mut TypeScriptSnapshotHostState,
    ) -> Result<()> {
        unsafe {
            self.set_context_opaque((state as *mut TypeScriptSnapshotHostState).cast());
        }
        self.install_global_object_native_function(
            "chidori",
            "template",
            Some(native_runtime_template),
            3,
        )
    }

    /// Installs the Rust-backed `chidori.execJs` method for the snapshot
    /// runtime.
    ///
    /// # Safety
    ///
    /// `state` must outlive this context or the context opaque pointer must be
    /// cleared before `state` is dropped.
    pub unsafe fn install_runtime_exec_js_host(
        &mut self,
        state: &mut TypeScriptSnapshotHostState,
    ) -> Result<()> {
        unsafe {
            self.set_context_opaque((state as *mut TypeScriptSnapshotHostState).cast());
        }
        self.install_global_object_native_function(
            "chidori",
            "execJs",
            Some(native_runtime_exec_js),
            2,
        )
    }

    /// Installs the Rust-backed `chidori.execPython` method for the snapshot
    /// runtime.
    ///
    /// # Safety
    ///
    /// `state` must outlive this context or the context opaque pointer must be
    /// cleared before `state` is dropped.
    pub unsafe fn install_runtime_exec_python_host(
        &mut self,
        state: &mut TypeScriptSnapshotHostState,
    ) -> Result<()> {
        unsafe {
            self.set_context_opaque((state as *mut TypeScriptSnapshotHostState).cast());
        }
        self.install_global_object_native_function(
            "chidori",
            "execPython",
            Some(native_runtime_exec_python),
            2,
        )
    }

    /// Installs the Rust-backed `chidori.execWasm` method for the snapshot
    /// runtime.
    ///
    /// # Safety
    ///
    /// `state` must outlive this context or the context opaque pointer must be
    /// cleared before `state` is dropped.
    pub unsafe fn install_runtime_exec_wasm_host(
        &mut self,
        state: &mut TypeScriptSnapshotHostState,
    ) -> Result<()> {
        unsafe {
            self.set_context_opaque((state as *mut TypeScriptSnapshotHostState).cast());
        }
        self.install_global_object_native_function(
            "chidori",
            "execWasm",
            Some(native_runtime_exec_wasm),
            2,
        )
    }

    /// Installs the Rust-backed `chidori.http` method for the snapshot runtime.
    ///
    /// # Safety
    ///
    /// `state` must outlive this context or the context opaque pointer must be
    /// cleared before `state` is dropped.
    pub unsafe fn install_runtime_http_host(
        &mut self,
        state: &mut TypeScriptSnapshotHostState,
    ) -> Result<()> {
        unsafe {
            self.set_context_opaque((state as *mut TypeScriptSnapshotHostState).cast());
        }
        self.install_global_object_native_function("chidori", "http", Some(native_runtime_http), 2)
    }

    /// Installs the Rust-backed `chidori.input` method for the snapshot runtime.
    ///
    /// # Safety
    ///
    /// `state` must outlive this context or the context opaque pointer must be
    /// cleared before `state` is dropped.
    pub unsafe fn install_runtime_input_host(
        &mut self,
        state: &mut TypeScriptSnapshotHostState,
    ) -> Result<()> {
        unsafe {
            self.set_context_opaque((state as *mut TypeScriptSnapshotHostState).cast());
        }
        self.install_global_object_native_function(
            "chidori",
            "input",
            Some(native_runtime_input),
            2,
        )
    }

    /// Installs the Rust-backed plain-text `chidori.prompt` method for the
    /// snapshot runtime.
    ///
    /// # Safety
    ///
    /// `state` must outlive this context or the context opaque pointer must be
    /// cleared before `state` is dropped.
    pub unsafe fn install_runtime_prompt_host(
        &mut self,
        state: &mut TypeScriptSnapshotHostState,
    ) -> Result<()> {
        unsafe {
            self.set_context_opaque((state as *mut TypeScriptSnapshotHostState).cast());
        }
        self.install_global_object_native_function(
            "chidori",
            "prompt",
            Some(native_runtime_prompt),
            2,
        )
    }

    /// Installs the Rust-backed `chidori.tool` method for the snapshot runtime.
    ///
    /// # Safety
    ///
    /// `state` must outlive this context or the context opaque pointer must be
    /// cleared before `state` is dropped.
    pub unsafe fn install_runtime_tool_host(
        &mut self,
        state: &mut TypeScriptSnapshotHostState,
    ) -> Result<()> {
        unsafe {
            self.set_context_opaque((state as *mut TypeScriptSnapshotHostState).cast());
        }
        self.install_global_object_native_function("chidori", "tool", Some(native_runtime_tool), 2)
    }

    /// Installs the Rust-backed `chidori.callAgent` method for the snapshot
    /// runtime.
    ///
    /// # Safety
    ///
    /// `state` must outlive this context or the context opaque pointer must be
    /// cleared before `state` is dropped.
    pub unsafe fn install_runtime_call_agent_host(
        &mut self,
        state: &mut TypeScriptSnapshotHostState,
    ) -> Result<()> {
        unsafe {
            self.set_context_opaque((state as *mut TypeScriptSnapshotHostState).cast());
        }
        self.install_global_object_native_function(
            "chidori",
            "callAgent",
            Some(native_runtime_call_agent),
            2,
        )
    }

    /// Installs the complete Rust-backed `chidori` object currently available
    /// on the snapshot runtime.
    ///
    /// # Safety
    ///
    /// `state` must outlive this context or the context opaque pointer must be
    /// cleared before `state` is dropped.
    pub unsafe fn install_runtime_host(
        &mut self,
        state: &mut TypeScriptSnapshotHostState,
    ) -> Result<()> {
        unsafe {
            self.set_context_opaque((state as *mut TypeScriptSnapshotHostState).cast());
        }
        self.install_global_object_native_function("chidori", "log", Some(native_runtime_log), 2)?;
        self.install_global_object_native_function(
            "chidori",
            "checkpoint",
            Some(native_runtime_checkpoint),
            2,
        )?;
        self.install_global_object_native_function(
            "chidori",
            "memory",
            Some(native_runtime_memory),
            4,
        )?;
        self.install_global_object_native_function(
            "chidori",
            "template",
            Some(native_runtime_template),
            3,
        )?;
        self.install_global_object_native_function(
            "chidori",
            "execJs",
            Some(native_runtime_exec_js),
            2,
        )?;
        self.install_global_object_native_function(
            "chidori",
            "execPython",
            Some(native_runtime_exec_python),
            2,
        )?;
        self.install_global_object_native_function(
            "chidori",
            "execWasm",
            Some(native_runtime_exec_wasm),
            2,
        )?;
        self.install_global_object_native_function(
            "chidori",
            "http",
            Some(native_runtime_http),
            2,
        )?;
        self.install_global_object_native_function(
            "chidori",
            "input",
            Some(native_runtime_input),
            2,
        )?;
        self.install_global_object_native_function(
            "chidori",
            "prompt",
            Some(native_runtime_prompt),
            2,
        )?;
        self.install_global_object_native_function(
            "chidori",
            "tool",
            Some(native_runtime_tool),
            2,
        )?;
        self.install_global_object_native_function(
            "chidori",
            "callAgent",
            Some(native_runtime_call_agent),
            2,
        )?;
        self.install_global_native_function(
            "__chidori_workspace_list",
            Some(native_runtime_workspace_list),
            1,
        )?;
        self.install_global_native_function(
            "__chidori_workspace_read",
            Some(native_runtime_workspace_read),
            1,
        )?;
        self.install_global_native_function(
            "__chidori_workspace_write",
            Some(native_runtime_workspace_write),
            3,
        )?;
        self.install_global_native_function(
            "__chidori_workspace_delete",
            Some(native_runtime_workspace_delete),
            2,
        )?;
        self.install_global_native_function(
            "__chidori_workspace_manifest",
            Some(native_runtime_workspace_manifest),
            0,
        )?;
        self.install_global_native_function("__chidori_fs_read", Some(native_runtime_fs_read), 1)?;
        self.install_global_native_function("__chidori_fs_write", Some(native_runtime_fs_write), 2)?;
        self.install_global_native_function(
            "__chidori_fs_append",
            Some(native_runtime_fs_append),
            2,
        )?;
        self.install_global_native_function(
            "__chidori_fs_readdir",
            Some(native_runtime_fs_readdir),
            1,
        )?;
        self.install_global_native_function("__chidori_fs_mkdir", Some(native_runtime_fs_mkdir), 2)?;
        self.install_global_native_function("__chidori_fs_rm", Some(native_runtime_fs_rm), 3)?;
        self.install_global_native_function("__chidori_fs_stat", Some(native_runtime_fs_stat), 1)?;
        self.install_global_native_function(
            "__chidori_fs_exists",
            Some(native_runtime_fs_exists),
            1,
        )?;
        self.install_global_native_function(
            "__chidori_fs_rename",
            Some(native_runtime_fs_rename),
            2,
        )?;
        self.install_global_native_function(
            "__chidori_crypto_random",
            Some(native_runtime_crypto_random),
            1,
        )?;
        self.install_global_native_function(
            "__chidori_crypto_hash",
            Some(native_runtime_crypto_hash),
            2,
        )?;
        self.install_global_native_function(
            "__chidori_crypto_hmac",
            Some(native_runtime_crypto_hmac),
            3,
        )?;
        self.install_global_native_function(
            "__chidori_note_capability",
            Some(native_runtime_note_capability),
            1,
        )?;
        self.install_js_helpers()
    }

    pub fn install_js_helpers(&mut self) -> Result<()> {
        self.eval_json_expression("install-chidori-js-helpers.js", CHIDORI_JS_HELPERS_SCRIPT)?;
        Ok(())
    }

    pub fn new_host_promise(
        &mut self,
        id: chidori_quickjs::HostPromiseId,
    ) -> Result<chidori_quickjs::JsValue> {
        self.context
            .new_host_promise(id)
            .map_err(|err| anyhow::anyhow!(err))
    }

    pub fn set_global_js_value(
        &mut self,
        property: &str,
        value: chidori_quickjs::JsValue,
    ) -> Result<()> {
        let property_name = property.to_string();
        let property = CString::new(property)?;
        unsafe {
            let ctx = self.context.raw_context();
            let global = chidori_quickjs::sys::JS_GetGlobalObject(ctx);
            let status = chidori_quickjs::sys::JS_SetPropertyStr(
                ctx,
                global,
                property.as_ptr(),
                chidori_quickjs::sys::JS_DupValue(ctx, value.raw()),
            );
            chidori_quickjs::sys::JS_FreeValue(ctx, global);
            if status < 0 {
                anyhow::bail!("failed to set global {}", property_name);
            }
        }
        Ok(())
    }

    pub fn install_host_promise_method(
        &mut self,
        method: &str,
        id: chidori_quickjs::HostPromiseId,
    ) -> Result<()> {
        self.install_host_promise_method_sequence(method, &[id])
    }

    pub fn install_host_promise_method_sequence(
        &mut self,
        method: &str,
        ids: &[chidori_quickjs::HostPromiseId],
    ) -> Result<()> {
        let mut entries = Vec::with_capacity(ids.len());
        for id in ids {
            let promise = self.new_host_promise(*id)?;
            let promise_property = format!("__chidori_host_promise_{}", id.0);
            self.set_global_js_value(&promise_property, promise)?;
            entries.push(serde_json::json!({
                "id": id.0,
                "promiseProperty": promise_property,
            }));
        }
        let method_json = serde_json::to_string(method)?;
        let entries_json = serde_json::to_string(&entries)?;
        self.eval_json_expression(
            "install-chidori-host-method.js",
            &format!(
                r#"
                (globalThis.__chidori_host_calls = globalThis.__chidori_host_calls || [],
                 globalThis.__chidori_host_method_queues = globalThis.__chidori_host_method_queues || {{}},
                 globalThis.__chidori_host_method_queues[{method}] = globalThis.__chidori_host_method_queues[{method}] || [],
                 globalThis.__chidori_host_method_queues[{method}].push(...{entries}.map(entry => ({{
                    id: entry.id,
                    promise: globalThis[entry.promiseProperty]
                 }}))),
                 globalThis.chidori = Object.assign(globalThis.chidori || {{}}, {{
                    [{method}](...args) {{
                        const queue = globalThis.__chidori_host_method_queues[{method}] || [];
                        if (queue.length === 0) {{
                            throw new Error(`No snapshot host promise installed for chidori.${{String({method})}}`);
                        }}
                        const entry = queue.shift();
                        globalThis.__chidori_active_host_operation_id = entry.id;
                        globalThis.__chidori_host_calls.push({{
                            id: entry.id,
                            method: {method},
                            args
                        }});
                        return entry.promise;
                    }}
                 }}),
                 null)
                "#,
                method = method_json,
                entries = entries_json
            ),
        )?;
        Ok(())
    }

    pub fn install_input_host_promise(&mut self, id: chidori_quickjs::HostPromiseId) -> Result<()> {
        self.install_host_promise_method("input", id)
    }

    pub fn install_host_promise_operations(
        &mut self,
        operations: &[PendingHostOperation],
    ) -> Result<()> {
        let mut methods: Vec<(&'static str, Vec<chidori_quickjs::HostPromiseId>)> = Vec::new();
        for operation in operations {
            let method = snapshot_method_for_host_operation(operation)?;
            let id = chidori_quickjs::HostPromiseId(operation.id.0);
            if let Some((_, ids)) = methods
                .iter_mut()
                .find(|(existing_method, _)| *existing_method == method)
            {
                ids.push(id);
            } else {
                methods.push((method, vec![id]));
            }
        }

        for (method, ids) in methods {
            self.install_host_promise_method_sequence(method, &ids)?;
        }
        Ok(())
    }

    pub fn install_pending_host_promise_records(
        &mut self,
        records: &[HostPromiseRecord],
    ) -> Result<()> {
        let operations = records
            .iter()
            .filter_map(|record| match record.state {
                HostPromiseState::Pending => Some(record.operation.clone()),
                HostPromiseState::Resolved { .. } | HostPromiseState::Rejected { .. } => None,
            })
            .collect::<Vec<_>>();
        self.install_host_promise_operations(&operations)
    }

    pub fn install_host_promise_records(&mut self, records: &[HostPromiseRecord]) -> Result<()> {
        let mut records = records.to_vec();
        records.sort_by_key(|record| record.operation.seq);
        let operations = records
            .iter()
            .map(|record| record.operation.clone())
            .collect::<Vec<_>>();
        self.install_host_promise_operations(&operations)?;
        for record in records {
            let id = chidori_quickjs::HostPromiseId(record.operation.id.0);
            match record.state {
                HostPromiseState::Pending => {}
                HostPromiseState::Resolved { value, .. } => {
                    self.resolve_host_promise(id, value)?;
                }
                HostPromiseState::Rejected { error, .. } => {
                    self.reject_host_promise(id, error)?;
                }
            }
        }
        Ok(())
    }

    pub fn install_future_host_promises(
        &mut self,
        records: &[HostPromiseRecord],
        methods: &[(&str, u64)],
    ) -> Result<()> {
        let mut next_id = records
            .iter()
            .map(|record| record.operation.id.0)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        for (method, count) in methods {
            if *count == 0 {
                continue;
            }
            let ids = (next_id..next_id.saturating_add(*count))
                .map(chidori_quickjs::HostPromiseId)
                .collect::<Vec<_>>();
            next_id = next_id.saturating_add(*count);
            self.install_host_promise_method_sequence(method, &ids)?;
        }
        Ok(())
    }

    pub fn resolve_host_promise(
        &mut self,
        id: chidori_quickjs::HostPromiseId,
        value: serde_json::Value,
    ) -> Result<()> {
        self.context
            .resolve_host_promise(id, value)
            .map_err(|err| anyhow::anyhow!(err))
    }

    pub fn resolve_host_promise_and_run(
        &mut self,
        id: chidori_quickjs::HostPromiseId,
        value: serde_json::Value,
    ) -> Result<chidori_quickjs::RunState> {
        self.context
            .resolve_host_promise_and_run(id, value)
            .map_err(|err| anyhow::anyhow!(err))
    }

    pub fn reject_host_promise(
        &mut self,
        id: chidori_quickjs::HostPromiseId,
        error: String,
    ) -> Result<()> {
        self.context
            .reject_host_promise(id, error)
            .map_err(|err| anyhow::anyhow!(err))
    }

    pub fn reject_host_promise_and_run(
        &mut self,
        id: chidori_quickjs::HostPromiseId,
        error: String,
    ) -> Result<chidori_quickjs::RunState> {
        self.context
            .reject_host_promise_and_run(id, error)
            .map_err(|err| anyhow::anyhow!(err))
    }

    pub fn eval_json_expression(
        &mut self,
        name: &str,
        expression: &str,
    ) -> Result<serde_json::Value> {
        self.context
            .eval_json_expression(name, expression)
            .map_err(|err| anyhow::anyhow!(err))
    }

    pub fn run_jobs_until_blocked(&mut self) -> Result<chidori_quickjs::RunState> {
        self.context
            .run_jobs_until_blocked()
            .map_err(|err| anyhow::anyhow!(err))
    }

    pub fn snapshot_runtime(&mut self) -> Result<chidori_quickjs::RuntimeSnapshot> {
        self.context
            .snapshot_runtime()
            .map_err(|err| anyhow::anyhow!(err))
    }

    pub fn raw_context(&self) -> *mut chidori_quickjs::sys::JSContext {
        self.context.raw_context()
    }
}

fn snapshot_method_for_host_operation(operation: &PendingHostOperation) -> Result<&'static str> {
    match &operation.kind {
        PendingHostOperationKind::Prompt => Ok("prompt"),
        PendingHostOperationKind::Input => Ok("input"),
        PendingHostOperationKind::Tool => Ok("tool"),
        PendingHostOperationKind::CallAgent => Ok("callAgent"),
        PendingHostOperationKind::Http => Ok("http"),
        PendingHostOperationKind::Template => Ok("template"),
        PendingHostOperationKind::Memory => Ok("memory"),
        PendingHostOperationKind::Checkpoint => Ok("checkpoint"),
        PendingHostOperationKind::Log => Ok("log"),
        PendingHostOperationKind::PolicyApproval => {
            anyhow::bail!("policy approval is resolved outside the snapshot chidori host object")
        }
        PendingHostOperationKind::Timer => {
            anyhow::bail!("virtual timers fire inside the job drain, not as snapshot host calls")
        }
        PendingHostOperationKind::Sandbox => match operation.function.as_deref() {
            Some("exec_js") => Ok("execJs"),
            Some("exec_python") => Ok("execPython"),
            Some("exec") => Ok("execWasm"),
            Some(function) => anyhow::bail!(
                "sandbox host operation function `{}` cannot be mapped to a snapshot chidori method",
                function
            ),
            None => anyhow::bail!(
                "sandbox host operations need the concrete exec method before snapshot restore"
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::providers::{ContentBlock, LlmProvider, LlmResponse};
    use crate::runtime::snapshot::{
        DatePolicy, MapSetSnapshotPolicy, RandomPolicy, SnapshotBlobKind, TypeScriptImportPolicy,
    };

    use super::*;
    use crate::runtime::snapshot::{CryptoPolicy, FsPolicy, TimerPolicy};

    #[test]
    fn export_default_assigns_namespace_default() {
        let out = export_statement("export default fs;", "__chidori_module")
            .unwrap()
            .unwrap();
        assert_eq!(out, "__chidori_module.default = fs;\n");
        let obj = export_statement("export default { Buffer };", "ns")
            .unwrap()
            .unwrap();
        assert_eq!(obj, "ns.default = { Buffer };\n");
    }

    #[test]
    fn default_import_binds_namespace_default() {
        let parsed = parse_import_line("import fs from \"node:fs\";").unwrap();
        let stmt = parsed
            .binding_statement(Path::new("a.ts"), 1, "NS", "const")
            .unwrap();
        assert_eq!(stmt, "const fs = NS.default;\n");
    }

    #[test]
    fn default_plus_named_import_binds_both() {
        let parsed = parse_import_line("import fs, { readFileSync } from \"node:fs\";").unwrap();
        let stmt = parsed
            .binding_statement(Path::new("a.ts"), 1, "NS", "const")
            .unwrap();
        assert_eq!(
            stmt,
            "const fs = NS.default;\nconst { readFileSync } = NS;\n"
        );
    }

    struct SnapshotPromptProvider;

    #[async_trait::async_trait]
    impl LlmProvider for SnapshotPromptProvider {
        fn supports_model(&self, _model: &str) -> bool {
            true
        }

        async fn send(&self, request: &LlmRequest) -> Result<LlmResponse> {
            let text = match request
                .messages
                .first()
                .and_then(|message| message.content.first())
            {
                Some(ContentBlock::Text { text }) => format!("snapshot: {text}"),
                _ => "snapshot".to_string(),
            };
            Ok(LlmResponse {
                content: text.clone(),
                blocks: vec![ContentBlock::Text { text }],
                tool_calls: Vec::new(),
                stop_reason: "end_turn".to_string(),
                input_tokens: 2,
                output_tokens: 3,
            })
        }
    }

    struct SnapshotToolUseProvider {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmProvider for SnapshotToolUseProvider {
        fn supports_model(&self, _model: &str) -> bool {
            true
        }

        async fn send(&self, request: &LlmRequest) -> Result<LlmResponse> {
            assert_eq!(request.tools.len(), 1);
            assert_eq!(request.tools[0].name, "echo");
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(LlmResponse {
                    content: String::new(),
                    blocks: vec![ContentBlock::ToolUse {
                        id: "toolu_1".to_string(),
                        name: "echo".to_string(),
                        input: serde_json::json!({ "value": 41 }),
                    }],
                    tool_calls: vec![crate::providers::ToolCall {
                        id: "toolu_1".to_string(),
                        name: "echo".to_string(),
                        input: serde_json::json!({ "value": 41 }),
                    }],
                    stop_reason: "tool_use".to_string(),
                    input_tokens: 4,
                    output_tokens: 5,
                })
            } else {
                assert!(request.messages.iter().any(|message| matches!(
                    message.content.first(),
                    Some(ContentBlock::ToolResult {
                        is_error: false,
                        ..
                    })
                )));
                Ok(LlmResponse {
                    content: "final answer".to_string(),
                    blocks: vec![ContentBlock::Text {
                        text: "final answer".to_string(),
                    }],
                    tool_calls: Vec::new(),
                    stop_reason: "end_turn".to_string(),
                    input_tokens: 6,
                    output_tokens: 7,
                })
            }
        }
    }

    unsafe extern "C" fn native_record_log(
        ctx: *mut chidori_quickjs::sys::JSContext,
        _this_val: chidori_quickjs::sys::JSValue,
        argc: std::ffi::c_int,
        argv: *mut chidori_quickjs::sys::JSValue,
    ) -> chidori_quickjs::sys::JSValue {
        let Some(calls) = (unsafe { chidori_quickjs::context_opaque_mut::<Vec<String>>(ctx) })
        else {
            return unsafe { chidori_quickjs::throw_string(ctx, "missing native callback state") };
        };
        match unsafe { chidori_quickjs::callback_arg_to_string(ctx, argc, argv, 0) } {
            Ok(message) => {
                calls.push(message);
                unsafe { chidori_quickjs::json_to_js_value(ctx, serde_json::Value::Null) }
                    .unwrap_or_else(|err| unsafe {
                        chidori_quickjs::throw_string(ctx, &err.to_string())
                    })
            }
            Err(err) => unsafe { chidori_quickjs::throw_string(ctx, &err.to_string()) },
        }
    }

    #[test]
    fn snapshot_runtime_restores_transpiled_async_agent_state() {
        let id = chidori_quickjs::HostPromiseId(401);
        let path = Path::new("agent.ts");
        let source = r#"
            export async function agent(input) {
                globalThis.__events.push("before");
                const value = await globalThis.__host_promise;
                globalThis.__events.push(value.answer);
                return { answer: value.answer + input.delta };
            }
        "#;

        let snapshot = {
            let runtime =
                TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                    .unwrap();
            let mut context = runtime.eval_agent_source(path, source).unwrap();
            let promise = context.new_host_promise(id).unwrap().raw();
            let property = CString::new("__host_promise").unwrap();
            unsafe {
                let global = chidori_quickjs::sys::JS_GetGlobalObject(context.raw_context());
                assert!(
                    chidori_quickjs::sys::JS_SetPropertyStr(
                        context.raw_context(),
                        global,
                        property.as_ptr(),
                        chidori_quickjs::sys::JS_DupValue(context.raw_context(), promise),
                    ) >= 0
                );
                chidori_quickjs::sys::JS_FreeValue(context.raw_context(), global);
            }
            context
                .eval_json_expression("setup.js", "(globalThis.__events = [], null)")
                .unwrap();
            context
                .eval_json_expression(
                    "call.js",
                    "(globalThis.__result = globalThis.__chidori_exports.agent({ delta: 1 }), null)",
                )
                .unwrap();
            context
                .snapshot_roots(&[
                    "__chidori_exports",
                    "__host_promise",
                    "__events",
                    "__result",
                ])
                .unwrap()
        };

        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let mut context = runtime.restore_context(&snapshot).unwrap();
        context
            .resolve_host_promise(id, serde_json::json!({ "answer": 41 }))
            .unwrap();

        assert_eq!(
            context
                .eval_json_expression(
                    "check.js",
                    r#"
                    (globalThis.__result.then(value => {
                        globalThis.__events.push(value.answer);
                    }),
                    globalThis.__events)
                    "#,
                )
                .unwrap(),
            serde_json::json!(["before", 41])
        );
        context.run_jobs_until_blocked().unwrap();
        assert_eq!(
            context
                .eval_json_expression("check-drained.js", "globalThis.__events")
                .unwrap(),
            serde_json::json!(["before", 41, 42])
        );
    }

    #[test]
    fn snapshot_runtime_defines_empty_process_env() {
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("process-env")).unwrap();
        let mut context = runtime
            .eval_agent_source(
                Path::new("agent.ts"),
                r#"
                export async function agent() {
                    return { token: process.env.BRAVE_SEARCH_API_KEY ?? "" };
                }
                "#,
            )
            .unwrap();

        let output = context.call_agent(serde_json::json!({})).unwrap();
        assert_eq!(
            output,
            chidori_quickjs::RunState::Completed(serde_json::json!({ "token": "" }))
        );
    }

    #[test]
    fn snapshot_runtime_installs_native_chidori_method_for_typescript_agent() {
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let mut calls = Vec::new();
        let mut context = runtime
            .eval_agent_source(
                Path::new("agent.ts"),
                r#"
                export async function agent(input, chidori) {
                    chidori.log("hello " + input.name);
                    return { ok: true };
                }
                "#,
            )
            .unwrap();
        unsafe {
            context.set_context_opaque((&mut calls as *mut Vec<String>).cast());
        }
        context
            .install_global_object_native_function("chidori", "log", Some(native_record_log), 1)
            .unwrap();

        assert_eq!(
            context
                .call_agent(serde_json::json!({ "name": "TS" }))
                .unwrap(),
            chidori_quickjs::RunState::Completed(serde_json::json!({ "ok": true }))
        );
        assert_eq!(calls, vec!["hello TS".to_string()]);

        unsafe {
            context.set_context_opaque(std::ptr::null_mut());
        }
    }

    #[test]
    fn snapshot_runtime_native_log_method_records_runtime_context_call() {
        let runtime_ctx = RuntimeContext::new();
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let mut host_state = TypeScriptSnapshotHostState::new(runtime_ctx.clone());
        let mut context = runtime
            .eval_agent_source(
                Path::new("agent.ts"),
                r#"
                export async function agent(input, chidori) {
                    await chidori.log("hello " + input.name, { source: "snapshot" });
                    return { ok: true };
                }
                "#,
            )
            .unwrap();
        unsafe {
            context.install_runtime_log_host(&mut host_state).unwrap();
        }

        assert_eq!(
            context
                .call_agent(serde_json::json!({ "name": "runtime" }))
                .unwrap(),
            chidori_quickjs::RunState::Completed(serde_json::json!({ "ok": true }))
        );
        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "log");
        assert_eq!(
            records[0].args,
            serde_json::json!({
                "message": "hello runtime",
                "fields": { "source": "snapshot" }
            })
        );

        unsafe {
            context.set_context_opaque(std::ptr::null_mut());
        }
    }

    #[test]
    fn snapshot_runtime_native_checkpoint_method_records_runtime_context_call() {
        let runtime_ctx = RuntimeContext::new();
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let mut host_state = TypeScriptSnapshotHostState::new(runtime_ctx.clone());
        let mut context = runtime
            .eval_agent_source(
                Path::new("agent.ts"),
                r#"
                export async function agent(input, chidori) {
                    await chidori.checkpoint("draft", { count: input.count });
                    return { ok: true };
                }
                "#,
            )
            .unwrap();
        unsafe {
            context
                .install_runtime_checkpoint_host(&mut host_state)
                .unwrap();
        }

        assert_eq!(
            context
                .call_agent(serde_json::json!({ "count": 2 }))
                .unwrap(),
            chidori_quickjs::RunState::Completed(serde_json::json!({ "ok": true }))
        );
        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "checkpoint");
        assert_eq!(
            records[0].args,
            serde_json::json!({
                "label": "draft",
                "data": { "count": 2 }
            })
        );

        unsafe {
            context.set_context_opaque(std::ptr::null_mut());
        }
    }

    #[test]
    fn snapshot_runtime_native_memory_method_records_runtime_context_calls() {
        let runtime_ctx = RuntimeContext::new();
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let namespace = format!("snapshot-memory-{}", uuid::Uuid::new_v4());
        let mut host_state = TypeScriptSnapshotHostState::new(runtime_ctx.clone());
        let mut context = runtime
            .eval_agent_source(
                Path::new("agent.ts"),
                r#"
                export async function agent(input, chidori) {
                    await chidori.memory.set(
                        "item",
                        { count: input.count },
                        { namespace: input.namespace }
                    );
                    const value = await chidori.memory.get(
                        "item",
                        { namespace: input.namespace }
                    );
                    await chidori.memory.clear(
                        { namespace: input.namespace }
                    );
                    return { value };
                }
                "#,
            )
            .unwrap();
        unsafe {
            context
                .install_runtime_memory_host(&mut host_state)
                .unwrap();
        }

        assert_eq!(
            context
                .call_agent(serde_json::json!({
                    "count": 7,
                    "namespace": namespace
                }))
                .unwrap(),
            chidori_quickjs::RunState::Completed(serde_json::json!({
                "value": { "count": 7 }
            }))
        );
        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].function, "memory");
        assert_eq!(records[1].function, "memory");
        assert_eq!(records[2].function, "memory");
        assert_eq!(records[0].args["action"], serde_json::json!("set"));
        assert_eq!(records[1].args["action"], serde_json::json!("get"));
        assert_eq!(records[2].args["action"], serde_json::json!("clear"));
        assert_eq!(records[0].args["namespace"], serde_json::json!(namespace));

        unsafe {
            context.set_context_opaque(std::ptr::null_mut());
        }
    }

    #[test]
    fn snapshot_runtime_native_template_method_records_runtime_context_call() {
        let runtime_ctx = RuntimeContext::new();
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let template_engine = std::sync::Arc::new(TemplateEngine::new("."));
        let mut host_state =
            TypeScriptSnapshotHostState::with_template_engine(runtime_ctx.clone(), template_engine);
        let mut context = runtime
            .eval_agent_source(
                Path::new("agent.ts"),
                r#"
                export async function agent(input, chidori) {
                    const text = await chidori.template(
                        "Hello {{ name }}",
                        { name: input.name }
                    );
                    return { text };
                }
                "#,
            )
            .unwrap();
        unsafe {
            context
                .install_runtime_template_host(&mut host_state)
                .unwrap();
        }

        assert_eq!(
            context
                .call_agent(serde_json::json!({ "name": "Snapshot" }))
                .unwrap(),
            chidori_quickjs::RunState::Completed(serde_json::json!({
                "text": "Hello Snapshot"
            }))
        );
        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "template");
        assert_eq!(
            records[0].args,
            serde_json::json!({
                "template": "Hello {{ name }}",
                "vars": { "name": "Snapshot" }
            })
        );

        unsafe {
            context.set_context_opaque(std::ptr::null_mut());
        }
    }

    #[test]
    fn snapshot_runtime_native_exec_js_method_records_runtime_context_call() {
        let runtime_ctx = RuntimeContext::new();
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let mut host_state = TypeScriptSnapshotHostState::new(runtime_ctx.clone());
        let mut context = runtime
            .eval_agent_source(
                Path::new("agent.ts"),
                r#"
                export async function agent(_input, chidori) {
                    const output = await chidori.execJs("1 + 2", { fuel: 200000000 });
                    return { output };
                }
                "#,
            )
            .unwrap();
        unsafe {
            context
                .install_runtime_exec_js_host(&mut host_state)
                .unwrap();
        }

        assert_eq!(
            context.call_agent(serde_json::json!({})).unwrap(),
            chidori_quickjs::RunState::Completed(serde_json::json!({ "output": "3" }))
        );
        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "exec_js");
        assert_eq!(
            records[0].args,
            serde_json::json!({
                "source": "1 + 2",
                "fuel": 200000000
            })
        );

        unsafe {
            context.set_context_opaque(std::ptr::null_mut());
        }
    }

    #[test]
    fn snapshot_runtime_native_exec_python_method_records_runtime_context_call() {
        let runtime_ctx = RuntimeContext::new();
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let mut host_state = TypeScriptSnapshotHostState::new(runtime_ctx.clone());
        let mut context = runtime
            .eval_agent_source(
                Path::new("agent.ts"),
                r#"
                export async function agent(_input, chidori) {
                    const output = await chidori.execPython(
                        "result = 2 + 3",
                        { fuel: 200000000 }
                    );
                    return { output };
                }
                "#,
            )
            .unwrap();
        unsafe {
            context
                .install_runtime_exec_python_host(&mut host_state)
                .unwrap();
        }

        assert_eq!(
            context.call_agent(serde_json::json!({})).unwrap(),
            chidori_quickjs::RunState::Completed(serde_json::json!({ "output": "5" }))
        );
        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "exec_python");
        assert_eq!(
            records[0].args,
            serde_json::json!({
                "source": "result = 2 + 3",
                "fuel": 200000000
            })
        );

        unsafe {
            context.set_context_opaque(std::ptr::null_mut());
        }
    }

    #[test]
    fn snapshot_runtime_native_exec_wasm_method_records_runtime_context_call() {
        let runtime_ctx = RuntimeContext::new();
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let mut host_state = TypeScriptSnapshotHostState::new(runtime_ctx.clone());
        let mut context = runtime
            .eval_agent_source(
                Path::new("agent.ts"),
                r#"
                export async function agent(_input, chidori) {
                    const output = await chidori.execWasm(`
                        (module
                            (func $add (export "add") (param i32 i32) (result i32)
                                local.get 0
                                local.get 1
                                i32.add)
                        )
                    `, { function: "add", args: [2, 3], fuel: 1000000, memoryPages: 1 });
                    return { value: output.returns[0] };
                }
                "#,
            )
            .unwrap();
        unsafe {
            context
                .install_runtime_exec_wasm_host(&mut host_state)
                .unwrap();
        }

        assert_eq!(
            context.call_agent(serde_json::json!({})).unwrap(),
            chidori_quickjs::RunState::Completed(serde_json::json!({ "value": 5 }))
        );
        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "exec");
        assert_eq!(records[0].args["function"], serde_json::json!("add"));
        assert_eq!(records[0].args["args"], serde_json::json!([2, 3]));
        assert_eq!(records[0].args["fuel"], serde_json::json!(1000000));
        assert_eq!(records[0].args["memory_pages"], serde_json::json!(1));
        assert_eq!(records[0].result["returns"], serde_json::json!([5]));

        unsafe {
            context.set_context_opaque(std::ptr::null_mut());
        }
    }

    #[test]
    fn snapshot_runtime_native_http_method_records_policy_denial() {
        let runtime_ctx = RuntimeContext::new();
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let policy = Arc::new(PolicyConfig {
            rules: vec![crate::policy::PolicyRule {
                target: "http".to_string(),
                decision: Decision::NeverAllow,
                match_args: None,
                reason: Some("snapshot deny".to_string()),
            }],
            default: Decision::AlwaysAllow,
        });
        let tokio_rt = Arc::new(tokio::runtime::Runtime::new().unwrap());
        let mut host_state = TypeScriptSnapshotHostState::with_http(
            runtime_ctx.clone(),
            tokio_rt,
            policy,
            Arc::new(StdMutex::new(PolicyCache::default())),
        );
        let mut context = runtime
            .eval_agent_source(
                Path::new("agent.ts"),
                r#"
                export async function agent(_input, chidori) {
                    await chidori.http("https://example.invalid");
                    return { ok: true };
                }
                "#,
            )
            .unwrap();
        unsafe {
            context.install_runtime_http_host(&mut host_state).unwrap();
        }

        let err = context.call_agent(serde_json::json!({})).unwrap_err();
        assert!(err
            .to_string()
            .contains("policy: `http` denied (snapshot deny)"));
        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "http");
        assert_eq!(records[0].result, serde_json::Value::Null);
        assert_eq!(
            records[0].error.as_deref(),
            Some("policy: `http` denied (snapshot deny)")
        );

        unsafe {
            context.set_context_opaque(std::ptr::null_mut());
        }
    }

    #[test]
    fn snapshot_runtime_native_http_method_sets_pending_policy_approval() {
        let runtime_ctx = RuntimeContext::new();
        runtime_ctx.set_input_mode(InputMode::Pause);
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let policy = Arc::new(PolicyConfig {
            rules: vec![crate::policy::PolicyRule {
                target: "http".to_string(),
                decision: Decision::AskBefore,
                match_args: None,
                reason: Some("snapshot approval".to_string()),
            }],
            default: Decision::AlwaysAllow,
        });
        let tokio_rt = Arc::new(tokio::runtime::Runtime::new().unwrap());
        let mut host_state = TypeScriptSnapshotHostState::with_http(
            runtime_ctx.clone(),
            tokio_rt,
            policy,
            Arc::new(StdMutex::new(PolicyCache::default())),
        );
        let mut context = runtime
            .eval_agent_source(
                Path::new("agent.ts"),
                r#"
                export async function agent(_input, chidori) {
                    await chidori.http("https://example.invalid", { method: "post" });
                    return { ok: true };
                }
                "#,
            )
            .unwrap();
        unsafe {
            context.install_runtime_http_host(&mut host_state).unwrap();
        }

        let err = context.call_agent(serde_json::json!({})).unwrap_err();
        assert!(err.to_string().contains(PAUSE_MARKER));
        let approval = runtime_ctx
            .take_pending_approval()
            .expect("expected pending approval");
        assert_eq!(approval.target, "http");
        assert_eq!(
            approval.args["url"],
            serde_json::json!("https://example.invalid")
        );
        assert_eq!(approval.args["method"], serde_json::json!("POST"));
        assert_eq!(approval.reason.as_deref(), Some("snapshot approval"));
        assert!(runtime_ctx.call_log().into_records().is_empty());

        unsafe {
            context.set_context_opaque(std::ptr::null_mut());
        }
    }

    /// Spin up a one-shot loopback HTTP server (on its own thread, so it
    /// doesn't contend with the host's tokio runtime) that replies to the first
    /// connection with `200 OK` and the given JSON body. Returns the URL.
    fn one_shot_json_server(body: &'static str) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/echo", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        url
    }

    fn http_host_with_policy(
        runtime_ctx: RuntimeContext,
        policy: PolicyConfig,
    ) -> TypeScriptSnapshotHostState {
        let tokio_rt = Arc::new(tokio::runtime::Runtime::new().unwrap());
        TypeScriptSnapshotHostState::with_http(
            runtime_ctx,
            tokio_rt,
            Arc::new(policy),
            Arc::new(StdMutex::new(PolicyCache::default())),
        )
    }

    #[test]
    fn snapshot_runtime_fetch_routes_through_policy_deny() {
        let runtime_ctx = RuntimeContext::new();
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let mut host_state = http_host_with_policy(
            runtime_ctx.clone(),
            PolicyConfig {
                rules: vec![crate::policy::PolicyRule {
                    target: "http".to_string(),
                    decision: Decision::NeverAllow,
                    match_args: None,
                    reason: Some("no net".to_string()),
                }],
                default: Decision::AlwaysAllow,
            },
        );
        let mut context = runtime
            .eval_agent_source(
                Path::new("agent.ts"),
                r#"
                export async function agent(_input, _chidori) {
                    await fetch("https://example.invalid");
                    return { ok: true };
                }
                "#,
            )
            .unwrap();
        unsafe {
            context.install_runtime_http_host(&mut host_state).unwrap();
        }

        let err = context.call_agent(serde_json::json!({})).unwrap_err();
        assert!(
            err.to_string().contains("policy: `http` denied (no net)"),
            "fetch must route through the http policy, got: {err}"
        );
        // The denied call is recorded against the `http` host op, proving fetch
        // shares the policy-gated path rather than reaching the network directly.
        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "http");

        unsafe {
            context.set_context_opaque(std::ptr::null_mut());
        }
    }

    #[test]
    fn snapshot_runtime_fetch_sets_pending_policy_approval() {
        let runtime_ctx = RuntimeContext::new();
        runtime_ctx.set_input_mode(InputMode::Pause);
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let mut host_state = http_host_with_policy(
            runtime_ctx.clone(),
            PolicyConfig {
                rules: vec![crate::policy::PolicyRule {
                    target: "http".to_string(),
                    decision: Decision::AskBefore,
                    match_args: None,
                    reason: Some("confirm net".to_string()),
                }],
                default: Decision::AlwaysAllow,
            },
        );
        let mut context = runtime
            .eval_agent_source(
                Path::new("agent.ts"),
                r#"
                export async function agent(_input, _chidori) {
                    await fetch("https://example.invalid/data", { method: "post" });
                    return { ok: true };
                }
                "#,
            )
            .unwrap();
        unsafe {
            context.install_runtime_http_host(&mut host_state).unwrap();
        }

        let err = context.call_agent(serde_json::json!({})).unwrap_err();
        assert!(
            err.to_string().contains(PAUSE_MARKER),
            "fetch under an AskBefore policy must pause, got: {err}"
        );
        let approval = runtime_ctx
            .take_pending_approval()
            .expect("fetch should record a pending approval");
        assert_eq!(approval.target, "http");
        assert_eq!(approval.args["method"], serde_json::json!("POST"));
        assert!(runtime_ctx.call_log().into_records().is_empty());

        unsafe {
            context.set_context_opaque(std::ptr::null_mut());
        }
    }

    #[test]
    fn snapshot_runtime_fetch_success_roundtrips_json_body() {
        let url = one_shot_json_server(r#"{"hello":"world"}"#);
        let runtime_ctx = RuntimeContext::new();
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let mut host_state = http_host_with_policy(
            runtime_ctx.clone(),
            PolicyConfig {
                rules: vec![],
                default: Decision::AlwaysAllow,
            },
        );
        let mut context = runtime
            .eval_agent_source(
                Path::new("agent.ts"),
                r#"
                export async function agent(input, _chidori) {
                    const res = await fetch(input.url);
                    const data = await res.json();
                    return { status: res.status, ok: res.ok, hello: data.hello };
                }
                "#,
            )
            .unwrap();
        unsafe {
            context.install_runtime_http_host(&mut host_state).unwrap();
        }

        let state = context
            .call_agent(serde_json::json!({ "url": url }))
            .unwrap();
        match state {
            chidori_quickjs::RunState::Completed(value) => {
                assert_eq!(value["status"], serde_json::json!(200));
                assert_eq!(value["ok"], serde_json::json!(true));
                assert_eq!(value["hello"], serde_json::json!("world"));
            }
            other => panic!("expected fetch agent to complete, got {other:?}"),
        }

        unsafe {
            context.set_context_opaque(std::ptr::null_mut());
        }
    }

    #[test]
    fn snapshot_runtime_node_http_get_routes_through_policy_deny() {
        let runtime_ctx = RuntimeContext::new();
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let mut host_state = http_host_with_policy(
            runtime_ctx.clone(),
            PolicyConfig {
                rules: vec![crate::policy::PolicyRule {
                    target: "http".to_string(),
                    decision: Decision::NeverAllow,
                    match_args: None,
                    reason: Some("no net".to_string()),
                }],
                default: Decision::AlwaysAllow,
            },
        );
        let mut context = runtime
            .eval_agent_source(
                Path::new("agent.ts"),
                r#"
                import http from "node:http";
                export async function agent(_input, _chidori) {
                    return await new Promise((resolve) => {
                        http.get("http://example.invalid/data", () => {})
                            .on("error", (err) => resolve({ error: String(err.message || err) }));
                    });
                }
                "#,
            )
            .unwrap();
        unsafe {
            context.install_runtime_http_host(&mut host_state).unwrap();
        }

        let state = context.call_agent(serde_json::json!({})).unwrap();
        match state {
            chidori_quickjs::RunState::Completed(value) => {
                // node:http surfaces the policy denial as an 'error' event, the
                // node convention — proving the request was gated before any
                // bytes left the process.
                let msg = value["error"].as_str().unwrap_or_default();
                assert!(
                    msg.contains("policy: `http` denied (no net)"),
                    "node:http denial should carry the policy reason, got: {msg}"
                );
            }
            other => panic!("expected node:http deny agent to complete, got {other:?}"),
        }
        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "http");

        unsafe {
            context.set_context_opaque(std::ptr::null_mut());
        }
    }

    #[test]
    fn snapshot_runtime_node_http_get_success_emits_body() {
        let url = one_shot_json_server(r#"{"value":42}"#);
        let runtime_ctx = RuntimeContext::new();
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let mut host_state = http_host_with_policy(
            runtime_ctx.clone(),
            PolicyConfig {
                rules: vec![],
                default: Decision::AlwaysAllow,
            },
        );
        let mut context = runtime
            .eval_agent_source(
                Path::new("agent.ts"),
                r#"
                import http from "node:http";
                export async function agent(input, _chidori) {
                    return await new Promise((resolve, reject) => {
                        http.get(input.url, (res) => {
                            let body = "";
                            res.on("data", (d) => { body += d; });
                            res.on("end", () => resolve({ status: res.statusCode, body }));
                        }).on("error", reject);
                    });
                }
                "#,
            )
            .unwrap();
        unsafe {
            context.install_runtime_http_host(&mut host_state).unwrap();
        }

        let state = context
            .call_agent(serde_json::json!({ "url": url }))
            .unwrap();
        match state {
            chidori_quickjs::RunState::Completed(value) => {
                assert_eq!(value["status"], serde_json::json!(200));
                assert_eq!(value["body"], serde_json::json!(r#"{"value":42}"#));
            }
            other => panic!("expected node:http agent to complete, got {other:?}"),
        }

        unsafe {
            context.set_context_opaque(std::ptr::null_mut());
        }
    }

    #[test]
    fn snapshot_runtime_native_input_method_sets_pending_input_pause() {
        let runtime_ctx = RuntimeContext::new();
        runtime_ctx.set_input_mode(InputMode::Pause);
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let mut host_state = TypeScriptSnapshotHostState::new(runtime_ctx.clone());
        let mut context = runtime
            .eval_agent_source(
                Path::new("agent.ts"),
                r#"
                export async function agent(_input, chidori) {
                    const answer = await chidori.input("Continue?");
                    return { answer };
                }
                "#,
            )
            .unwrap();
        unsafe {
            context.install_runtime_input_host(&mut host_state).unwrap();
        }

        let err = context.call_agent(serde_json::json!({})).unwrap_err();
        assert!(err.to_string().contains(PAUSE_MARKER));
        let pending = runtime_ctx
            .take_pending_input()
            .expect("expected pending input");
        assert_eq!(pending.seq, 1);
        assert_eq!(pending.prompt, "Continue?");
        assert!(runtime_ctx.call_log().into_records().is_empty());

        unsafe {
            context.set_context_opaque(std::ptr::null_mut());
        }
    }

    #[test]
    fn snapshot_runtime_native_prompt_method_records_plain_text_prompt() {
        let runtime_ctx = RuntimeContext::new();
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(SnapshotPromptProvider));
        let providers = Arc::new(providers);
        let tokio_rt = Arc::new(tokio::runtime::Runtime::new().unwrap());
        let mut host_state =
            TypeScriptSnapshotHostState::with_prompt(runtime_ctx.clone(), providers, tokio_rt);
        let mut context = runtime
            .eval_agent_source(
                Path::new("agent.ts"),
                r#"
                export async function agent(input, chidori) {
                    const text = await chidori.prompt("hello " + input.name, {
                        type: "progress"
                    });
                    return { text };
                }
                "#,
            )
            .unwrap();
        unsafe {
            context
                .install_runtime_prompt_host(&mut host_state)
                .unwrap();
        }

        assert_eq!(
            context
                .call_agent(serde_json::json!({ "name": "prompt" }))
                .unwrap(),
            chidori_quickjs::RunState::Completed(serde_json::json!({
                "text": "snapshot: hello prompt"
            }))
        );
        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "prompt");
        assert_eq!(records[0].args["text"], serde_json::json!("hello prompt"));
        assert_eq!(records[0].args["type"], serde_json::json!("progress"));
        assert_eq!(
            records[0]
                .token_usage
                .as_ref()
                .map(|usage| (usage.input_tokens, usage.output_tokens)),
            Some((2, 3))
        );

        unsafe {
            context.set_context_opaque(std::ptr::null_mut());
        }
    }

    #[test]
    fn snapshot_runtime_native_prompt_tool_loop_invokes_registered_tool() {
        let runtime_ctx = RuntimeContext::new();
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let dir = std::env::temp_dir().join(format!(
            "chidori-snapshot-prompt-tool-loop-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tool_path = dir.join("echo.ts");
        std::fs::write(
            &tool_path,
            r#"
            export async function run(args, chidori) {
              return { value: args.value + 1 };
            }
            "#,
        )
        .unwrap();

        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(SnapshotToolUseProvider {
            calls: AtomicUsize::new(0),
        }));
        let mut registry = ToolRegistry::new();
        registry.register(crate::tools::ToolDef {
            name: "echo".to_string(),
            description: "Echo a value".to_string(),
            params: vec![crate::tools::ToolParam {
                name: "value".to_string(),
                description: Some("Value to increment".to_string()),
                param_type: "number".to_string(),
                default: None,
                required: true,
            }],
            source_path: tool_path,
            source_fingerprint: None,
            backend: ToolBackend::TypeScript,
        });
        let tokio_rt = Arc::new(tokio::runtime::Runtime::new().unwrap());
        let mut host_state = TypeScriptSnapshotHostState::with_tools(
            runtime_ctx.clone(),
            Arc::new(providers),
            Arc::new(TemplateEngine::new(".")),
            tokio_rt,
            Arc::new(PolicyConfig::default()),
            Arc::new(StdMutex::new(PolicyCache::default())),
            RuntimePolicy::durable_default("snapshot-test"),
            Arc::new(registry),
            Arc::new(McpManager::new()),
        );
        let mut context = runtime
            .eval_agent_source(
                Path::new("agent.ts"),
                r#"
                export async function agent(_input, chidori) {
                    const text = await chidori.prompt("use the echo tool", {
                        tools: ["echo"]
                    });
                    return { text };
                }
                "#,
            )
            .unwrap();
        unsafe {
            context
                .install_runtime_prompt_host(&mut host_state)
                .unwrap();
        }

        assert_eq!(
            context.call_agent(serde_json::json!({})).unwrap(),
            chidori_quickjs::RunState::Completed(serde_json::json!({
                "text": "final answer"
            }))
        );
        let records = runtime_ctx.call_log().into_records();
        assert_eq!(
            records
                .iter()
                .map(|record| record.function.as_str())
                .collect::<Vec<_>>(),
            vec!["prompt", "tool", "prompt"]
        );
        assert_eq!(records[0].args["tools"], serde_json::json!(["echo"]));
        assert_eq!(records[0].args["turn"], serde_json::json!(0));
        assert_eq!(
            records[1].args,
            serde_json::json!({
                "name": "echo",
                "kwargs": { "value": 41 }
            })
        );
        assert_eq!(records[1].result, serde_json::json!({ "value": 42 }));
        assert_eq!(records[2].args["turn"], serde_json::json!(1));

        unsafe {
            context.set_context_opaque(std::ptr::null_mut());
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn snapshot_runtime_native_prompt_tool_loop_honors_max_turns_option() {
        let runtime_ctx = RuntimeContext::new();
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let dir = std::env::temp_dir().join(format!(
            "chidori-snapshot-prompt-tool-max-turns-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tool_path = dir.join("echo.ts");
        std::fs::write(
            &tool_path,
            r#"
            export async function run(args, chidori) {
              return { value: args.value + 1 };
            }
            "#,
        )
        .unwrap();

        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(SnapshotToolUseProvider {
            calls: AtomicUsize::new(0),
        }));
        let mut registry = ToolRegistry::new();
        registry.register(crate::tools::ToolDef {
            name: "echo".to_string(),
            description: "Echo a value".to_string(),
            params: vec![crate::tools::ToolParam {
                name: "value".to_string(),
                description: Some("Value to increment".to_string()),
                param_type: "number".to_string(),
                default: None,
                required: true,
            }],
            source_path: tool_path,
            source_fingerprint: None,
            backend: ToolBackend::TypeScript,
        });
        let tokio_rt = Arc::new(tokio::runtime::Runtime::new().unwrap());
        let mut host_state = TypeScriptSnapshotHostState::with_tools(
            runtime_ctx.clone(),
            Arc::new(providers),
            Arc::new(TemplateEngine::new(".")),
            tokio_rt,
            Arc::new(PolicyConfig::default()),
            Arc::new(StdMutex::new(PolicyCache::default())),
            RuntimePolicy::durable_default("snapshot-test"),
            Arc::new(registry),
            Arc::new(McpManager::new()),
        );
        let mut context = runtime
            .eval_agent_source(
                Path::new("agent.ts"),
                r#"
                export async function agent(_input, chidori) {
                    const text = await chidori.prompt("use the echo tool", {
                        tools: ["echo"],
                        maxTurns: 1
                    });
                    return { text };
                }
                "#,
            )
            .unwrap();
        unsafe {
            context
                .install_runtime_prompt_host(&mut host_state)
                .unwrap();
        }

        assert_eq!(
            context.call_agent(serde_json::json!({})).unwrap(),
            chidori_quickjs::RunState::Completed(serde_json::json!({
                "text": ""
            }))
        );
        let records = runtime_ctx.call_log().into_records();
        assert_eq!(
            records
                .iter()
                .map(|record| record.function.as_str())
                .collect::<Vec<_>>(),
            vec!["prompt", "tool"]
        );
        assert_eq!(records[0].args["max_turns"], serde_json::json!(1));

        unsafe {
            context.set_context_opaque(std::ptr::null_mut());
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn snapshot_runtime_native_tool_method_invokes_registered_typescript_tool() {
        let runtime_ctx = RuntimeContext::new();
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let dir = std::env::temp_dir().join(format!(
            "chidori-snapshot-ts-tool-run-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let tool_path = dir.join("echo.ts");
        std::fs::write(
            &tool_path,
            r#"
            export const tool = {
              name: "echo",
              description: "Echo a value",
              parameters: {
                type: "object",
                properties: { value: { type: "number" } },
                required: ["value"],
              },
            };

            export async function run(args, chidori) {
              return { value: args.value + 1 };
            }
            "#,
        )
        .unwrap();

        let mut registry = ToolRegistry::new();
        registry.register(crate::tools::ToolDef {
            name: "echo".to_string(),
            description: "Echo a value".to_string(),
            params: Vec::new(),
            source_path: tool_path,
            source_fingerprint: None,
            backend: ToolBackend::TypeScript,
        });
        let tools = Arc::new(registry);
        let tokio_rt = Arc::new(tokio::runtime::Runtime::new().unwrap());
        let mut host_state = TypeScriptSnapshotHostState::with_tools(
            runtime_ctx.clone(),
            Arc::new(ProviderRegistry::new()),
            Arc::new(TemplateEngine::new(".")),
            tokio_rt,
            Arc::new(PolicyConfig::default()),
            Arc::new(StdMutex::new(PolicyCache::default())),
            RuntimePolicy::durable_default("snapshot-test"),
            tools,
            Arc::new(McpManager::new()),
        );
        let mut context = runtime
            .eval_agent_source(
                Path::new("agent.ts"),
                r#"
                export async function agent(input, chidori) {
                    const result = await chidori.tool("echo", { value: input.value });
                    return result;
                }
                "#,
            )
            .unwrap();
        unsafe {
            context.install_runtime_tool_host(&mut host_state).unwrap();
        }

        assert_eq!(
            context
                .call_agent(serde_json::json!({ "value": 41 }))
                .unwrap(),
            chidori_quickjs::RunState::Completed(serde_json::json!({ "value": 42 }))
        );
        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "tool");
        assert_eq!(records[0].args["name"], serde_json::json!("echo"));
        assert_eq!(
            records[0].args["kwargs"],
            serde_json::json!({ "value": 41 })
        );
        assert_eq!(records[0].result, serde_json::json!({ "value": 42 }));

        unsafe {
            context.set_context_opaque(std::ptr::null_mut());
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn snapshot_runtime_native_call_agent_method_invokes_typescript_child_agent() {
        let runtime_ctx = RuntimeContext::new();
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let dir = std::env::temp_dir().join(format!(
            "chidori-snapshot-call-agent-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let child_path = dir.join("child.ts");
        std::fs::write(
            &child_path,
            r#"
            export async function agent(input, chidori) {
                return { value: input.value + 1 };
            }
            "#,
        )
        .unwrap();

        let tokio_rt = Arc::new(tokio::runtime::Runtime::new().unwrap());
        let mut host_state = TypeScriptSnapshotHostState::with_tools(
            runtime_ctx.clone(),
            Arc::new(ProviderRegistry::new()),
            Arc::new(TemplateEngine::new(".")),
            tokio_rt,
            Arc::new(PolicyConfig::default()),
            Arc::new(StdMutex::new(PolicyCache::default())),
            RuntimePolicy::durable_default("snapshot-test"),
            Arc::new(ToolRegistry::new()),
            Arc::new(McpManager::new()),
        );
        let mut context = runtime
            .eval_agent_source(
                Path::new("agent.ts"),
                r#"
                export async function agent(input, chidori) {
                    const result = await chidori.callAgent(input.path, {
                        value: input.value
                    });
                    return result;
                }
                "#,
            )
            .unwrap();
        unsafe {
            context
                .install_runtime_call_agent_host(&mut host_state)
                .unwrap();
        }

        assert_eq!(
            context
                .call_agent(serde_json::json!({
                    "path": child_path,
                    "value": 41
                }))
                .unwrap(),
            chidori_quickjs::RunState::Completed(serde_json::json!({ "value": 42 }))
        );
        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "call_agent");
        assert_eq!(records[0].args["path"], serde_json::json!(child_path));
        assert_eq!(records[0].args["input"], serde_json::json!({ "value": 41 }));
        assert_eq!(records[0].result, serde_json::json!({ "value": 42 }));

        unsafe {
            context.set_context_opaque(std::ptr::null_mut());
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn snapshot_runtime_installs_chidori_js_helpers() {
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let mut context = runtime
            .eval_agent_source(
                Path::new("agent.ts"),
                r#"
                export async function agent(_input, chidori) {
                    let attempts = 0;
                    const value = await chidori.retry(async () => {
                        attempts += 1;
                        if (attempts < 2) {
                            throw new Error("again");
                        }
                        return 7;
                    }, { attempts: 3 });
                    const caught = await chidori.tryCall(async () => {
                        throw new Error("handled");
                    });
                    const values = await chidori.parallel([
                        async () => value,
                        async () => caught.ok,
                        async () => caught.error,
                    ], { concurrency: 2 });
                    return { attempts, values };
                }
                "#,
            )
            .unwrap();
        context.install_js_helpers().unwrap();

        assert_eq!(
            context.call_agent(serde_json::json!({})).unwrap(),
            chidori_quickjs::RunState::Completed(serde_json::json!({
                "attempts": 2,
                "values": [7, false, "handled"]
            }))
        );
    }

    #[test]
    fn snapshot_runtime_restores_agent_suspended_on_chidori_input() {
        let id = chidori_quickjs::HostPromiseId(402);
        let path = Path::new("agent.ts");
        let source = r#"
            export async function agent(input, chidori) {
                const answer = await chidori.input("Continue?");
                return { answer, delta: input.delta };
            }
        "#;

        let snapshot = {
            let runtime =
                TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                    .unwrap();
            let mut context = runtime.eval_agent_source(path, source).unwrap();
            context.install_input_host_promise(id).unwrap();

            assert_eq!(
                context
                    .call_agent(serde_json::json!({ "delta": 2 }))
                    .unwrap(),
                chidori_quickjs::RunState::BlockedOnHostOperation(id)
            );
            assert_eq!(
                context
                    .eval_json_expression("input-calls.js", "globalThis.__chidori_host_calls")
                    .unwrap(),
                serde_json::json!([{ "id": 402, "method": "input", "args": ["Continue?"] }])
            );

            context.snapshot().unwrap()
        };

        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let mut context = runtime.restore_context(&snapshot).unwrap();
        assert_eq!(
            context
                .eval_json_expression("restored-input-calls.js", "globalThis.__chidori_host_calls")
                .unwrap(),
            serde_json::json!([{ "id": 402, "method": "input", "args": ["Continue?"] }])
        );
        context
            .resolve_host_promise(id, serde_json::json!("yes"))
            .unwrap();
        context.run_jobs_until_blocked().unwrap();

        assert_eq!(
            context
                .eval_json_expression("input-result.js", "globalThis.__chidori_call_result")
                .unwrap(),
            serde_json::json!({ "answer": "yes", "delta": 2 })
        );
        assert_eq!(
            context
                .eval_json_expression(
                    "input-active-cleared.js",
                    "typeof globalThis.__chidori_active_host_operation_id"
                )
                .unwrap(),
            serde_json::json!("undefined")
        );
    }

    #[test]
    fn snapshot_runtime_restores_agent_suspended_on_generic_chidori_host_method() {
        let id = chidori_quickjs::HostPromiseId(403);
        let path = Path::new("agent.ts");
        let source = r#"
            export async function agent(input, chidori) {
                const text = await chidori.prompt("Status?", { type: "progress" });
                return { text, label: input.label };
            }
        "#;

        let snapshot = {
            let runtime =
                TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                    .unwrap();
            let mut context = runtime.eval_agent_source(path, source).unwrap();
            context.install_host_promise_method("prompt", id).unwrap();

            assert_eq!(
                context
                    .call_agent(serde_json::json!({ "label": "demo" }))
                    .unwrap(),
                chidori_quickjs::RunState::BlockedOnHostOperation(id)
            );
            assert_eq!(
                context
                    .eval_json_expression("prompt-calls.js", "globalThis.__chidori_host_calls")
                    .unwrap(),
                serde_json::json!([
                    {
                        "id": 403,
                        "method": "prompt",
                        "args": ["Status?", { "type": "progress" }]
                    }
                ])
            );

            context.snapshot().unwrap()
        };

        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let mut context = runtime.restore_context(&snapshot).unwrap();
        assert_eq!(
            context
                .eval_json_expression(
                    "restored-prompt-calls.js",
                    "globalThis.__chidori_host_calls"
                )
                .unwrap(),
            serde_json::json!([
                {
                    "id": 403,
                    "method": "prompt",
                    "args": ["Status?", { "type": "progress" }]
                }
            ])
        );
        context
            .resolve_host_promise(id, serde_json::json!("green"))
            .unwrap();
        context.run_jobs_until_blocked().unwrap();

        assert_eq!(
            context
                .eval_json_expression("prompt-result.js", "globalThis.__chidori_call_result")
                .unwrap(),
            serde_json::json!({ "text": "green", "label": "demo" })
        );
        assert_eq!(
            context
                .eval_json_expression(
                    "prompt-active-cleared.js",
                    "typeof globalThis.__chidori_active_host_operation_id"
                )
                .unwrap(),
            serde_json::json!("undefined")
        );
    }

    #[test]
    fn snapshot_runtime_clears_active_operation_after_rejected_host_method() {
        let id = chidori_quickjs::HostPromiseId(404);
        let path = Path::new("agent.ts");
        let source = r#"
            export async function agent(input, chidori) {
                await chidori.prompt("Status?");
                return { recovered: false };
            }
        "#;

        let snapshot = {
            let runtime =
                TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                    .unwrap();
            let mut context = runtime.eval_agent_source(path, source).unwrap();
            context.install_host_promise_method("prompt", id).unwrap();

            assert_eq!(
                context.call_agent(serde_json::json!({})).unwrap(),
                chidori_quickjs::RunState::BlockedOnHostOperation(id)
            );
            context.snapshot().unwrap()
        };

        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let mut context = runtime.restore_context(&snapshot).unwrap();
        let err = context
            .reject_host_promise_and_run(id, "provider failed".to_string())
            .unwrap_err();
        assert!(err.to_string().contains("provider failed"));

        assert_eq!(
            context
                .eval_json_expression("prompt-reject-result.js", "globalThis.__chidori_call_error")
                .unwrap(),
            serde_json::json!("provider failed")
        );
        assert_eq!(
            context
                .eval_json_expression(
                    "prompt-reject-active-cleared.js",
                    "typeof globalThis.__chidori_active_host_operation_id"
                )
                .unwrap(),
            serde_json::json!("undefined")
        );
    }

    #[test]
    fn snapshot_runtime_resumes_to_second_host_pause_with_distinct_operation_id() {
        let prompt_id = chidori_quickjs::HostPromiseId(405);
        let input_id = chidori_quickjs::HostPromiseId(406);
        let path = Path::new("agent.ts");
        let source = r#"
            export async function agent(input, chidori) {
                const text = await chidori.prompt("Status?");
                const approved = await chidori.input("Approve?");
                return { text, approved };
            }
        "#;

        let snapshot = {
            let runtime =
                TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                    .unwrap();
            let mut context = runtime.eval_agent_source(path, source).unwrap();
            context
                .install_host_promise_method_sequence("prompt", &[prompt_id])
                .unwrap();
            context
                .install_host_promise_method_sequence("input", &[input_id])
                .unwrap();

            assert_eq!(
                context.call_agent(serde_json::json!({})).unwrap(),
                chidori_quickjs::RunState::BlockedOnHostOperation(prompt_id)
            );
            context.snapshot().unwrap()
        };

        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let (mut context, state) = runtime
            .resolve_host_promise_from_snapshot(&snapshot, prompt_id, serde_json::json!("green"))
            .unwrap();
        assert_eq!(
            state,
            chidori_quickjs::RunState::BlockedOnHostOperation(input_id)
        );

        assert_eq!(
            context
                .eval_json_expression(
                    "second-pause-active.js",
                    "globalThis.__chidori_active_host_operation_id"
                )
                .unwrap(),
            serde_json::json!(406)
        );
        assert_eq!(
            context
                .eval_json_expression("second-pause-calls.js", "globalThis.__chidori_host_calls")
                .unwrap(),
            serde_json::json!([
                { "id": 405, "method": "prompt", "args": ["Status?"] },
                { "id": 406, "method": "input", "args": ["Approve?"] }
            ])
        );

        assert_eq!(
            context
                .resolve_host_promise_and_run(input_id, serde_json::json!(true))
                .unwrap(),
            chidori_quickjs::RunState::Completed(
                serde_json::json!({ "text": "green", "approved": true })
            )
        );

        assert_eq!(
            context
                .eval_json_expression("second-pause-result.js", "globalThis.__chidori_call_result")
                .unwrap(),
            serde_json::json!({ "text": "green", "approved": true })
        );
        assert_eq!(
            context
                .eval_json_expression(
                    "second-pause-active-cleared.js",
                    "typeof globalThis.__chidori_active_host_operation_id"
                )
                .unwrap(),
            serde_json::json!("undefined")
        );
    }

    #[test]
    fn snapshot_runtime_return_await_call_agent_reports_second_host_pause() {
        let input_id = chidori_quickjs::HostPromiseId(509);
        let call_agent_id = chidori_quickjs::HostPromiseId(510);
        let path = Path::new("agent.ts");
        let source = r#"
            export async function agent(_input, chidori) {
                const first = await chidori.input("first?");
                return await chidori.callAgent("child.ts", { value: first });
            }
        "#;

        let snapshot = {
            let runtime =
                TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                    .unwrap();
            let mut context = runtime.eval_agent_source(path, source).unwrap();
            context
                .install_host_promise_method_sequence("input", &[input_id])
                .unwrap();
            context
                .install_host_promise_method_sequence("callAgent", &[call_agent_id])
                .unwrap();

            assert_eq!(
                context.call_agent(serde_json::json!({})).unwrap(),
                chidori_quickjs::RunState::BlockedOnHostOperation(input_id)
            );
            context.snapshot().unwrap()
        };

        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let (mut context, state) = runtime
            .resolve_host_promise_from_snapshot(&snapshot, input_id, serde_json::json!("go"))
            .unwrap();
        assert_eq!(
            state,
            chidori_quickjs::RunState::BlockedOnHostOperation(call_agent_id)
        );
        assert_eq!(
            context
                .eval_json_expression(
                    "return-await-call-agent-calls.js",
                    "globalThis.__chidori_host_calls"
                )
                .unwrap(),
            serde_json::json!([
                { "id": 509, "method": "input", "args": ["first?"] },
                {
                    "id": 510,
                    "method": "callAgent",
                    "args": ["child.ts", { "value": "go" }]
                }
            ])
        );
    }

    #[test]
    fn snapshot_runtime_installs_host_promises_from_pending_operations() {
        let prompt_id = chidori_quickjs::HostPromiseId(407);
        let input_id = chidori_quickjs::HostPromiseId(408);
        let path = Path::new("agent.ts");
        let source = r#"
            export async function agent(input, chidori) {
                const text = await chidori.prompt("Status?");
                const approved = await chidori.input("Approve?");
                return { text, approved };
            }
        "#;
        let operations = vec![
            PendingHostOperation::new(
                HostOperationId(prompt_id.0),
                1,
                PendingHostOperationKind::Prompt,
                serde_json::json!({ "text": "Status?" }),
            ),
            PendingHostOperation::new(
                HostOperationId(input_id.0),
                2,
                PendingHostOperationKind::Input,
                serde_json::json!({ "prompt": "Approve?" }),
            ),
        ];

        let snapshot = {
            let runtime =
                TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                    .unwrap();
            let mut context = runtime.eval_agent_source(path, source).unwrap();
            context
                .install_host_promise_operations(&operations)
                .unwrap();

            assert_eq!(
                context.call_agent(serde_json::json!({})).unwrap(),
                chidori_quickjs::RunState::BlockedOnHostOperation(prompt_id)
            );
            context.snapshot().unwrap()
        };

        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let (mut context, state) = runtime
            .resolve_host_promise_from_snapshot(&snapshot, prompt_id, serde_json::json!("green"))
            .unwrap();
        assert_eq!(
            state,
            chidori_quickjs::RunState::BlockedOnHostOperation(input_id)
        );
        assert_eq!(
            context
                .resolve_host_promise_and_run(input_id, serde_json::json!("yes"))
                .unwrap(),
            chidori_quickjs::RunState::Completed(serde_json::json!({
                "text": "green",
                "approved": "yes"
            }))
        );
        assert_eq!(
            context
                .eval_json_expression(
                    "pending-operation-host-calls.js",
                    "globalThis.__chidori_host_calls"
                )
                .unwrap(),
            serde_json::json!([
                { "id": 407, "method": "prompt", "args": ["Status?"] },
                { "id": 408, "method": "input", "args": ["Approve?"] }
            ])
        );
    }

    #[test]
    fn snapshot_runtime_rejects_ambiguous_pending_host_operations() {
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let mut context = runtime
            .eval_agent_source(
                Path::new("agent.ts"),
                "export async function agent() { return null; }",
            )
            .unwrap();
        let operation = PendingHostOperation::new(
            HostOperationId(409),
            1,
            PendingHostOperationKind::Sandbox,
            serde_json::json!({ "source": "1 + 1" }),
        );

        let err = context
            .install_host_promise_operations(&[operation])
            .unwrap_err();

        assert!(err
            .to_string()
            .contains("sandbox host operations need the concrete exec method"));
    }

    #[test]
    fn snapshot_runtime_installs_sandbox_pending_operation_with_function_name() {
        let id = chidori_quickjs::HostPromiseId(410);
        let path = Path::new("agent.ts");
        let source = r#"
            export async function agent(input, chidori) {
                const answer = await chidori.execJs("1 + 1", { fuel: 100 });
                return { answer };
            }
        "#;
        let operation = PendingHostOperation::new(
            HostOperationId(id.0),
            1,
            PendingHostOperationKind::Sandbox,
            serde_json::json!({ "source": "1 + 1", "fuel": 100 }),
        )
        .with_function("exec_js");

        let snapshot = {
            let runtime =
                TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                    .unwrap();
            let mut context = runtime.eval_agent_source(path, source).unwrap();
            context
                .install_host_promise_operations(&[operation])
                .unwrap();

            assert_eq!(
                context.call_agent(serde_json::json!({})).unwrap(),
                chidori_quickjs::RunState::BlockedOnHostOperation(id)
            );
            context.snapshot().unwrap()
        };

        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let (mut context, state) = runtime
            .resolve_host_promise_from_snapshot(&snapshot, id, serde_json::json!("2"))
            .unwrap();

        assert_eq!(
            state,
            chidori_quickjs::RunState::Completed(serde_json::json!({ "answer": "2" }))
        );
        assert_eq!(
            context
                .eval_json_expression("sandbox-host-calls.js", "globalThis.__chidori_host_calls")
                .unwrap(),
            serde_json::json!([
                { "id": 410, "method": "execJs", "args": ["1 + 1", { "fuel": 100 }] }
            ])
        );
    }

    #[test]
    fn snapshot_runtime_installs_only_pending_host_promise_records() {
        let pending_id = chidori_quickjs::HostPromiseId(411);
        let completed_id = chidori_quickjs::HostPromiseId(412);
        let path = Path::new("agent.ts");
        let source = r#"
            export async function agent(input, chidori) {
                const answer = await chidori.input("Approve?");
                return { answer };
            }
        "#;
        let records = vec![
            crate::runtime::snapshot::HostPromiseRecord {
                operation: PendingHostOperation::new(
                    HostOperationId(completed_id.0),
                    1,
                    PendingHostOperationKind::Prompt,
                    serde_json::json!({ "text": "Already done" }),
                ),
                state: HostPromiseState::Resolved {
                    value: serde_json::json!("done"),
                    completed_at: chrono::Utc::now(),
                },
            },
            crate::runtime::snapshot::HostPromiseRecord {
                operation: PendingHostOperation::new(
                    HostOperationId(pending_id.0),
                    2,
                    PendingHostOperationKind::Input,
                    serde_json::json!({ "prompt": "Approve?" }),
                ),
                state: HostPromiseState::Pending,
            },
        ];

        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let mut context = runtime.eval_agent_source(path, source).unwrap();
        context
            .install_pending_host_promise_records(&records)
            .unwrap();

        assert_eq!(
            context.call_agent(serde_json::json!({})).unwrap(),
            chidori_quickjs::RunState::BlockedOnHostOperation(pending_id)
        );
        assert_eq!(
            context
                .eval_json_expression("record-host-calls.js", "globalThis.__chidori_host_calls")
                .unwrap(),
            serde_json::json!([{ "id": 411, "method": "input", "args": ["Approve?"] }])
        );
        assert_eq!(
            context
                .eval_json_expression(
                    "record-prompt-queue.js",
                    "globalThis.__chidori_host_method_queues.prompt === undefined"
                )
                .unwrap(),
            serde_json::json!(true)
        );
    }

    #[test]
    fn snapshot_live_agent_state_replays_resolved_records_to_pending_operation() {
        let prompt_id = chidori_quickjs::HostPromiseId(413);
        let input_id = chidori_quickjs::HostPromiseId(414);
        let path = Path::new("agent.ts");
        let source = r#"
            export async function agent(input, chidori) {
                const text = await chidori.prompt("Status?");
                const approved = await chidori.input("Approve?");
                return { text, approved };
            }
        "#;
        let prompt = PendingHostOperation::new(
            HostOperationId(prompt_id.0),
            1,
            PendingHostOperationKind::Prompt,
            serde_json::json!({ "text": "Status?" }),
        );
        let input = PendingHostOperation::new(
            HostOperationId(input_id.0),
            2,
            PendingHostOperationKind::Input,
            serde_json::json!({ "prompt": "Approve?" }),
        );
        let records = vec![
            HostPromiseRecord {
                operation: prompt,
                state: HostPromiseState::Resolved {
                    value: serde_json::json!("green"),
                    completed_at: chrono::Utc::now(),
                },
            },
            HostPromiseRecord {
                operation: input.clone(),
                state: HostPromiseState::Pending,
            },
        ];

        let snapshot = snapshot_live_agent_state(
            path,
            source,
            serde_json::json!({}),
            RuntimePolicy::durable_default("snapshot-test"),
            &records,
            Some(&input),
        )
        .unwrap();
        snapshot.ensure_restorable().unwrap();

        let mut runtime = chidori_quickjs::SnapshotRuntime::restore(&snapshot.0).unwrap();
        runtime
            .resolve_host_promise(input_id, serde_json::json!("yes"))
            .unwrap();

        assert_eq!(
            runtime.run_jobs_until_blocked().unwrap(),
            chidori_quickjs::RunState::Completed(serde_json::json!({
                "text": "green",
                "approved": "yes"
            }))
        );
    }

    #[test]
    fn initial_agent_snapshot_restores_exports() {
        let snapshot = snapshot_initial_agent_state(
            Path::new("agent.ts"),
            "export async function agent(input) { return { ok: input.ok }; }",
            RuntimePolicy::durable_default("snapshot-test"),
        )
        .unwrap();

        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let mut context = runtime.restore_context(&snapshot).unwrap();

        assert_eq!(
            context
                .eval_json_expression(
                    "exports-check.js",
                    "typeof globalThis.__chidori_exports.agent"
                )
                .unwrap(),
            serde_json::json!("function")
        );
    }

    #[test]
    fn initial_agent_snapshot_restores_local_import_bindings() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-ts-snapshot-imports-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let lib_path = dir.join("math.ts");
        let agent_path = dir.join("agent.ts");
        std::fs::write(
            &lib_path,
            r#"
            export const base = 40;
            export function add(delta) {
                return base + delta;
            }
            "#,
        )
        .unwrap();
        let source = r#"
            import { base, add as plus } from "./math.ts";
            export async function agent(input) {
                return { answer: plus(input.delta), base };
            }
        "#;
        std::fs::write(&agent_path, source).unwrap();

        let snapshot = snapshot_initial_agent_state(
            &agent_path,
            source,
            RuntimePolicy::durable_default("snapshot-test"),
        )
        .unwrap();
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let mut context = runtime.restore_context(&snapshot).unwrap();

        assert_eq!(
            context
                .call_agent(serde_json::json!({ "delta": 2 }))
                .unwrap(),
            chidori_quickjs::RunState::Completed(serde_json::json!({ "answer": 42, "base": 40 }))
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn initial_agent_snapshot_restores_namespace_import_bindings() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-ts-snapshot-namespace-imports-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let lib_path = dir.join("math.ts");
        let agent_path = dir.join("agent.ts");
        std::fs::write(
            &lib_path,
            r#"
            export const base = 12;
            export function multiply(left, right) {
                return left * right;
            }
            "#,
        )
        .unwrap();
        let source = r#"
            import * as math from "./math.ts";
            export async function agent(input) {
                return { answer: math.multiply(math.base, input.factor) };
            }
        "#;
        std::fs::write(&agent_path, source).unwrap();

        let snapshot = snapshot_initial_agent_state(
            &agent_path,
            source,
            RuntimePolicy::durable_default("snapshot-test"),
        )
        .unwrap();
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let mut context = runtime.restore_context(&snapshot).unwrap();

        assert_eq!(
            context
                .call_agent(serde_json::json!({ "factor": 3 }))
                .unwrap(),
            chidori_quickjs::RunState::Completed(serde_json::json!({ "answer": 36 }))
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn initial_agent_snapshot_restores_module_namespace_root() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-ts-snapshot-module-root-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let lib_path = dir.join("math.ts");
        let agent_path = dir.join("agent.ts");
        std::fs::write(&lib_path, "export const base = 7;").unwrap();
        let source = r#"
            import * as math from "./math.ts";
            export async function agent(input) {
                return { answer: math.base + input.delta };
            }
        "#;
        std::fs::write(&agent_path, source).unwrap();

        let snapshot = snapshot_initial_agent_state(
            &agent_path,
            source,
            RuntimePolicy::durable_default("snapshot-test"),
        )
        .unwrap();
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let mut context = runtime.restore_context(&snapshot).unwrap();
        let module_key = serde_json::to_string(&lib_path.to_string_lossy().to_string()).unwrap();

        assert_eq!(
            context
                .eval_json_expression(
                    "module-root-check.js",
                    &format!(
                        "globalThis.__chidori_modules && globalThis.__chidori_modules[{module_key}].base"
                    ),
                )
                .unwrap(),
            serde_json::json!(7)
        );
        assert_eq!(
            context
                .call_agent(serde_json::json!({ "delta": 5 }))
                .unwrap(),
            chidori_quickjs::RunState::Completed(serde_json::json!({ "answer": 12 }))
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn snapshot_module_fingerprints_match_bundled_local_imports() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-ts-snapshot-fingerprints-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let lib_path = dir.join("math.ts");
        let agent_path = dir.join("agent.ts");
        std::fs::write(&lib_path, "export const base = 40;").unwrap();
        let source = r#"
            import { base } from "./math.ts";
            export async function agent(input) {
                return { answer: base + input.delta };
            }
        "#;
        std::fs::write(&agent_path, source).unwrap();

        let modules = snapshot_module_fingerprints(
            &agent_path,
            source,
            &RuntimePolicy::durable_default("snapshot-test"),
        )
        .unwrap();

        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].path, lib_path);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn snapshot_module_graph_records_local_import_edges() {
        let dir = std::env::temp_dir().join(format!(
            "chidori-ts-module-graph-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let entry_path = dir.join("agent.ts");
        let module_path = dir.join("lib.ts");
        let child_path = dir.join("child.ts");
        let source = r#"
            import { value } from "./lib.ts";
            export async function agent() { return value; }
        "#;
        std::fs::write(&entry_path, source).unwrap();
        std::fs::write(
            &module_path,
            r#"
            import { child } from "./child.ts";
            export const value = child + 1;
            "#,
        )
        .unwrap();
        std::fs::write(&child_path, "export const child = 1;").unwrap();

        let graph = snapshot_module_graph(
            &entry_path,
            source,
            &RuntimePolicy::durable_default("snapshot-test"),
        )
        .unwrap();

        let entry = graph.iter().find(|entry| entry.path == entry_path).unwrap();
        let lib = graph
            .iter()
            .find(|entry| entry.path == module_path)
            .unwrap();
        let child = graph.iter().find(|entry| entry.path == child_path).unwrap();
        assert_eq!(entry.imports[0].specifier, "./lib.ts");
        assert_eq!(entry.imports[0].resolved_path, Some(module_path.clone()));
        assert_eq!(lib.imports[0].specifier, "./child.ts");
        assert_eq!(lib.imports[0].resolved_path, Some(child_path));
        assert!(child.imports.is_empty());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn forked_contexts_start_from_parent_snapshot_independently() {
        let parent_snapshot = snapshot_initial_agent_state(
            Path::new("agent.ts"),
            r#"
            export async function agent(input) {
                globalThis.__branchCounter =
                    (globalThis.__branchCounter || 0) + input.delta;
                return { counter: globalThis.__branchCounter };
            }
            "#,
            RuntimePolicy::durable_default("snapshot-test"),
        )
        .unwrap();

        let runtime_a =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let runtime_b =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let mut branch_a = runtime_a.fork_context(&parent_snapshot).unwrap();
        let mut branch_b = runtime_b.fork_context(&parent_snapshot).unwrap();

        assert_eq!(
            branch_a
                .call_agent(serde_json::json!({ "delta": 1 }))
                .unwrap(),
            chidori_quickjs::RunState::Completed(serde_json::json!({ "counter": 1 }))
        );
        assert_eq!(
            branch_b
                .call_agent(serde_json::json!({ "delta": 10 }))
                .unwrap(),
            chidori_quickjs::RunState::Completed(serde_json::json!({ "counter": 10 }))
        );
        assert_eq!(
            branch_a
                .call_agent(serde_json::json!({ "delta": 1 }))
                .unwrap(),
            chidori_quickjs::RunState::Completed(serde_json::json!({ "counter": 2 }))
        );
        assert_eq!(
            branch_b
                .call_agent(serde_json::json!({ "delta": 10 }))
                .unwrap(),
            chidori_quickjs::RunState::Completed(serde_json::json!({ "counter": 20 }))
        );
    }

    #[test]
    fn parallel_branches_start_from_parent_snapshot_manifest() {
        let parent_snapshot = snapshot_initial_agent_state(
            Path::new("agent.ts"),
            r#"
            export async function agent(input) {
                globalThis.__branchCounter =
                    (globalThis.__branchCounter || 0) + input.delta;
                return {
                    branch: input.branch,
                    counter: globalThis.__branchCounter
                };
            }
            "#,
            RuntimePolicy::durable_default("snapshot-test"),
        )
        .unwrap();
        let manifest = ParallelBranchManifest::new(
            "run-1",
            crate::runtime::snapshot::HostOperationId(3),
            3,
            2,
        );
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();

        let merged = runtime
            .run_parallel_branches_from_snapshot(
                &manifest,
                &parent_snapshot,
                &[
                    serde_json::json!({ "branch": 0, "delta": 1 }),
                    serde_json::json!({ "branch": 1, "delta": 10 }),
                    serde_json::json!({ "branch": 2, "delta": 100 }),
                ],
            )
            .unwrap();

        assert_eq!(
            merged.outputs,
            vec![
                serde_json::json!({ "branch": 0, "counter": 1 }),
                serde_json::json!({ "branch": 1, "counter": 10 }),
                serde_json::json!({ "branch": 2, "counter": 100 }),
            ]
        );
        assert!(merged.call_log.is_empty());
    }

    #[test]
    fn parallel_branches_start_from_persisted_parent_snapshot_store() {
        let run_dir = std::env::temp_dir().join(format!(
            "chidori-ts-branch-start-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SnapshotStore::new(&run_dir);
        let policy = RuntimePolicy::durable_default("snapshot-test");
        let source = r#"
            export async function agent(input) {
                return {
                    branch: input.branch,
                    answer: input.delta + 1
                };
            }
        "#;
        let entry = SourceFingerprint::from_source("agent.ts", source);
        let parent_snapshot =
            snapshot_initial_agent_state(Path::new("agent.ts"), source, policy.clone()).unwrap();
        let parent_manifest = SnapshotManifest::new(
            "run-1",
            SnapshotAbi::current("chidori-quickjs"),
            policy.clone(),
            entry.clone(),
            Vec::new(),
            None,
            0,
        );
        store.save(&parent_manifest, &parent_snapshot, &[]).unwrap();

        let manifest = ParallelBranchManifest::new(
            "run-1",
            crate::runtime::snapshot::HostOperationId(9),
            2,
            2,
        );
        let runtime = TypeScriptSnapshotRuntime::new(policy).unwrap();

        let merged = runtime
            .run_parallel_branches_from_store(
                &store,
                &manifest,
                &[
                    serde_json::json!({ "branch": 0, "delta": 10 }),
                    serde_json::json!({ "branch": 1, "delta": 20 }),
                ],
                &entry,
                &[],
            )
            .unwrap();

        assert_eq!(
            merged.outputs,
            vec![
                serde_json::json!({ "branch": 0, "answer": 11 }),
                serde_json::json!({ "branch": 1, "answer": 21 }),
            ]
        );
        assert!(merged.call_log.is_empty());
        assert_eq!(
            store
                .load_parallel_branch_manifest(crate::runtime::snapshot::HostOperationId(9))
                .unwrap(),
            manifest
        );
        for branch_index in 0..2 {
            let loaded = store
                .branch_store(&manifest, branch_index)
                .unwrap()
                .load()
                .unwrap();
            assert_eq!(loaded.manifest.entry, entry);
            assert_eq!(
                loaded.manifest.branch,
                Some(SnapshotBranchMetadata {
                    parent_run_id: manifest.parent_run_id.clone(),
                    parallel_op_id: manifest.parallel_op_id,
                    branch_index,
                    branch_operation_id: manifest
                        .branch(branch_index)
                        .unwrap()
                        .operation_id
                        .clone(),
                })
            );
            assert!(!loaded.blob.is_empty());
        }

        let _ = std::fs::remove_dir_all(run_dir);
    }

    #[test]
    fn live_vm_restore_from_store_rejects_scaffold_snapshot_kind() {
        let run_dir = std::env::temp_dir().join(format!(
            "chidori-ts-live-restore-kind-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SnapshotStore::new(&run_dir);
        let policy = RuntimePolicy::durable_default("snapshot-test");
        let source = "export async function agent() { return 1; }";
        let entry = SourceFingerprint::from_source("agent.ts", source);
        let manifest = SnapshotManifest::new(
            "run-1",
            SnapshotAbi::current("chidori-quickjs"),
            policy.clone(),
            entry.clone(),
            Vec::new(),
            None,
            0,
        );
        store.save(&manifest, b"snapshot-bytes", &[]).unwrap();

        let err = TypeScriptSnapshotRuntime::restore_live_vm_from_store(
            &store,
            &policy,
            &entry,
            &[],
            &[],
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("runtime snapshot blob kind mismatch"));

        let _ = std::fs::remove_dir_all(run_dir);
    }

    #[test]
    fn live_vm_restore_from_store_reaches_quickjs_restore_for_live_snapshot_kind() {
        let run_dir = std::env::temp_dir().join(format!(
            "chidori-ts-live-restore-fork-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SnapshotStore::new(&run_dir);
        let policy = RuntimePolicy::durable_default("snapshot-test");
        let source = "export async function agent() { return 1; }";
        let entry = SourceFingerprint::from_source("agent.ts", source);
        let manifest = SnapshotManifest::new(
            "run-1",
            SnapshotAbi::current("chidori-quickjs"),
            policy.clone(),
            entry.clone(),
            Vec::new(),
            None,
            0,
        )
        .with_snapshot_kind(SnapshotBlobKind::LiveQuickJsVm);
        let snapshot = chidori_quickjs::RuntimeSnapshot::from_payload(b"snapshot-bytes");
        store.save(&manifest, &snapshot.0, &[]).unwrap();

        let err = TypeScriptSnapshotRuntime::restore_live_vm_from_store(
            &store,
            &policy,
            &entry,
            &[],
            &[],
        )
        .unwrap_err();
        assert!(err.to_string().contains("patched QuickJS fork"));

        let _ = std::fs::remove_dir_all(run_dir);
    }

    #[test]
    fn live_vm_save_to_store_reaches_quickjs_snapshot_boundary_without_writing_manifest() {
        let run_dir = std::env::temp_dir().join(format!(
            "chidori-ts-live-save-fork-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SnapshotStore::new(&run_dir);
        let policy = RuntimePolicy::durable_default("snapshot-test");
        let source = "export async function agent() { return 1; }";
        let entry = SourceFingerprint::from_source("agent.ts", source);
        let manifest = SnapshotManifest::new(
            "run-1",
            SnapshotAbi::current("chidori-quickjs"),
            policy.clone(),
            entry,
            Vec::new(),
            None,
            0,
        );
        let mut runtime = TypeScriptSnapshotRuntime::new(policy).unwrap();

        let err = runtime
            .save_live_vm_to_store(&store, &manifest, &[])
            .unwrap_err();
        assert!(err.to_string().contains("context payload is empty"));
        assert!(!run_dir
            .join(crate::runtime::snapshot::SNAPSHOT_MANIFEST_FILE)
            .exists());

        let _ = std::fs::remove_dir_all(run_dir);
    }

    #[test]
    fn paused_parallel_branch_resumes_from_branch_snapshot() {
        let runtime =
            TypeScriptSnapshotRuntime::new(RuntimePolicy::durable_default("snapshot-test"))
                .unwrap();
        let mut branch = runtime
            .eval_agent_source(
                Path::new("agent.ts"),
                r#"
                export async function agent(input) {
                    const value = await globalThis.__branch_host;
                    return {
                        branch: input.branch,
                        answer: value.answer + input.delta
                    };
                }
                "#,
            )
            .unwrap();
        let host_promise_id = chidori_quickjs::HostPromiseId(901);
        let promise = branch.new_host_promise(host_promise_id).unwrap();
        branch
            .set_global_js_value("__branch_host", promise)
            .unwrap();
        branch
            .eval_json_expression(
                "start-branch.js",
                r#"
                (
                    globalThis.__branch_result = null,
                    globalThis.__branch_promise =
                        globalThis.__chidori_exports.agent({ branch: 1, delta: 2 }),
                    globalThis.__branch_promise.then(value => {
                        globalThis.__branch_result = value;
                    }),
                    null
                )
                "#,
            )
            .unwrap();
        let branch_snapshot = branch
            .snapshot_roots(&[
                "__chidori_exports",
                "__branch_host",
                "__branch_promise",
                "__branch_result",
            ])
            .unwrap();
        let manifest = ParallelBranchManifest::new(
            "run-1",
            crate::runtime::snapshot::HostOperationId(7),
            2,
            2,
        );

        let outcome = runtime
            .resume_paused_branch_from_snapshot(
                &manifest,
                1,
                &branch_snapshot,
                host_promise_id,
                serde_json::json!({ "answer": 40 }),
                "globalThis.__branch_result",
            )
            .unwrap();

        assert_eq!(outcome.branch_index, 1);
        assert_eq!(
            outcome.output.unwrap(),
            serde_json::json!({ "branch": 1, "answer": 42 })
        );
        assert!(outcome.call_log.is_empty());
    }

    #[test]
    fn paused_parallel_branch_resumes_from_persisted_branch_store() {
        let run_dir = std::env::temp_dir().join(format!(
            "chidori-ts-branch-resume-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SnapshotStore::new(&run_dir);
        let policy = RuntimePolicy::durable_default("snapshot-test");
        let runtime = TypeScriptSnapshotRuntime::new(policy.clone()).unwrap();
        let source = r#"
            export async function agent(input) {
                const value = await globalThis.__branch_host;
                return {
                    branch: input.branch,
                    answer: value.answer + input.delta
                };
            }
        "#;
        let mut branch = runtime
            .eval_agent_source(Path::new("agent.ts"), source)
            .unwrap();
        let host_promise_id = chidori_quickjs::HostPromiseId(902);
        let promise = branch.new_host_promise(host_promise_id).unwrap();
        branch
            .set_global_js_value("__branch_host", promise)
            .unwrap();
        branch
            .eval_json_expression(
                "start-branch.js",
                r#"
                (
                    globalThis.__branch_result = null,
                    globalThis.__branch_promise =
                        globalThis.__chidori_exports.agent({ branch: 1, delta: 2 }),
                    globalThis.__branch_promise.then(value => {
                        globalThis.__branch_result = value;
                    }),
                    null
                )
                "#,
            )
            .unwrap();
        let branch_snapshot = branch
            .snapshot_roots(&[
                "__chidori_exports",
                "__branch_host",
                "__branch_promise",
                "__branch_result",
            ])
            .unwrap();
        let manifest = ParallelBranchManifest::new(
            "run-1",
            crate::runtime::snapshot::HostOperationId(8),
            2,
            2,
        );
        store.save_parallel_branch_manifest(&manifest).unwrap();

        let branch_store = store.branch_store(&manifest, 1).unwrap();
        let entry = SourceFingerprint::from_source("agent.ts", source);
        let snapshot_manifest = crate::runtime::snapshot::SnapshotManifest::new(
            "run-1",
            SnapshotAbi::current("chidori-quickjs"),
            policy,
            entry.clone(),
            Vec::new(),
            None,
            0,
        )
        .with_branch_metadata(SnapshotBranchMetadata {
            parent_run_id: manifest.parent_run_id.clone(),
            parallel_op_id: manifest.parallel_op_id,
            branch_index: 1,
            branch_operation_id: manifest.branch(1).unwrap().operation_id.clone(),
        });
        branch_store
            .save(&snapshot_manifest, &branch_snapshot, &[])
            .unwrap();

        let err = runtime
            .resume_paused_branch_from_store(
                &store,
                crate::runtime::snapshot::HostOperationId(8),
                1,
                &SourceFingerprint::from_source("agent.ts", "changed"),
                &[],
                host_promise_id,
                serde_json::json!({ "answer": 40 }),
                "globalThis.__branch_result",
            )
            .unwrap_err();
        assert!(err.to_string().contains("runtime snapshot source mismatch"));

        let outcome = runtime
            .resume_paused_branch_from_store(
                &store,
                crate::runtime::snapshot::HostOperationId(8),
                1,
                &entry,
                &[],
                host_promise_id,
                serde_json::json!({ "answer": 40 }),
                "globalThis.__branch_result",
            )
            .unwrap();

        assert_eq!(outcome.branch_index, 1);
        assert_eq!(
            outcome.output.unwrap(),
            serde_json::json!({ "branch": 1, "answer": 42 })
        );
        assert!(outcome.call_log.is_empty());

        let wrong_branch_manifest = crate::runtime::snapshot::SnapshotManifest::new(
            "run-1",
            SnapshotAbi::current("chidori-quickjs"),
            RuntimePolicy::durable_default("snapshot-test"),
            entry.clone(),
            Vec::new(),
            None,
            0,
        )
        .with_branch_metadata(SnapshotBranchMetadata {
            parent_run_id: manifest.parent_run_id.clone(),
            parallel_op_id: manifest.parallel_op_id,
            branch_index: 0,
            branch_operation_id: manifest.branch(0).unwrap().operation_id.clone(),
        });
        branch_store
            .save(&wrong_branch_manifest, &branch_snapshot, &[])
            .unwrap();
        let err = runtime
            .resume_paused_branch_from_store(
                &store,
                crate::runtime::snapshot::HostOperationId(8),
                1,
                &entry,
                &[],
                host_promise_id,
                serde_json::json!({ "answer": 40 }),
                "globalThis.__branch_result",
            )
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("runtime snapshot branch metadata mismatch"));

        let _ = std::fs::remove_dir_all(run_dir);
    }

    /// End-to-end bundling test for the `Node` import policy: an agent that
    /// imports both a bare npm package (from a fixture `node_modules`) and a
    /// `node:process` builtin should bundle into a single string whose
    /// `__chidori_modules` table contains entries for each resolved path.
    #[test]
    fn build_snapshot_bundle_resolves_node_packages_and_builtins() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Anchor the workspace with a package.json so `find_workspace_root`
        // picks this directory rather than walking past it.
        std::fs::write(root.join("package.json"), r#"{"name":"agent-fixture"}"#).unwrap();
        // A fake npm package with `exports` and a real on-disk module.
        let pkg_dir = root.join("node_modules/@chidori-integrations/example");
        std::fs::create_dir_all(pkg_dir.join("dist")).unwrap();
        std::fs::write(
            pkg_dir.join("package.json"),
            r#"{"name":"@chidori-integrations/example","exports":"./dist/index.js"}"#,
        )
        .unwrap();
        std::fs::write(
            pkg_dir.join("dist/index.js"),
            "export const greeting = \"hello from example pkg\";\n",
        )
        .unwrap();

        let agent_path = root.join("agent.ts");
        let agent_source = r#"
            import { greeting } from "@chidori-integrations/example";
            import { env } from "node:process";
            export async function agent(_input, _chidori) {
                return { greeting, hasEnv: !!env };
            }
        "#;
        std::fs::write(&agent_path, agent_source).unwrap();

        let policy = RuntimePolicy {
            typescript_imports: TypeScriptImportPolicy::Node,
            date: DatePolicy::Fixed,
            random: RandomPolicy::Seeded,
            maps_sets: MapSetSnapshotPolicy::Reject,
            fs: FsPolicy::Captured,
            crypto: CryptoPolicy::Captured,
            timers: TimerPolicy::Virtual,
            deterministic_seed: "0000000000000000".to_string(),
        };
        let bundle = build_snapshot_bundle(&agent_path, agent_source, &policy).unwrap();

        // The package module should be bundled under its resolved path.
        assert!(
            bundle.contains("dist/index.js"),
            "bundle missing package module:\n{bundle}"
        );
        assert!(
            bundle.contains("hello from example pkg"),
            "bundle missing package source:\n{bundle}"
        );
        // The node:process shim should be inlined under the synthetic
        // builtin path; we only need to confirm the shim's marker line shows
        // up so we know the builtin source took the place of an FS read.
        assert!(
            bundle.contains("__node_builtins__"),
            "bundle missing node builtin path:\n{bundle}"
        );
        assert!(
            bundle.contains("globalThis.process"),
            "bundle missing node:process shim body:\n{bundle}"
        );
        // The agent's own import bindings should rewrite into
        // __chidori_modules lookups instead of staying as raw `import`s. We
        // can't grep the specifier string directly because the shim source
        // mentions `node:process` in a comment, so check that no statement
        // line begins with `import ` (the bundler rewrites them all).
        for line in bundle.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("import ") && !trimmed.starts_with("import type ") {
                panic!("bundle leaked raw import statement: {line}\nfull bundle:\n{bundle}");
            }
        }
    }
}
