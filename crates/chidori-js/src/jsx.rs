//! JSX → `React.createElement` transpilation, so agents can author components in
//! JSX instead of hand-rolling `createElement`. Kept out of the default
//! [`crate::compiler`] path (which stays strictly JS, for Test262 conformance):
//! callers opt in by running source through [`transpile_jsx`] first.
//!
//! Uses oxc's transformer in the **classic** runtime (pragma `React.createElement`),
//! and strips TypeScript types in the same pass, so `.tsx` agent source becomes
//! plain JS the engine compiles normally.

use oxc::allocator::Allocator;
use oxc::codegen::Codegen;
use oxc::parser::Parser;
use oxc::semantic::SemanticBuilder;
use oxc::span::SourceType;
use oxc::transformer::{JsxRuntime, TransformOptions, Transformer};
use std::path::Path;

/// Transpile JSX/TSX source to plain JS. JSX elements become
/// `React.createElement(...)` calls (classic runtime), and TypeScript type
/// syntax is stripped. Non-JSX input passes through (re-printed) unchanged in
/// meaning.
pub fn transpile_jsx(src: &str) -> Result<String, String> {
    let allocator = Allocator::default();
    let source_type = SourceType::default()
        .with_typescript(true)
        .with_jsx(true)
        .with_module(true);
    let parsed = Parser::new(&allocator, src, source_type).parse();
    if !parsed.errors.is_empty() {
        return Err(format!(
            "SyntaxError: {}",
            parsed
                .errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    let mut program = parsed.program;
    let scoping = SemanticBuilder::new()
        .build(&program)
        .semantic
        .into_scoping();

    let mut options = TransformOptions::default();
    options.jsx.runtime = JsxRuntime::Classic; // emit React.createElement, not jsx-runtime imports
    let ret = Transformer::new(&allocator, Path::new("agent.tsx"), &options)
        .build_with_scoping(scoping, &mut program);
    if !ret.errors.is_empty() {
        return Err(format!(
            "JSX transform error: {}",
            ret.errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    Ok(Codegen::new().build(&program).code)
}
