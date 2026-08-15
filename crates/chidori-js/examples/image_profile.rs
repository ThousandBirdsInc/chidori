//! Where image-restore time actually goes, phase by phase.
//!
//! `from_image` is a fixed prologue (fresh VM, effect installation, baseline
//! walk, bundle compile) followed by work proportional to the image (serde
//! decode, graph rebuild). Only the prologue is amortizable, so it is worth
//! knowing how big it is before optimizing anything.
//!
//! Run with `cargo run --release -p chidori-js --example image_profile`.

use chidori_js::replay::{DriveOutcome, DurableBlob, ReplayRuntime};
use chidori_js::vm::Vm;
use serde_json::{json, Value as Json};
use std::time::Instant;

const BUNDLE: &str = r#"
    async function main() {
        let n = 0;
        for (let i = 0; i < ITERS; i++) { n += await step(i); }
        await step('suspend');
        report(n);
    }
    main();
"#;

const EFFECTS: &[&str] = &["step", "report"];

/// Drive to the suspension after `iters` journaled calls; return the artifact.
fn artifact(iters: usize) -> Vec<u8> {
    let bundle = BUNDLE.replace("ITERS", &iters.to_string());
    let mut rt = ReplayRuntime::record(&bundle, EFFECTS);
    rt.enable_imaging();
    let mut served = 0usize;
    let mut handler = |name: &str, _args: &Json| -> Option<Result<Json, String>> {
        if name == "step" {
            served += 1;
            if served > iters {
                return None;
            }
            return Some(Ok(json!(1)));
        }
        Some(Ok(json!(null)))
    };
    match rt.drive(&mut handler).unwrap() {
        DriveOutcome::Suspended { .. } => {}
        DriveOutcome::Completed => panic!("expected a suspension"),
    }
    rt.to_blob(EFFECTS)
}

/// Median of `n` runs of `f`, in microseconds.
fn med(n: usize, mut f: impl FnMut()) -> f64 {
    let mut vs: Vec<f64> = (0..n)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_secs_f64() * 1e6
        })
        .collect();
    vs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    vs[n / 2]
}

fn main() {
    let reps = 40;
    for iters in [10usize, 100, 400, 1600] {
        let blob = artifact(iters);
        let bundle = BUNDLE.replace("ITERS", &iters.to_string());

        // Phases, each measured on its own so the prologue can be separated
        // from the image-proportional work.
        let t_vm_new = med(reps, || {
            std::hint::black_box(Vm::new());
        });
        let t_record = med(reps, || {
            std::hint::black_box(ReplayRuntime::record(&bundle, EFFECTS));
        });
        let t_record_imaged = med(reps, || {
            let mut rt = ReplayRuntime::record(&bundle, EFFECTS);
            rt.enable_imaging();
            std::hint::black_box(rt);
        });
        let t_compile = med(reps, || {
            std::hint::black_box(chidori_js::compiler::compile_script_cached(&bundle).unwrap());
        });
        let t_decode = med(reps, || {
            let b: DurableBlob = serde_json::from_slice(&blob).unwrap();
            std::hint::black_box(b);
        });
        let decoded: DurableBlob = serde_json::from_slice(&blob).unwrap();
        let t_journal = med(reps, || {
            std::hint::black_box(decoded.journal.parse().unwrap());
        });
        let t_total = med(reps, || {
            std::hint::black_box(ReplayRuntime::from_blob(&blob).unwrap());
        });

        // Derived: install_effects+install_memo, the baseline mark, and
        // whatever restore_image costs once everything else is accounted for.
        let t_install = t_record - t_vm_new;
        let t_baseline = t_record_imaged - t_record;
        let t_rebuild = t_total - t_record_imaged - t_compile - t_decode - t_journal;

        println!("--- history {iters} effects  (blob {} bytes)", blob.len());
        println!("  Vm::new              {t_vm_new:8.1} us");
        println!("  install effects+memo {t_install:8.1} us");
        println!("  mark_image_baseline  {t_baseline:8.1} us");
        println!("  compile (cached)     {t_compile:8.1} us");
        println!("  serde decode (blob)  {t_decode:8.1} us");
        println!("  Journal::from_bytes  {t_journal:8.1} us");
        println!("  restore_image (rest) {t_rebuild:8.1} us");
        println!("  ------------------------------");
        println!("  from_blob TOTAL      {t_total:8.1} us");
        let prologue = t_vm_new + t_install + t_baseline;
        println!(
            "  amortizable prologue {prologue:8.1} us  ({:.0}% of total)",
            100.0 * prologue / t_total
        );
    }
}
