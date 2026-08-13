//! Live holdings introspection: what is this run holding *right now*?
//!
//! The call log answers "what happened"; this module answers the operational
//! question — the pending host operation a paused run is parked on, the
//! signals queued in its inbox, the actors it spawned and never settled, the
//! detached agents it launched, its open branches, and the compensations it
//! has armed. All of it already exists across the run directory (pending
//! operation blob, signal inbox, snapshot manifest, journal, branch stores);
//! this aggregates the pieces into one view for `chidori holdings <run_id>`
//! and `GET /sessions/{id}/holdings`.

use std::path::Path;

use anyhow::Result;
use serde_json::{json, Value};

use crate::runtime::call_log::CallRecord;
use crate::runtime::snapshot::{
    PendingHostOperation, QueuedSignal, SnapshotStore, PENDING_HOST_OPERATION_FILE,
    SIGNAL_INBOX_FILE,
};
use crate::runtime::store::RunStore;

/// Aggregate a run's current holdings. `registry_lookup` resolves a detached
/// agent name to its registry entry (the registry lives at the runs base, not
/// in the run directory) — pass a no-op closure when no registry is at hand.
pub fn compute_holdings(
    run_id: &str,
    store: &dyn RunStore,
    run_dir: &Path,
    registry_lookup: &dyn Fn(&str) -> Option<Value>,
) -> Result<Value> {
    let records = store.load_call_log()?.unwrap_or_default();

    // Coarse lifecycle hint from the run directory's artifacts alone.
    let completed = store.get_blob("output.json")?.is_some();
    let pending_blob = store.get_blob(PENDING_HOST_OPERATION_FILE)?;
    let failed =
        !completed && pending_blob.is_none() && records.last().is_some_and(|r| r.error.is_some());
    let status_hint = if completed {
        "completed"
    } else if pending_blob.is_some() {
        "paused"
    } else if failed {
        "failed"
    } else if records.is_empty() {
        "unstarted"
    } else {
        "unknown"
    };

    // The pending host operation a paused run is parked on (input prompt,
    // signal listen set, approval) — deadline and options live in its args.
    let pending: Option<Value> = pending_blob
        .and_then(|bytes| serde_json::from_slice::<PendingHostOperation>(&bytes).ok())
        .map(|op| serde_json::to_value(op).unwrap_or(Value::Null));

    // Queued-but-unconsumed signal deliveries.
    let inbox: Vec<QueuedSignal> = store
        .get_blob(SIGNAL_INBOX_FILE)?
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let inbox_names: Vec<&str> = inbox.iter().map(|s| s.name.as_str()).collect();

    // Pending entries in the host promise table (from the snapshot manifest).
    let manifest = SnapshotStore::new(run_dir).load_manifest().ok();
    let promises_pending = manifest
        .as_ref()
        .map(|m| {
            m.host_promises
                .iter()
                .filter(|p| matches!(p.state, crate::runtime::snapshot::HostPromiseState::Pending))
                .count()
        })
        .unwrap_or(0);

    Ok(json!({
        "run_id": run_id,
        "status_hint": status_hint,
        "pending": pending,
        "signal_inbox": {
            "queued": inbox.len(),
            "names": inbox_names,
        },
        "host_promises_pending": promises_pending,
        "actors": open_actors(&records),
        "detached_agents": detached_agents(&records, registry_lookup),
        "branches": crate::runtime::host_branch::list_branches(run_dir).unwrap_or_default(),
        "compensations": {
            "registered": crate::runtime::compensation::pending_compensations(&records).len(),
            "rolled_back": store
                .get_blob(crate::runtime::compensation::ROLLBACK_FILE)?
                .is_some(),
        },
    }))
}

/// Actors spawned by this journal that no `join_actor`/`stop_actor` record
/// has settled — the supervision obligations still open.
fn open_actors(records: &[CallRecord]) -> Vec<Value> {
    let settled: std::collections::HashSet<&str> = records
        .iter()
        .filter(|r| matches!(r.function.as_str(), "join_actor" | "stop_actor") && r.error.is_none())
        .filter_map(|r| r.args.get("pid").and_then(Value::as_str))
        .collect();
    records
        .iter()
        .filter(|r| r.function == "spawn_actor" && r.error.is_none())
        .filter_map(|r| {
            let pid = r.result.get("pid")?.as_str()?;
            Some(json!({
                "pid": pid,
                "name": r.args.get("options").and_then(|o| o.get("name")).cloned(),
                "source": r.args.get("source").cloned(),
                "settled": settled.contains(pid),
                "spawned_at_seq": r.seq,
            }))
        })
        .collect()
}

/// Detached agents this journal spawned, enriched with their current registry
/// state (status, what they're waiting on, alarm deadline) when the registry
/// is reachable.
fn detached_agents(
    records: &[CallRecord],
    registry_lookup: &dyn Fn(&str) -> Option<Value>,
) -> Vec<Value> {
    records
        .iter()
        .filter(|r| r.function == "spawn_agent" && r.error.is_none())
        .filter_map(|r| {
            let name = r.result.get("name")?.as_str()?.to_string();
            let mut entry = json!({
                "name": name,
                "run_id": r.result.get("runId").cloned(),
                "spawned_at_seq": r.seq,
            });
            if let Some(descriptor) =
                registry_lookup(&name).and_then(|v| v.get("descriptor").cloned())
            {
                entry["status"] = descriptor.get("status").cloned().unwrap_or(Value::Null);
                entry["waiting_for"] = descriptor
                    .get("listen")
                    .and_then(|l| l.get("names"))
                    .cloned()
                    .unwrap_or(Value::Null);
                entry["deadline"] = descriptor
                    .get("listen")
                    .and_then(|l| l.get("deadline"))
                    .cloned()
                    .unwrap_or(Value::Null);
            }
            Some(entry)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::store::FsRunStore;
    use chrono::Utc;

    fn record(seq: u64, function: &str, args: Value, result: Value) -> CallRecord {
        CallRecord {
            seq,
            parent_seq: None,
            function: function.to_string(),
            args,
            result,
            duration_ms: 0,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        }
    }

    #[test]
    fn holdings_report_open_actors_inbox_and_compensations() {
        let dir = std::env::temp_dir().join(format!("chidori-holdings-{}", uuid::Uuid::new_v4()));
        let run_dir = dir.join("run-1");
        let store = FsRunStore::new(&run_dir);
        store
            .write_call_log(&[
                record(
                    1,
                    "spawn_actor",
                    json!({ "source": "w.ts", "options": { "name": "worker" } }),
                    json!({ "pid": "actor-1" }),
                ),
                record(
                    2,
                    "spawn_actor",
                    json!({ "source": "w2.ts", "options": {} }),
                    json!({ "pid": "actor-2" }),
                ),
                record(3, "join_actor", json!({ "pid": "actor-1" }), json!({})),
                record(
                    4,
                    "compensation",
                    json!({ "name": "undo", "agent": "c.ts", "input": null }),
                    json!({ "registered": true }),
                ),
                record(
                    5,
                    "spawn_agent",
                    json!({ "source": "d.ts" }),
                    json!({ "name": "sentinel", "runId": "run-d" }),
                ),
            ])
            .unwrap();
        store
            .put_blob(
                SIGNAL_INBOX_FILE,
                serde_json::to_vec(&vec![QueuedSignal {
                    delivery_seq: 1,
                    name: "review".to_string(),
                    payload: Value::Null,
                    from: Value::Null,
                    enqueued_at: Utc::now(),
                }])
                .unwrap()
                .as_slice(),
            )
            .unwrap();

        let lookup = |name: &str| {
            (name == "sentinel").then(|| {
                json!({ "descriptor": {
                    "status": "hibernating",
                    "listen": { "names": ["tick"], "deadline": null },
                }})
            })
        };
        let holdings = compute_holdings("run-1", &store, &run_dir, &lookup).unwrap();

        assert_eq!(holdings["run_id"], json!("run-1"));
        assert_eq!(holdings["signal_inbox"]["queued"], json!(1));
        assert_eq!(holdings["signal_inbox"]["names"], json!(["review"]));

        let actors = holdings["actors"].as_array().unwrap();
        assert_eq!(actors.len(), 2);
        assert_eq!(actors[0]["pid"], json!("actor-1"));
        assert_eq!(actors[0]["settled"], json!(true));
        assert_eq!(actors[1]["pid"], json!("actor-2"));
        assert_eq!(actors[1]["settled"], json!(false));

        let agents = holdings["detached_agents"].as_array().unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0]["name"], json!("sentinel"));
        assert_eq!(agents[0]["status"], json!("hibernating"));
        assert_eq!(agents[0]["waiting_for"], json!(["tick"]));

        assert_eq!(holdings["compensations"]["registered"], json!(1));
        assert_eq!(holdings["compensations"]["rolled_back"], json!(false));
        // Last record is a successful spawn; no output.json, no pending blob.
        assert_eq!(holdings["status_hint"], json!("unknown"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn holdings_status_hints_track_run_artifacts() {
        let dir =
            std::env::temp_dir().join(format!("chidori-holdings-st-{}", uuid::Uuid::new_v4()));
        let run_dir = dir.join("run-1");
        let store = FsRunStore::new(&run_dir);
        let none = |_: &str| None;

        let holdings = compute_holdings("run-1", &store, &run_dir, &none).unwrap();
        assert_eq!(holdings["status_hint"], json!("unstarted"));

        let mut failed = record(1, "prompt", json!({}), Value::Null);
        failed.error = Some("boom".to_string());
        store.write_call_log(&[failed]).unwrap();
        let holdings = compute_holdings("run-1", &store, &run_dir, &none).unwrap();
        assert_eq!(holdings["status_hint"], json!("failed"));

        store.put_blob("output.json", b"{}").unwrap();
        let holdings = compute_holdings("run-1", &store, &run_dir, &none).unwrap();
        assert_eq!(holdings["status_hint"], json!("completed"));

        let _ = std::fs::remove_dir_all(dir);
    }
}
