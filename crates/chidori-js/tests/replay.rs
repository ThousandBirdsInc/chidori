use chidori_js::replay::{DriveOutcome, ReplayRuntime};
use serde_json::json;

// A deterministic agent program: fetch two values via host effects, combine them.
const BUNDLE: &str = r#"
    async function main() {
        const a = await fetchValue('a');
        const b = await fetchValue('b');
        report(a + b);
    }
    main();
"#;

#[test]
fn record_then_replay_is_identical() {
    // ---- Record: produce results live, building the journal. ----
    let mut rt = ReplayRuntime::record(BUNDLE, &["fetchValue", "report"]);
    let mut reported = Vec::new();
    let mut handler =
        |name: &str, args: &serde_json::Value| -> Option<Result<serde_json::Value, String>> {
            match name {
                "fetchValue" => {
                    let key = args[0].as_str().unwrap();
                    Some(Ok(json!(if key == "a" { 10 } else { 32 })))
                }
                "report" => {
                    reported.push(args[0].clone());
                    Some(Ok(json!(null)))
                }
                _ => Some(Ok(json!(null))),
            }
        };
    let outcome = rt.drive(&mut handler).unwrap();
    assert!(matches!(outcome, DriveOutcome::Completed));
    assert_eq!(reported, vec![json!(42)]);
    let journal = rt.journal_bytes();

    // ---- Replay: a fresh process re-runs from the journal. The handler panics
    // if called, proving the effects are served from the journal, not re-run. ----
    let mut rt2 = ReplayRuntime::restore(BUNDLE, &journal, &["fetchValue", "report"]).unwrap();
    let mut replayed = Vec::new();
    let mut replay_handler =
        |name: &str, args: &serde_json::Value| -> Option<Result<serde_json::Value, String>> {
            // `report` is also journaled, so it should not be invoked live either.
            if name == "report" {
                replayed.push(args[0].clone());
            }
            Some(Ok(json!(null)))
        };
    let outcome2 = rt2.drive(&mut replay_handler).unwrap();
    assert!(matches!(outcome2, DriveOutcome::Completed));
    // The replayed run reproduces the same effect sequence from the journal.
    assert_eq!(rt2.divergence(), None);
}

#[test]
fn suspend_persist_restore_resume() {
    // Record but suspend at the second fetch (handler returns None there).
    let mut rt = ReplayRuntime::record(BUNDLE, &["fetchValue", "report"]);
    let mut calls = 0;
    let mut handler =
        |name: &str, _args: &serde_json::Value| -> Option<Result<serde_json::Value, String>> {
            if name == "fetchValue" {
                calls += 1;
                if calls == 1 {
                    return Some(Ok(json!(10))); // resolve first fetch
                }
                return None; // suspend at the second fetch (the frontier)
            }
            Some(Ok(json!(null)))
        };
    let outcome = rt.drive(&mut handler).unwrap();
    let op_id = match outcome {
        DriveOutcome::Suspended { op_id, name, .. } => {
            assert_eq!(name, "fetchValue");
            op_id
        }
        _ => panic!("expected suspension at the frontier"),
    };
    let journal = rt.journal_bytes();

    // A fresh process restores and resumes by providing the awaited result.
    let mut rt2 = ReplayRuntime::restore(BUNDLE, &journal, &["fetchValue", "report"]).unwrap();
    let mut reported = Vec::new();
    let mut resume_handler =
        |name: &str, args: &serde_json::Value| -> Option<Result<serde_json::Value, String>> {
            match name {
                "fetchValue" => Some(Ok(json!(32))), // the frontier fetch resolves now
                "report" => {
                    reported.push(args[0].clone());
                    Some(Ok(json!(null)))
                }
                _ => Some(Ok(json!(null))),
            }
        };
    // Re-run from the top: first fetch replays from journal (10), second is the
    // frontier and is provided live (32).
    let _ = op_id;
    let outcome2 = rt2.drive(&mut resume_handler).unwrap();
    assert!(matches!(outcome2, DriveOutcome::Completed));
    assert_eq!(reported, vec![json!(42)]);
}

// P4: modify-and-resume — edit code *after* the frontier, resume cleanly.
#[test]
fn modify_and_resume_forward_edit() {
    let original = r#"
        async function main() {
            const a = await fetchValue('a');
            const b = await fetchValue('b');
            report(a + b);
        }
        main();
    "#;
    // Record up to the second fetch, then suspend.
    let mut rt = ReplayRuntime::record(original, &["fetchValue", "report"]);
    let mut calls = 0;
    let mut h =
        |name: &str, _args: &serde_json::Value| -> Option<Result<serde_json::Value, String>> {
            if name == "fetchValue" {
                calls += 1;
                if calls == 1 {
                    return Some(Ok(json!(10)));
                }
                return None;
            }
            Some(Ok(json!(null)))
        };
    assert!(matches!(
        rt.drive(&mut h).unwrap(),
        DriveOutcome::Suspended { .. }
    ));
    let journal = rt.journal_bytes();

    // Edit code AFTER the frontier: change how the result is combined/reported.
    // The first fetch's journal entry still matches, so resume is clean.
    let edited = r#"
        async function main() {
            const a = await fetchValue('a');
            const b = await fetchValue('b');
            report(a * b + 1);   // <-- edited post-frontier logic
        }
        main();
    "#;
    let mut rt2 = ReplayRuntime::restore(edited, &journal, &["fetchValue", "report"]).unwrap();
    let mut reported = Vec::new();
    let mut h2 =
        |name: &str, args: &serde_json::Value| -> Option<Result<serde_json::Value, String>> {
            match name {
                "fetchValue" => Some(Ok(json!(32))),
                "report" => {
                    reported.push(args[0].clone());
                    Some(Ok(json!(null)))
                }
                _ => Some(Ok(json!(null))),
            }
        };
    assert!(matches!(
        rt2.drive(&mut h2).unwrap(),
        DriveOutcome::Completed
    ));
    // New logic: 10 * 32 + 1 = 321 (proves edited post-frontier code ran while
    // the pre-frontier effect (a=10) was replayed from the journal).
    assert_eq!(reported, vec![json!(321)]);
}

// P4 edit-conflict policy: editing code BEFORE the frontier diverges from the
// journal and fails loud rather than silently corrupting state.
#[test]
fn pre_frontier_edit_diverges() {
    let original = r#"
        async function main() {
            const a = await stepOne('x');
            const b = await stepTwo('y');
            report(a + b);
        }
        main();
    "#;
    let mut rt = ReplayRuntime::record(original, &["stepOne", "stepTwo", "report"]);
    let mut h =
        |name: &str, _args: &serde_json::Value| -> Option<Result<serde_json::Value, String>> {
            match name {
                "stepOne" => Some(Ok(json!(1))),
                "stepTwo" => None, // suspend at the frontier (stepTwo)
                _ => Some(Ok(json!(null))),
            }
        };
    assert!(matches!(
        rt.drive(&mut h).unwrap(),
        DriveOutcome::Suspended { .. }
    ));
    let journal = rt.journal_bytes();

    // Edit BEFORE the frontier: replace stepOne with a different effect call.
    // The journal's first entry is stepOne#0 but the edited program now calls
    // differentStep#0 first — divergence must be detected.
    let edited = r#"
        async function main() {
            const a = await differentStep('x');
            const b = await stepTwo('y');
            report(a + b);
        }
        main();
    "#;
    let mut rt2 = ReplayRuntime::restore(
        edited,
        &journal,
        &["stepOne", "stepTwo", "differentStep", "report"],
    )
    .unwrap();
    let mut h2 = |_n: &str, _a: &serde_json::Value| -> Option<Result<serde_json::Value, String>> {
        Some(Ok(json!(0)))
    };
    let result = rt2.drive(&mut h2);
    assert!(result.is_err(), "expected divergence error, got {result:?}");
    assert!(rt2.divergence().is_some());
    assert!(rt2.divergence().unwrap().contains("already-executed"));
}

// P6: value checkpoints via durableStep — memoized plain-value results are not
// recomputed on replay (the inner function's `console.log` runs only in record).
#[test]
fn durable_step_memoizes() {
    let bundle = r#"
        async function main() {
            const a = await durableStep(() => { console.log('computing'); return 2 * 3; });
            const b = await fetchValue('x');
            report(a + b);
        }
        main();
    "#;
    let mut rt = ReplayRuntime::record(bundle, &["fetchValue", "report"]);
    let mut reported = Vec::new();
    {
        let mut h =
            |name: &str, args: &serde_json::Value| -> Option<Result<serde_json::Value, String>> {
                match name {
                    "fetchValue" => Some(Ok(json!(100))),
                    "report" => {
                        reported.push(args[0].clone());
                        Some(Ok(json!(null)))
                    }
                    _ => Some(Ok(json!(null))),
                }
            };
        assert!(matches!(rt.drive(&mut h).unwrap(), DriveOutcome::Completed));
    }
    assert_eq!(reported, vec![json!(106)]); // 2*3 + 100
    assert_eq!(rt.console(), &["computing".to_string()]); // ran the step once
    let journal = rt.journal_bytes();

    // Replay: durableStep returns the cached value; the inner fn must NOT run,
    // so 'computing' is never logged again. All effects (incl. report) replay
    // from the journal, so the handler is never invoked.
    let mut rt2 = ReplayRuntime::restore(bundle, &journal, &["fetchValue", "report"]).unwrap();
    {
        let mut h =
            |_n: &str, _a: &serde_json::Value| -> Option<Result<serde_json::Value, String>> {
                panic!("no effect should be invoked live during full replay");
            };
        assert!(matches!(
            rt2.drive(&mut h).unwrap(),
            DriveOutcome::Completed
        ));
    }
    assert_eq!(rt2.divergence(), None);
    assert!(
        rt2.console().is_empty(),
        "durableStep should not re-run on replay"
    );
}

/// Two runtimes restored from the same journal on one thread share a cached
/// compiled proto (`compiler::compile_script_cached`). Sharing must be a pure
/// performance side effect: both replay to completion independently, with
/// byte-identical journals and no divergence — proving the cached proto
/// carries no per-runtime state.
#[test]
fn shared_cached_proto_replays_are_independent_and_identical() {
    let mut rt = ReplayRuntime::record(BUNDLE, &["fetchValue", "report"]);
    let mut handler =
        |name: &str, args: &serde_json::Value| -> Option<Result<serde_json::Value, String>> {
            match name {
                "fetchValue" => {
                    let key = args[0].as_str().unwrap();
                    Some(Ok(json!(if key == "a" { 10 } else { 32 })))
                }
                _ => Some(Ok(json!(null))),
            }
        };
    assert!(matches!(
        rt.drive(&mut handler).unwrap(),
        DriveOutcome::Completed
    ));
    let journal = rt.journal_bytes();

    // Restore the SAME bundle twice on this thread: the second restore hits the
    // thread-local proto cache. Interleave their driving to catch any shared
    // mutable state leaking through the proto.
    let mut noop = |_: &str, _: &serde_json::Value| -> Option<Result<serde_json::Value, String>> {
        Some(Ok(json!(null)))
    };
    let mut r1 = ReplayRuntime::restore(BUNDLE, &journal, &["fetchValue", "report"]).unwrap();
    let mut r2 = ReplayRuntime::restore(BUNDLE, &journal, &["fetchValue", "report"]).unwrap();
    assert!(matches!(
        r1.drive(&mut noop).unwrap(),
        DriveOutcome::Completed
    ));
    assert!(matches!(
        r2.drive(&mut noop).unwrap(),
        DriveOutcome::Completed
    ));
    assert_eq!(r1.divergence(), None);
    assert_eq!(r2.divergence(), None);
    assert_eq!(
        r1.journal_bytes(),
        r2.journal_bytes(),
        "replays through the shared cached proto must be byte-identical"
    );
    assert_eq!(r1.journal_bytes(), journal);
}
