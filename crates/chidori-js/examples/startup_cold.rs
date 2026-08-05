//! Cold-process startup phase breakdown: what one `spawn → eval → exit` run
//! pays, phase by phase, in a FRESH process.
//!
//! The companion profiler `realm_profile` reports warm min-of-N numbers —
//! useful for section-vs-section comparison, but it hides exactly the costs a
//! process-per-script consumer (the benchmark harness startup baseline, CLI
//! one-shots) actually pays: first-touch page faults, lazy statics, and the
//! one-and-only realm build. Run this once per measurement; the numbers vary
//! run to run, so take a few and read the minimum.
//!
//! Run: `cargo run -q --release --example startup_cold -p chidori-js -- \
//!       crates/chidori-js/benchmarks/workloads/startup.js`

use std::time::Instant;

fn main() {
    let t_main = Instant::now();
    let src = std::fs::read_to_string(std::env::args().nth(1).unwrap()).unwrap();
    let t0 = Instant::now();
    let mut e = chidori_js::Engine::new();
    let t1 = Instant::now();
    let v = e.eval(&src);
    let t2 = Instant::now();
    for l in e.console() {
        println!("{l}");
    }
    match v {
        Ok(v) => println!("=> {}", e.vm.to_string_lossy(&v)),
        Err(err) => println!("ERROR: {err}"),
    }
    let t3 = Instant::now();
    eprintln!(
        "read: {:.3}ms  engine_new: {:.3}ms  eval: {:.3}ms  print: {:.3}ms  total-in-main: {:.3}ms",
        (t0 - t_main).as_secs_f64() * 1e3,
        (t1 - t0).as_secs_f64() * 1e3,
        (t2 - t1).as_secs_f64() * 1e3,
        (t3 - t2).as_secs_f64() * 1e3,
        (t3 - t_main).as_secs_f64() * 1e3,
    );
}
