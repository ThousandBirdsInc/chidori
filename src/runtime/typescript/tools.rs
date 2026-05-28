use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::runtime::snapshot::RuntimePolicy;
use crate::runtime::typescript::transpile::{transpile_module, TranspileOptions};

#[allow(dead_code)]
#[derive(Debug)]
pub struct TypeScriptToolCheck {
    pub javascript: String,
    pub tool: TypeScriptToolDefinition,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TypeScriptToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[allow(dead_code)]
pub fn check_tool_file(path: &Path, policy: &RuntimePolicy) -> Result<TypeScriptToolCheck> {
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    check_tool_source(path, &source, policy)
}

pub fn check_tool_source(
    path: &Path,
    source: &str,
    policy: &RuntimePolicy,
) -> Result<TypeScriptToolCheck> {
    if !source_declares_tool(source) {
        anyhow::bail!("No `export const tool` found in {}", path.display());
    }

    let javascript = transpile_module(
        path,
        source,
        &TranspileOptions {
            import_policy: policy.typescript_imports,
        },
    )?;

    if !exports_async_function(&javascript, "run") {
        anyhow::bail!(
            "No `export async function run(args, chidori)` found in {}",
            path.display()
        );
    }

    let tool = evaluate_tool_definition(path, &javascript)?;

    Ok(TypeScriptToolCheck { javascript, tool })
}

pub(super) fn source_declares_tool(source: &str) -> bool {
    source.contains("export const tool")
}

fn exports_async_function(source: &str, name: &str) -> bool {
    let needle = format!("export async function {name}");
    source.contains(&needle)
}

fn evaluate_tool_definition(path: &Path, javascript: &str) -> Result<TypeScriptToolDefinition> {
    let runtime = chidori_quickjs::SnapshotRuntime::new(chidori_quickjs::RuntimeLimits::default())
        .map_err(|err| anyhow::anyhow!(err))?;
    let mut context = runtime.new_context().map_err(|err| anyhow::anyhow!(err))?;
    let module_name = path.display().to_string();
    let mut source = String::from(
        r#"
            globalThis.chidori = undefined;
            globalThis.fetch = undefined;
            globalThis.XMLHttpRequest = undefined;
            globalThis.setTimeout = undefined;
            globalThis.setInterval = undefined;
            globalThis.Date = function Date() {
              throw new Error("Date is disabled during Chidori tool metadata evaluation");
            };
            Math.random = function random() {
              throw new Error("Math.random is disabled during Chidori tool metadata evaluation");
            };
            "#,
    );
    // Mirror the agent runtime's globals so a tool that's valid at run time
    // isn't rejected at discovery: populate process.env from CHIDORI_AGENT_ENV
    // and provide URLSearchParams. (chidori/fetch/Date/Math.random stay
    // disabled — metadata evaluation must be deterministic and side-effect free.)
    source.push_str(&format!(
        "globalThis.process = Object.freeze({{ env: Object.freeze({}) }});\n",
        super::snapshot::chidori_agent_env_json()
    ));
    source.push_str(super::snapshot::URL_SEARCH_PARAMS_POLYFILL);
    source.push_str(javascript);

    context
        .eval_module(&module_name, &source)
        .map_err(|err| {
            let bundle = write_tool_eval_failure_bundle(path, &source)
                .map(|path| format!("; transpiled tool bundle written to {}", path.display()))
                .unwrap_or_default();
            anyhow::anyhow!("{err}{bundle}")
        })
        .with_context(|| format!("evaluating {}", path.display()))?;
    let tool = context
        .eval_json_expression(
            "tool-metadata.js",
            r#"
            (() => {
                if (!Object.prototype.hasOwnProperty.call(globalThis.__chidori_exports || {}, "tool")) {
                    throw new Error("missing exported `tool` value");
                }
                return globalThis.__chidori_exports.tool;
            })()
            "#,
        )
        .map_err(|err| anyhow::anyhow!(err))
        .with_context(|| {
            format!(
                "{}: exported `tool` metadata must be JSON-compatible",
                path.display()
            )
        })?;
    parse_tool_definition(path, tool)
}

fn write_tool_eval_failure_bundle(
    path: &Path,
    javascript: &str,
) -> std::io::Result<std::path::PathBuf> {
    let stem = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("tool")
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    let bundle_path = std::env::temp_dir().join(format!(
        "chidori-tool-eval-failed-{}-{}-{}.js",
        std::process::id(),
        stem,
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&bundle_path, javascript)?;
    Ok(bundle_path)
}

fn parse_tool_definition(path: &Path, value: Value) -> Result<TypeScriptToolDefinition> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("{}: `tool` export must be an object", path.display()))?;

    let name = string_field(path, object, "name")?;
    let description = string_field(path, object, "description")?;
    if name.trim().is_empty() {
        anyhow::bail!("{}: `tool.name` must not be empty", path.display());
    }
    if description.trim().is_empty() {
        anyhow::bail!("{}: `tool.description` must not be empty", path.display());
    }

    let parameters = object.get("parameters").cloned().ok_or_else(|| {
        anyhow::anyhow!(
            "{}: `tool.parameters` JSON schema is required",
            path.display()
        )
    })?;
    validate_parameters_schema(path, &parameters)?;

    Ok(TypeScriptToolDefinition {
        name,
        description,
        parameters,
    })
}

fn string_field(
    path: &Path,
    object: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow::anyhow!("{}: `tool.{field}` must be a string", path.display()))
}

fn validate_parameters_schema(path: &Path, parameters: &Value) -> Result<()> {
    let object = parameters.as_object().ok_or_else(|| {
        anyhow::anyhow!(
            "{}: `tool.parameters` must be a JSON object",
            path.display()
        )
    })?;
    if object.get("type").and_then(Value::as_str) != Some("object") {
        anyhow::bail!(
            "{}: `tool.parameters.type` must be \"object\"",
            path.display()
        );
    }
    if let Some(properties) = object.get("properties") {
        if !properties.is_object() {
            anyhow::bail!(
                "{}: `tool.parameters.properties` must be an object",
                path.display()
            );
        }
    }
    if let Some(required) = object.get("required") {
        let Some(items) = required.as_array() else {
            anyhow::bail!(
                "{}: `tool.parameters.required` must be an array",
                path.display()
            );
        };
        if items.iter().any(|item| !item.is_string()) {
            anyhow::bail!(
                "{}: `tool.parameters.required` entries must be strings",
                path.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> RuntimePolicy {
        RuntimePolicy::durable_default("run")
    }

    #[test]
    fn accepts_tool_metadata_and_run_export() {
        let source = r#"
            import { Chidori, ToolDefinition } from "chidori";

            const apiKey = process.env.BRAVE_SEARCH_API_KEY ?? "";

            export const tool = {
                name: "web_search",
                description: "Search the web",
                parameters: {
                    type: "object",
                    properties: {
                        query: { type: "string", description: "Search query" },
                    },
                    required: ["query"] as const,
                },
            } satisfies ToolDefinition;

            export async function run(
                args: { query: string },
                chidori: Chidori,
            ): Promise<{ ok: boolean; query: string }> {
                return { ok: true, query: args.query, has_key: apiKey.length > 0 };
            }
        "#;

        let result = check_tool_source(
            Path::new("/tmp/project/tools/web_search.ts"),
            source,
            &policy(),
        )
        .unwrap();

        assert_eq!(result.tool.name, "web_search");
        assert_eq!(result.tool.description, "Search the web");
        assert!(result.javascript.contains("export async function run("));
        assert!(result.javascript.contains("args,"));
        assert!(result.javascript.contains("chidori,"));
        assert!(!result.javascript.contains("satisfies ToolDefinition"));
        assert!(!result.javascript.contains("Promise<{"));
    }

    #[test]
    fn rejects_tool_without_run_export() {
        let source = r#"
            export const tool = {
                name: "web_search",
                description: "Search the web",
                parameters: { type: "object", properties: {} },
            };
        "#;

        let err = check_tool_source(
            Path::new("/tmp/project/tools/web_search.ts"),
            source,
            &policy(),
        )
        .unwrap_err();

        assert!(err.to_string().contains("export async function run"));
    }

    #[test]
    fn rejects_invalid_parameters_schema() {
        let source = r#"
            export const tool = {
                name: "web_search",
                description: "Search the web",
                parameters: { type: "string" },
            };

            export async function run(args, chidori) {
                return {};
            }
        "#;

        let err = check_tool_source(
            Path::new("/tmp/project/tools/web_search.ts"),
            source,
            &policy(),
        )
        .unwrap_err();

        assert!(err.to_string().contains("parameters.type"));
    }

    #[test]
    fn parses_single_quoted_metadata() {
        let source = r#"
            export const tool = {
                name: 'lookup',
                description: 'Lookup "quoted" text',
                parameters: { type: 'object', required: ['query'] },
            };

            export async function run(args, chidori) {
                return {};
            }
        "#;

        let result =
            check_tool_source(Path::new("/tmp/project/tools/lookup.ts"), source, &policy())
                .unwrap();

        assert_eq!(result.tool.description, "Lookup \"quoted\" text");
    }

    #[test]
    fn evaluates_computed_tool_metadata_in_restricted_vm() {
        let source = r#"
            const baseName = "lookup";
            const textProperty = { type: "string", description: "Search query" };
            const parameters = {
                type: "object",
                properties: { query: textProperty },
                required: ["query"],
            };

            export const tool = {
                name: `${baseName}_tool`,
                description: ["Lookup", "tool"].join(" "),
                parameters,
            };

            export async function run(args, chidori) {
                return {};
            }
        "#;

        let result =
            check_tool_source(Path::new("/tmp/project/tools/lookup.ts"), source, &policy())
                .unwrap();

        assert_eq!(result.tool.name, "lookup_tool");
        assert_eq!(result.tool.description, "Lookup tool");
        assert_eq!(
            result.tool.parameters["properties"]["query"]["description"],
            "Search query"
        );
    }

    #[test]
    fn rejects_metadata_that_uses_disabled_host_nondeterminism() {
        let source = r#"
            export const tool = {
                name: "clock",
                description: new Date().toISOString(),
                parameters: { type: "object", properties: {} },
            };

            export async function run(args, chidori) {
                return {};
            }
        "#;

        let err = check_tool_source(Path::new("/tmp/project/tools/clock.ts"), source, &policy())
            .unwrap_err();

        assert!(format!("{err:?}").contains("Date is disabled"), "{err:?}");
    }

    #[test]
    fn rejects_metadata_that_uses_disabled_chidori_host_object() {
        let source = r#"
            export const tool = {
                name: "hosted",
                description: String(chidori.input("name")),
                parameters: { type: "object", properties: {} },
            };

            export async function run(args, chidori) {
                return {};
            }
        "#;

        let err = check_tool_source(Path::new("/tmp/project/tools/hosted.ts"), source, &policy())
            .unwrap_err();

        assert!(format!("{err:?}").contains("undefined"), "{err:?}");
    }
}
