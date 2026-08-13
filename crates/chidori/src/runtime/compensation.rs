//! Saga-style rollback: run a journal's registered compensations in reverse.
//!
//! `chidori.compensation.register(name, agent, input?)` journals one durable
//! `compensation` record — an inverse action (an agent module + its recorded
//! input) for a side effect the run just performed. The journal runs forward;
//! this module runs it backward: on rollback, every registered compensation
//! executes newest-first, each as its own ordinary run (journaled, replayable,
//! visible in `chidori trace` like any other).
//!
//! Compensations are void on success — a completed run's registrations are
//! history, not obligations. Rollback applies to runs that stopped short:
//! cancelled, failed, or abandoned mid-flight. It is explicitly invoked
//! (`chidori rollback <run_id>`, or `POST /sessions/{id}/cancel` with
//! `"compensate": true`) rather than automatic, because compensations perform
//! real side effects and re-firing them must be an operator decision.
//!
//! Idempotency: a completed rollback writes `rollback.json` into the run
//! directory; a second rollback refuses instead of re-firing inverse actions.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::runtime::call_log::CallRecord;
use crate::runtime::store::RunStore;

/// The blob a completed rollback leaves in the run directory.
pub const ROLLBACK_FILE: &str = "rollback.json";

/// One registered compensation, extracted from the journal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingCompensation {
    pub seq: u64,
    pub name: String,
    pub agent: String,
    pub input: Value,
}

/// How one compensation's execution went.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackOutcome {
    pub name: String,
    pub agent: String,
    /// The compensation run's id when it executed (successfully or not).
    pub run_id: Option<String>,
    /// The failure, when the compensation itself failed. Rollback continues
    /// past a failed compensation — the remaining inverse actions still run.
    pub error: Option<String>,
}

/// The `compensation` records of a journal, in registration order. Only
/// successful registrations count; a registration that errored never armed.
pub fn pending_compensations(records: &[CallRecord]) -> Vec<PendingCompensation> {
    records
        .iter()
        .filter(|r| r.function == "compensation" && r.error.is_none())
        .filter_map(|r| {
            Some(PendingCompensation {
                seq: r.seq,
                name: r.args.get("name")?.as_str()?.to_string(),
                agent: r.args.get("agent")?.as_str()?.to_string(),
                input: r.args.get("input").cloned().unwrap_or(Value::Null),
            })
        })
        .collect()
}

/// Execute a run's rollback: load its journal through `store`, refuse the
/// cases that must not roll back (already rolled back; completed run), then
/// run every registered compensation newest-first via `run_agent` — a caller-
/// supplied closure that executes one agent module with an input and returns
/// its run id (each compensation is an ordinary run in the caller's engine).
///
/// A failed compensation is recorded and rollback continues — the remaining
/// inverse actions are independent obligations. The outcome list (and
/// `rollback.json`) reports every one.
pub fn rollback_run(
    store: &dyn RunStore,
    base_dir: &Path,
    run_agent: &mut dyn FnMut(&Path, &Value) -> std::result::Result<String, String>,
) -> Result<Vec<RollbackOutcome>> {
    if store.get_blob(ROLLBACK_FILE)?.is_some() {
        anyhow::bail!(
            "this run was already rolled back (rollback.json present) — compensations \
             perform real side effects and are not re-fired"
        );
    }
    if store.get_blob("output.json")?.is_some() {
        anyhow::bail!(
            "this run completed successfully — its compensations are void, there is \
             nothing to roll back"
        );
    }
    let records = store
        .load_call_log()?
        .context("no checkpoint found for this run")?;
    let mut pending = pending_compensations(&records);
    pending.reverse();

    // Nothing registered: report empty WITHOUT writing rollback.json — a
    // rollback that did nothing must not block a later legitimate one (e.g.
    // after a `resume --retry-failed` that registers compensations and fails
    // again).
    if pending.is_empty() {
        return Ok(Vec::new());
    }

    let mut outcomes = Vec::with_capacity(pending.len());
    for compensation in &pending {
        let resolved = resolve_agent(base_dir, &compensation.agent);
        let outcome = match run_agent(&resolved, &compensation.input) {
            Ok(run_id) => RollbackOutcome {
                name: compensation.name.clone(),
                agent: compensation.agent.clone(),
                run_id: Some(run_id),
                error: None,
            },
            Err(error) => RollbackOutcome {
                name: compensation.name.clone(),
                agent: compensation.agent.clone(),
                run_id: None,
                error: Some(error),
            },
        };
        outcomes.push(outcome);
    }

    let report = json!({
        "rolled_back_at": Utc::now(),
        "outcomes": outcomes,
    });
    store.put_blob(ROLLBACK_FILE, &serde_json::to_vec_pretty(&report)?)?;
    Ok(outcomes)
}

fn resolve_agent(base_dir: &Path, agent: &str) -> PathBuf {
    let path = Path::new(agent);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::store::FsRunStore;

    fn compensation_record(seq: u64, name: &str, agent: &str, input: Value) -> CallRecord {
        CallRecord {
            seq,
            parent_seq: None,
            function: "compensation".to_string(),
            args: json!({ "name": name, "agent": agent, "input": input }),
            result: json!({ "registered": true }),
            duration_ms: 0,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        }
    }

    fn other_record(seq: u64, function: &str) -> CallRecord {
        CallRecord {
            seq,
            parent_seq: None,
            function: function.to_string(),
            args: Value::Null,
            result: Value::Null,
            duration_ms: 0,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        }
    }

    #[test]
    fn pending_compensations_keeps_registration_order_and_skips_failures() {
        let mut failed = compensation_record(3, "never-armed", "c.ts", Value::Null);
        failed.error = Some("refused".to_string());
        let records = vec![
            compensation_record(1, "deprovision", "comp/deprovision.ts", json!({ "id": 7 })),
            other_record(2, "mark"),
            failed,
            compensation_record(4, "notify", "comp/notify.ts", Value::Null),
        ];
        let pending = pending_compensations(&records);
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].name, "deprovision");
        assert_eq!(pending[0].input, json!({ "id": 7 }));
        assert_eq!(pending[1].name, "notify");
    }

    #[test]
    fn rollback_runs_newest_first_continues_past_failures_and_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("chidori-rollback-{}", uuid::Uuid::new_v4()));
        let store = FsRunStore::new(dir.join("run-1"));
        store
            .write_call_log(&[
                compensation_record(1, "first", "comp/first.ts", json!({ "n": 1 })),
                compensation_record(2, "second", "comp/second.ts", json!({ "n": 2 })),
            ])
            .unwrap();

        let mut executed: Vec<String> = Vec::new();
        let mut run_agent = |path: &Path, input: &Value| {
            executed.push(format!(
                "{} {}",
                path.file_name().unwrap().to_string_lossy(),
                input["n"]
            ));
            if path.ends_with("second.ts") {
                Err("compensation blew up".to_string())
            } else {
                Ok("comp-run-1".to_string())
            }
        };
        let outcomes = rollback_run(&store, Path::new("/project"), &mut run_agent).unwrap();

        // Newest-first, and the failure didn't stop the older compensation.
        assert_eq!(executed, vec!["second.ts 2", "first.ts 1"]);
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].name, "second");
        assert!(outcomes[0].error.as_deref() == Some("compensation blew up"));
        assert_eq!(outcomes[1].name, "first");
        assert_eq!(outcomes[1].run_id.as_deref(), Some("comp-run-1"));

        // Second rollback refuses: inverse actions are not re-fired.
        let err = rollback_run(&store, Path::new("/project"), &mut |_, _| {
            panic!("must not re-execute")
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("already rolled back"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn empty_rollback_writes_no_marker_so_a_later_rollback_still_runs() {
        let dir =
            std::env::temp_dir().join(format!("chidori-rollback-empty-{}", uuid::Uuid::new_v4()));
        let store = FsRunStore::new(dir.join("run-1"));
        store.write_call_log(&[other_record(1, "prompt")]).unwrap();

        // No compensations registered: empty outcome, no rollback.json.
        let outcomes =
            rollback_run(&store, Path::new("/p"), &mut |_, _| Ok("r".to_string())).unwrap();
        assert!(outcomes.is_empty());
        assert!(store.get_blob(ROLLBACK_FILE).unwrap().is_none());

        // The run later registers a compensation (e.g. a retry that got
        // further) — rollback still runs it.
        store
            .write_call_log(&[
                other_record(1, "prompt"),
                compensation_record(2, "undo", "comp/undo.ts", Value::Null),
            ])
            .unwrap();
        let outcomes =
            rollback_run(&store, Path::new("/p"), &mut |_, _| Ok("r2".to_string())).unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(store.get_blob(ROLLBACK_FILE).unwrap().is_some());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rollback_refuses_a_completed_run() {
        let dir =
            std::env::temp_dir().join(format!("chidori-rollback-done-{}", uuid::Uuid::new_v4()));
        let store = FsRunStore::new(dir.join("run-1"));
        store
            .write_call_log(&[compensation_record(1, "x", "c.ts", Value::Null)])
            .unwrap();
        store.put_blob("output.json", b"{}").unwrap();
        let err = rollback_run(&store, Path::new("/p"), &mut |_, _| Ok("r".to_string()))
            .unwrap_err();
        assert!(format!("{err:#}").contains("completed successfully"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
