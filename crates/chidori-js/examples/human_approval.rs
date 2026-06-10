//! Durable pause / resume — human-in-the-loop across process restarts.
//!
//! A long-running agent often has to stop and wait: for a human approval, a
//! webhook, a slow job. You don't want to keep a process (and its VM) pinned in
//! memory for hours. Record-and-replay lets the agent SUSPEND at the waiting
//! point — persist a tiny journal — and a completely fresh process later
//! RESTORE and resume, feeding in the awaited answer. Everything before the
//! wait is replayed from the journal; nothing is re-executed.
//!
//! Run with:
//!     cargo run -p chidori-js --example human_approval

use chidori_js::replay::{DriveOutcome, ReplayRuntime};
use serde_json::{json, Value as Json};

// Agent: assemble a refund, ask a human to approve it, then act on the answer.
const AGENT: &str = r#"
    async function main() {
        const refund = await computeRefund({ order: "A-1007" });
        const decision = await requestApproval({
            kind: "refund",
            amount: refund.amount,
        });
        if (decision === "approve") {
            const r = await issueRefund(refund);
            report({ status: "refunded", receipt: r });
        } else {
            report({ status: "denied" });
        }
    }
    main();
"#;

const EFFECTS: &[&str] = &["computeRefund", "requestApproval", "issueRefund", "report"];

fn main() {
    // ---- Process #1: run until the approval is needed, then SUSPEND. ----
    println!("== process #1: run until human input is needed ==");
    let mut rt = ReplayRuntime::record(AGENT, EFFECTS);
    let suspended_at;
    {
        let mut handler = |name: &str, args: &Json| -> Option<Result<Json, String>> {
            match name {
                "computeRefund" => {
                    println!("  [live] computing refund for {}", args[0]);
                    Some(Ok(json!({ "order": "A-1007", "amount": 4200 })))
                }
                // The frontier: we have no answer yet. Returning None suspends
                // the whole process here — persist and walk away.
                "requestApproval" => {
                    println!("  [live] need human approval: {}", args[0]);
                    None
                }
                _ => Some(Ok(Json::Null)),
            }
        };
        match rt.drive(&mut handler).expect("drive") {
            DriveOutcome::Suspended { name, args, .. } => {
                suspended_at = name;
                println!("  -> suspended awaiting '{suspended_at}' with args {args}");
            }
            DriveOutcome::Completed => panic!("should have suspended for approval"),
        }
    }
    assert_eq!(suspended_at, "requestApproval");

    // Persist the journal. In a real system this is all you store while waiting
    // — no live VM, no pinned process.
    let journal = rt.journal_bytes();
    println!(
        "  persisted journal ({} bytes) — process can now exit.\n",
        journal.len()
    );
    drop(rt);

    // ... hours later, a human clicks "approve" in some UI ...

    // ---- Process #2: a FRESH runtime restores and resumes. ----
    println!("== process #2: human approved — restore and resume ==");
    let mut rt2 = ReplayRuntime::restore(AGENT, &journal, EFFECTS).expect("restore");
    let mut final_report = Json::Null;
    let mut issued = 0u32;
    {
        let mut handler = |name: &str, args: &Json| -> Option<Result<Json, String>> {
            match name {
                // Replayed from journal — handler not consulted for these.
                "computeRefund" => panic!("computeRefund should replay from journal"),
                // The frontier resolves now, live, with the human's answer.
                "requestApproval" => {
                    println!("  [live] human answered: approve");
                    Some(Ok(json!("approve")))
                }
                "issueRefund" => {
                    issued += 1;
                    println!("  [live] issuing refund: {}", args[0]);
                    Some(Ok(json!({ "receipt": "RCPT-55" })))
                }
                "report" => {
                    final_report = args[0].clone();
                    Some(Ok(Json::Null))
                }
                _ => Some(Ok(Json::Null)),
            }
        };
        let outcome = rt2.drive(&mut handler).expect("resume drive");
        assert!(matches!(outcome, DriveOutcome::Completed));
    }

    assert_eq!(rt2.divergence(), None);
    assert_eq!(issued, 1, "refund issued exactly once, after approval");
    assert_eq!(final_report["status"], json!("refunded"));
    println!("  final: {final_report}");
    println!("\nOK: paused for a human, resumed cleanly in a new process.");
}
