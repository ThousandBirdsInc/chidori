use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{Context as AnyhowContext, Result};

use crate::mcp::McpManager;
use crate::providers::ProviderRegistry;
use crate::runtime::context::RuntimeContext;
use crate::runtime::snapshot::RuntimePolicy;
use crate::runtime::template::TemplateEngine;
use crate::runtime::typescript::snapshot::{
    TypeScriptSnapshotHostState, TypeScriptSnapshotRuntime,
};
use crate::tools::ToolRegistry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostBindingCall {
    pub function: String,
    pub args: serde_json::Value,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct TypeScriptRunResult {
    pub output: serde_json::Value,
    pub host_calls: Vec<HostBindingCall>,
}

#[allow(dead_code)]
pub struct TypeScriptVmRuntime {
    policy: RuntimePolicy,
}

#[allow(dead_code)]
impl TypeScriptVmRuntime {
    pub fn new(policy: RuntimePolicy) -> Result<Self> {
        policy.ensure_durable_safe()?;
        Ok(Self { policy })
    }

    pub fn run_agent_file(
        &self,
        path: &Path,
        input: &serde_json::Value,
    ) -> Result<TypeScriptRunResult> {
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        self.run_agent_source(path, &source, input)
    }

    pub fn run_agent_file_with_context(
        &self,
        path: &Path,
        input: &serde_json::Value,
        runtime_ctx: RuntimeContext,
        providers: Arc<ProviderRegistry>,
        template_engine: Arc<TemplateEngine>,
        tokio_rt: Arc<tokio::runtime::Runtime>,
        policy: Arc<crate::policy::PolicyConfig>,
        policy_cache: Arc<StdMutex<crate::policy::PolicyCache>>,
        tools: Arc<ToolRegistry>,
        mcp: Arc<McpManager>,
    ) -> Result<serde_json::Value> {
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        self.run_agent_source_with_context(
            path,
            &source,
            input,
            runtime_ctx,
            providers,
            template_engine,
            tokio_rt,
            policy,
            policy_cache,
            tools,
            mcp,
        )
    }

    pub fn run_tool_file_with_context(
        &self,
        path: &Path,
        args: &serde_json::Value,
        runtime_ctx: RuntimeContext,
        providers: Arc<ProviderRegistry>,
        template_engine: Arc<TemplateEngine>,
        tokio_rt: Arc<tokio::runtime::Runtime>,
        policy: Arc<crate::policy::PolicyConfig>,
        policy_cache: Arc<StdMutex<crate::policy::PolicyCache>>,
        tools: Arc<ToolRegistry>,
        mcp: Arc<McpManager>,
    ) -> Result<serde_json::Value> {
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        self.run_exported_function_with_context(
            path,
            &source,
            "run",
            args,
            runtime_ctx,
            providers,
            template_engine,
            tokio_rt,
            policy,
            policy_cache,
            tools,
            mcp,
        )
    }

    pub fn run_agent_source(
        &self,
        path: &Path,
        source: &str,
        input: &serde_json::Value,
    ) -> Result<TypeScriptRunResult> {
        let runtime = TypeScriptSnapshotRuntime::new(self.policy.clone())?;
        let mut context = runtime.eval_agent_source(path, source)?;
        context.eval_json_expression(
            "install-recorder-chidori.js",
            RECORDER_CHIDORI_INSTALL_SCRIPT,
        )?;

        let output = match context.call_agent(input.clone())? {
            chidori_quickjs::RunState::Completed(output) => output,
            chidori_quickjs::RunState::BlockedOnHostOperation(id) => {
                anyhow::bail!("recorder TypeScript VM blocked on host operation {}", id.0);
            }
        };
        let host_calls = recorder_host_calls_from_context(&mut context)?;

        Ok(TypeScriptRunResult { output, host_calls })
    }

    pub fn run_agent_source_with_context(
        &self,
        path: &Path,
        source: &str,
        input: &serde_json::Value,
        runtime_ctx: RuntimeContext,
        providers: Arc<ProviderRegistry>,
        template_engine: Arc<TemplateEngine>,
        tokio_rt: Arc<tokio::runtime::Runtime>,
        policy: Arc<crate::policy::PolicyConfig>,
        policy_cache: Arc<StdMutex<crate::policy::PolicyCache>>,
        tools: Arc<ToolRegistry>,
        mcp: Arc<McpManager>,
    ) -> Result<serde_json::Value> {
        self.run_exported_function_with_context(
            path,
            source,
            "agent",
            input,
            runtime_ctx,
            providers,
            template_engine,
            tokio_rt,
            policy,
            policy_cache,
            tools,
            mcp,
        )
    }

    fn run_exported_function_with_context(
        &self,
        path: &Path,
        source: &str,
        export_name: &str,
        input: &serde_json::Value,
        runtime_ctx: RuntimeContext,
        providers: Arc<ProviderRegistry>,
        template_engine: Arc<TemplateEngine>,
        tokio_rt: Arc<tokio::runtime::Runtime>,
        policy: Arc<crate::policy::PolicyConfig>,
        policy_cache: Arc<StdMutex<crate::policy::PolicyCache>>,
        tools: Arc<ToolRegistry>,
        mcp: Arc<McpManager>,
    ) -> Result<serde_json::Value> {
        let runtime = TypeScriptSnapshotRuntime::new(self.policy.clone())?;
        let mut context = runtime.eval_agent_source(path, source)?;
        let mut host_state = TypeScriptSnapshotHostState::with_tools(
            runtime_ctx,
            providers,
            template_engine,
            tokio_rt,
            policy,
            policy_cache,
            self.policy.clone(),
            tools,
            mcp,
        );
        unsafe {
            context.install_runtime_host(&mut host_state)?;
        }
        let state = context.call_export(export_name, input.clone());
        unsafe {
            context.set_context_opaque(std::ptr::null_mut());
        }
        let state = state.map_err(snapshot_export_error)?;

        match state {
            chidori_quickjs::RunState::Completed(output) => Ok(output),
            chidori_quickjs::RunState::BlockedOnHostOperation(id) => {
                anyhow::bail!(
                    "TypeScript VM blocked on host operation {} while running export `{}`",
                    id.0,
                    export_name
                )
            }
        }
    }
}

fn snapshot_export_error(err: anyhow::Error) -> anyhow::Error {
    let message = err.to_string();
    if let Some(js_message) = message.strip_prefix("QuickJS evaluation failed: ") {
        anyhow::anyhow!("JavaScript exception: {js_message}")
    } else {
        err
    }
}

const RECORDER_CHIDORI_INSTALL_SCRIPT: &str = r#"
(() => {
    globalThis.__chidori_recorder_calls = [];
    const unsupported = (name) => () => {
        throw new Error(`chidori.${name} requires the runtime host backend`);
    };
    globalThis.chidori = {
        log(message) {
            globalThis.__chidori_recorder_calls.push({
                function: "log",
                args: { message: String(message) },
            });
            return null;
        },
        checkpoint(label, data) {
            globalThis.__chidori_recorder_calls.push({
                function: "checkpoint",
                args: { label, data: data === undefined ? null : data },
            });
            return null;
        },
        prompt: unsupported("prompt"),
        input: unsupported("input"),
        memory: unsupported("memory"),
        template: unsupported("template"),
        http: unsupported("http"),
        tool: unsupported("tool"),
        callAgent: unsupported("callAgent"),
        execJs: unsupported("execJs"),
        execPython: unsupported("execPython"),
        execWasm: unsupported("execWasm"),
    };

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

    return null;
})()
"#;

fn recorder_host_calls_from_context(
    context: &mut crate::runtime::typescript::snapshot::TypeScriptSnapshotContext<'_>,
) -> Result<Vec<HostBindingCall>> {
    let calls = context.eval_json_expression(
        "recorder-host-calls.js",
        "globalThis.__chidori_recorder_calls || []",
    )?;
    let Some(calls) = calls.as_array() else {
        anyhow::bail!("recorder host calls must be an array");
    };
    calls
        .iter()
        .map(|call| {
            let function = call
                .get("function")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("recorder host call is missing function"))?
                .to_string();
            let args = call.get("args").cloned().unwrap_or(serde_json::Value::Null);
            Ok(HostBindingCall { function, args })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::snapshot::DatePolicy;

    fn runtime() -> TypeScriptVmRuntime {
        TypeScriptVmRuntime::new(RuntimePolicy::durable_default("ts-runtime-test")).unwrap()
    }

    fn template_engine() -> Arc<TemplateEngine> {
        Arc::new(TemplateEngine::new("."))
    }

    fn runtime_host() -> (
        Arc<tokio::runtime::Runtime>,
        Arc<crate::policy::PolicyConfig>,
        Arc<StdMutex<crate::policy::PolicyCache>>,
    ) {
        (
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
            Arc::new(crate::policy::PolicyConfig::default()),
            Arc::new(StdMutex::new(crate::policy::PolicyCache::default())),
        )
    }

    #[test]
    fn runs_simple_agent_and_returns_json() {
        let source = r#"
            import type { Chidori } from "chidori";
            export async function agent(input: { name: string }, chidori: Chidori) {
                await chidori.log("starting");
                return { greeting: "Hello, " + input.name };
            }
        "#;

        let result = runtime()
            .run_agent_source(
                Path::new("/tmp/agent.ts"),
                source,
                &serde_json::json!({ "name": "Chidori" }),
            )
            .unwrap();

        assert_eq!(
            result.output,
            serde_json::json!({ "greeting": "Hello, Chidori" })
        );
        assert_eq!(result.host_calls.len(), 1);
        assert_eq!(result.host_calls[0].function, "log");
    }

    #[test]
    fn runtime_context_records_log_calls() {
        let source = r#"
            export async function agent(input, chidori) {
                await chidori.log("starting");
                return { ok: true };
            }
        "#;
        let runtime_ctx = RuntimeContext::new();
        let (tokio_rt, policy, policy_cache) = runtime_host();

        let output = runtime()
            .run_agent_source_with_context(
                Path::new("/tmp/agent.ts"),
                source,
                &serde_json::json!({}),
                runtime_ctx.clone(),
                Arc::new(ProviderRegistry::new()),
                template_engine(),
                tokio_rt,
                policy,
                policy_cache,
                Arc::new(ToolRegistry::new()),
                Arc::new(McpManager::new()),
            )
            .unwrap();

        assert_eq!(output, serde_json::json!({ "ok": true }));
        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].seq, 1);
        assert_eq!(records[0].function, "log");
    }

    #[test]
    fn runtime_context_records_checkpoint_calls() {
        let source = r#"
            export async function agent(input, chidori) {
                await chidori.checkpoint("draft", { count: 2 });
                return { ok: true };
            }
        "#;
        let runtime_ctx = RuntimeContext::new();
        let (tokio_rt, policy, policy_cache) = runtime_host();

        let output = runtime()
            .run_agent_source_with_context(
                Path::new("/tmp/agent.ts"),
                source,
                &serde_json::json!({}),
                runtime_ctx.clone(),
                Arc::new(ProviderRegistry::new()),
                template_engine(),
                tokio_rt,
                policy,
                policy_cache,
                Arc::new(ToolRegistry::new()),
                Arc::new(McpManager::new()),
            )
            .unwrap();

        assert_eq!(output, serde_json::json!({ "ok": true }));
        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "checkpoint");
        assert_eq!(
            records[0].args,
            serde_json::json!({
                "label": "draft",
                "data": { "count": 2 },
            })
        );
    }

    #[test]
    fn runtime_context_records_memory_calls() {
        let namespace = format!("ts-memory-{}", uuid::Uuid::new_v4());
        let source = format!(
            r#"
            export async function agent(input, chidori) {{
                await chidori.memory("set", "answer", {{ value: 42 }}, {{ namespace: "{namespace}" }});
                const value = await chidori.memory("get", "answer", null, {{ namespace: "{namespace}" }});
                return value;
            }}
        "#
        );
        let runtime_ctx = RuntimeContext::new();
        let (tokio_rt, policy, policy_cache) = runtime_host();

        let output = runtime()
            .run_agent_source_with_context(
                Path::new("/tmp/agent.ts"),
                &source,
                &serde_json::json!({}),
                runtime_ctx.clone(),
                Arc::new(ProviderRegistry::new()),
                template_engine(),
                tokio_rt,
                policy,
                policy_cache,
                Arc::new(ToolRegistry::new()),
                Arc::new(McpManager::new()),
            )
            .unwrap();

        assert_eq!(output, serde_json::json!({ "value": 42 }));
        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].function, "memory");
        assert_eq!(records[1].function, "memory");
        assert_eq!(records[1].result, serde_json::json!({ "value": 42 }));
    }

    fn node_imports_runtime() -> TypeScriptVmRuntime {
        let mut policy = RuntimePolicy::durable_default("ts-fs-test");
        policy.typescript_imports = crate::runtime::snapshot::TypeScriptImportPolicy::Node;
        TypeScriptVmRuntime::new(policy).unwrap()
    }

    #[test]
    fn vfs_is_reachable_under_durable_default_policy() {
        // Regression guard: the durable default policy must resolve `node:fs`
        // so the captured VFS is usable in production without setting
        // CHIDORI_TS_IMPORTS. `runtime()` uses RuntimePolicy::durable_default.
        let source = r#"
            import { writeFileSync, readFileSync } from "node:fs";
            export async function agent(input, chidori) {
                writeFileSync("/note.txt", "reachable");
                return { text: readFileSync("/note.txt", "utf8") };
            }
        "#;
        let out = run_agent(&runtime(), source, RuntimeContext::new());
        assert_eq!(out, serde_json::json!({ "text": "reachable" }));
    }

    #[test]
    fn node_fs_round_trips_through_captured_vfs() {
        use crate::runtime::capability::Capability;
        let source = r#"
            import * as fs from "node:fs";
            export async function agent(input, chidori) {
                fs.mkdirSync("/work", { recursive: true });
                fs.writeFileSync("/work/note.txt", "hello vfs");
                const text = fs.readFileSync("/work/note.txt", "utf8");
                const exists = fs.existsSync("/work/note.txt");
                const entries = fs.readdirSync("/work");
                return { text, exists, entries };
            }
        "#;
        let runtime_ctx = RuntimeContext::new();
        let (tokio_rt, policy, policy_cache) = runtime_host();

        let output = node_imports_runtime()
            .run_agent_source_with_context(
                Path::new("/tmp/agent.ts"),
                source,
                &serde_json::json!({}),
                runtime_ctx.clone(),
                Arc::new(ProviderRegistry::new()),
                template_engine(),
                tokio_rt,
                policy,
                policy_cache,
                Arc::new(ToolRegistry::new()),
                Arc::new(McpManager::new()),
            )
            .unwrap();

        assert_eq!(
            output,
            serde_json::json!({
                "text": "hello vfs",
                "exists": true,
                "entries": ["note.txt"],
            })
        );
        // Capability flags are raised, but VFS ops are not call-logged (the
        // tree rides the snapshot, so there is nothing to replay).
        let caps = runtime_ctx.capabilities();
        assert!(caps.contains(Capability::FsWrite));
        assert!(caps.contains(Capability::FsRead));
        assert!(runtime_ctx.vfs_snapshot().exists("/work/note.txt"));
        assert!(runtime_ctx
            .call_log()
            .into_records()
            .iter()
            .all(|r| r.function != "fs"));
    }

    fn run_agent(rt: &TypeScriptVmRuntime, source: &str, ctx: RuntimeContext) -> serde_json::Value {
        let (tokio_rt, policy, policy_cache) = runtime_host();
        rt.run_agent_source_with_context(
            Path::new("/tmp/agent.ts"),
            source,
            &serde_json::json!({}),
            ctx,
            Arc::new(ProviderRegistry::new()),
            template_engine(),
            tokio_rt,
            policy,
            policy_cache,
            Arc::new(ToolRegistry::new()),
            Arc::new(McpManager::new()),
        )
        .unwrap()
    }

    #[test]
    fn virtual_timers_fire_in_deadline_order_and_advance_clock() {
        use crate::runtime::capability::Capability;
        // Timers scheduled out of deadline order must fire in deadline order,
        // and Date.now() must reflect the logical clock advanced by the timers.
        let source = r#"
            export async function agent(input, chidori) {
                const order = [];
                const t0 = Date.now();
                await new Promise((resolve) => {
                    setTimeout(() => { order.push("b@" + Date.now()); }, 50);
                    setTimeout(() => { order.push("a@" + Date.now()); }, 10);
                    setTimeout(() => { order.push("c@" + Date.now()); resolve(); }, 100);
                });
                return { order, t0, tEnd: Date.now() };
            }
        "#;
        let ctx = RuntimeContext::new();
        let out = run_agent(&runtime(), source, ctx.clone());
        assert_eq!(
            out["order"],
            serde_json::json!(["a@10", "b@50", "c@100"])
        );
        assert_eq!(out["t0"], 0);
        assert_eq!(out["tEnd"], 100);
        assert!(ctx.capabilities().contains(Capability::Timer));
    }

    #[test]
    fn set_interval_repeats_until_cleared() {
        let source = r#"
            export async function agent(input, chidori) {
                let ticks = 0;
                await new Promise((resolve) => {
                    const handle = setInterval(() => {
                        ticks += 1;
                        if (ticks === 3) { clearInterval(handle); resolve(); }
                    }, 20);
                });
                return { ticks, now: Date.now() };
            }
        "#;
        let out = run_agent(&runtime(), source, RuntimeContext::new());
        assert_eq!(out["ticks"], 3);
        assert_eq!(out["now"], 60);
    }

    #[test]
    fn timers_coexist_with_recorded_host_calls_and_replay_identically() {
        // Timers interleaved with recorded host calls must reconstruct
        // deterministically on the replay-from-top path: the host calls replay
        // from the log while the timer queue is rebuilt by re-execution.
        let source = r#"
            export async function agent(input, chidori) {
                const events = [];
                await chidori.log("start");
                await new Promise((resolve) => {
                    setTimeout(() => { events.push("b@" + Date.now()); }, 10);
                    setTimeout(() => { events.push("a@" + Date.now()); resolve(); }, 5);
                });
                await chidori.log("end");
                return { events, now: Date.now() };
            }
        "#;
        let ctx = RuntimeContext::new();
        let first = run_agent(&runtime(), source, ctx.clone());
        assert_eq!(first["events"], serde_json::json!(["a@5", "b@10"]));
        assert_eq!(first["now"], 10);
        let records = ctx.call_log().into_records();
        assert_eq!(
            records.iter().filter(|r| r.function == "log").count(),
            2,
            "both log host calls are recorded"
        );

        // Replay from the recorded log: host calls replay, timers re-run.
        let replay_ctx = RuntimeContext::with_replay(records);
        let second = run_agent(&runtime(), source, replay_ctx);
        assert_eq!(first, second, "replay reproduces timer-driven output");
    }

    #[test]
    fn timers_disabled_policy_throws() {
        let source = r#"
            export async function agent(input, chidori) {
                setTimeout(() => {}, 0);
                return { ok: true };
            }
        "#;
        let mut policy = RuntimePolicy::durable_default("ts-timers-disabled");
        policy.timers = crate::runtime::snapshot::TimerPolicy::Disabled;
        let (tokio_rt, pol, cache) = runtime_host();
        let err = TypeScriptVmRuntime::new(policy)
            .unwrap()
            .run_agent_source_with_context(
                Path::new("/tmp/agent.ts"),
                source,
                &serde_json::json!({}),
                RuntimeContext::new(),
                Arc::new(ProviderRegistry::new()),
                template_engine(),
                tokio_rt,
                pol,
                cache,
                Arc::new(ToolRegistry::new()),
                Arc::new(McpManager::new()),
            )
            .unwrap_err();
        assert!(err.to_string().contains("timers are disabled"));
    }

    #[test]
    fn node_crypto_hash_is_inline_and_flagged() {
        use crate::runtime::capability::Capability;
        // sha256("abc") known vector, hex-encoded.
        let source = r#"
            import { createHash } from "node:crypto";
            export async function agent(input, chidori) {
                const hex = createHash("sha256").update("abc").digest("hex");
                return { hex };
            }
        "#;
        let runtime_ctx = RuntimeContext::new();
        let (tokio_rt, policy, policy_cache) = runtime_host();
        let output = node_imports_runtime()
            .run_agent_source_with_context(
                Path::new("/tmp/agent.ts"),
                source,
                &serde_json::json!({}),
                runtime_ctx.clone(),
                Arc::new(ProviderRegistry::new()),
                template_engine(),
                tokio_rt,
                policy,
                policy_cache,
                Arc::new(ToolRegistry::new()),
                Arc::new(McpManager::new()),
            )
            .unwrap();
        assert_eq!(
            output["hex"],
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(runtime_ctx.capabilities().contains(Capability::CryptoHash));
        // Hashing is deterministic, so nothing is call-logged.
        assert!(runtime_ctx
            .call_log()
            .into_records()
            .iter()
            .all(|r| !r.function.starts_with("crypto")));
    }

    #[test]
    fn node_crypto_random_is_captured_and_replays() {
        use crate::runtime::capability::Capability;
        let source = r#"
            import { randomBytes } from "node:crypto";
            export async function agent(input, chidori) {
                return { hex: randomBytes(16).toString("hex") };
            }
        "#;
        // First run: captures real random bytes into the call log.
        let ctx = RuntimeContext::new();
        let (tokio_rt, policy, policy_cache) = runtime_host();
        let first = node_imports_runtime()
            .run_agent_source_with_context(
                Path::new("/tmp/agent.ts"),
                source,
                &serde_json::json!({}),
                ctx.clone(),
                Arc::new(ProviderRegistry::new()),
                template_engine(),
                tokio_rt,
                policy,
                policy_cache,
                Arc::new(ToolRegistry::new()),
                Arc::new(McpManager::new()),
            )
            .unwrap();
        assert!(ctx.capabilities().contains(Capability::CryptoRandom));
        let records = ctx.call_log().into_records();
        let random_records: Vec<_> = records
            .iter()
            .filter(|r| r.function == "crypto.random")
            .collect();
        assert_eq!(random_records.len(), 1, "random draw is captured once");

        // Replay run: feeding the recorded log back reproduces the exact bytes
        // without drawing fresh randomness.
        let (tokio_rt2, policy2, policy_cache2) = runtime_host();
        let replay_ctx = RuntimeContext::with_replay(records.clone());
        let second = node_imports_runtime()
            .run_agent_source_with_context(
                Path::new("/tmp/agent.ts"),
                source,
                &serde_json::json!({}),
                replay_ctx,
                Arc::new(ProviderRegistry::new()),
                template_engine(),
                tokio_rt2,
                policy2,
                policy_cache2,
                Arc::new(ToolRegistry::new()),
                Arc::new(McpManager::new()),
            )
            .unwrap();
        assert_eq!(first["hex"], second["hex"], "replay reproduces bytes");
    }

    #[test]
    fn web_crypto_global_digest_and_uuid() {
        // globalThis.crypto (no import) must provide subtle.digest, randomUUID,
        // and getRandomValues, routed through the captured native.
        let source = r#"
            export async function agent(input, chidori) {
                const buf = await crypto.subtle.digest("SHA-256", new TextEncoder().encode("abc"));
                const bytes = new Uint8Array(buf);
                let hex = "";
                for (let i = 0; i < bytes.length; i++) hex += bytes[i].toString(16).padStart(2, "0");
                const arr = new Uint8Array(4);
                crypto.getRandomValues(arr);
                return { hex, uuid: crypto.randomUUID(), filled: arr.length };
            }
        "#;
        let out = run_agent(&node_imports_runtime(), source, RuntimeContext::new());
        assert_eq!(
            out["hex"],
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // UUID v4 shape: 8-4-4-4-12 with version nibble 4.
        let uuid = out["uuid"].as_str().unwrap();
        assert_eq!(uuid.len(), 36);
        assert_eq!(&uuid[14..15], "4");
        assert_eq!(out["filled"], 4);
    }

    #[test]
    fn node_fs_promises_async_api() {
        let source = r#"
            import { mkdir, writeFile, readFile, readdir } from "node:fs/promises";
            export async function agent(input, chidori) {
                await mkdir("/p", { recursive: true });
                await writeFile("/p/a.txt", "async");
                const text = await readFile("/p/a.txt", "utf8");
                const entries = await readdir("/p");
                return { text, entries };
            }
        "#;
        let out = run_agent(&node_imports_runtime(), source, RuntimeContext::new());
        assert_eq!(
            out,
            serde_json::json!({ "text": "async", "entries": ["a.txt"] })
        );
    }

    #[test]
    fn seeded_crypto_is_reproducible_across_runs() {
        // Under CryptoPolicy::Seeded, two independent runs with the same seed
        // draw identical bytes (no capture needed for reproducibility).
        let source = r#"
            import { randomBytes } from "node:crypto";
            export async function agent(input, chidori) {
                return { hex: randomBytes(16).toString("hex") };
            }
        "#;
        let mut policy = RuntimePolicy::durable_default("seed-fixed");
        policy.typescript_imports = crate::runtime::snapshot::TypeScriptImportPolicy::Node;
        policy.crypto = crate::runtime::snapshot::CryptoPolicy::Seeded;

        let run_once = || {
            let rt = TypeScriptVmRuntime::new(policy.clone()).unwrap();
            run_agent(&rt, source, RuntimeContext::new())
        };
        let a = run_once();
        let b = run_once();
        assert_eq!(a["hex"], b["hex"], "seeded crypto is reproducible");
    }

    #[test]
    fn node_fs_disabled_policy_throws() {
        let source = r#"
            import * as fs from "node:fs";
            export async function agent(input, chidori) {
                fs.writeFileSync("/x.txt", "nope");
                return { ok: true };
            }
        "#;
        let mut policy = RuntimePolicy::durable_default("ts-fs-disabled");
        policy.typescript_imports = crate::runtime::snapshot::TypeScriptImportPolicy::Node;
        policy.fs = crate::runtime::snapshot::FsPolicy::Disabled;
        let runtime_ctx = RuntimeContext::new();
        let (tokio_rt, pol, cache) = runtime_host();

        let err = TypeScriptVmRuntime::new(policy)
            .unwrap()
            .run_agent_source_with_context(
                Path::new("/tmp/agent.ts"),
                source,
                &serde_json::json!({}),
                runtime_ctx,
                Arc::new(ProviderRegistry::new()),
                template_engine(),
                tokio_rt,
                pol,
                cache,
                Arc::new(ToolRegistry::new()),
                Arc::new(McpManager::new()),
            )
            .unwrap_err();
        assert!(err.to_string().contains("fs=disabled") || err.to_string().contains("disabled"));
    }

    #[test]
    fn runtime_context_records_template_calls() {
        let source = r#"
            export async function agent(input, chidori) {
                const rendered = await chidori.template("Hello {{ name }}!", { name: input.name });
                return { rendered };
            }
        "#;
        let runtime_ctx = RuntimeContext::new();
        let (tokio_rt, policy, policy_cache) = runtime_host();

        let output = runtime()
            .run_agent_source_with_context(
                Path::new("/tmp/agent.ts"),
                source,
                &serde_json::json!({ "name": "TypeScript" }),
                runtime_ctx.clone(),
                Arc::new(ProviderRegistry::new()),
                template_engine(),
                tokio_rt,
                policy,
                policy_cache,
                Arc::new(ToolRegistry::new()),
                Arc::new(McpManager::new()),
            )
            .unwrap();

        assert_eq!(
            output,
            serde_json::json!({ "rendered": "Hello TypeScript!" })
        );
        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "template");
        assert_eq!(records[0].result, serde_json::json!("Hello TypeScript!"));
    }

    #[test]
    fn runtime_context_rejects_wrong_shape_template_vars() {
        let source = r#"
            export async function agent(input, chidori) {
                return await chidori.template(
                    "{% for source in sources %}{{ source.title }}{% endfor %}",
                    { sources: "not a source list" }
                );
            }
        "#;
        let runtime_ctx = RuntimeContext::new();
        let (tokio_rt, policy, policy_cache) = runtime_host();

        let error = runtime()
            .run_agent_source_with_context(
                Path::new("/tmp/agent.ts"),
                source,
                &serde_json::json!({}),
                runtime_ctx.clone(),
                Arc::new(ProviderRegistry::new()),
                template_engine(),
                tokio_rt,
                policy,
                policy_cache,
                Arc::new(ToolRegistry::new()),
                Arc::new(McpManager::new()),
            )
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("Failed to render inline template"));
        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "template");
        assert!(records[0].error.is_some());
    }

    #[test]
    fn runtime_context_runs_try_call_and_retry_helpers() {
        let source = r#"
            export async function agent(input, chidori) {
                let attempts = 0;
                const value = await chidori.retry(async () => {
                    attempts += 1;
                    if (attempts < 2) {
                        throw new Error("again");
                    }
                    return 42;
                }, { attempts: 3 });
                const caught = await chidori.tryCall(async () => {
                    throw new Error("handled");
                });
                return { value, attempts, caught };
            }
        "#;
        let runtime_ctx = RuntimeContext::new();
        let (tokio_rt, policy, policy_cache) = runtime_host();

        let output = runtime()
            .run_agent_source_with_context(
                Path::new("/tmp/agent.ts"),
                source,
                &serde_json::json!({}),
                runtime_ctx,
                Arc::new(ProviderRegistry::new()),
                template_engine(),
                tokio_rt,
                policy,
                policy_cache,
                Arc::new(ToolRegistry::new()),
                Arc::new(McpManager::new()),
            )
            .unwrap();

        assert_eq!(
            output,
            serde_json::json!({
                "value": 42,
                "attempts": 2,
                "caught": {
                    "ok": false,
                    "error": "handled",
                },
            })
        );
    }

    #[test]
    fn runtime_context_replays_log_calls() {
        let source = r#"
            export async function agent(input, chidori) {
                await chidori.log("live");
                return { ok: true };
            }
        "#;
        let replay = vec![crate::runtime::call_log::CallRecord {
            seq: 1,
            parent_seq: None,
            function: "log".to_string(),
            args: serde_json::json!({ "message": "cached" }),
            result: serde_json::Value::Null,
            duration_ms: 11,
            token_usage: None,
            timestamp: chrono::Utc::now(),
            error: None,
        }];
        let runtime_ctx = RuntimeContext::with_replay(replay.clone());
        let (tokio_rt, policy, policy_cache) = runtime_host();

        let output = runtime()
            .run_agent_source_with_context(
                Path::new("/tmp/agent.ts"),
                source,
                &serde_json::json!({}),
                runtime_ctx.clone(),
                Arc::new(ProviderRegistry::new()),
                template_engine(),
                tokio_rt,
                policy,
                policy_cache,
                Arc::new(ToolRegistry::new()),
                Arc::new(McpManager::new()),
            )
            .unwrap();

        assert_eq!(output, serde_json::json!({ "ok": true }));
        let records = runtime_ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].args, replay[0].args);
        assert_eq!(records[0].duration_ms, 11);
    }

    #[test]
    fn unsupported_host_operation_fails_loudly() {
        let source = r#"
            export async function agent(input, chidori) {
                return await chidori.parallel([1]);
            }
        "#;

        let err = runtime()
            .run_agent_source(Path::new("/tmp/agent.ts"), source, &serde_json::json!({}))
            .unwrap_err();

        assert!(err
            .to_string()
            .contains("chidori.parallel task 0 must be a function"));
    }

    #[test]
    fn seeded_random_is_deterministic_for_same_policy() {
        let source = r#"
            export async function agent(input, chidori) {
                return { values: [Math.random(), Math.random()] };
            }
        "#;
        let policy = RuntimePolicy::durable_default("same-run");
        let runtime_a = TypeScriptVmRuntime::new(policy.clone()).unwrap();
        let runtime_b = TypeScriptVmRuntime::new(policy).unwrap();

        let a = runtime_a
            .run_agent_source(Path::new("/tmp/agent.ts"), source, &serde_json::json!({}))
            .unwrap();
        let b = runtime_b
            .run_agent_source(Path::new("/tmp/agent.ts"), source, &serde_json::json!({}))
            .unwrap();

        assert_eq!(a.output, b.output);
    }

    #[test]
    fn map_and_set_reject_policy_fails_clearly() {
        let source = r#"
            export async function agent(input, chidori) {
                return { map: new Map() };
            }
        "#;

        let err = runtime()
            .run_agent_source(Path::new("/tmp/agent.ts"), source, &serde_json::json!({}))
            .unwrap_err();

        assert!(err.to_string().contains("Map is disabled"));
    }

    #[test]
    fn weak_ref_and_finalizers_fail_clearly() {
        let source = r#"
            export async function agent(input, chidori) {
                return { weak: new WeakRef({}) };
            }
        "#;
        let err = runtime()
            .run_agent_source(Path::new("/tmp/agent.ts"), source, &serde_json::json!({}))
            .unwrap_err();
        assert!(err.to_string().contains("WeakRef is disabled"));

        let source = r#"
            export async function agent(input, chidori) {
                return { registry: new FinalizationRegistry(() => {}) };
            }
        "#;
        let err = runtime()
            .run_agent_source(Path::new("/tmp/agent.ts"), source, &serde_json::json!({}))
            .unwrap_err();
        assert!(err.to_string().contains("FinalizationRegistry is disabled"));
    }

    #[test]
    fn shared_memory_fails_clearly() {
        let source = r#"
            export async function agent(input, chidori) {
                return { buffer: new SharedArrayBuffer(8) };
            }
        "#;
        let err = runtime()
            .run_agent_source(Path::new("/tmp/agent.ts"), source, &serde_json::json!({}))
            .unwrap_err();
        assert!(err.to_string().contains("SharedArrayBuffer is disabled"));
    }

    #[test]
    fn fixed_date_policy_makes_new_date_deterministic() {
        let source = r#"
            export async function agent(input, chidori) {
                return { now: Date.now(), iso: new Date().toISOString() };
            }
        "#;

        let result = runtime()
            .run_agent_source(Path::new("/tmp/agent.ts"), source, &serde_json::json!({}))
            .unwrap();

        assert_eq!(
            result.output,
            serde_json::json!({ "now": 0, "iso": "1970-01-01T00:00:00.000Z" })
        );
    }

    #[test]
    fn disabled_date_policy_fails_clearly() {
        let mut policy = RuntimePolicy::durable_default("date-disabled");
        policy.date = DatePolicy::Disabled;
        let runtime = TypeScriptVmRuntime::new(policy).unwrap();
        let source = r#"
            export async function agent(input, chidori) {
                return { now: Date.now() };
            }
        "#;

        let err = runtime
            .run_agent_source(Path::new("/tmp/agent.ts"), source, &serde_json::json!({}))
            .unwrap_err();

        assert!(err.to_string().contains("Date.now is disabled"));
    }
}
