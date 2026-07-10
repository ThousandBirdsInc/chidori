//! Phase-0 (carried-forward) **agent-replay benchmark**: how much of an agent
//! run's wall-clock is actually JS execution, versus host calls and the
//! journal/serialization machinery?
//!
//! This is the empirical backbone of the whole "no JIT" stance in
//! `docs/interpreter-optimization.md`: if JS is a small minority of an agent's
//! wall-clock (because each host call — an LLM round-trip — dwarfs the glue code
//! between calls), then heroic interpreter speedups (a JIT) buy little, and the
//! interpreter-level work in Phases 1–2 is the right ceiling of effort.
//!
//! It builds a representative agent — real glue compute (prompt building, string
//! scanning, modular arithmetic) interleaved with recorded host effects (an
//! `llm` call per step) — then measures three things on the SAME workload:
//!
//! ```text
//! compute_only  — the JS compute with no host boundary (pure interpreter).
//! record        — a live run building the journal (host returns instantly).
//! replay        — re-running from the journal (no host calls at all).
//! ```
//!
//! It also models the live wall-clock under representative LLM latencies.
//!
//! Run: `cargo run -q --release --example agent_replay -p chidori-js`

use std::time::{Duration, Instant};

use chidori_js::replay::{DriveOutcome, ReplayRuntime};
use chidori_js::Engine;
use serde_json::{json, Value as Json};

const STEPS: usize = 200;
const INNER: usize = 150;
/// The canned "LLM response" each host call returns — inlined into the
/// compute-only bundle so its post-processing compute matches exactly.
const RESP: &str = "response-0123456789-abcdefghijklmnop";

/// Agent glue compute for one step, shared by both bundles. `RESP_EXPR` is the
/// expression that yields the step's "response": a host `await llm(...)` in the
/// real agent, or the inlined literal in the compute-only variant.
fn step_body(resp_expr: &str) -> String {
    format!(
        r#"
        let prompt = 'step ' + step + ': ';
        for (let i = 0; i < {INNER}; i++) {{ prompt += ((i * step + 3) % 97); }}
        const resp = {resp_expr};
        let n = 0;
        for (let i = 0; i < resp.length; i++) {{ n = (n + resp.charCodeAt(i)) % 1000003; }}
        acc = (acc + n + prompt.length) % 1000000007;
        "#
    )
}

fn agent_bundle() -> String {
    format!(
        r#"
        async function main() {{
            let acc = 0;
            for (let step = 0; step < {STEPS}; step++) {{
                {body}
            }}
            report(acc);
        }}
        main();
        "#,
        body = step_body("await llm(prompt)")
    )
}

fn compute_only_bundle() -> String {
    format!(
        r#"
        (function() {{
            let acc = 0;
            for (let step = 0; step < {STEPS}; step++) {{
                {body}
            }}
            return acc;
        }})()
        "#,
        body = step_body(&format!("{RESP:?}"))
    )
}

/// Best (min) of `iters` timed runs of `f` — min is far more stable than mean on
/// a noisy shared/cloud host (it is the run least perturbed by interference).
fn best(iters: usize, mut f: impl FnMut()) -> Duration {
    let mut best = Duration::MAX;
    for _ in 0..iters {
        let t = Instant::now();
        f();
        best = best.min(t.elapsed());
    }
    best
}

fn record_once() -> (Vec<u8>, usize) {
    let mut rt = ReplayRuntime::record(&agent_bundle(), &["llm", "report"]);
    let mut calls = 0usize;
    let mut handler = |name: &str, _args: &Json| -> Option<Result<Json, String>> {
        if name == "llm" {
            calls += 1;
            Some(Ok(json!(RESP)))
        } else {
            Some(Ok(json!(null)))
        }
    };
    let outcome = rt.drive(&mut handler).unwrap();
    assert!(
        matches!(outcome, DriveOutcome::Completed),
        "agent must complete"
    );
    (rt.journal_bytes(), calls)
}

fn replay_once(journal: &[u8]) {
    let mut rt = ReplayRuntime::restore(&agent_bundle(), journal, &["llm", "report"]).unwrap();
    // The handler panics nothing: on replay every effect is served from the
    // journal, so a correct replay never calls it for a journaled effect.
    let mut handler =
        |_n: &str, _a: &Json| -> Option<Result<Json, String>> { Some(Ok(json!(null))) };
    let outcome = rt.drive(&mut handler).unwrap();
    assert!(matches!(outcome, DriveOutcome::Completed));
    assert_eq!(rt.divergence(), None, "replay must not diverge");
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn main() {
    // Warm + measure.
    let (journal, host_calls) = record_once();

    let t_compute = best(20, || {
        let mut e = Engine::new();
        let _ = e.eval(&compute_only_bundle()).expect("compute runs");
    });
    let t_record = best(10, || {
        let _ = record_once();
    });
    let t_replay = best(20, || replay_once(&journal));

    // engine_new (realm setup) is paid once per run and is inside the figures
    // above; subtract a measured estimate so the per-phase numbers are about the
    // agent work, not realm construction.
    let t_engine_new = best(50, || {
        let e = Engine::new();
        std::hint::black_box(&e);
    });

    let compute = ms(t_compute) - ms(t_engine_new);
    let replay = ms(t_replay);
    let record = ms(t_record);
    // The non-host work the journal adds on top of raw JS compute
    // (promise/microtask machinery, journal lookup + (de)serialization).
    let journal_overhead = (replay - compute).max(0.0);

    println!("=== Agent-replay composition ({STEPS} steps, {host_calls} host calls) ===\n");
    println!("journal size:            {} bytes", journal.len());
    println!(
        "engine_new (realm):      {:.3} ms (subtracted below)",
        ms(t_engine_new)
    );
    println!("JS compute only:         {compute:.3} ms");
    println!("journal/promise machinery:{journal_overhead:>8.3} ms");
    println!("full replay (no host):   {replay:.3} ms");
    println!("full record (host=0):    {record:.3} ms");
    println!(
        "  → per step: compute {:.4} ms, replay {:.4} ms",
        compute / STEPS as f64,
        replay / STEPS as f64
    );

    // Live wall-clock model: a real run pays the non-host work PLUS one real host
    // latency per host call. `record` (live, host=0) is the closest measured
    // proxy for that non-host work — it includes the journal *writes* a live run
    // does — so use it as the base. JS share = base / (base + host_calls * lat).
    let base = record;
    println!("\n=== Modeled LIVE wall-clock (JS+journal is fixed; host dominates) ===");
    println!("non-host base (JS + journal, = record): {base:.3} ms for {host_calls} calls\n");
    println!(
        "  {:>14}   {:>14}   {:>10}",
        "LLM latency/call", "live total", "JS+jrnl %"
    );
    for &lat_ms in &[50.0_f64, 200.0, 500.0, 1000.0, 2000.0] {
        let host = lat_ms * host_calls as f64;
        let live = base + host;
        println!(
            "  {:>11.0} ms   {:>11.0} ms   {:>9.3}%",
            lat_ms,
            live,
            100.0 * base / live
        );
    }
    println!(
        "\nInterpretation: even with host latency at an optimistic {} ms/call, JS+journal\n\
         work is a small fraction of live wall-clock — and a JIT would only chip at the\n\
         JS portion of that fraction. See docs/interpreter-optimization.md §11/§13.",
        50
    );
}
