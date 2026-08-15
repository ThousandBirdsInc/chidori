//! Resume cost: VM image vs journal replay, as a function of run history.
//!
//! The claim under test is the one that decides whether a suspended agent can
//! be picked up by a different process: replay resume is O(total history),
//! image resume is O(live state). Both are measured on the same suspension.
//!
//! Run with `cargo bench -p chidori-js --bench vm_image`.

use chidori_js::replay::{DriveOutcome, ReplayRuntime};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use serde_json::{json, Value as Json};

/// A run that accumulates `iters` journaled host calls and then suspends,
/// while its live heap stays the same size throughout.
const BUNDLE: &str = r#"
    async function main() {
        let n = 0;
        for (let i = 0; i < ITERS; i++) { n += await step(i); }
        await step('suspend');
        report(n);
    }
    main();
"#;

/// Drive to the suspension after `iters` journaled calls; return the artifact.
fn artifact(iters: usize, imaging: bool) -> Vec<u8> {
    let bundle = BUNDLE.replace("ITERS", &iters.to_string());
    let mut rt = ReplayRuntime::record(&bundle, &["step", "report"]);
    if imaging {
        rt.enable_imaging();
    }
    let mut served = 0usize;
    let mut handler = |name: &str, _args: &Json| -> Option<Result<Json, String>> {
        if name == "step" {
            served += 1;
            if served > iters {
                return None; // suspend here
            }
            return Some(Ok(json!(1)));
        }
        Some(Ok(json!(null)))
    };
    match rt.drive(&mut handler).unwrap() {
        DriveOutcome::Suspended { .. } => {}
        DriveOutcome::Completed => panic!("expected a suspension"),
    }
    rt.to_blob(&["step", "report"])
}

fn bench_resume(c: &mut Criterion) {
    let mut group = c.benchmark_group("resume");
    for iters in [10usize, 100, 400, 1600] {
        let with_image = artifact(iters, true);
        let without = artifact(iters, false);

        group.throughput(Throughput::Elements(iters as u64));
        group.bench_with_input(BenchmarkId::new("replay", iters), &without, |b, blob| {
            b.iter(|| ReplayRuntime::from_blob(blob).unwrap())
        });
        group.bench_with_input(BenchmarkId::new("image", iters), &with_image, |b, blob| {
            b.iter(|| ReplayRuntime::from_blob(blob).unwrap())
        });
    }
    group.finish();
}

criterion_group!(benches, bench_resume);
criterion_main!(benches);
