use std::collections::HashMap;
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
    // Import validation ALWAYS runs — it consults the filesystem (relative
    // import resolution, package.json lookups), so its outcome can change
    // between calls with identical source and must never be cached.
    validate_imports(path, source, options.import_policy)?;

    // The oxc pipeline below (parse → semantic → transform → codegen → strip)
    // is a pure function of `(path, source)` — the transform options
    // are compile-time constants and nothing reads the environment — so its
    // output is memoized process-wide. This is the dominant fixed cost paid on
    // EVERY agent execution: initial runs, every pause→resume re-execution,
    // tool files, sub-agents, branch waves/resumes, and each imported module —
    // all re-transpile byte-identical sources today. Keyed by path with the
    // source matched by full string equality, so a hit can never alias
    // distinct inputs; only successes are cached (errors are cheap and
    // deterministic to recompute).
    {
        let cache = transpile_cache().lock().expect("transpile cache poisoned");
        if let Some(entries) = cache.get(path) {
            if let Some((_, js)) = entries.iter().find(|(cached, _)| cached == source) {
                return Ok(js.clone());
            }
        }
    }
    let js = transpile_source(path, source)?;
    {
        let mut cache = transpile_cache().lock().expect("transpile cache poisoned");
        // Bound the cache; a process sees dozens of distinct module sources.
        // Clearing wholesale at the caps is simpler than LRU and has no
        // order-dependent behavior.
        if cache.len() >= 256 {
            cache.clear();
        }
        let entries = cache.entry(path.to_path_buf()).or_default();
        if entries.len() >= 8 {
            entries.clear();
        }
        entries.push((source.to_string(), js.clone()));
    }
    Ok(js)
}

/// Keyed by path, with the source carried in the per-path entries and matched
/// by FULL string equality — a hit can never alias distinct inputs. The
/// two-level shape lets a lookup borrow `&Path`/`&str` directly: the old
/// `(PathBuf, String)` key forced a heap copy of the whole source (and a hash
/// over every byte of it) on EVERY probe, hits included — per module, per
/// execution, per resume.
type TranspileCache = HashMap<PathBuf, Vec<(String, String)>>;

fn transpile_cache() -> &'static std::sync::Mutex<TranspileCache> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<TranspileCache>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Render oxc diagnostics through miette's graphical report handler (the
/// diagnostic toolkit oxc's errors are built on): each error shows its
/// `file:line:column`, the offending source line, and a caret under the exact
/// span — instead of a bare message that leaves the user hunting for the
/// position. Unicode box-drawing, no ANSI color (output lands in error chains
/// and logs, not always a terminal).
fn render_diagnostics(
    path: &Path,
    source: &str,
    errors: &[oxc::diagnostics::OxcDiagnostic],
) -> String {
    use oxc::diagnostics::{GraphicalReportHandler, GraphicalTheme, NamedSource};
    let handler = GraphicalReportHandler::new_themed(GraphicalTheme::unicode_nocolor());
    let mut out = String::new();
    for err in errors {
        let report = err
            .clone()
            .with_source_code(NamedSource::new(path.to_string_lossy(), source.to_string()));
        if handler.render_report(&mut out, report.as_ref()).is_err() {
            // Rendering is best-effort presentation; never mask the error itself.
            out.push_str(&format!("\n{err}"));
        }
    }
    out
}

/// The pure transpile pipeline (no import validation, no filesystem access).
fn transpile_source(path: &Path, source: &str) -> Result<String> {
    Ok(transpile_source_impl(path, source, false)?.0)
}

/// As [`transpile_source`], additionally returning the codegen source map
/// (original TypeScript → emitted JavaScript). Used by [`remap_to_original`]
/// on the error path; the hot execution path skips map generation.
pub fn transpile_source_with_map(
    path: &Path,
    source: &str,
) -> Result<(String, oxc_sourcemap::SourceMap)> {
    let (js, map) = transpile_source_impl(path, source, true)?;
    map.map(|m| (js, m))
        .ok_or_else(|| anyhow::anyhow!("codegen produced no source map"))
}

fn transpile_source_impl(
    path: &Path,
    source: &str,
    with_map: bool,
) -> Result<(String, Option<oxc_sourcemap::SourceMap>)> {
    // Treat input as TypeScript regardless of extension — agents may live in
    // `agent.ts` / `tools/*.ts` and the snapshot pipeline only calls us with TS.
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::ts());
    let allocator = Allocator::default();

    let parser_ret = Parser::new(&allocator, source, source_type).parse();
    if !parser_ret.errors.is_empty() {
        anyhow::bail!(
            "{}: TypeScript parse error:{}",
            path.display(),
            render_diagnostics(path, source, &parser_ret.errors)
        );
    }
    let mut program = parser_ret.program;

    let semantic_ret = SemanticBuilder::new()
        // Transformer roughly triples scope/symbol/reference allocations.
        .with_excess_capacity(2.0)
        .build(&program);
    if !semantic_ret.errors.is_empty() {
        anyhow::bail!(
            "{}: TypeScript semantic error:{}",
            path.display(),
            render_diagnostics(path, source, &semantic_ret.errors)
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
        anyhow::bail!(
            "{}: TypeScript transform error:{}",
            path.display(),
            render_diagnostics(path, source, &transformer_ret.errors)
        );
    }

    // Emit no comments — they serve no runtime purpose and keeping codegen
    // output minimal keeps the transpile cache and error positions stable.
    // When `with_map` is set, codegen also records the source map that error
    // frames are remapped through (original .ts → emitted JS).
    let codegen_ret = Codegen::new()
        .with_options(CodegenOptions {
            comments: CommentOptions::disabled(),
            source_map_path: with_map.then(|| path.to_path_buf()),
            ..CodegenOptions::default()
        })
        .build(&program);

    // The `chidori:agent` SDK import marks host-injected globals (Chidori,
    // ToolDefinition, etc.) — there's no real module at module-resolution time,
    // so any surviving `import ... from "chidori:agent"` would crash the loader.
    // oxc's TS pass elides import-of-type-only specifiers but keeps value
    // imports, so we BLANK the remaining `from "chidori:agent"` lines in the
    // emitted code — blanked rather than removed so every other line keeps its
    // line number and the source map (hence error-frame remapping) stays valid.
    //
    // Note the emitted code is NOT collapsed onto one line per top-level
    // statement anymore: nothing line-walks the transpiled output today, and
    // preserving codegen's line structure is what lets runtime stack frames
    // map back to real positions in the original TypeScript.
    let js = strip_chidori_sdk_imports(&codegen_ret.code);
    Ok((js, codegen_ret.map))
}

/// Blank (not remove) each `chidori:agent` import line, preserving all other
/// lines' positions.
fn strip_chidori_sdk_imports(js: &str) -> String {
    let mut out = String::with_capacity(js.len());
    for line in js.lines() {
        if !is_chidori_sdk_import(line.trim_start()) {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// A stack-frame position translated back into the original TypeScript file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OriginalPosition {
    /// 1-based line in the original source.
    pub line: u32,
    /// 1-based character column in the original source.
    pub column: u32,
    /// Byte offset into the original source (for miette span labels).
    pub offset: usize,
}

/// Translate a 1-based `(line, character-column)` position in the transpiled
/// output of `source` back to the original TypeScript, via the codegen source
/// map. This re-runs the transpile pipeline with map generation on — it is an
/// error-path helper (a run renders at most a handful of frames), not
/// something to call per executed function.
pub fn remap_to_original(
    path: &Path,
    source: &str,
    line: u32,
    column: u32,
) -> Option<OriginalPosition> {
    let (js, map) = transpile_source_with_map(path, source).ok()?;
    // The engine counts columns in characters; source-map columns are UTF-16
    // code units. Convert against the transpiled line's text.
    let gen_line0 = line.checked_sub(1)?;
    let gen_text = js.lines().nth(gen_line0 as usize)?;
    let gen_col16: u32 = gen_text
        .chars()
        .take(column.saturating_sub(1) as usize)
        .map(|c| c.len_utf16() as u32)
        .sum();
    let table = map.generate_lookup_table();
    let token = map.lookup_token(&table, gen_line0, gen_col16)?;
    let src_line0 = token.get_src_line();
    let src_col16 = token.get_src_col();
    // UTF-16 column → byte offset + 1-based character column in the original.
    let line_start: usize = source
        .split_inclusive('\n')
        .take(src_line0 as usize)
        .map(str::len)
        .sum();
    let line_text = source[line_start..].split('\n').next().unwrap_or_default();
    let mut units: u32 = 0;
    let mut chars: u32 = 0;
    let mut bytes: usize = 0;
    for c in line_text.chars() {
        if units >= src_col16 {
            break;
        }
        units += c.len_utf16() as u32;
        chars += 1;
        bytes += c.len_utf8();
    }
    Some(OriginalPosition {
        line: src_line0 + 1,
        column: chars + 1,
        offset: line_start + bytes,
    })
}

/// Byte offset of the first dynamic `import(...)` expression in `source`, or
/// `None` if there is none — or if the source fails to parse (the transpile
/// step owns parse-error reporting).
fn find_dynamic_import(path: &Path, source: &str) -> Option<usize> {
    use oxc::ast::ast::ImportExpression;
    use oxc::ast_visit::Visit;

    struct Finder {
        first: Option<usize>,
    }
    impl<'a> Visit<'a> for Finder {
        fn visit_import_expression(&mut self, it: &ImportExpression<'a>) {
            // Visitation is in source order, so the first hit is the earliest.
            if self.first.is_none() {
                self.first = Some(it.span.start as usize);
            }
        }
    }

    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::ts());
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if !parsed.errors.is_empty() {
        return None;
    }
    let mut finder = Finder { first: None };
    finder.visit_program(&parsed.program);
    finder.first
}

pub fn validate_imports(
    path: &Path,
    source: &str,
    policy: TypeScriptImportPolicy,
) -> Result<Vec<ModuleImport>> {
    validate_imports_inner(path, source, policy, true)
}

/// Like [`validate_imports`], but tuned for describing the module graph of
/// THIRD-PARTY files (under `node_modules`): the dynamic-import rejection is
/// skipped there — a package's lazily-imported paths are not executed at link
/// time, and the runtime enforces the dynamic-import policy at the moment of
/// use — while every *static* edge still resolves strictly (an unresolvable
/// static edge fails the engine's eager loader the same way). Project files
/// keep the full policy.
pub fn module_graph_imports(
    path: &Path,
    source: &str,
    policy: TypeScriptImportPolicy,
) -> Result<Vec<ModuleImport>> {
    let third_party = path.components().any(|c| c.as_os_str() == "node_modules");
    validate_imports_inner(path, source, policy, !third_party)
}

fn validate_imports_inner(
    path: &Path,
    source: &str,
    policy: TypeScriptImportPolicy,
    reject_dynamic_import: bool,
) -> Result<Vec<ModuleImport>> {
    let mut imports = Vec::new();
    let project_root = path.parent().unwrap_or_else(|| Path::new("."));

    // Dynamic import is rejected from the AST, not a text scan: `import(` in a
    // comment, a string literal, or an identifier ending in "import" must not
    // fail the file. If the source doesn't parse, skip the check here — the
    // transpile step reports parse errors with full diagnostics.
    if let Some(span_start) = reject_dynamic_import
        .then(|| find_dynamic_import(path, source))
        .flatten()
    {
        let line_no = source[..span_start].bytes().filter(|&b| b == b'\n').count() + 1;
        anyhow::bail!(
            "{}:{}: dynamic import is disabled in durable TypeScript agents",
            path.display(),
            line_no
        );
    }

    // Lazily construct the Node resolver only when needed — it touches the
    // filesystem to read package.json files, which we don't want under the
    // legacy policies.
    let mut node_resolver: Option<Resolver> = None;

    for (line_no, specifier, type_only) in import_specifiers(path, source) {
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

        // Type-only imports/exports are erased by transpilation: no runtime
        // edge, nothing to resolve (a types-only package need not exist on
        // disk). The name checks above still apply.
        if type_only {
            continue;
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

/// One collected specifier: (line, specifier, type_only). Type-only imports
/// are erased by transpilation — they get the name checks (legacy-SDK
/// migration, policy) but never resolve to a module-graph edge.
type CollectedSpecifier = (usize, String, bool);

fn import_specifiers(path: &Path, source: &str) -> Vec<CollectedSpecifier> {
    // AST first: real dist files are minified (`import"node:module";var …`),
    // which no line scan can see. The text scan below stays as the fallback
    // for sources oxc cannot parse — the transpile step reports those with
    // full diagnostics anyway.
    ast_import_specifiers(path, source).unwrap_or_else(|| text_import_specifiers(source))
}

/// Every static module-graph edge, from the AST: `import … from`, side-effect
/// `import "x"`, `export * from`, `export {a} from`, and `export * as ns
/// from`.
fn ast_import_specifiers(path: &Path, source: &str) -> Option<Vec<CollectedSpecifier>> {
    use oxc::ast::ast::Statement;

    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::ts());
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if !parsed.errors.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    let mut push = |start: usize, specifier: &str, type_only: bool| {
        let line_no = source[..start].bytes().filter(|&b| b == b'\n').count() + 1;
        out.push((line_no, specifier.to_string(), type_only));
    };
    for stmt in &parsed.program.body {
        match stmt {
            Statement::ImportDeclaration(d) => {
                push(
                    d.span.start as usize,
                    d.source.value.as_str(),
                    d.import_kind.is_type(),
                );
            }
            Statement::ExportAllDeclaration(d) => {
                push(
                    d.span.start as usize,
                    d.source.value.as_str(),
                    d.export_kind.is_type(),
                );
            }
            Statement::ExportNamedDeclaration(d) => {
                if let Some(src) = &d.source {
                    push(
                        d.span.start as usize,
                        src.value.as_str(),
                        d.export_kind.is_type(),
                    );
                }
            }
            _ => {}
        }
    }
    Some(out)
}

fn text_import_specifiers(source: &str) -> Vec<CollectedSpecifier> {
    let mut out = Vec::new();
    for (line_no, line) in source.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("import ") {
            if let Some(specifier) =
                specifier_after_from(trimmed).or_else(|| side_effect_import(trimmed))
            {
                out.push((line_no + 1, specifier, false));
            }
        } else if trimmed.starts_with("export ") {
            if let Some(specifier) = specifier_after_from(trimmed) {
                out.push((line_no + 1, specifier, false));
            }
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
    fn dynamic_import_expression_is_rejected_with_its_line() {
        let source = "const x = 1;\nconst mod = await import(\"./other.ts\");\n";
        let err = validate_imports(
            Path::new("/tmp/project/agent.ts"),
            source,
            TypeScriptImportPolicy::Relative,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("dynamic import is disabled"), "{msg}");
        assert!(msg.contains("agent.ts:2"), "line should be 2: {msg}");
    }

    #[test]
    fn import_in_comment_string_or_identifier_is_not_dynamic_import() {
        let source = r#"
            // A comment mentioning import( must not fail the file.
            const s = "also fine in a string: import(x)";
            function reimport(v: string) { return v; }
            const t = reimport(s);
            export async function agent() { return { t }; }
        "#;
        let imports = validate_imports(
            Path::new("/tmp/project/agent.ts"),
            source,
            TypeScriptImportPolicy::Relative,
        )
        .unwrap();
        assert!(imports.is_empty());
    }

    #[test]
    fn unparseable_source_defers_to_transpile_for_diagnostics() {
        // A parse error must not be masked by (or misreported as) a dynamic-
        // import rejection — transpile owns parse diagnostics.
        let source = "const value = { a: 1, b: 2 ;\n";
        assert!(validate_imports(
            Path::new("/tmp/project/agent.ts"),
            source,
            TypeScriptImportPolicy::Relative,
        )
        .is_ok());
    }

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

    /// The transpile cache is keyed by `(path, source)` only — a warm cache
    /// entry must never mask import-policy validation, which always re-runs.
    #[test]
    fn transpile_cache_never_skips_import_validation() {
        let source = r#"
            import { helper } from "./helper.ts";
            export async function agent() { return helper(); }
        "#;
        let path = Path::new("/tmp/project-cache-validate/agent.ts");
        // Permissive policy: succeeds and warms the cache for (path, source).
        transpile_module(
            path,
            source,
            &TranspileOptions {
                import_policy: TypeScriptImportPolicy::Relative,
            },
        )
        .unwrap();
        // Restrictive policy, SAME (path, source): validation must still reject
        // the local import even though the pipeline output is cached.
        let err = transpile_module(
            path,
            source,
            &TranspileOptions {
                import_policy: TypeScriptImportPolicy::None,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("imports are disabled"));
    }

    /// Repeated transpiles of identical input return byte-identical output
    /// (the cached result IS the computed result), and distinct sources at the
    /// same path never alias.
    #[test]
    fn transpile_cache_is_transparent_and_never_aliases() {
        let path = Path::new("/tmp/project-cache-alias/agent.ts");
        let opts = TranspileOptions {
            import_policy: TypeScriptImportPolicy::Relative,
        };
        let a1 = transpile_module(path, "export const x: number = 1;", &opts).unwrap();
        let a2 = transpile_module(path, "export const x: number = 1;", &opts).unwrap();
        assert_eq!(a1, a2);
        let b = transpile_module(path, "export const x: number = 2;", &opts).unwrap();
        assert_ne!(a1, b);
        assert!(b.contains("= 2"));
    }

    /// Timing probe (not a CI assertion — run with `--ignored --nocapture`):
    /// prints the cold vs warm cost of `transpile_module` on a synthetic
    /// agent-sized source, i.e. the per-execution cost the cache removes.
    #[test]
    fn remap_recovers_original_positions_across_type_stripping() {
        // The interface block and the type-only import vanish in the
        // transpiled output, so `priced` sits on a different line there; the
        // codegen source map takes its position back to the original line 6.
        let source = "import type { ToolDefinition } from \"chidori:agent\";\n\
                      interface Order {\n\
                      \x20 total: number;\n\
                      }\n\n\
                      export function priced(o: Order): number {\n\
                      \x20 return o.total;\n\
                      }\n";
        let path = Path::new("remap-test.ts");
        let (js, _) = transpile_source_with_map(path, source).unwrap();
        let (line0, text) = js
            .lines()
            .enumerate()
            .find(|(_, l)| l.contains("function priced"))
            .expect("transpiled output keeps the function");
        assert_ne!(line0 + 1, 6, "the stripped types must shift the line");
        let col0 = text.find("priced").unwrap();
        let pos = remap_to_original(path, source, line0 as u32 + 1, col0 as u32 + 1)
            .expect("position remaps");
        assert_eq!(pos.line, 6, "definition maps back to the original line");
        assert!(
            source[pos.offset..].starts_with("priced")
                || source[pos.offset..].starts_with("function priced")
                || source[pos.offset..].starts_with("export function priced"),
            "offset lands on the definition: {:?}",
            &source[pos.offset..pos.offset + 20]
        );
    }

    #[test]
    fn stripped_sdk_import_lines_are_blanked_not_removed() {
        // Blanking keeps every other line's number stable, which is what
        // keeps the source map (and error-frame remapping) valid. The import
        // must actually be used — oxc elides unused imports before the strip
        // ever sees them.
        let source = "import { chidori, run } from \"chidori:agent\";\n\
                      export function f(): number {\n\
                      \x20 return 1;\n\
                      }\n\
                      run(async () => f());\n";
        let js = transpile_source(Path::new("blank-test.ts"), source).unwrap();
        assert!(!js.contains("chidori:agent"), "sdk import stripped: {js}");
        assert!(js.contains("run("), "the agent body survives: {js}");
        assert_eq!(
            js.lines().next(),
            Some(""),
            "the import line is blanked in place, not removed: {js}"
        );
    }

    #[test]
    #[ignore]
    fn transpile_cache_timing_probe() {
        let mut source = String::from("import type { Chidori } from \"chidori:agent\";\n");
        for i in 0..400 {
            source.push_str(&format!(
                "export function tool{i}(input: {{ q: string; n?: number }}): {{ ok: boolean; v: number }} {{\n\
                 const v: number = (input.n ?? {i}) * 2;\n\
                 return {{ ok: true, v }} as {{ ok: boolean; v: number }};\n}}\n"
            ));
        }
        let path = Path::new("/tmp/project-timing/agent.ts");
        let opts = TranspileOptions {
            import_policy: TypeScriptImportPolicy::Relative,
        };
        println!("source: {} KB", source.len() / 1024);
        let t0 = std::time::Instant::now();
        let a = transpile_module(path, &source, &opts).unwrap();
        let cold = t0.elapsed();
        let t1 = std::time::Instant::now();
        let b = transpile_module(path, &source, &opts).unwrap();
        let warm = t1.elapsed();
        assert_eq!(a, b);
        println!(
            "transpile cold: {:.3} ms   warm (cached): {:.3} ms",
            cold.as_secs_f64() * 1e3,
            warm.as_secs_f64() * 1e3
        );
    }
}
