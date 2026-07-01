//! Per-section realm-construction profiler: where `Engine::new()` time goes.
//!
//! Iterates the same [`chidori_js::builtins::SECTIONS`] table `install()`
//! runs, timing each section on a fresh realm, plus the whole-engine total.
//! This is the measurement behind the realm numbers in
//! `docs/resume-performance.md` — rerun it before acting on them.
//!
//! Run: `cargo run -q --release --example realm_profile -p chidori-js`

use std::time::Instant;

fn main() {
    const REALMS: usize = 20;

    // Whole-engine total (realm placeholder + wiring + all sections),
    // min-of-N to shed allocator/frequency noise.
    let mut totals = Vec::new();
    for _ in 0..REALMS {
        let t0 = Instant::now();
        let e = chidori_js::Engine::new();
        totals.push(t0.elapsed());
        std::hint::black_box(&e);
    }
    let total = totals.iter().min().unwrap();

    // Per-section breakdown. `Vm::new()` runs the full install, so time the
    // sections on their own pass: build the realm scaffolding by constructing
    // a Vm (sections are idempotent enough to re-run for timing purposes —
    // each re-installs over the same prototypes deterministically).
    let mut vm = chidori_js::Engine::new().vm;
    let mut rows: Vec<(&str, f64)> = Vec::new();
    for (name, section) in chidori_js::builtins::SECTIONS {
        let t0 = Instant::now();
        section(&mut vm);
        rows.push((name, t0.elapsed().as_secs_f64() * 1e3));
    }

    println!(
        "engine_new total: {:.3} ms (min of {REALMS})\n",
        total.as_secs_f64() * 1e3
    );
    println!("  {:<14} {:>9}  {:>6}", "section", "ms", "share");
    let sum: f64 = rows.iter().map(|(_, ms)| ms).sum();
    let mut sorted = rows.clone();
    sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    for (name, ms) in &sorted {
        println!("  {:<14} {:>9.3}  {:>5.1}%", name, ms, 100.0 * ms / sum);
    }
    println!("  {:<14} {:>9.3}", "(sum)", sum);
}
