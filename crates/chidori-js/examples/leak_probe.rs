//! RSS across repeated from_blob restores — the leak the ReplayRuntime Drop fixes.
use chidori_js::replay::{DriveOutcome, ReplayRuntime};
use serde_json::Value as Json;

fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap()
}

fn main() {
    let bundle = "async function main(){ await step(1); } main();";
    let mut rt = ReplayRuntime::record(bundle, &["step"]);
    rt.enable_imaging();
    let mut h = |_: &str, _: &Json| -> Option<Result<Json, String>> { None };
    match rt.drive(&mut h).unwrap() {
        DriveOutcome::Suspended { .. } => {}
        _ => panic!(),
    }
    let blob = rt.to_blob(&["step"]);
    drop(rt);

    for _ in 0..20 {
        let _ = ReplayRuntime::from_blob(&blob).unwrap();
    }
    let before = rss_kb();
    for _ in 0..300 {
        let _ = ReplayRuntime::from_blob(&blob).unwrap();
    }
    let after = rss_kb();
    println!(
        "300 restores: RSS {before} KB -> {after} KB ({:+} KB, {:.1} KB/restore)",
        after as i64 - before as i64,
        (after as f64 - before as f64) / 300.0
    );
}
