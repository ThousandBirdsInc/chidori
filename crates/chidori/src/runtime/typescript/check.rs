use std::path::Path;

use anyhow::{Context, Result};

use crate::runtime::snapshot::RuntimePolicy;
use crate::runtime::typescript::transpile::{transpile_module, TranspileOptions};

pub struct TypeScriptCheck {
    #[allow(dead_code)]
    pub javascript: String,
}

/// Validate a `.ts` agent file: it must transpile and register an entrypoint
/// (`run(handler)` at the top level, or the legacy `export async function
/// agent`). Tools are no longer standalone files — they are defined in-agent
/// with `defineTool(...)` — so every `.ts` checked here is an agent.
pub fn check_agent_file(path: &Path, policy: &RuntimePolicy) -> Result<TypeScriptCheck> {
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    check_agent_source(path, &source, policy)
}

fn check_agent_source(
    path: &Path,
    source: &str,
    policy: &RuntimePolicy,
) -> Result<TypeScriptCheck> {
    let javascript = transpile_module(
        path,
        source,
        &TranspileOptions {
            import_policy: policy.typescript_imports,
        },
    )?;

    // Walk the FULL module graph — local imports, node_modules packages,
    // re-export edges, builtin shims — with the same resolver the runtime
    // uses, so an import that would fail at `chidori run` (a missing package
    // deep in a dependency, a non-allowlisted node builtin) fails here, with
    // the importing file and line. Checking only the entry file made `check`
    // vouch for graphs the engine could not load.
    crate::runtime::typescript::module_graph::snapshot_modules(path, source, policy)
        .context("resolving the agent's module graph")?;

    if !declares_run_entrypoint(&javascript) && !exports_async_function(&javascript, "agent") {
        anyhow::bail!(
            "No agent entrypoint found in {}: call `run(handler)` at the top level \
             (import it from \"chidori:agent\"), or export the legacy \
             `export async function agent(input, chidori)`",
            path.display()
        );
    }

    Ok(TypeScriptCheck { javascript })
}

/// Does the transpiled module register its entrypoint with `run(handler)`?
/// The `chidori:agent` import is stripped by transpilation, so a top-level
/// `run(...)` call is what remains of the canonical authoring style.
fn declares_run_entrypoint(source: &str) -> bool {
    source
        .lines()
        .any(|line| line.trim_start().starts_with("run("))
}

fn exports_async_function(source: &str, name: &str) -> bool {
    let needle = format!("export async function {name}");
    source.contains(&needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::snapshot::RuntimePolicy;

    #[test]
    fn check_agent_accepts_exported_async_agent() {
        let policy = RuntimePolicy::durable_default("run");
        let dir = std::env::temp_dir().join(format!("chidori-ts-check-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(
            &path,
            r#"
                import type { Chidori } from "chidori:agent";
                export async function agent(input: { name: string }, chidori: Chidori) {
                    return { hello: input.name };
                }
            "#,
        )
        .unwrap();

        let result = check_agent_file(&path, &policy).unwrap();
        assert!(result.javascript.contains("export async function agent"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn check_agent_accepts_run_entrypoint() {
        let policy = RuntimePolicy::durable_default("run");
        let dir = std::env::temp_dir().join(format!("chidori-ts-check-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(
            &path,
            r#"
                import { chidori, run } from "chidori:agent";
                run(async (input: { name: string }) => {
                    await chidori.log("hello", { name: input.name });
                    return { hello: input.name };
                });
            "#,
        )
        .unwrap();

        let result = check_agent_file(&path, &policy).unwrap();
        assert!(result.javascript.contains("run("));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn check_accepts_cyclic_local_imports() {
        // Cycles are legal ES modules; the engine links them. `check` (and the
        // manifest walk under it) must accept them rather than bail.
        let policy = RuntimePolicy::durable_default("run");
        let dir = std::env::temp_dir().join(format!("chidori-ts-check-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("agent.ts"),
            r#"
                import { chidori, run } from "chidori:agent";
                import { fromB } from "./b.ts";
                export function fromA(): string { return "A"; }
                run(async () => ({ out: fromB() }));
            "#,
        )
        .unwrap();
        std::fs::write(
            dir.join("b.ts"),
            r#"
                import { fromA } from "./agent.ts";
                export function fromB(): string { return fromA() + "B"; }
            "#,
        )
        .unwrap();

        check_agent_file(&dir.join("agent.ts"), &policy).unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn check_walks_reexport_edges_and_names_the_failing_file() {
        // A package whose entry only RE-EXPORTS (`export * from`) a module
        // that imports a non-allowlisted node builtin: `check` must follow the
        // re-export edge and fail naming the importing file — not say OK and
        // let `run` explode later.
        let policy = RuntimePolicy::durable_default("run");
        let dir = std::env::temp_dir().join(format!("chidori-ts-check-{}", uuid::Uuid::new_v4()));
        let pkg = dir.join("node_modules/badpkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(dir.join("package.json"), r#"{"name":"t","private":true}"#).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"badpkg","version":"1.0.0","type":"module","main":"index.js"}"#,
        )
        .unwrap();
        std::fs::write(pkg.join("index.js"), "export * from \"./impl.js\";\n").unwrap();
        std::fs::write(
            pkg.join("impl.js"),
            "import { createRequire } from \"node:module\";\nexport const x = 1;\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("agent.ts"),
            r#"
                import { chidori, run } from "chidori:agent";
                import { x } from "badpkg";
                run(async () => ({ x }));
            "#,
        )
        .unwrap();

        let err = match check_agent_file(&dir.join("agent.ts"), &policy) {
            Ok(_) => panic!("check should fail on the unshimmed builtin import"),
            // `{:#}` includes the whole context chain; the file/line live in
            // the inner resolver error.
            Err(err) => format!("{err:#}"),
        };
        assert!(err.contains("impl.js"), "error should name the file: {err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn check_agent_rejects_missing_agent_export() {
        let policy = RuntimePolicy::durable_default("run");
        let dir = std::env::temp_dir().join(format!("chidori-ts-check-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(&path, "export async function other() { return 1; }").unwrap();

        assert!(check_agent_file(&path, &policy).is_err());

        let _ = std::fs::remove_dir_all(dir);
    }
}
