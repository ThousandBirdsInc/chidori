//! Heap-utilization benchmarks for the chidori-js engine: how much memory the
//! realm, the compiler, and the interpreter actually use — the complement to
//! the wall-clock numbers in `benches/execution.rs`, over the same workloads.
//!
//! A counting `#[global_allocator]` (wrapping std's `System`) tracks live
//! bytes, the high-water mark, and cumulative allocation traffic, so every
//! number below is exact for this process, not an RSS approximation. Because
//! the engine is deterministic by design, the numbers are stable run-to-run;
//! each measurement is taken once rather than sampled.
//!
//! Four angles:
//!   * `realm` — footprint of a fresh `Engine::new()`: bytes retained by the
//!     realm (globals + built-in prototypes), the peak while constructing it,
//!     and total allocation traffic.
//!   * `compile` — bytecode footprint: bytes retained by the compiled
//!     `FuncProto` for each workload, plus front-end churn.
//!   * `eval` — end-to-end run on a fresh engine: peak heap over the realm
//!     baseline, allocation churn, bytes the engine retains after the run,
//!     and after `collect_cycles()`.
//!   * `steady_state` — leak check: run each workload many times on one
//!     engine (compile once) and report retained growth per run after
//!     warmup + cycle collection. For a durable execution engine this should
//!     be ~0.
//!
//! Run with: `cargo bench -p chidori-js --bench memory`
//! Filter:   `cargo bench -p chidori-js --bench memory -- fib`

use std::alloc::{GlobalAlloc, Layout, System};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use chidori_js::bytecode::FuncProto;
use chidori_js::compiler::compile_script;
use chidori_js::{Engine, Value};

// Workload corpus shared with `benches/execution.rs` so time and memory
// numbers describe the same programs.
#[path = "common/workloads.rs"]
mod workloads;
use workloads::WORKLOADS;

// ---------------------------------------------------------------------------
// Tracking allocator
// ---------------------------------------------------------------------------

/// Live (allocated-minus-freed) bytes.
static LIVE: AtomicUsize = AtomicUsize::new(0);
/// High-water mark of `LIVE`. Reset to the current level at the start of each
/// measurement (see [`Section::measure`]).
static PEAK: AtomicUsize = AtomicUsize::new(0);
/// Cumulative bytes ever allocated (churn; never decremented).
static TOTAL: AtomicUsize = AtomicUsize::new(0);
/// Cumulative allocation events (growing reallocs count as one event).
static EVENTS: AtomicUsize = AtomicUsize::new(0);

fn charge(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Relaxed) + bytes;
    PEAK.fetch_max(live, Relaxed);
    TOTAL.fetch_add(bytes, Relaxed);
    EVENTS.fetch_add(1, Relaxed);
}

/// `System` plus byte accounting. The bench runs single-threaded, so relaxed
/// atomics are exact here, and resetting `PEAK` between measurements is sound.
struct TrackingAlloc;

unsafe impl GlobalAlloc for TrackingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = System.alloc(layout);
        if !p.is_null() {
            charge(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = System.alloc_zeroed(layout);
        if !p.is_null() {
            charge(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        LIVE.fetch_sub(layout.size(), Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = System.realloc(ptr, layout, new_size);
        if !p.is_null() {
            // Charge only the delta: a grow is one event of `new - old` fresh
            // bytes, a shrink just returns the difference to the pool.
            if new_size >= layout.size() {
                charge(new_size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Relaxed);
            }
        }
        p
    }
}

#[global_allocator]
static ALLOC: TrackingAlloc = TrackingAlloc;

// ---------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------

/// Heap deltas across one measured closure.
struct Measure {
    /// Live-byte growth from entry to exit (negative = net free).
    retained: isize,
    /// High-water mark reached inside the closure, relative to entry.
    peak: usize,
    /// Bytes allocated inside the closure, freed or not (churn).
    churn: usize,
    /// Allocation events inside the closure.
    events: usize,
}

/// Run `f` and report its heap deltas. The peak high-water mark is reset to
/// the current live level on entry so `peak` is relative to this measurement.
fn measure<R>(f: impl FnOnce() -> R) -> (Measure, R) {
    let live0 = LIVE.load(Relaxed);
    PEAK.store(live0, Relaxed);
    let total0 = TOTAL.load(Relaxed);
    let events0 = EVENTS.load(Relaxed);
    let r = f();
    let m = Measure {
        retained: LIVE.load(Relaxed) as isize - live0 as isize,
        peak: PEAK.load(Relaxed).saturating_sub(live0),
        churn: TOTAL.load(Relaxed) - total0,
        events: EVENTS.load(Relaxed) - events0,
    };
    (m, r)
}

fn fmt_bytes(n: f64) -> String {
    let (mag, sign) = (n.abs(), if n < 0.0 { "-" } else { "" });
    if mag >= 1024.0 * 1024.0 {
        format!("{sign}{:.2} MiB", mag / (1024.0 * 1024.0))
    } else if mag >= 1024.0 {
        format!("{sign}{:.1} KiB", mag / 1024.0)
    } else {
        format!("{sign}{mag:.0} B")
    }
}

fn fmt_count(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1e6)
    } else if n >= 10_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

/// Build a fresh closure for `proto` and run it to completion on `engine`
/// (same shape as the `interp` group in `benches/execution.rs`).
fn run_proto(engine: &mut Engine, proto: &Rc<FuncProto>) -> Value {
    let func = engine.vm.make_closure(proto.clone(), Vec::new());
    engine
        .vm
        .call(Value::Object(func), Value::Undefined, &[])
        .expect("benchmark workload must not throw")
}

// ---------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------

fn bench_realm() {
    println!("== realm: Engine::new() footprint ==");
    let (m, engine) = measure(Engine::new);
    println!(
        "  retained {:>10}   construction peak {:>10}   churn {:>10} in {} allocs   {} live objects",
        fmt_bytes(m.retained as f64),
        fmt_bytes(m.peak as f64),
        fmt_bytes(m.churn as f64),
        fmt_count(m.events),
        fmt_count(engine.vm.gc_tracked_live()),
    );
    drop(engine);
    println!();
}

fn bench_compile(filter: Option<&str>) {
    println!("== compile: bytecode footprint (parse + lower, no execution) ==");
    println!(
        "  {:<16} {:>10} {:>10} {:>10} {:>8}",
        "workload", "retained", "peak", "churn", "allocs"
    );
    for (name, src) in selected(filter) {
        let (m, proto) = measure(|| compile_script(src).expect("compiles"));
        println!(
            "  {:<16} {:>10} {:>10} {:>10} {:>8}",
            name,
            fmt_bytes(m.retained as f64),
            fmt_bytes(m.peak as f64),
            fmt_bytes(m.churn as f64),
            fmt_count(m.events),
        );
        drop(proto);
    }
    println!();
}

fn bench_eval(filter: Option<&str>) {
    println!("== eval: full run on a fresh engine (peak/retained over the realm baseline) ==");
    println!(
        "  {:<16} {:>10} {:>10} {:>8} {:>10} {:>10}",
        "workload", "peak", "churn", "allocs", "retained", "after gc"
    );
    for (name, src) in selected(filter) {
        let mut engine = Engine::new();
        let live0 = LIVE.load(Relaxed) as isize;
        let (m, value) = measure(|| engine.eval(src).expect("evaluates"));
        drop(value);
        // What the engine still holds once the run's garbage cycles are gone
        // (interned atoms, shapes, caches — the cost of keeping a warm realm).
        engine.vm.collect_cycles();
        let after_gc = LIVE.load(Relaxed) as isize - live0;
        println!(
            "  {:<16} {:>10} {:>10} {:>8} {:>10} {:>10}",
            name,
            fmt_bytes(m.peak as f64),
            fmt_bytes(m.churn as f64),
            fmt_count(m.events),
            fmt_bytes(m.retained as f64),
            fmt_bytes(after_gc as f64),
        );
    }
    println!();
}

fn bench_steady_state(filter: Option<&str>) {
    const RUNS: usize = 10;
    println!("== steady_state: {RUNS} repeat runs on one engine (leak check; growth/run should be ~0) ==");
    println!(
        "  {:<16} {:>12} {:>12} {:>10}",
        "workload", "growth/run", "peak/run", "churn/run"
    );
    for (name, src) in selected(filter) {
        let proto = Rc::new(compile_script(src).expect("compiles"));
        let mut engine = Engine::new();
        // Warm up once so one-time lazy work (interned strings, caches) is
        // attributed to warmup, not counted as steady-state growth.
        drop(run_proto(&mut engine, &proto));
        engine.vm.collect_cycles();

        // Peak is tracked per run (reset to that run's live level each time),
        // so `worst_peak` is the worst single run — the outer `m.peak` would
        // be clobbered by those resets and is not used here.
        let (m, worst_peak) = measure(|| {
            let mut worst = 0usize;
            for _ in 0..RUNS {
                let base = LIVE.load(Relaxed);
                PEAK.store(base, Relaxed);
                drop(run_proto(&mut engine, &proto));
                worst = worst.max(PEAK.load(Relaxed).saturating_sub(base));
            }
            engine.vm.collect_cycles();
            worst
        });
        println!(
            "  {:<16} {:>12} {:>12} {:>10}",
            name,
            fmt_bytes(m.retained as f64 / RUNS as f64),
            fmt_bytes(worst_peak as f64),
            fmt_bytes(m.churn as f64 / RUNS as f64),
        );
    }
    println!();
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

fn selected(filter: Option<&str>) -> impl Iterator<Item = (&'static str, &'static str)> + '_ {
    WORKLOADS
        .iter()
        .copied()
        .filter(move |(name, _)| filter.is_none_or(|f| name.contains(f)))
}

fn main() {
    let mut filter: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            // cargo passes `--bench` to every bench binary; criterion-style
            // positional filters are supported for muscle-memory parity.
            "--bench" => {}
            "--filter" => filter = args.next(),
            "-h" | "--help" => {
                println!(
                    "usage: cargo bench -p chidori-js --bench memory -- [FILTER]\n\n\
                     Reports heap utilization (retained/peak/churn) for the realm,\n\
                     the compiler, and the interpreter over the shared benchmark\n\
                     workloads. FILTER is a substring match on workload names."
                );
                return;
            }
            other if !other.starts_with('-') => filter = Some(other.to_string()),
            other => {
                eprintln!("unknown option: {other}");
                std::process::exit(2);
            }
        }
    }
    let filter = filter.as_deref();

    println!("chidori-js heap utilization (exact, via tracking global allocator)\n");
    bench_realm();
    bench_compile(filter);
    bench_eval(filter);
    bench_steady_state(filter);
}
