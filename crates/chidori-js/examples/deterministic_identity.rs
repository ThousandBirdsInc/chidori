//! Deterministic non-determinism — reproducible IDs, clocks, and choices.
//!
//! Agents constantly reach for non-deterministic primitives: the current time,
//! a random UUID, a sampled choice. If you replay a run for debugging, an audit,
//! or a time-travel "what did the agent see here?", those values must come back
//! identical — otherwise the replay diverges from history. Modelling them as
//! host effects means the first run records the value and every replay reads it
//! back, so an agent's run is perfectly reproducible.
//!
//! Run with:
//!     cargo run -p chidori-js --example deterministic_identity

use chidori_js::replay::{DriveOutcome, ReplayRuntime};
use serde_json::{json, Value as Json};

// Agent: stamp a workflow with a clock reading + a fresh id, then branch on a
// "random" sample. All three are non-deterministic host effects.
const AGENT: &str = r#"
    async function main() {
        const runId = await newId();
        const startedAt = await now();
        const roll = await sample();           // a number in [0, 1)
        const lane = roll < 0.5 ? "fast" : "slow";
        console.log("RESULT::" + JSON.stringify({ runId, startedAt, roll, lane }));
    }
    main();
"#;

const EFFECTS: &[&str] = &["newId", "now", "sample"];

fn result_line(console: &[String]) -> String {
    console
        .iter()
        .find_map(|l| l.strip_prefix("RESULT::"))
        .expect("agent printed a RESULT:: line")
        .to_string()
}

// Returns (printed result, live-effect-call count, journal bytes).
fn run(label: &str, journal: Option<&[u8]>) -> (String, u32, Vec<u8>) {
    let mut rt = match journal {
        None => ReplayRuntime::record(AGENT, EFFECTS),
        Some(bytes) => ReplayRuntime::restore(AGENT, bytes, EFFECTS).expect("restore"),
    };
    let mut live_calls = 0u32;
    {
        let mut handler = |name: &str, _args: &Json| -> Option<Result<Json, String>> {
            // These only run live during record. On replay they are served from
            // the journal and the handler is never reached.
            live_calls += 1;
            match name {
                "newId" => Some(Ok(json!("01J8Z3K9Qrecord"))),
                "now" => Some(Ok(json!(1_717_286_400_000i64))),
                "sample" => Some(Ok(json!(0.37))),
                _ => Some(Ok(Json::Null)),
            }
        };
        let outcome = rt.drive(&mut handler).expect("drive");
        assert!(matches!(outcome, DriveOutcome::Completed));
        println!("  [{label}] live non-deterministic calls: {live_calls}");
    }
    assert_eq!(rt.divergence(), None);
    (result_line(rt.console()), live_calls, rt.journal_bytes())
}

fn main() {
    println!("== record ==");
    let (recorded, rec_calls, journal) = run("record", None);
    println!("  stamped: {recorded}");
    assert_eq!(
        rec_calls, 3,
        "all three primitives sampled live during record"
    );

    println!("== replay ==");
    let (replayed, replay_calls, _) = run("replay", Some(&journal));
    println!("  stamped: {replayed}");
    assert_eq!(replay_calls, 0, "nothing sampled live on replay");

    assert_eq!(
        recorded, replayed,
        "id, clock, and sampled choice are identical on replay"
    );
    assert!(replayed.contains("\"lane\":\"fast\"")); // 0.37 < 0.5
    println!("\nOK: non-deterministic values reproduced exactly (0 live calls on replay).");
}
