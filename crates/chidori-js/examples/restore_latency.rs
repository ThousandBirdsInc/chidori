//! Restore/replay latency probe: what a durable resume actually costs, and
//! what the compiled-bundle proto cache (`compiler::compile_script_cached`)
//! saves (see `docs/resume-performance.md`).
//!
//! A resume (`ReplayRuntime::from_blob` → drive) re-runs the recorded run:
//! it re-compiles the bundle and re-executes all JS with host effects served
//! from the journal. The proto cache removes the re-compile on every restore
//! after a thread's first. This probe records a run with a configurable
//! amount of code + journal entries, then measures restore+replay latency on
//! the first (cold: compiles) and subsequent (warm: cached proto) restores.
//!
//! Run: `cargo run -q --release --example restore_latency -p chidori-js`

use std::time::Instant;

use chidori_js::replay::{DriveOutcome, ReplayRuntime};
use serde_json::json;

/// Build a synthetic agent bundle: `funcs` distinct small functions (so the
/// bundle has realistic parse/compile surface, and each is genuinely reachable)
/// plus a driver that interleaves compute with `steps` host effects.
fn make_bundle(funcs: usize, steps: usize) -> String {
    let mut src = String::new();
    for i in 0..funcs {
        src.push_str(&format!(
            "function f{i}(x) {{ let s = x; for (let k = 0; k < 3; k++) s += (x * {i} + k) % 7; return s; }}\n"
        ));
    }
    src.push_str(&format!(
        r#"
        async function main() {{
            let acc = 0;
            for (let i = 0; i < {steps}; i++) {{
                const r = await effect('step-' + i);
                acc = f0(acc + r);
                for (let j = 1; j < {funcs}; j++) acc = (acc + fx(j, acc)) % 1000003;
            }}
            report(acc);
        }}
        function fx(j, v) {{ switch (j % 4) {{ {arms} }} return v; }}
        main();
    "#,
        steps = steps,
        funcs = funcs,
        arms = "case 0: return f1(v); case 1: return f2(v); case 2: return f3(v); default: return f4(v);"
    ));
    src
}

fn main() {
    const FUNCS: usize = 300;
    const STEPS: usize = 50;
    const RESTORES: usize = 20;

    let bundle = make_bundle(FUNCS, STEPS);
    println!("bundle: {} KB, {} host effects", bundle.len() / 1024, STEPS);

    // Record the run once to produce the durable blob.
    let mut rt = ReplayRuntime::record(&bundle, &["effect", "report"]);
    let mut handler =
        |name: &str, args: &serde_json::Value| -> Option<Result<serde_json::Value, String>> {
            match name {
                "effect" => {
                    let k = args[0].as_str().unwrap_or("");
                    Some(Ok(json!(k.len() as i64)))
                }
                _ => Some(Ok(json!(null))),
            }
        };
    match rt.drive(&mut handler).unwrap() {
        DriveOutcome::Completed => {}
        other => panic!("expected completion, got {other:?}"),
    }
    let blob = rt.to_blob(&["effect", "report"]);
    println!("blob: {} KB (bundle + journal)\n", blob.len() / 1024);

    // Measure restore+replay repeatedly on this (fresh) thread: restore #1
    // compiles the bundle (cold); every later restore reuses the thread's
    // cached proto (warm). The replay/re-execution portion is identical in
    // both, so cold − warm ≈ the compile cost the cache removes per restore.
    let mut noop = |_: &str, _: &serde_json::Value| -> Option<Result<serde_json::Value, String>> {
        Some(Ok(json!(null)))
    };
    let mut timings = Vec::new();
    for _ in 0..RESTORES {
        let t0 = Instant::now();
        let mut r = ReplayRuntime::from_blob(&blob).unwrap();
        match r.drive(&mut noop).unwrap() {
            DriveOutcome::Completed => {}
            other => panic!("expected completion, got {other:?}"),
        }
        assert_eq!(r.divergence(), None);
        timings.push(t0.elapsed());
    }

    let cold = timings[0];
    let warm_min = timings[1..].iter().min().unwrap();
    println!(
        "restore+replay  cold (compile + realm + replay): {:>8.3} ms",
        cold.as_secs_f64() * 1e3
    );
    println!(
        "restore+replay  warm (cached proto):             {:>8.3} ms",
        warm_min.as_secs_f64() * 1e3
    );
    println!(
        "per-restore compile cost removed by the cache:    ~{:>7.3} ms",
        (cold.as_secs_f64() - warm_min.as_secs_f64()) * 1e3
    );
}
