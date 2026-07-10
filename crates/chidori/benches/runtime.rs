//! Runtime-layer benchmarks: the durability and per-run costs that live
//! OUTSIDE the JS engine. The `chidori-js` crate has deep bench coverage of
//! the interpreter (`benches/execution.rs`, `benches/memory.rs`, the
//! cross-runtime harness in `benchmarks/`); this file gives the runtime crate
//! the same footing for the paths a live agent actually pays per host call,
//! per state transition, and per run:
//!
//!   * `replay_lookup`      — `RuntimeContext::try_replay` over a growing
//!                            journal. A resume calls this once per recorded
//!                            effect; the lookup is a linear scan today
//!                            (`runtime/context.rs`), so a full resume sweep
//!                            is O(N²) in journal length.
//!   * `record_call`        — the live-path cost of recording one host call
//!                            as payloads grow (deep `CallRecord` clones).
//!   * `session_store_put`  — `SqliteStore::put` as the session's call log
//!                            grows. Every pause/resume/approval state
//!                            transition re-serializes the WHOLE session blob
//!                            and pays a full fsync (no WAL) today
//!                            (`storage.rs`).
//!   * `run_store_append`   — `SqliteRunStore::append_record` against a run
//!                            that already holds K records. The append's
//!                            `MAX(pos)` subquery scans the run's rows, so
//!                            appending the K-th record is O(K) today
//!                            (`runtime/store.rs`) despite the trait's O(1)
//!                            contract.
//!   * `per_run_setup`      — fixed construction costs paid per agent run /
//!                            per fetch: `new_tokio_runtime()` (built per run
//!                            in `server.rs`/`scheduler.rs`) and a
//!                            `reqwest::Client` (built per `fetch()` host
//!                            call in `runtime/host_core.rs`).
//!
//! Run with: `cargo bench -p chidori --bench runtime`
//! Smoke-check (each bench once, no statistics): append `-- --test`.
//!
//! The scaling groups parameterize size so the growth CURVE is visible in one
//! report — a fix for any of the costs above should flatten the curve, and a
//! regression re-steepens it.

use std::time::Duration;

use chidori::framework::{CallRecord, RuntimeContext};
use chidori::runtime::store::{RunStore, SqliteRunStore, SqliteRunStoreShared};
use chidori::storage::{SessionStatus, SessionStore, SqliteStore, StoredSession};
use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use std::hint::black_box;

/// A representative recorded host call with a result payload of roughly
/// `payload_bytes` of JSON. `seq` is 1-based to match the runtime's counter.
fn record(seq: u64, payload_bytes: usize) -> CallRecord {
    CallRecord {
        seq,
        parent_seq: None,
        function: "tool".into(),
        args: serde_json::json!({ "name": "search", "query": "chidori" }),
        result: serde_json::json!({ "text": "x".repeat(payload_bytes) }),
        duration_ms: 3,
        token_usage: None,
        timestamp: chrono::Utc::now(),
        error: None,
    }
}

fn journal(n: u64, payload_bytes: usize) -> Vec<CallRecord> {
    (1..=n).map(|seq| record(seq, payload_bytes)).collect()
}

/// A resume sweep: look up every recorded seq in order and absorb its
/// subtree, exactly as `host_core::execute_host_call` does per replay hit.
/// Fresh context per iteration because `try_replay` re-records each hit into
/// the new call log.
fn bench_replay_lookup(c: &mut Criterion) {
    let mut g = c.benchmark_group("replay_lookup");
    for n in [100u64, 1000, 4000] {
        let base = journal(n, 256);
        g.throughput(Throughput::Elements(n));
        g.bench_function(format!("sweep_n{n}"), |b| {
            b.iter_batched(
                || RuntimeContext::with_replay(base.clone()),
                |ctx| {
                    for seq in 1..=n {
                        black_box(ctx.try_replay(seq));
                        ctx.absorb_replayed_subtree(seq);
                    }
                },
                BatchSize::LargeInput,
            )
        });
    }
    g.finish();
}

/// Live-path recording cost per host call as the result payload grows —
/// dominated by the deep `CallRecord` clone(s) in `record_call`.
fn bench_record_call(c: &mut Criterion) {
    const CALLS: u64 = 32;
    let mut g = c.benchmark_group("record_call");
    for payload in [1usize << 10, 64 << 10, 512 << 10] {
        let rec = record(1, payload);
        g.throughput(Throughput::Elements(CALLS));
        g.bench_function(format!("payload_{}k", payload >> 10), |b| {
            b.iter_batched(
                RuntimeContext::new,
                |ctx| {
                    for seq in 1..=CALLS {
                        let mut r = rec.clone();
                        r.seq = seq;
                        ctx.record_call(r);
                    }
                },
                BatchSize::LargeInput,
            )
        });
    }
    g.finish();
}

/// One session state transition (`put`) as the session's call log grows.
/// Upserts the same row repeatedly, matching production (every transition
/// rewrites the one session), so the database does not grow across
/// iterations.
fn bench_session_store_put(c: &mut Criterion) {
    let mut g = c.benchmark_group("session_store_put");
    // fsync-bound: keep samples low so the group stays fast.
    g.sample_size(20).measurement_time(Duration::from_secs(4));
    for n in [10u64, 100, 1000] {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SqliteStore::open(dir.path().join("sessions.db")).expect("open");
        let session = StoredSession {
            id: "bench-session".into(),
            run_id: Some("bench-run".into()),
            status: SessionStatus::Paused,
            input: serde_json::json!({ "goal": "benchmark" }),
            output: None,
            call_log: journal(n, 1 << 10),
            error: None,
            pending_seq: Some(n),
            pending_prompt: Some("continue?".into()),
            pending_signal_name: None,
            pending_signal_names: Vec::new(),
            pending_signal_deadline: None,
            pending_approval: None,
            approvals: Vec::new(),
            policy_profile: None,
            created_at: chrono::Utc::now(),
        };
        g.bench_function(format!("log_n{n}"), |b| {
            b.iter(|| store.put(black_box(&session)).expect("put"))
        });
    }
    g.finish();
}

/// Appending one record to a run that already holds K records. The append
/// upserts the same seq every iteration (replace semantics), so the run stays
/// at K+1 rows while the `MAX(pos)` subquery still scans all of them —
/// isolating the per-append cost at that journal size.
fn bench_run_store_append(c: &mut Criterion) {
    let mut g = c.benchmark_group("run_store_append");
    g.sample_size(20).measurement_time(Duration::from_secs(4));
    for k in [0u64, 1000, 5000] {
        let dir = tempfile::tempdir().expect("tempdir");
        let shared = SqliteRunStoreShared::open(&dir.path().join("runs.db")).expect("open");
        let store = SqliteRunStore::new(shared, "bench-run");
        for seq in 1..=k {
            store.append_record(&record(seq, 256)).expect("prefill");
        }
        let next = record(k + 1, 256);
        g.bench_function(format!("after_k{k}"), |b| {
            b.iter(|| store.append_record(black_box(&next)).expect("append"))
        });
    }
    g.finish();
}

/// Fixed construction costs currently paid per run (tokio runtime, built in
/// `server.rs::build_engine` and `scheduler.rs::run_once`) and per `fetch()`
/// host call (`reqwest::Client`, built in `host_core.rs`).
fn bench_per_run_setup(c: &mut Criterion) {
    let mut g = c.benchmark_group("per_run_setup");
    g.sample_size(20).measurement_time(Duration::from_secs(4));
    g.bench_function("tokio_runtime_build_drop", |b| {
        b.iter(|| drop(black_box(chidori::new_tokio_runtime().expect("runtime"))))
    });
    g.bench_function("tokio_runtime_shared", |b| {
        b.iter(|| black_box(chidori::shared_tokio_runtime().expect("runtime")))
    });
    g.bench_function("reqwest_client_build", |b| {
        b.iter(|| {
            black_box(
                reqwest::Client::builder()
                    .gzip(true)
                    .build()
                    .expect("client"),
            )
        })
    });
    g.finish();
}

criterion_group!(
    benches,
    bench_replay_lookup,
    bench_record_call,
    bench_session_store_put,
    bench_run_store_append,
    bench_per_run_setup
);
criterion_main!(benches);
