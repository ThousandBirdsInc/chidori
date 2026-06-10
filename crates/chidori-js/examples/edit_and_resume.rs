//! Modify-and-resume — edit the agent mid-flight, keep the work you paid for.
//!
//! Because durability is deterministic *replay of a journal* (not a frozen
//! VM image), you can edit the agent's source and resume from a checkpoint.
//! Code BEFORE the execution frontier must still line up with the journal — its
//! effects already happened — so editing it is caught and rejected (fail-loud
//! divergence) rather than silently corrupting state. Code AFTER the frontier
//! is free to change: you keep every expensive effect already recorded and only
//! the new tail runs live.
//!
//! This is what lets you fix an agent's downstream logic after it has already
//! spent real money/time on upstream tool calls, and resume without redoing it.
//!
//! Run with:
//!     cargo run -p chidori-js --example edit_and_resume

use chidori_js::replay::{DriveOutcome, ReplayRuntime};
use serde_json::{json, Value as Json};

const EFFECTS: &[&str] = &["gather", "enrich", "publish", "report"];

// v1: gather data, enrich it, then publish a summary. We suspend at `enrich`
// (the frontier) after the expensive `gather` has already run.
const V1: &str = r#"
    async function main() {
        const raw = await gather({ source: "crm" });
        const enriched = await enrich(raw);
        const summary = "rows=" + enriched.rows;
        await publish({ summary });
        report({ summary });
    }
    main();
"#;

fn main() {
    // ---- Run v1 until the frontier (after `gather`), then suspend. ----
    println!("== run v1: gather succeeds, suspend at enrich ==");
    let mut rt = ReplayRuntime::record(V1, EFFECTS);
    {
        let mut handler = |name: &str, _args: &Json| -> Option<Result<Json, String>> {
            match name {
                "gather" => {
                    println!("  [live] expensive gather from CRM...");
                    Some(Ok(json!({ "rows": 128 })))
                }
                // frontier: not ready, suspend.
                "enrich" => None,
                _ => Some(Ok(Json::Null)),
            }
        };
        assert!(matches!(
            rt.drive(&mut handler).expect("drive"),
            DriveOutcome::Suspended { .. }
        ));
    }
    let journal = rt.journal_bytes();
    println!("  suspended; `gather` result is banked in the journal.\n");

    // ---- Edit the agent AFTER the frontier and resume. ----
    // We changed how the summary is built and added a second publish field.
    // `gather` (pre-frontier) is untouched, so resume is clean and `gather`
    // does NOT run again.
    let v2 = r#"
        async function main() {
            const raw = await gather({ source: "crm" });
            const enriched = await enrich(raw);
            const summary = "rows=" + enriched.rows + " (enriched)";
            await publish({ summary, format: "v2" });
            report({ summary, version: 2 });
        }
        main();
    "#;
    println!("== resume v2: edited post-frontier logic ==");
    let mut rt2 = ReplayRuntime::restore(v2, &journal, EFFECTS).expect("restore");
    let mut final_report = Json::Null;
    {
        let mut handler = |name: &str, _args: &Json| -> Option<Result<Json, String>> {
            match name {
                "gather" => panic!("gather must NOT re-run — it is pre-frontier"),
                "enrich" => {
                    println!("  [live] enriching (frontier resolves now)");
                    Some(Ok(json!({ "rows": 128 })))
                }
                "publish" => {
                    println!("  [live] publish: {}", _args[0]);
                    Some(Ok(Json::Null))
                }
                "report" => {
                    final_report = _args[0].clone();
                    Some(Ok(Json::Null))
                }
                _ => Some(Ok(Json::Null)),
            }
        };
        assert!(matches!(
            rt2.drive(&mut handler).expect("resume drive"),
            DriveOutcome::Completed
        ));
    }
    assert_eq!(rt2.divergence(), None);
    assert_eq!(final_report["version"], json!(2));
    println!("  resumed with v2 logic: {final_report}\n");

    // ---- The safety rail: editing PRE-frontier code is rejected. ----
    // Here we swap the already-executed `gather` for a different effect. The
    // journal's first entry is `gather#0`, but the edited program calls
    // `gatherV2#0` first — divergence is detected and the run fails loud.
    let bad = r#"
        async function main() {
            const raw = await gatherV2({ source: "crm" });   // <-- pre-frontier edit
            const enriched = await enrich(raw);
            report({ rows: enriched.rows });
        }
        main();
    "#;
    println!("== resume with an illegal pre-frontier edit ==");
    let mut rt3 = ReplayRuntime::restore(
        bad,
        &journal,
        &["gather", "gatherV2", "enrich", "publish", "report"],
    )
    .expect("restore");
    let err = {
        let mut handler =
            |_n: &str, _a: &Json| -> Option<Result<Json, String>> { Some(Ok(Json::Null)) };
        rt3.drive(&mut handler)
    };
    assert!(err.is_err(), "pre-frontier edit must fail loud");
    let div = rt3.divergence().expect("divergence recorded");
    assert!(div.contains("already-executed"));
    println!("  rejected as expected: {div}");
    println!("\nOK: post-frontier edits resume cleanly; pre-frontier edits are blocked.");
}
