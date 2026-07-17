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
