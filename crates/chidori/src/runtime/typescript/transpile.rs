use std::path::{Path, PathBuf};

use anyhow::Result;
use oxc::allocator::Allocator;
use oxc::codegen::{Codegen, CodegenOptions, CommentOptions};
use oxc::parser::Parser;
use oxc::semantic::SemanticBuilder;
use oxc::span::SourceType;
use oxc::transformer::{TransformOptions, Transformer};

use crate::runtime::snapshot::TypeScriptImportPolicy;
use crate::runtime::typescript::resolver::{
    Resolution, ResolutionKind, Resolver, DEFAULT_CONDITIONS,
};

#[derive(Debug, Clone, Copy)]
pub struct TranspileOptions {
    pub import_policy: TypeScriptImportPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleImport {
    pub specifier: String,
    pub resolved_path: Option<PathBuf>,
    /// Kind of resolution, when the resolver was used. `None` for legacy
    /// `Relative`/`Project` paths where resolution shape isn't tracked.
    pub kind: Option<ResolutionKindTag>,
}

/// Plain-data mirror of `resolver::ResolutionKind` for callers that don't want
/// to depend on the resolver module directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolutionKindTag {
    Relative,
    Package { name: String, subpath: String },
    NodeBuiltin { name: String },
}

impl From<&ResolutionKind> for ResolutionKindTag {
    fn from(kind: &ResolutionKind) -> Self {
        match kind {
            ResolutionKind::Relative => ResolutionKindTag::Relative,
            ResolutionKind::Package { name, subpath } => ResolutionKindTag::Package {
                name: name.clone(),
                subpath: subpath.clone(),
            },
            ResolutionKind::NodeBuiltin { name } => {
                ResolutionKindTag::NodeBuiltin { name: name.clone() }
            }
        }
    }
}

/// Allowlist of `node:` builtins the resolver will accept under the `Node`
/// policy. The corresponding shim sources are registered by
/// `runtime::typescript::snapshot` when bundling.
pub const NODE_BUILTIN_ALLOWLIST: &[&str] = &[
    "process",
    "buffer",
    "util",
    "fs",
    "fs/promises",
    "crypto",
    "http",
    "https",
    "path",
    "path/posix",
    "events",
    "url",
    "assert",
    "assert/strict",
    "os",
];

/// Walk up from `start` looking for a `package.json` and return the directory
/// that contains it. Falls back to `start`'s parent (or the cwd) if none
/// exists in the chain — this keeps single-file agent harnesses working.
pub fn find_workspace_root(start: &Path) -> PathBuf {
    let mut dir = start.parent().map(Path::to_path_buf);
    while let Some(current) = dir {
        if current.join("package.json").is_file() {
            return current;
        }
        dir = current.parent().map(Path::to_path_buf);
    }
    start
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn transpile_module(path: &Path, source: &str, options: &TranspileOptions) -> Result<String> {
    validate_imports(path, source, options.import_policy)?;

    // Treat input as TypeScript regardless of extension — agents may live in
    // `agent.ts` / `tools/*.ts` and the snapshot pipeline only calls us with TS.
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::ts());
    let allocator = Allocator::default();

    let parser_ret = Parser::new(&allocator, source, source_type).parse();
    if !parser_ret.errors.is_empty() {
        let messages: Vec<String> = parser_ret
            .errors
            .iter()
            .map(|err| err.to_string())
            .collect();
        anyhow::bail!(
            "{}: TypeScript parse error: {}",
            path.display(),
            messages.join("; ")
        );
    }
    let mut program = parser_ret.program;

    let semantic_ret = SemanticBuilder::new()
        // Transformer roughly triples scope/symbol/reference allocations.
        .with_excess_capacity(2.0)
        .build(&program);
    if !semantic_ret.errors.is_empty() {
        let messages: Vec<String> = semantic_ret
            .errors
            .iter()
            .map(|err| err.to_string())
            .collect();
        anyhow::bail!(
            "{}: TypeScript semantic error: {}",
            path.display(),
            messages.join("; ")
        );
    }
    let scoping = semantic_ret.semantic.into_scoping();

    // Defaults strip TypeScript syntax (types, interfaces, `as`/`satisfies`,
    // `import type`) and leave modern JS untouched. `enable_all()` would also
    // downlevel async/await, optional chaining, and nullish coalescing into
    // helper-import-heavy generator code — the engine supports the
    // modern forms natively, so we explicitly avoid that.
    let mut transform_options = TransformOptions::default();
    // Lower JSX in `.tsx` agents to `React.createElement` (classic runtime) so it
    // runs against an in-scope `React` (the engine executes real React), rather
    // than the automatic runtime's unresolvable `react/jsx-runtime` imports.
    // No effect on `.ts` sources (not JSX) — only JSX syntax is rewritten.
    transform_options.jsx.runtime = oxc::transformer::JsxRuntime::Classic;
    let transformer_ret = Transformer::new(&allocator, path, &transform_options)
        .build_with_scoping(scoping, &mut program);
    if !transformer_ret.errors.is_empty() {
        let messages: Vec<String> = transformer_ret
            .errors
            .iter()
            .map(|err| err.to_string())
            .collect();
        anyhow::bail!(
            "{}: TypeScript transform error: {}",
            path.display(),
            messages.join("; ")
        );
    }

    // Emit no comments. The bundler collapses each top-level statement onto a
    // single line (see `collapse_top_level_statements`); a surviving `//` line
    // comment would then swallow the rest of that line — including closing
    // braces — corrupting the bundle. Comments serve no runtime purpose, so we
    // drop them at codegen rather than trying to rewrite them during collapse.
    let codegen_ret = Codegen::new()
        .with_options(CodegenOptions {
            comments: CommentOptions::disabled(),
            ..CodegenOptions::default()
        })
        .build(&program);

    // The `chidori:agent` SDK import marks host-injected globals (Chidori,
    // ToolDefinition, etc.) — there's no real module at module-resolution time,
    // so any surviving `import ... from "chidori:agent"` would crash the loader.
    // oxc's TS pass elides import-of-type-only specifiers but keeps value
    // imports, so we filter the remaining `from "chidori:agent"` lines out of
    // the emitted code.
    let js = strip_chidori_sdk_imports(&codegen_ret.code);
    // The snapshot bundler line-walks the output to rewrite `import` / `export`
    // statements. oxc splits object literals and function bodies across many
    // lines, so we join everything inside nested braces/parens/brackets onto
    // the same line. That keeps each top-level statement on a single line
    // while leaving string and template-literal contents untouched.
    Ok(collapse_top_level_statements(&js))
}

fn strip_chidori_sdk_imports(js: &str) -> String {
    let mut out = String::with_capacity(js.len());
    for line in js.lines() {
        if is_chidori_sdk_import(line.trim_start()) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Walk `js` and replace newlines that sit inside `{...}`, `(...)`, `[...]`,
/// or template-literal interpolations with spaces, so each top-level
/// statement ends up on a single line. Quoted strings and template-literal
/// text are passed through untouched.
fn collapse_top_level_statements(js: &str) -> String {
    enum Mode {
        Code,
        DoubleQuote,
        SingleQuote,
        TemplateText,
    }
    let mut out = String::with_capacity(js.len());
    let mut mode = Mode::Code;
    let mut depth: i32 = 0;
    // Stack of template literals we're currently inside, so we know when a `}`
    // closes a `${...}` interpolation vs. an object/block.
    let mut template_interp_starts: Vec<i32> = Vec::new();
    let mut chars = js.chars().peekable();
    while let Some(ch) = chars.next() {
        match mode {
            Mode::Code => match ch {
                '"' => {
                    mode = Mode::DoubleQuote;
                    out.push(ch);
                }
                '\'' => {
                    mode = Mode::SingleQuote;
                    out.push(ch);
                }
                '`' => {
                    mode = Mode::TemplateText;
                    out.push(ch);
                }
                '{' | '(' | '[' => {
                    depth += 1;
                    out.push(ch);
                }
                '}' => {
                    // If this `}` closes a `${...}` interpolation, drop back into the
                    // surrounding template literal. The matching `${` already
                    // incremented depth, so we always decrement here.
                    depth -= 1;
                    out.push(ch);
                    if template_interp_starts
                        .last()
                        .is_some_and(|&start| start == depth + 1)
                    {
                        template_interp_starts.pop();
                        mode = Mode::TemplateText;
                    }
                }
                ')' | ']' => {
                    depth -= 1;
                    out.push(ch);
                }
                '\n' if depth > 0 => {
                    out.push(' ');
                }
                _ => out.push(ch),
            },
            Mode::DoubleQuote => {
                out.push(ch);
                if ch == '\\' {
                    if let Some(next) = chars.next() {
                        out.push(next);
                    }
                } else if ch == '"' {
                    mode = Mode::Code;
                }
            }
            Mode::SingleQuote => {
                out.push(ch);
                if ch == '\\' {
                    if let Some(next) = chars.next() {
                        out.push(next);
                    }
                } else if ch == '\'' {
                    mode = Mode::Code;
                }
            }
            Mode::TemplateText => {
                out.push(ch);
                if ch == '\\' {
                    if let Some(next) = chars.next() {
                        out.push(next);
                    }
                } else if ch == '`' {
                    mode = Mode::Code;
                } else if ch == '$' && chars.peek() == Some(&'{') {
                    let brace = chars.next().unwrap();
                    out.push(brace);
                    depth += 1;
                    template_interp_starts.push(depth);
                    mode = Mode::Code;
                }
            }
        }
    }
    out
}

pub fn validate_imports(
    path: &Path,
    source: &str,
    policy: TypeScriptImportPolicy,
) -> Result<Vec<ModuleImport>> {
    let mut imports = Vec::new();
    let project_root = path.parent().unwrap_or_else(|| Path::new("."));

    for (line_no, line) in source.lines().enumerate() {
        if line.contains("import(") || line.contains("import (") {
            anyhow::bail!(
                "{}:{}: dynamic import is disabled in durable TypeScript agents",
                path.display(),
                line_no + 1
            );
        }
    }

    // Lazily construct the Node resolver only when needed — it touches the
    // filesystem to read package.json files, which we don't want under the
    // legacy policies.
    let mut node_resolver: Option<Resolver> = None;

    for (line_no, specifier) in import_specifiers(source) {
        if is_chidori_sdk_specifier(&specifier) {
            imports.push(ModuleImport {
                specifier,
                resolved_path: None,
                kind: None,
            });
            continue;
        }

        if is_legacy_chidori_specifier(&specifier) {
            anyhow::bail!(
                "{}:{}: importing the agent SDK from \"{}\" is no longer supported. \
                 Import from \"{}\" instead — the bare `chidori` npm name belongs to \
                 an unrelated package, so the SDK now uses an un-installable virtual \
                 specifier. (For editor types: `npm install -D @1kbirds/chidori` and \
                 add `/// <reference types=\"@1kbirds/chidori/agent-env\" />`.)",
                path.display(),
                line_no,
                specifier,
                CHIDORI_AGENT_SPECIFIER,
            );
        }

        // Vendored packages (react, react-dom/server, …) are served from the
        // built-in registry, not the filesystem — accept them under any policy.
        if crate::runtime::typescript::builtins::is_vendored_package(&specifier) {
            imports.push(ModuleImport {
                specifier,
                resolved_path: None,
                kind: None,
            });
            continue;
        }

        match policy {
            TypeScriptImportPolicy::None => {
                anyhow::bail!(
                    "{}:{}: local TypeScript imports are disabled: {}",
                    path.display(),
                    line_no,
                    specifier
                );
            }
            TypeScriptImportPolicy::Relative => {
                if !is_relative_import(&specifier) {
                    anyhow::bail!(
                        "{}:{}: bare TypeScript imports are disabled: {}",
                        path.display(),
                        line_no,
                        specifier
                    );
                }
                let resolved = resolve_relative_import(path, project_root, &specifier, line_no)?;
                imports.push(ModuleImport {
                    specifier,
                    resolved_path: Some(resolved),
                    kind: None,
                });
            }
            TypeScriptImportPolicy::Project => {
                if is_relative_import(&specifier) {
                    let resolved =
                        resolve_relative_import(path, project_root, &specifier, line_no)?;
                    imports.push(ModuleImport {
                        specifier,
                        resolved_path: Some(resolved),
                        kind: None,
                    });
                } else {
                    imports.push(ModuleImport {
                        specifier,
                        resolved_path: None,
                        kind: None,
                    });
                }
            }
            TypeScriptImportPolicy::Node => {
                let resolver = node_resolver.get_or_insert_with(|| {
                    let root = find_workspace_root(path);
                    Resolver::new(
                        root,
                        DEFAULT_CONDITIONS.iter().copied(),
                        NODE_BUILTIN_ALLOWLIST.iter().copied(),
                    )
                });
                let Resolution {
                    kind,
                    resolved_path,
                } = resolver.resolve(&specifier, path).map_err(|err| {
                    anyhow::anyhow!(
                        "{}:{}: cannot resolve `{}`: {}",
                        path.display(),
                        line_no,
                        specifier,
                        err
                    )
                })?;
                imports.push(ModuleImport {
                    specifier,
                    resolved_path: Some(resolved_path),
                    kind: Some(ResolutionKindTag::from(&kind)),
                });
            }
        }
    }

    Ok(imports)
}

fn import_specifiers(source: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (line_no, line) in source.lines().enumerate() {
        let trimmed = line.trim();
        if !trimmed.starts_with("import ") {
            continue;
        }
        if let Some(specifier) =
            specifier_after_from(trimmed).or_else(|| side_effect_import(trimmed))
        {
            out.push((line_no + 1, specifier));
        }
    }
    out
}

/// The virtual SDK module has no on-disk form at runtime; the host injects its
/// exports. Agents may import it by the bare historical name or by the
/// published npm package name.
/// The virtual module specifier that marks the host-injected agent SDK
/// (`Chidori`, `ToolDefinition`, the `chidori`/`run` globals, …). It is
/// deliberately a URL-style scheme — like `node:fs` or `bun:test` — that can
/// never be an installable npm package, so there is no third-party package to
/// confuse it with. The runtime strips this import and supplies the values at
/// execution time.
pub(crate) const CHIDORI_AGENT_SPECIFIER: &str = "chidori:agent";

/// Specifiers that used to mark the injected SDK. The bare `chidori` name
/// belongs to an unrelated npm package — a dependency-confusion hazard, since an
/// author (or an LLM generating agent code) who tried to `npm install chidori`
/// for editor types would pull a package we don't control. Agent files must now
/// import from `chidori:agent`; we still recognize the old spellings so we can
/// emit a clear migration error rather than an opaque resolution failure.
const LEGACY_CHIDORI_SPECIFIERS: &[&str] = &["chidori", "@1kbirds/chidori"];

fn is_chidori_sdk_specifier(specifier: &str) -> bool {
    specifier == CHIDORI_AGENT_SPECIFIER
}

fn is_legacy_chidori_specifier(specifier: &str) -> bool {
    LEGACY_CHIDORI_SPECIFIERS.contains(&specifier)
}

fn is_chidori_sdk_import(line: &str) -> bool {
    line.starts_with("import ")
        && specifier_after_from(line)
            .as_deref()
            .is_some_and(is_chidori_sdk_specifier)
}

fn specifier_after_from(line: &str) -> Option<String> {
    let from_index = line.find(" from ")?;
    quoted_specifier(&line[from_index + 6..])
}

fn side_effect_import(line: &str) -> Option<String> {
    let rest = line.strip_prefix("import ")?;
    quoted_specifier(rest)
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

fn is_relative_import(specifier: &str) -> bool {
    specifier.starts_with("./") || specifier.starts_with("../")
}

pub(crate) fn resolve_relative_import(
    source_path: &Path,
    project_root: &Path,
    specifier: &str,
    line_no: usize,
) -> Result<PathBuf> {
    let path = project_root.join(specifier);
    let normalized = normalize_path(&path);
    let root = normalize_path(project_root);
    if !normalized.starts_with(&root) {
        anyhow::bail!(
            "{}:{}: TypeScript import escapes project root: {}",
            source_path.display(),
            line_no,
            specifier
        );
    }
    // Bundler-style extension probing. ts-node, Bun, Deno, and the TS Bundler
    // resolver all let `import "./tools/x"` resolve to `./tools/x.ts`; agents
    // should not have to know our module loader is stricter. If the specifier
    // already names a file (with or without a known extension), use it as-is.
    // Otherwise probe `.ts`/`.tsx`/`.js`/`.mjs`/`.cjs` and return the first
    // that exists. If none exist, default to the `.ts` candidate so a missing-
    // file error names the most likely intended path.
    if normalized.is_file() {
        return Ok(normalized);
    }
    let known_extensions = ["ts", "tsx", "js", "mjs", "cjs", "json"];
    let has_known_ext = Path::new(specifier)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| known_extensions.contains(&ext))
        .unwrap_or(false);
    if has_known_ext {
        return Ok(normalized);
    }
    for ext in ["ts", "tsx", "js", "mjs", "cjs"] {
        let mut candidate = normalized.clone().into_os_string();
        candidate.push(".");
        candidate.push(ext);
        let candidate = PathBuf::from(candidate);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    let mut fallback = normalized.into_os_string();
    fallback.push(".ts");
    Ok(PathBuf::from(fallback))
}

fn normalize_path(path: &Path) -> PathBuf {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::snapshot::TypeScriptImportPolicy;

    #[test]
    fn transpile_strips_basic_type_syntax() {
        let source = r#"
            import type { Chidori } from "chidori:agent";
            type Input = { name: string };
            export async function agent(input: Input, chidori: Chidori): Promise<object> {
                const greeting: string = input.name;
                return { greeting };
            }
        "#;

        let js = transpile_module(
            Path::new("/tmp/project/agent.ts"),
            source,
            &TranspileOptions {
                import_policy: TypeScriptImportPolicy::Relative,
            },
        )
        .unwrap();

        assert!(!js.contains("import type"));
        assert!(!js.contains("type Input"));
        assert!(js.contains("export async function agent(input, chidori) {"));
        assert!(js.contains("const greeting = input.name;"));
    }

    #[test]
    fn transpile_strips_multiline_function_parameter_types() {
        let source = r#"
            import type { Chidori } from "chidori:agent";
            export async function agent(
                input: { name?: string },
                chidori: Chidori,
            ) {
                const name = input.name ?? "world";
                return { greeting: name };
            }
        "#;

        let js = transpile_module(
            Path::new("/tmp/project/agent.ts"),
            source,
            &TranspileOptions {
                import_policy: TypeScriptImportPolicy::Relative,
            },
        )
        .unwrap();

        assert!(!js.contains("name?: string"));
        assert!(!js.contains("chidori: Chidori"));
        // oxc collapses the multi-line parameter list into `(input, chidori)`;
        // the relevant invariant is that both names survive untyped.
        assert!(js.contains("export async function agent(input, chidori)"));
        assert!(js.contains("const name = input.name ?? \"world\";"));
    }

    #[test]
    fn transpile_strips_multiline_type_and_interface_declarations() {
        let source = r#"
            import type { Chidori } from "chidori:agent";
            export interface Input {
                topic: string;
                limit?: number;
            }
            export type Result =
                | { ok: true; topic: string }
                | { ok: false; error: string };
            export async function agent(input: Input, chidori: Chidori): Promise<Result> {
                await chidori.log("topic", { topic: input.topic });
                return { ok: true, topic: input.topic };
            }
        "#;

        let js = transpile_module(
            Path::new("/tmp/project/agent.ts"),
            source,
            &TranspileOptions {
                import_policy: TypeScriptImportPolicy::Relative,
            },
        )
        .unwrap();

        assert!(!js.contains("interface Input"));
        assert!(!js.contains("export interface Input"));
        assert!(!js.contains("topic: string;"));
        assert!(!js.contains("type Result"));
        assert!(!js.contains("export type Result"));
        assert!(!js.contains("ok: true;"));
        assert!(js.contains("topic: input.topic"));
        assert!(js.contains("export async function agent(input, chidori) {"));
    }

    #[test]
    fn transpile_strips_arrow_function_parameter_types() {
        let source = r#"
            export async function agent(input, chidori) {
                const top5 = [];
                const pages = await chidori.parallel(
                    top5.map((result: { url: string; title: string }) => async () => {
                        return { url: result.url, title: result.title };
                    })
                );
                const links = pages.map(
                    (p: { url: string; title: string }, i: number) => ({
                        label: `${i}: ${p.title}`,
                        url: p.url,
                    })
                );
                return { links };
            }
        "#;

        let js = transpile_module(
            Path::new("/tmp/project/agent.ts"),
            source,
            &TranspileOptions {
                import_policy: TypeScriptImportPolicy::Relative,
            },
        )
        .unwrap();

        assert!(!js.contains("result: { url: string; title: string }"));
        assert!(!js.contains("p: { url: string; title: string }"));
        assert!(!js.contains("i: number"));
        assert!(js.contains("top5.map((result) => async () => {"));
        assert!(js.contains("(p, i) => ({"));
        assert!(js.contains("url: result.url"));
        assert!(js.contains("label: `${i}: ${p.title}`"));
    }

    #[test]
    fn transpile_strips_arrow_function_return_types_and_predicates() {
        let source = r#"
            export async function agent(input, chidori) {
                const titles = [1, null, 2]
                    .map((x) => (typeof x === "number" ? String(x) : null))
                    .filter((t): t is string => t !== null);
                const double = (n: number): number => n * 2;
                const obj = (k): { v: number } => ({ v: 1 });
                return { titles, doubled: double(3), obj: obj("a") };
            }
        "#;

        let js = transpile_module(
            Path::new("/tmp/project/agent.ts"),
            source,
            &TranspileOptions {
                import_policy: TypeScriptImportPolicy::Relative,
            },
        )
        .unwrap();

        assert!(
            !js.contains(": t is string"),
            "type predicate not stripped:\n{js}"
        );
        assert!(
            !js.contains(": number =>"),
            "primitive arrow return not stripped:\n{js}"
        );
        assert!(
            !js.contains(": { v: number }"),
            "object arrow return not stripped:\n{js}"
        );
        assert!(js.contains(".filter((t) => t !== null)"));
        assert!(js.contains("const double = (n) => n * 2;"));
        assert!(js.contains("const obj = (k) => ({ v: 1 });"));
    }

    #[test]
    fn transpile_drops_line_comments_so_collapse_does_not_swallow_code() {
        // Regression: line comments inside a function body must not survive
        // codegen. The bundler collapses each top-level statement onto one
        // physical line, so a `//` comment would otherwise comment out the rest
        // of the line — including the function's closing braces — corrupting
        // the bundle. (Found via end-to-end package validation.)
        let source = r#"
            export async function agent(input, chidori) {
                const before = 1; // a trailing comment
                // a full-line comment
                const after = 2;
                return { before, after };
            }
        "#;
        let js = transpile_module(
            Path::new("/tmp/project/agent.ts"),
            source,
            &TranspileOptions {
                import_policy: TypeScriptImportPolicy::Relative,
            },
        )
        .unwrap();

        assert!(!js.contains("//"), "line comments must be stripped:\n{js}");
        assert!(
            js.contains("const after = 2"),
            "code after a comment survived:\n{js}"
        );
        assert!(js.contains("return {"), "return survived:\n{js}");
        // Braces stay balanced after collapse.
        assert_eq!(
            js.matches('{').count(),
            js.matches('}').count(),
            "balanced braces:\n{js}"
        );
    }

    #[test]
    fn transpile_preserves_object_literal_values() {
        let source = r#"
            import { Chidori, ToolDefinition } from "chidori:agent";

            export const tool: ToolDefinition = {
                name: "web_search",
                parameters: { type: "object" },
            };
            export async function agent(input, chidori) {
                return { now: Date.now(), iso: new Date().toISOString() };
            }
        "#;

        let js = transpile_module(
            Path::new("/tmp/project/tools/web_search.ts"),
            source,
            &TranspileOptions {
                import_policy: TypeScriptImportPolicy::Relative,
            },
        )
        .unwrap();

        assert!(!js.contains("chidori:agent"));
        assert!(!js.contains(": ToolDefinition"));
        assert!(js.contains(r#"name: "web_search""#));
        assert!(js.contains(r#"type: "object""#));
        assert!(js.contains("now: Date.now()"));
        assert!(js.contains("iso: new Date().toISOString()"));
    }

    #[test]
    fn transpile_rejects_legacy_chidori_specifiers_with_migration_error() {
        // The bare `chidori` name and the `@1kbirds/chidori` scope used to mark
        // the injected SDK. Both are now rejected in favor of `chidori:agent`,
        // with an error that names the new specifier.
        for legacy in ["chidori", "@1kbirds/chidori"] {
            let source = format!(
                "import type {{ Chidori }} from \"{legacy}\";\n\
                 export async function agent(input, chidori: Chidori) {{ return {{ ok: true }}; }}\n"
            );

            let err = transpile_module(
                Path::new("/tmp/project/agents/legacy.ts"),
                &source,
                &TranspileOptions {
                    import_policy: TypeScriptImportPolicy::Relative,
                },
            )
            .unwrap_err()
            .to_string();

            assert!(
                err.contains("chidori:agent"),
                "error should point at the new specifier, got: {err}"
            );
            assert!(
                err.contains(legacy),
                "error should name the legacy specifier {legacy}, got: {err}"
            );
        }
    }

    #[test]
    fn transpile_strips_value_imports_from_virtual_specifier() {
        let source = r#"
            import { chidori, run } from "chidori:agent";

            run(async (input) => {
                await chidori.log("hi");
                return { ok: true };
            });
        "#;

        let js = transpile_module(
            Path::new("/tmp/project/agents/inline.ts"),
            source,
            &TranspileOptions {
                import_policy: TypeScriptImportPolicy::Relative,
            },
        )
        .unwrap();

        assert!(
            !js.contains("chidori:agent"),
            "virtual SDK import survived:\n{js}"
        );
        assert!(js.contains("run(async"));
    }

    #[test]
    fn transpile_strips_satisfies_and_as_const_assertions() {
        let source = r#"
            import { ToolDefinition } from "chidori:agent";

            const prompt = "Only call chidori.tool(name, args) directly for deterministic work when the exact args are already known and the tool is either implemented by this project or explicitly described by the user as an available runtime tool. ";
            const template = `Do not strip as const or satisfies ToolDefinition inside templates`;
            export const tool = {
                name: "web_search",
                description: "Search",
                parameters: {
                    type: "object",
                    required: ["query"] as const,
                },
            } as const satisfies ToolDefinition;

            const fallback = { ok: true } as { ok: boolean };
        "#;

        let js = transpile_module(
            Path::new("/tmp/project/tools/web_search.ts"),
            source,
            &TranspileOptions {
                import_policy: TypeScriptImportPolicy::Relative,
            },
        )
        .unwrap();

        assert!(!js.contains("} as const satisfies ToolDefinition"));
        assert!(!js.contains(r#"["query"] as const"#));
        assert!(!js.contains(" as { ok: boolean }"));
        assert!(js.contains("};"));
        assert!(js.contains(r#"required: ["query"]"#));
        assert!(js.contains("as an available runtime tool"));
        assert!(js.contains("as const or satisfies ToolDefinition inside templates"));
    }

    #[test]
    fn transpile_strips_return_type_with_object_literal() {
        let source = r#"
            import { Chidori } from "chidori:agent";

            export async function run(
                args: { url: string },
                chidori: Chidori,
            ): Promise<{ url: string; content: string }> {
                return { url: args.url, content: "ok" };
            }

            export async function inline(args: { url: string }, chidori: Chidori): { url: string; content: string } {
                return { url: args.url, content: "ok" };
            }
        "#;

        let js = transpile_module(
            Path::new("/tmp/project/tools/read_url.ts"),
            source,
            &TranspileOptions {
                import_policy: TypeScriptImportPolicy::Relative,
            },
        )
        .unwrap();

        assert!(js.contains("export async function run("));
        assert!(js.contains(") {"));
        assert!(js.contains("export async function inline(args, chidori) {"));
        assert!(!js.contains("Promise<{"));
        assert!(!js.contains("content: string"));
        assert!(js.contains("content: \"ok\""));
    }

    #[test]
    fn transpile_strips_multiline_as_type_assertion_and_catch_annotation() {
        let source = r#"
            export async function run(args, chidori) {
                const response = await fetch("https://example.test");
                let content: string;
                const data = response.body as {
                    web?: {
                        results?: Array<{
                            title: string;
                            url: string;
                        }>
                    }
                };
                try {
                    return { count: data?.web?.results?.length ?? 0 };
                } catch (error: any) {
                    return { error: error?.message ?? "failed" };
                }
            }
        "#;

        let js = transpile_module(
            Path::new("/tmp/project/tools/web_search.ts"),
            source,
            &TranspileOptions {
                import_policy: TypeScriptImportPolicy::Relative,
            },
        )
        .unwrap();

        assert!(js.contains("const data = response.body;"));
        assert!(js.contains("let content;"));
        assert!(!js.contains("content: string"));
        assert!(!js.contains("web?:"));
        assert!(!js.contains("title: string"));
        assert!(js.contains("catch (error)"));
        assert!(!js.contains("error: any"));
        assert!(js.contains("error: error?.message"));
    }

    #[test]
    fn transpile_preserves_colons_inside_initializers() {
        let source = r#"
            export async function run(args, chidori) {
                const response = await fetch("https://api.search.brave.com/res/v1/web/search", {
                    method: "GET",
                });
                const message = args.ok ? "ok" : String(args.error);
                return { response, message };
            }
        "#;

        let js = transpile_module(
            Path::new("/tmp/project/tools/web_search.ts"),
            source,
            &TranspileOptions {
                import_policy: TypeScriptImportPolicy::Relative,
            },
        )
        .unwrap();

        assert!(js.contains(r#"fetch("https://api.search.brave.com/res/v1/web/search", {"#));
        assert!(js.contains(r#"const message = args.ok ? "ok" : String(args.error);"#));
    }

    #[test]
    fn relative_import_policy_rejects_bare_imports() {
        let source = r#"import { x } from "left-pad";"#;
        let err = validate_imports(
            Path::new("/tmp/project/agent.ts"),
            source,
            TypeScriptImportPolicy::Relative,
        )
        .unwrap_err();
        assert!(err.to_string().contains("bare TypeScript imports"));
    }

    #[test]
    fn none_import_policy_rejects_relative_imports() {
        let source = r#"import { x } from "./x.ts";"#;
        let err = validate_imports(
            Path::new("/tmp/project/agent.ts"),
            source,
            TypeScriptImportPolicy::None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("disabled"));
        assert!(err.to_string().contains(":1:"));
    }

    #[test]
    fn dynamic_imports_are_rejected() {
        let source = r#"const x = await import("./x.ts");"#;
        let err = validate_imports(
            Path::new("/tmp/project/agent.ts"),
            source,
            TypeScriptImportPolicy::Project,
        )
        .unwrap_err();
        assert!(err.to_string().contains("dynamic import"));
        assert!(err.to_string().contains(":1:"));
    }
}
