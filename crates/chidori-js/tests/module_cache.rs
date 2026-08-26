//! The thread-local module compile cache: repeated compilations of the same
//! (label, source) reuse one artifact — the fixed per-run cost of re-walking
//! an unchanged module graph drops to a hash lookup per module — while any
//! change to the label or the source misses.

use chidori_js::compiler::{compile_module_labeled, compile_module_labeled_cached};

#[test]
fn same_label_and_source_share_one_proto() {
    let src = "export const answer = 42;\nexport function double(x) { return x * 2; }\n";
    let a = compile_module_labeled_cached(src, Some("/proj/a.ts")).unwrap();
    let b = compile_module_labeled_cached(src, Some("/proj/a.ts")).unwrap();
    // A hit is the SAME artifact, not an equal recompile.
    assert!(std::rc::Rc::ptr_eq(&a.proto, &b.proto));
    assert_eq!(a.cell_of_name, b.cell_of_name);
}

#[test]
fn label_and_source_changes_miss() {
    let src = "export const answer = 42;\n";
    let a = compile_module_labeled_cached(src, Some("/proj/a.ts")).unwrap();
    let other_label = compile_module_labeled_cached(src, Some("/proj/b.ts")).unwrap();
    assert!(!std::rc::Rc::ptr_eq(&a.proto, &other_label.proto));
    let other_src =
        compile_module_labeled_cached("export const answer = 43;\n", Some("/proj/a.ts")).unwrap();
    assert!(!std::rc::Rc::ptr_eq(&a.proto, &other_src.proto));
}

#[test]
fn cached_artifact_matches_a_fresh_compile() {
    let src = "export default function agent() { return 1 + 2; }\n";
    let cached = compile_module_labeled_cached(src, Some("/proj/agent.ts")).unwrap();
    let fresh = compile_module_labeled(src, Some("/proj/agent.ts")).unwrap();
    assert_eq!(cached.num_cells, fresh.num_cells);
    assert_eq!(cached.has_tla, fresh.has_tla);
    assert_eq!(cached.requested, fresh.requested);
    assert_eq!(cached.cell_of_name, fresh.cell_of_name);
}

#[test]
fn errors_are_not_cached_and_still_error() {
    let bad = "export const = ;";
    assert!(compile_module_labeled_cached(bad, Some("/proj/bad.ts")).is_err());
    assert!(compile_module_labeled_cached(bad, Some("/proj/bad.ts")).is_err());
}

/// Manual timing harness (not asserted in CI): `cargo test --release -p
/// chidori-js --test module_cache -- --ignored --nocapture` prints the fixed
/// per-run cost of a 40-module graph with the compile cache cold vs warm.
#[test]
#[ignore]
fn timing_repeated_graph_runs() {
    use std::time::Instant;
    let modules: Vec<(String, String)> = (0..40)
        .map(|i| {
            let pads: String = (0..60)
                .map(|k| {
                    format!(
                        "export function pad{i}_{k}(a, b) {{ const o = {{ a, b, k: {k} }}; \
                         return o.a + o.b + o.k; }}\n"
                    )
                })
                .collect();
            let body = format!(
                "export function f{i}(x) {{ let s = 0; for (let j = 0; j < 10; j++) s += x + j; \
                 return s; }}\n{pads}"
            );
            (format!("/proj/m{i}.ts"), body)
        })
        .collect();
    let entry_imports: String = (0..40)
        .map(|i| format!("import {{ f{i} }} from \"/proj/m{i}.ts\";\n"))
        .collect();
    let entry = format!("{entry_imports}export function agent() {{ return f0(1) + f39(2); }}\n");

    let run_once = || {
        let mut engine = chidori_js::Engine::new();
        let mut load = |spec: &str, _importer: &str| -> Result<(String, String), String> {
            modules
                .iter()
                .find(|(k, _)| k == spec)
                .map(|(k, s)| (k.clone(), s.clone()))
                .ok_or_else(|| format!("unknown {spec}"))
        };
        let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        engine
            .run_entrypoint_graph(
                "/proj/entry.ts",
                &entry,
                &serde_json::json!({}),
                &slot,
                "agent",
                &mut load,
            )
            .unwrap();
    };

    let t0 = Instant::now();
    run_once();
    let cold = t0.elapsed();
    let n = 30u32;
    let t1 = Instant::now();
    for _ in 0..n {
        run_once();
    }
    let warm = t1.elapsed() / n;
    println!("40-module graph: cold first run {cold:?}; steady-state per run {warm:?}");
}
