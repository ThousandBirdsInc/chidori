//! Value checkpoints with `durableStep` — bound replay cost on long histories.
//!
//! Replay re-executes your agent's code from the top, feeding journaled results
//! at each host call. That's cheap for I/O-bound agents, but an agent might also
//! do expensive *deterministic* work between effects: parse a big document,
//! build an embedding index, run a planner. Re-running that on every resume is
//! wasteful. `durableStep(fn)` runs `fn` once during record, journals its
//! (JSON-serializable) result, and on replay returns that value WITHOUT
//! re-running `fn`. The expensive work happens once, not on every replay.
//!
//! Run with:
//!     cargo run -p chidori-js --example durable_step

use chidori_js::replay::{DriveOutcome, ReplayRuntime};
use serde_json::{json, Value as Json};

// Agent: do an expensive deterministic "plan" step, then an I/O effect that
// uses it. The plan is wrapped in durableStep so it is memoized.
const AGENT: &str = r#"
    async function main() {
        const plan = await durableStep(() => {
            console.log("building expensive plan...");   // proof it ran
            let steps = [];
            for (let i = 0; i < 4; i++) steps.push("step-" + i);
            return { steps, cost: steps.length };
        });
        const ack = await dispatch({ plan });
        console.log("RESULT::" + JSON.stringify({ plan, ack }));
    }
    main();
"#;

const EFFECTS: &[&str] = &["dispatch"];

const BUILDING: &str = "building expensive plan...";

fn ran_expensive_step(console: &[String]) -> bool {
    console.iter().any(|l| l == BUILDING)
}

fn result_line(console: &[String]) -> String {
    console
        .iter()
        .find_map(|l| l.strip_prefix("RESULT::"))
        .expect("agent printed a RESULT:: line")
        .to_string()
}

fn main() {
    // ---- RECORD: the expensive step runs once. ----
    println!("== record ==");
    let mut rt = ReplayRuntime::record(AGENT, EFFECTS);
    {
        let mut handler = |name: &str, _args: &Json| -> Option<Result<Json, String>> {
            match name {
                "dispatch" => Some(Ok(json!({ "queued": true }))),
                _ => Some(Ok(Json::Null)),
            }
        };
        let outcome = rt.drive(&mut handler).expect("record drive");
        assert!(matches!(outcome, DriveOutcome::Completed));
    }
    assert!(ran_expensive_step(rt.console()), "step runs during record");
    let recorded = result_line(rt.console());
    println!("  expensive step ran during record: yes");
    println!("  result: {recorded}");

    let journal = rt.journal_bytes();

    // ---- REPLAY: the expensive step is NOT re-run; its value is cached. ----
    println!("== replay ==");
    let mut rt2 = ReplayRuntime::restore(AGENT, &journal, EFFECTS).expect("restore");
    {
        let mut handler = |name: &str, _args: &Json| -> Option<Result<Json, String>> {
            // dispatch replays from the journal — handler should not be hit.
            panic!("effect '{name}' fired live during replay");
        };
        let outcome = rt2.drive(&mut handler).expect("replay drive");
        assert!(matches!(outcome, DriveOutcome::Completed));
    }

    assert_eq!(rt2.divergence(), None);
    // The inner fn did NOT log again — it was memoized (its console line is
    // absent on replay), even though the surrounding pure code re-ran.
    assert!(
        !ran_expensive_step(rt2.console()),
        "durableStep must not re-run the expensive fn on replay"
    );
    let replayed = result_line(rt2.console());
    assert_eq!(recorded, replayed, "memoized value reproduced exactly");
    println!("  expensive step ran during replay: no (value served from journal)");
    println!("\nOK: expensive deterministic work happened once, reused on replay.");
}
