use std::path::Path;

use anyhow::{Context, Result};

use crate::runtime::snapshot::RuntimePolicy;
use crate::runtime::typescript::tools::{
    check_tool_source, source_declares_tool, TypeScriptToolCheck,
};
use crate::runtime::typescript::transpile::{transpile_module, TranspileOptions};

#[allow(dead_code)]
pub struct TypeScriptCheck {
    pub javascript: String,
}

#[allow(dead_code)]
pub enum TypeScriptFileCheck {
    Agent(TypeScriptCheck),
    Tool(TypeScriptToolCheck),
}

pub fn check_typescript_file(path: &Path, policy: &RuntimePolicy) -> Result<TypeScriptFileCheck> {
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    if source_declares_tool(&source) {
        return Ok(TypeScriptFileCheck::Tool(check_tool_source(
            path, &source, policy,
        )?));
    }

    Ok(TypeScriptFileCheck::Agent(check_agent_source(
        path, &source, policy,
    )?))
}

#[allow(dead_code)]
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
        &source,
        &TranspileOptions {
            import_policy: policy.typescript_imports,
        },
    )?;

    if !exports_async_function(&javascript, "agent") {
        anyhow::bail!(
            "No `export async function agent(input, chidori)` found in {}",
            path.display()
        );
    }

    Ok(TypeScriptCheck { javascript })
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
    fn check_agent_rejects_missing_agent_export() {
        let policy = RuntimePolicy::durable_default("run");
        let dir = std::env::temp_dir().join(format!("chidori-ts-check-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.ts");
        std::fs::write(&path, "export async function other() { return 1; }").unwrap();

        assert!(check_agent_file(&path, &policy).is_err());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn check_typescript_file_dispatches_tool_exports() {
        let policy = RuntimePolicy::durable_default("run");
        let dir = std::env::temp_dir().join(format!("chidori-ts-check-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("search.ts");
        std::fs::write(
            &path,
            r#"
                export const tool = {
                    name: "search",
                    description: "Search things",
                    parameters: { type: "object", properties: {} },
                };

                export async function run(args, chidori) {
                    return {};
                }
            "#,
        )
        .unwrap();

        let result = check_typescript_file(&path, &policy).unwrap();
        assert!(matches!(result, TypeScriptFileCheck::Tool(_)));

        let _ = std::fs::remove_dir_all(dir);
    }
}
