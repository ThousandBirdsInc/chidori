//! Resilient retries with a reproducible history.
//!
//! Agents call flaky things — rate-limited APIs, eventually-consistent stores,
//! services that 503 under load. The usual fix is a retry loop. With
//! record-and-replay the *whole* path the agent actually took (two failures,
//! then a success on the third try) is journaled. On replay you reproduce that
//! exact sequence without touching the live service — which may now be down, or
//! may now succeed on the first try and give you a different history. That makes
//! a flaky failure reproducible enough to debug.
//!
//! Run with:
//!     cargo run -p chidori-js --example retry_flaky_tool

use chidori_js::replay::{DriveOutcome, ReplayRuntime};
use serde_json::{json, Value as Json};

// Agent: retry a flaky fetch up to 5 times, recording each attempt's outcome.
const AGENT: &str = r#"
    async function main() {
        let attempts = [];
        let value = null;
        for (let i = 1; i <= 5; i++) {
            try {
                value = await flakyFetch({ key: "config", attempt: i });
                attempts.push("attempt " + i + ": ok");
                break;
            } catch (e) {
                attempts.push("attempt " + i + ": " + e.message);
            }
        }
        console.log("RESULT::" + JSON.stringify({ attempts, value }));
    }
    main();
"#;

const EFFECTS: &[&str] = &["flakyFetch"];

fn result_line(console: &[String]) -> String {
    console
        .iter()
        .find_map(|l| l.strip_prefix("RESULT::"))
        .expect("agent printed a RESULT:: line")
        .to_string()
}

fn main() {
    // ---- RECORD: the live service fails twice, then succeeds. ----
    println!("== record (live service is flaky) ==");
    let mut rt = ReplayRuntime::record(AGENT, EFFECTS);
    let mut live_attempts = 0u32;
    {
        let mut handler = |name: &str, _args: &Json| -> Option<Result<Json, String>> {
            match name {
                "flakyFetch" => {
                    live_attempts += 1;
                    if live_attempts < 3 {
                        println!("  [live] attempt {live_attempts}: 503 Service Unavailable");
                        // A rejected effect surfaces as a thrown Error in JS.
                        Some(Err("503 Service Unavailable".to_string()))
                    } else {
                        println!("  [live] attempt {live_attempts}: 200 OK");
                        Some(Ok(json!({ "config": { "flag": true } })))
                    }
                }
                _ => Some(Ok(Json::Null)),
            }
        };
        let outcome = rt.drive(&mut handler).expect("record drive");
        assert!(matches!(outcome, DriveOutcome::Completed));
    }
    assert_eq!(live_attempts, 3, "service hit exactly 3 times (2 fail, 1 ok)");
    let recorded = result_line(rt.console());
    println!("  recorded path: {recorded}");

    let journal = rt.journal_bytes();

    // ---- REPLAY: reproduce the exact failure/success path, no live calls. ----
    println!("== replay (service not touched) ==");
    let mut rt2 = ReplayRuntime::restore(AGENT, &journal, EFFECTS).expect("restore");
    {
        let mut handler = |_name: &str, _args: &Json| -> Option<Result<Json, String>> {
            panic!("flakyFetch hit the live service during replay!");
        };
        let outcome = rt2.drive(&mut handler).expect("replay drive");
        assert!(matches!(outcome, DriveOutcome::Completed));
    }

    assert_eq!(rt2.divergence(), None);
    let replayed = result_line(rt2.console());
    assert_eq!(recorded, replayed, "the retry history is reproduced exactly");
    println!("  replayed path: {replayed}");
    println!("\nOK: 2 failures + 1 success reproduced from the journal, 0 live calls.");
}
