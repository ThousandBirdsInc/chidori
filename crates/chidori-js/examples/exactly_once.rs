//! Exactly-once side effects — the core durability guarantee for agents.
//!
//! Agents take real-world actions: charge a card, send an email, provision a
//! resource, open a ticket. If a run crashes and resumes, or you replay it to
//! inspect what happened, those actions must NOT fire again. Record-and-replay
//! gives you that for free: a side-effecting host call runs once during record
//! and is served from the journal on every replay — the live handler is never
//! re-invoked.
//!
//! Run with:
//!     cargo run -p chidori-js --example exactly_once

use chidori_js::replay::{DriveOutcome, ReplayRuntime};
use serde_json::{json, Value as Json};

// A tiny "agent": notify a user, then provision a resource for them. Both are
// external side effects modelled as host calls. The final result is emitted via
// console.log (a pure VM op) rather than another effect — so it is recomputed
// on replay, letting us prove the replay reproduces it without the side effects
// firing again.
const AGENT: &str = r#"
    async function main() {
        const ticket = await openTicket({ subject: "onboard new user" });
        const email  = await sendEmail({ to: "ada@example.com", ticket });
        console.log("RESULT::" + JSON.stringify({ ticket, email }));
    }
    main();
"#;

const EFFECTS: &[&str] = &["openTicket", "sendEmail"];

// Pull the single `RESULT::<json>` line the agent prints out of the console.
fn result_line(console: &[String]) -> &str {
    console
        .iter()
        .find_map(|l| l.strip_prefix("RESULT::"))
        .expect("agent printed a RESULT:: line")
}

fn main() {
    // ---- RECORD: the side effects actually happen, exactly once each. ----
    let mut sends = 0u32;
    let mut tickets = 0u32;

    let mut rt = ReplayRuntime::record(AGENT, EFFECTS);
    {
        let mut handler = |name: &str, args: &Json| -> Option<Result<Json, String>> {
            match name {
                "openTicket" => {
                    tickets += 1;
                    println!("  [live] opening ticket #{tickets}: {}", args[0]);
                    Some(Ok(json!(format!("TICKET-{tickets:04}"))))
                }
                "sendEmail" => {
                    sends += 1;
                    println!("  [live] sending email (send #{sends})");
                    Some(Ok(
                        json!({ "delivered": true, "id": format!("msg-{sends}") }),
                    ))
                }
                _ => Some(Ok(Json::Null)),
            }
        };
        println!("== record ==");
        let outcome = rt.drive(&mut handler).expect("record drive");
        assert!(matches!(outcome, DriveOutcome::Completed));
    }
    assert_eq!(tickets, 1, "ticket opened exactly once");
    assert_eq!(sends, 1, "email sent exactly once");
    let recorded_result = result_line(rt.console()).to_string();
    println!("  computed: {recorded_result}");

    // The journal is the durable artifact: tiny, JSON, the source of truth.
    let journal = rt.journal_bytes();
    println!(
        "  journal: {} ({} bytes)",
        String::from_utf8_lossy(&journal),
        journal.len()
    );

    // ---- REPLAY: a fresh process re-runs the SAME code. No effect fires. ----
    // The handler panics if called, proving every side effect is served from
    // the journal rather than re-executed. The agent's pure code (the final
    // console.log) still runs, so we can compare the recomputed result.
    let mut rt2 = ReplayRuntime::restore(AGENT, &journal, EFFECTS).expect("restore");
    {
        let mut handler = |name: &str, _args: &Json| -> Option<Result<Json, String>> {
            panic!("side effect '{name}' re-fired during replay — durability broken!");
        };
        println!("== replay (no side effects) ==");
        let outcome = rt2.drive(&mut handler).expect("replay drive");
        assert!(matches!(outcome, DriveOutcome::Completed));
    }

    assert_eq!(rt2.divergence(), None);
    let replayed_result = result_line(rt2.console());
    assert_eq!(
        recorded_result, replayed_result,
        "replay reproduces the recorded result exactly"
    );
    println!("  result reproduced byte-for-byte: {replayed_result}");
    println!("\nOK: side effects ran exactly once; replay was pure.");
}
