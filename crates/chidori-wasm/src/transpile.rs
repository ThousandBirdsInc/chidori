//! TypeScript stripping for browser-authored agent bundles.
//!
//! Mirrors the native runtime's transpile defaults
//! (`crates/chidori/src/runtime/typescript/transpile.rs`): strip TS syntax
//! only, leave modern JS untouched (the engine executes async/await, optional
//! chaining, etc. natively), and lower JSX to the classic
//! `React.createElement` runtime. Kept dependency-light — no source maps and
//! no module-graph resolution: a browser bundle is a single source the page
//! hands us, not a file tree to walk.

use oxc::allocator::Allocator;
use oxc::codegen::{Codegen, CodegenOptions, CommentOptions};
use oxc::parser::Parser;
use oxc::semantic::SemanticBuilder;
use oxc::span::SourceType;
use oxc::transformer::{JsxRuntime, TransformOptions, Transformer};

/// Strip TypeScript syntax from `source`, returning plain JavaScript.
/// `filename` picks the dialect (`.tsx` enables JSX parsing); anything
/// unrecognized is treated as `.ts`.
pub fn strip_types(source: &str, filename: &str) -> Result<String, String> {
    let path = std::path::Path::new(filename);
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::ts());
    let allocator = Allocator::default();

    let parser_ret = Parser::new(&allocator, source, source_type).parse();
    if !parser_ret.errors.is_empty() {
        return Err(render_errors("parse", filename, &parser_ret.errors));
    }
    let mut program = parser_ret.program;

    let semantic_ret = SemanticBuilder::new()
        // Transformer roughly triples scope/symbol/reference allocations.
        .with_excess_capacity(2.0)
        .build(&program);
    if !semantic_ret.errors.is_empty() {
        return Err(render_errors("semantic", filename, &semantic_ret.errors));
    }
    let scoping = semantic_ret.semantic.into_scoping();

    let mut transform_options = TransformOptions::default();
    transform_options.jsx.runtime = JsxRuntime::Classic;
    let transformer_ret = Transformer::new(&allocator, path, &transform_options)
        .build_with_scoping(scoping, &mut program);
    if !transformer_ret.errors.is_empty() {
        return Err(render_errors(
            "transform",
            filename,
            &transformer_ret.errors,
        ));
    }

    let codegen_ret = Codegen::new()
        .with_options(CodegenOptions {
            comments: CommentOptions::disabled(),
            ..CodegenOptions::default()
        })
        .build(&program);

    // As in the native loader: `import ... from "chidori:agent"` marks
    // host-injected globals; no real module exists, so blank any surviving
    // value-import of it (type-only imports were already elided by oxc).
    // Blanked rather than removed so line numbers in error frames survive.
    let js = codegen_ret
        .code
        .lines()
        .map(|line| {
            if line.contains("\"chidori:agent\"") || line.contains("'chidori:agent'") {
                ""
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(js)
}

fn render_errors(
    stage: &str,
    filename: &str,
    errors: &[oxc::diagnostics::OxcDiagnostic],
) -> String {
    let details = errors
        .iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("; ");
    format!("{filename}: TypeScript {stage} error: {details}")
}

#[cfg(test)]
mod tests {
    use super::strip_types;

    #[test]
    fn strips_annotations_interfaces_and_satisfies() {
        let ts = r#"
            interface Greeting { text: string }
            const g = { text: 'hi' } satisfies Greeting;
            function greet(who: string): string { return g.text + ' ' + who; }
            const pick = (t: unknown): t is string => typeof t === 'string';
            export {};
        "#;
        let js = strip_types(ts, "agent.ts").unwrap();
        assert!(!js.contains("interface"));
        assert!(!js.contains("satisfies"));
        assert!(!js.contains(": string"));
        assert!(js.contains("greet"));
    }

    #[test]
    fn keeps_modern_js_undownleveled() {
        let ts = "async function f(a?: { b?: number }) { return a?.b ?? (await g()); }";
        let js = strip_types(ts, "agent.ts").unwrap();
        assert!(js.contains("async function"));
        assert!(js.contains("?."));
        assert!(js.contains("??"));
    }

    #[test]
    fn blanks_chidori_agent_imports_preserving_line_count() {
        let ts =
            "import { type Chidori, defineAgent } from \"chidori:agent\";\nconst x: number = 1;";
        let js = strip_types(ts, "agent.ts").unwrap();
        assert!(!js.contains("chidori:agent"));
        assert_eq!(js.lines().count(), 2, "blanked line must keep its slot");
    }

    #[test]
    fn parse_error_is_reported_not_panicked() {
        let err = strip_types("const = ;", "agent.ts").unwrap_err();
        assert!(err.contains("parse error"), "got: {err}");
    }
}
