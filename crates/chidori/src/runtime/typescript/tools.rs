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
    use crate::runtime::snapshot::TypeScriptImportPolicy;

    let mut engine = chidori_js::Engine::new();
    // Deterministic, side-effect-free metadata evaluation: disable host effects,
    // network, timers, Date, and randomness. Mirror the agent runtime's benign
    // globals (process.env from CHIDORI_AGENT_ENV, URLSearchParams) so a tool
    // that's valid at run time isn't rejected at discovery.
    let mut prelude = String::from(
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
    prelude.push_str(&format!(
        "globalThis.process = Object.freeze({{ env: Object.freeze({}) }});\n",
        super::helpers::chidori_agent_env_json()
    ));
    prelude.push_str(super::helpers::URL_SEARCH_PARAMS_POLYFILL);
    engine
        .eval(&prelude)
        .map_err(|err| anyhow::anyhow!("installing tool metadata prelude: {err}"))?;

    // Resolve relative + `node:` imports the same way the runtime engine does,
    // so a tool that splits its schema across sibling modules still discovers.
    let mut load =
        |specifier: &str, importer_key: &str| -> std::result::Result<(String, String), String> {
            if let Some(name) = specifier.strip_prefix("node:") {
                let src = super::builtins::shim_source(name)
                    .ok_or_else(|| format!("unsupported node: builtin '{specifier}'"))?;
                return Ok((format!("node:{name}"), src.to_string()));
            }
            let importer = Path::new(importer_key);
            let dir = importer.parent().unwrap_or_else(|| Path::new("."));
            let resolved = crate::runtime::typescript::transpile::resolve_relative_import(
                importer, dir, specifier, 0,
            )
            .map_err(|e| e.to_string())?;
            let key = resolved.to_string_lossy().to_string();
            let src = std::fs::read_to_string(&resolved)
                .map_err(|e| format!("reading module {}: {e}", resolved.display()))?;
            let js = transpile_module(
                &resolved,
                &src,
                &TranspileOptions {
                    import_policy: TypeScriptImportPolicy::Node,
                },
            )
            .map_err(|e| e.to_string())?;
            Ok((key, js))
        };

    let entry_key = path.display().to_string();
    let result = engine.eval_module_export(&entry_key, javascript, "tool", &mut load);
    // The export is already a host `serde_json::Value`; break the heap's Rc
    // cycles so per-tool discovery doesn't leak a realm each call.
    engine.vm.dispose();
    let tool = result
        .map_err(|err| {
            let bundle = write_tool_eval_failure_bundle(path, javascript)
                .map(|path| format!("; transpiled tool bundle written to {}", path.display()))
                .unwrap_or_default();
            anyhow::anyhow!("{err}{bundle}")
        })
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
            import { Chidori, ToolDefinition } from "chidori:agent";

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
        assert!(result
            .javascript
            .contains("export async function run(args, chidori)"));
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
