//! ES module graphs with circular imports and re-exported namespaces.
//!
//! Real npm packages (zod, smol-toml, …) ship legal-ESM cycles and wire whole
//! subtrees through `import * as ns; export { ns }`. The engine must link them
//! with correct live bindings regardless of the order modules are wired in —
//! regression tests for the "Cannot access binding before initialization"
//! failure where a re-exported namespace import's cell was replaced after an
//! importer had already captured it.

use std::collections::HashMap;

use chidori_js::Engine;

/// Run `entry` against an in-memory module map; specifiers resolve verbatim.
fn run_graph(entry: &str, modules: &[(&str, &str)]) -> Result<serde_json::Value, String> {
    let sources: HashMap<String, String> = modules
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let mut engine = Engine::new();
    let slot = engine.install_entrypoint();
    let mut load = |specifier: &str, _importer: &str| -> Result<(String, String), String> {
        sources
            .get(specifier)
            .map(|src| (specifier.to_string(), src.clone()))
            .ok_or_else(|| format!("unknown module: {specifier}"))
    };
    let entry_src = sources
        .get(entry)
        .cloned()
        .ok_or_else(|| format!("unknown entry: {entry}"))?;
    engine.run_entrypoint_graph(
        entry,
        &entry_src,
        &serde_json::json!({}),
        &slot,
        "agent",
        &mut load,
    )
}

#[test]
fn function_cycle_calls_work_after_evaluation() {
    let out = run_graph(
        "entry",
        &[
            (
                "entry",
                r#"import { callB } from "a"; run(async () => callB());"#,
            ),
            (
                "a",
                r#"import { fromB } from "b";
                   export function fromA() { return "A"; }
                   export function callB() { return fromB(); }"#,
            ),
            (
                "b",
                r#"import { fromA } from "a";
                   export function fromB() { return fromA() + "B"; }"#,
            ),
        ],
    )
    .unwrap();
    assert_eq!(out, serde_json::json!("AB"));
}

#[test]
fn namespace_import_cycle_works() {
    let out = run_graph(
        "entry",
        &[
            (
                "entry",
                r#"import { callB } from "a"; run(async () => callB());"#,
            ),
            (
                "a",
                r#"import * as b from "b";
                   export function fromA() { return "A"; }
                   export function callB() { return b.fromB(); }"#,
            ),
            (
                "b",
                r#"import * as a from "a";
                   export function fromB() { return a.fromA() + "B"; }"#,
            ),
        ],
    )
    .unwrap();
    assert_eq!(out, serde_json::json!("AB"));
}

/// The zod entry shape: `import * as z from "./inner"; export { z }`, with the
/// importer of `z` wired BEFORE the re-exporting module (the entry is first in
/// graph order). The captured cell must be the one the namespace lands in.
#[test]
fn reexported_namespace_import_is_initialized_for_earlier_importers() {
    let out = run_graph(
        "entry",
        &[
            (
                "entry",
                r#"import { z } from "index";
                   run(async () => typeof z.object + ":" + z.object());"#,
            ),
            (
                "index",
                r#"import * as z from "inner";
                   export * from "inner";
                   export { z };
                   export default z;"#,
            ),
            ("inner", r#"export function object() { return "obj"; }"#),
        ],
    )
    .unwrap();
    assert_eq!(out, serde_json::json!("function:obj"));
}

/// zod's internal cycle in miniature: two modules namespace-importing each
/// other, re-exported through `export *` chains up to the package entry.
#[test]
fn namespace_cycle_behind_star_reexport_chain() {
    let out = run_graph(
        "entry",
        &[
            (
                "entry",
                r#"import { z } from "index";
                   run(async () => new z.ZodString().kind + z.iso.datetime());"#,
            ),
            ("index", r#"import * as z from "external"; export { z };"#),
            (
                "external",
                r#"export * from "schemas"; export * as iso from "iso";"#,
            ),
            (
                "schemas",
                r#"import * as iso from "iso";
                   export class ZodString { constructor() { this.kind = "string:"; } }
                   export function usesIso() { return iso.datetime(); }"#,
            ),
            (
                "iso",
                r#"import * as schemas from "schemas";
                   export function datetime() { return "iso-datetime"; }
                   export function makesString() { return new schemas.ZodString(); }"#,
            ),
        ],
    )
    .unwrap();
    assert_eq!(out, serde_json::json!("string:iso-datetime"));
}
