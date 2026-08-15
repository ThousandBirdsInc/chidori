//! VM images: resume a suspended program in a fresh runtime without replaying
//! its journal.
//!
//! The contract these tests pin down:
//!   * a program suspended at a host effect can be imaged, rebuilt in a new
//!     runtime, and continued — with its heap, closures and async frames intact
//!     and WITHOUT the bundle re-executing;
//!   * the observable result and the resulting journal are identical to what
//!     replay-based resume produces (image is a fast path, not a fork);
//!   * state that has no image form is REFUSED, not guessed at.

use chidori_js::replay::{DriveOutcome, ReplayRuntime, RestorePath};
use serde_json::{json, Value as Json};

type Handler<'a> = &'a mut dyn FnMut(&str, &Json) -> Option<Result<Json, String>>;

/// Suspend at the first `fetchValue`, capturing nothing else.
fn suspend_at_first_fetch(name: &str, _args: &Json) -> Option<Result<Json, String>> {
    if name == "fetchValue" {
        None
    } else {
        Some(Ok(json!(null)))
    }
}

/// Drive to the first `fetchValue` and stop there, returning the runtime, the
/// id of the op it is blocked on, and the journal recorded so far (which an
/// image restore needs alongside the image, since the image deliberately does
/// not duplicate it).
fn record_until_suspended(bundle: &str, effects: &[&str]) -> (ReplayRuntime, u64, Vec<u8>) {
    let mut rt = ReplayRuntime::record(bundle, effects);
    rt.enable_imaging();
    let mut handler = suspend_at_first_fetch;
    match rt.drive(&mut handler).unwrap() {
        DriveOutcome::Suspended { op_id, .. } => {
            let journal = rt.journal_bytes();
            (rt, op_id, journal)
        }
        DriveOutcome::Completed => panic!("program completed without suspending"),
    }
}

// ---------------------------------------------------------------------------

const ASYNC_BUNDLE: &str = r#"
    let total = 0;
    const seen = [];
    async function main() {
        const a = await fetchValue('a');
        total += a;
        seen.push('a');
        const b = await fetchValue('b');
        total += b;
        seen.push('b');
        report({ total, seen });
    }
    main();
"#;

#[test]
fn suspended_async_frame_resumes_from_an_image_without_replay() {
    let (rt, op_id, journal) = record_until_suspended(ASYNC_BUNDLE, &["fetchValue", "report"]);
    let image = rt.to_image().expect("imageable at a host suspension");
    drop(rt);

    // A fresh runtime, as if this were another process on another machine.
    let mut rt2 =
        ReplayRuntime::from_image(&image, ASYNC_BUNDLE, &journal, &["fetchValue", "report"])
            .expect("image restores");

    // If the bundle had re-executed, `main()` would have run again and the
    // first `fetchValue` would be requested a second time. Panic if so.
    let mut reported: Vec<Json> = Vec::new();
    let mut fetches = 0;
    let mut handler = |name: &str, args: &Json| -> Option<Result<Json, String>> {
        match name {
            "fetchValue" => {
                fetches += 1;
                let key = args[0].as_str().unwrap();
                assert_eq!(key, "b", "the pre-image effect must not be re-requested");
                Some(Ok(json!(32)))
            }
            "report" => {
                reported.push(args[0].clone());
                Some(Ok(json!(null)))
            }
            _ => Some(Ok(json!(null))),
        }
    };
    let outcome = rt2
        .provide_and_drive(op_id, Ok(json!(10)), &mut handler as Handler)
        .unwrap();

    assert!(matches!(outcome, DriveOutcome::Completed));
    assert_eq!(fetches, 1, "only the post-image effect runs live");
    assert_eq!(reported, vec![json!({"total": 42, "seen": ["a", "b"]})]);
}

#[test]
fn image_resume_and_replay_resume_agree() {
    // Same suspension, resumed both ways: the journals and the reported value
    // must be byte-identical. This is the differential check that lets the
    // image be used as a cache over the journal rather than a second source of
    // truth.
    let (rt, op_id, journal) = record_until_suspended(ASYNC_BUNDLE, &["fetchValue", "report"]);
    let image = rt.to_image().unwrap();
    drop(rt);

    let finish = |mut rt: ReplayRuntime, op_id: u64| -> (Vec<Json>, Vec<u8>) {
        let mut reported = Vec::new();
        let mut handler = |name: &str, _args: &Json| -> Option<Result<Json, String>> {
            match name {
                "fetchValue" => Some(Ok(json!(32))),
                "report" => {
                    reported.push(_args[0].clone());
                    Some(Ok(json!(null)))
                }
                _ => Some(Ok(json!(null))),
            }
        };
        rt.provide_and_drive(op_id, Ok(json!(10)), &mut handler as Handler)
            .unwrap();
        let j = rt.journal_bytes();
        (reported, j)
    };

    let via_image = finish(
        ReplayRuntime::from_image(&image, ASYNC_BUNDLE, &journal, &["fetchValue", "report"])
            .unwrap(),
        op_id,
    );

    // The replay path re-runs the bundle against the journal to reach the same
    // frontier. Its op ids restart from zero, so ask it where it blocked.
    let mut rt3 =
        ReplayRuntime::restore(ASYNC_BUNDLE, &journal, &["fetchValue", "report"]).unwrap();
    let mut suspend = suspend_at_first_fetch;
    let replay_op = match rt3.drive(&mut suspend).unwrap() {
        DriveOutcome::Suspended { op_id, .. } => op_id,
        DriveOutcome::Completed => panic!("replay completed early"),
    };
    let via_replay = finish(rt3, replay_op);

    assert_eq!(via_image.0, via_replay.0, "reported values differ");
    assert_eq!(
        String::from_utf8_lossy(&via_image.1),
        String::from_utf8_lossy(&via_replay.1),
        "journals differ"
    );
}

// ---------------------------------------------------------------------------
// Heap coverage: the shapes an agent program actually holds across an await.
// ---------------------------------------------------------------------------

const RICH_BUNDLE: &str = r#"
    class Counter {
        #n = 0;
        static kind = 'counter';
        bump(by) { this.#n += by; return this.#n; }
        get value() { return this.#n; }
    }
    function makeAdder(base) {
        let calls = 0;
        return (x) => { calls += 1; return base + x + calls; };
    }
    const state = {
        counter: new Counter(),
        adder: makeAdder(100),
        map: new Map([['a', 1], ['b', { nested: true }]]),
        set: new Set([1, 'two', 3n]),
        date: new Date(86400000),
        big: 9007199254740993n,
        arr: [1, , 3],
        re: /ab+c/gi,
        sym: Symbol('tag'),
        nan: NaN,
        neg: -0,
        inf: -Infinity,
    };
    state.self = state;
    state.counter.bump(5);
    async function main() {
        const delta = await fetchValue('delta');
        state.counter.bump(delta);
        report({
            counter: state.counter.value,
            kind: Counter.kind,
            adder: state.adder(1),
            adder2: state.adder(1),
            mapB: state.map.get('b').nested,
            mapSize: state.map.size,
            setHas: state.set.has(3n),
            date: state.date.getTime(),
            big: String(state.big),
            arrHole: 1 in state.arr,
            arrLen: state.arr.length,
            re: state.re.test('abbc'),
            sym: state.sym.description,
            nan: Number.isNaN(state.nan),
            negZero: Object.is(state.neg, -0),
            inf: state.inf === -Infinity,
            cyclic: state.self === state,
        });
    }
    main();
"#;

#[test]
fn rich_heap_survives_the_round_trip() {
    let (rt, op_id, journal) = record_until_suspended(RICH_BUNDLE, &["fetchValue", "report"]);
    let image = rt.to_image().expect("imageable");
    drop(rt);

    let mut rt2 =
        ReplayRuntime::from_image(&image, RICH_BUNDLE, &journal, &["fetchValue", "report"])
            .expect("image restores");

    let mut reported = Vec::new();
    let mut handler = |name: &str, args: &Json| -> Option<Result<Json, String>> {
        if name == "report" {
            reported.push(args[0].clone());
        }
        Some(Ok(json!(null)))
    };
    rt2.provide_and_drive(op_id, Ok(json!(7)), &mut handler as Handler)
        .unwrap();

    assert_eq!(
        reported,
        vec![json!({
            "counter": 12,          // 5 before the image, +7 after
            "kind": "counter",
            "adder": 102,           // base 100 + 1 + first call
            "adder2": 103,          // the closure's own counter kept counting
            "mapB": true,
            "mapSize": 2,
            "setHas": true,
            "date": 86400000i64,
            "big": "9007199254740993",
            "arrHole": false,       // the elision is still a hole, not undefined
            "arrLen": 3,
            "re": true,
            "sym": "tag",
            "nan": true,
            "negZero": true,
            "inf": true,
            "cyclic": true,
        })]
    );
}

#[test]
fn generator_suspended_across_the_image_keeps_its_position() {
    const BUNDLE: &str = r#"
        function* counter() {
            let i = 0;
            while (true) { yield ++i; }
        }
        const g = counter();
        g.next();
        g.next();
        async function main() {
            await fetchValue('go');
            report([g.next().value, g.next().value]);
        }
        main();
    "#;
    let (rt, op_id, journal) = record_until_suspended(BUNDLE, &["fetchValue", "report"]);
    let image = rt
        .to_image()
        .expect("a yield-suspended generator is imageable");
    drop(rt);

    let mut rt2 = ReplayRuntime::from_image(&image, BUNDLE, &journal, &["fetchValue", "report"])
        .expect("image restores");
    let mut reported = Vec::new();
    let mut handler = |name: &str, args: &Json| -> Option<Result<Json, String>> {
        if name == "report" {
            reported.push(args[0].clone());
        }
        Some(Ok(json!(null)))
    };
    rt2.provide_and_drive(op_id, Ok(json!(null)), &mut handler as Handler)
        .unwrap();
    assert_eq!(
        reported,
        vec![json!([3, 4])],
        "the generator resumed where it stopped"
    );
}

#[test]
fn try_finally_and_nested_awaits_survive() {
    const BUNDLE: &str = r#"
        const log = [];
        async function inner() {
            try {
                log.push('try');
                const v = await fetchValue('inner');
                log.push('got:' + v);
                return v * 2;
            } finally {
                log.push('finally');
            }
        }
        async function main() {
            const doubled = await inner();
            report({ doubled, log });
        }
        main();
    "#;
    let (rt, op_id, journal) = record_until_suspended(BUNDLE, &["fetchValue", "report"]);
    let image = rt.to_image().expect("imageable inside a try/finally");
    drop(rt);

    let mut rt2 = ReplayRuntime::from_image(&image, BUNDLE, &journal, &["fetchValue", "report"])
        .expect("image restores");
    let mut reported = Vec::new();
    let mut handler = |name: &str, args: &Json| -> Option<Result<Json, String>> {
        if name == "report" {
            reported.push(args[0].clone());
        }
        Some(Ok(json!(null)))
    };
    rt2.provide_and_drive(op_id, Ok(json!(21)), &mut handler as Handler)
        .unwrap();
    assert_eq!(
        reported,
        vec![json!({"doubled": 42, "log": ["try", "got:21", "finally"]})]
    );
}

// ---------------------------------------------------------------------------
// Refusals: an image that cannot be taken must say so, not lie.
// ---------------------------------------------------------------------------

#[test]
fn a_live_native_closure_refuses_instead_of_guessing() {
    // `new Promise(executor)` hands the program native resolve/reject
    // functions whose Rust state cannot be written down. Holding one across
    // the suspension must fail the image cleanly.
    const BUNDLE: &str = r#"
        let release;
        const gate = new Promise((resolve) => { release = resolve; });
        async function main() {
            await fetchValue('x');
            release(1);
            report(await gate);
        }
        main();
    "#;
    let (rt, _op, _journal) = record_until_suspended(BUNDLE, &["fetchValue", "report"]);
    let err = rt
        .to_image()
        .expect_err("must refuse, not produce a broken image");
    assert!(
        err.contains("native function"),
        "the refusal should name what stopped it, got: {err}"
    );
}

#[test]
fn an_edited_bundle_is_refused() {
    let (rt, _op, journal) = record_until_suspended(ASYNC_BUNDLE, &["fetchValue", "report"]);
    let image = rt.to_image().unwrap();
    drop(rt);

    let edited = ASYNC_BUNDLE.replace("total += a;", "total += a * 2;");
    let err = match ReplayRuntime::from_image(&image, &edited, &journal, &["fetchValue", "report"])
    {
        Err(e) => e,
        Ok(_) => panic!("an image cannot absorb a source edit"),
    };
    assert!(err.contains("different bundle"), "got: {err}");
}

#[test]
fn imaging_must_be_enabled_before_it_can_be_used() {
    let mut rt = ReplayRuntime::record(ASYNC_BUNDLE, &["fetchValue", "report"]);
    let mut handler = suspend_at_first_fetch;
    rt.drive(&mut handler).unwrap();
    let err = rt.to_image().expect_err("no baseline was marked");
    assert!(err.contains("imaging was not enabled"), "got: {err}");
}

#[test]
fn a_mismatched_effect_set_is_caught_by_the_baseline_digest() {
    // A different effect list builds a different baseline, so ids would mean
    // different objects. The digest has to catch that before anything is wired
    // to the wrong function.
    let (rt, _op, journal) = record_until_suspended(ASYNC_BUNDLE, &["fetchValue", "report"]);
    let image = rt.to_image().unwrap();
    drop(rt);

    let err = match ReplayRuntime::from_image(
        &image,
        ASYNC_BUNDLE,
        &journal,
        &["fetchValue", "report", "extraEffect"],
    ) {
        Err(e) => e,
        Ok(_) => panic!("a different host surface must not silently restore"),
    };
    assert!(err.contains("baseline"), "got: {err}");
}

// ---------------------------------------------------------------------------
// The point of the exercise.
// ---------------------------------------------------------------------------

#[test]
fn image_size_tracks_live_state_not_history() {
    // A run whose journal grows without its heap growing must image at
    // essentially constant size. This is the whole O(live state) vs
    // O(history) claim, checked rather than asserted in prose — and it is
    // exactly the property a shared `pending` map or a duplicated journal
    // quietly destroys, so it is worth a test that would notice.
    const BUNDLE: &str = r#"
        async function main() {
            let n = 0;
            for (let i = 0; i < ITERS; i++) { n += await fetchValue(i); }
            await fetchValue('last');
            report(n);
        }
        main();
    "#;
    let measure = |iters: usize| -> (usize, usize) {
        let bundle = BUNDLE.replace("ITERS", &iters.to_string());
        let mut rt = ReplayRuntime::record(&bundle, &["fetchValue", "report"]);
        rt.enable_imaging();
        let mut served = 0usize;
        let mut handler = |name: &str, _args: &Json| -> Option<Result<Json, String>> {
            if name == "fetchValue" {
                served += 1;
                // Suspend only on the final call, after `iters` journaled ones.
                if served > iters {
                    return None;
                }
                return Some(Ok(json!(1)));
            }
            Some(Ok(json!(null)))
        };
        match rt.drive(&mut handler).unwrap() {
            DriveOutcome::Suspended { .. } => {}
            DriveOutcome::Completed => panic!("expected a suspension"),
        }
        let image = serde_json::to_vec(&rt.to_image().unwrap()).unwrap().len();
        (image, rt.journal_bytes().len())
    };

    let (short_image, short_journal) = measure(10);
    let (long_image, long_journal) = measure(1000);

    assert!(
        long_journal > short_journal * 20,
        "the history should have grown a lot: {short_journal} -> {long_journal}"
    );
    // Allow a little slack for wider integers in the loop counter, no more.
    assert!(
        long_image < short_image + short_image / 10,
        "image grew with history: {short_image} -> {long_image} \
         (journal {short_journal} -> {long_journal})"
    );
}

// ---------------------------------------------------------------------------
// The durable artifact: image rides along with the journal, never replaces it.
// ---------------------------------------------------------------------------

#[test]
fn durable_blob_prefers_the_image_and_still_carries_the_journal() {
    let (rt, op_id, _journal) = record_until_suspended(ASYNC_BUNDLE, &["fetchValue", "report"]);
    let blob = rt.to_blob(&["fetchValue", "report"]);
    drop(rt);

    // The artifact is still a superset of the old one: bundle + journal are
    // present and readable by anything that ignores the image field.
    let decoded: chidori_js::replay::DurableBlob = serde_json::from_slice(&blob).unwrap();
    assert_eq!(decoded.bundle, ASYNC_BUNDLE);
    assert!(
        !decoded.journal.parse().unwrap().bundle_hash.is_empty(),
        "the journal is still the record (it pins the bundle even before any entry lands)"
    );
    assert!(decoded.image.is_some(), "an image should have been taken");

    let (mut rt2, path) = ReplayRuntime::from_blob_reporting(&blob).unwrap();
    assert_eq!(
        path,
        RestorePath::Image,
        "the fast path should have been taken"
    );

    let mut reported = Vec::new();
    let mut handler = |name: &str, args: &Json| -> Option<Result<Json, String>> {
        if name == "report" {
            reported.push(args[0].clone());
        }
        Some(Ok(json!(32)))
    };
    rt2.provide_and_drive(op_id, Ok(json!(10)), &mut handler as Handler)
        .unwrap();
    assert_eq!(reported, vec![json!({"total": 42, "seen": ["a", "b"]})]);
}

#[test]
fn an_artifact_without_an_image_still_restores_by_replay() {
    // Both the pre-image artifacts already on disk and any state that refuses
    // to image take this path. It must stay ordinary.
    let mut rt = ReplayRuntime::record(ASYNC_BUNDLE, &["fetchValue", "report"]);
    let mut suspend = suspend_at_first_fetch;
    match rt.drive(&mut suspend).unwrap() {
        DriveOutcome::Suspended { .. } => {}
        DriveOutcome::Completed => panic!("expected a suspension"),
    };
    let blob = rt.to_blob(&["fetchValue", "report"]);
    let decoded: chidori_js::replay::DurableBlob = serde_json::from_slice(&blob).unwrap();
    assert!(decoded.image.is_none(), "imaging was never enabled");
    drop(rt);

    let (mut rt2, path) = ReplayRuntime::from_blob_reporting(&blob).unwrap();
    assert_eq!(path, RestorePath::Replay { reason: None });

    // Replay reaches the frontier by re-executing, so it mints fresh host-op
    // ids; the caller asks where it blocked rather than reusing the old id.
    // (An image restore keeps the ids, which is why it can be handed the
    // pending id straight from the artifact.)
    let mut suspend2 = suspend_at_first_fetch;
    let op_id = match rt2.drive(&mut suspend2).unwrap() {
        DriveOutcome::Suspended { op_id, .. } => op_id,
        DriveOutcome::Completed => panic!("expected a suspension"),
    };

    let mut reported = Vec::new();
    let mut handler = |name: &str, args: &Json| -> Option<Result<Json, String>> {
        if name == "report" {
            reported.push(args[0].clone());
        }
        Some(Ok(json!(32)))
    };
    rt2.provide_and_drive(op_id, Ok(json!(10)), &mut handler as Handler)
        .unwrap();
    assert_eq!(reported, vec![json!({"total": 42, "seen": ["a", "b"]})]);
}

#[test]
fn an_unusable_image_falls_back_instead_of_failing() {
    // An image that no longer applies — here because the artifact declares a
    // host surface the image was not taken against — must cost time, not
    // correctness: the journal is still there and still authoritative.
    let (rt, _op, _journal) = record_until_suspended(ASYNC_BUNDLE, &["fetchValue", "report"]);
    let blob = rt.to_blob(&["fetchValue", "report"]);
    drop(rt);

    let mut decoded: chidori_js::replay::DurableBlob = serde_json::from_slice(&blob).unwrap();
    decoded.effects.push("extraEffect".to_string());
    let doctored = serde_json::to_vec(&decoded).unwrap();

    let (mut rt2, path) = ReplayRuntime::from_blob_reporting(&doctored)
        .expect("an inapplicable image must not fail the resume");
    match path {
        RestorePath::Replay { reason: Some(_) } => {}
        other => panic!("expected a reported fallback, got {other:?}"),
    }

    // And it is a real, working runtime.
    let mut suspend = suspend_at_first_fetch;
    let op_id = match rt2.drive(&mut suspend).unwrap() {
        DriveOutcome::Suspended { op_id, .. } => op_id,
        DriveOutcome::Completed => panic!("expected a suspension"),
    };
    let mut reported = Vec::new();
    let mut handler = |name: &str, args: &Json| -> Option<Result<Json, String>> {
        if name == "report" {
            reported.push(args[0].clone());
        }
        Some(Ok(json!(32)))
    };
    rt2.provide_and_drive(op_id, Ok(json!(10)), &mut handler as Handler)
        .unwrap();
    assert_eq!(reported, vec![json!({"total": 42, "seen": ["a", "b"]})]);
}

// ---------------------------------------------------------------------------
// Pre-built restore targets (`prime_image_restore`).
//
// Priming moves the restore prologue off the restore itself; it must not move
// anything else. A primed restore has to be indistinguishable from a cold one,
// and a primed target must never be handed to a restore it does not fit.
// ---------------------------------------------------------------------------

/// Resume a suspension from its image and run it to completion, returning the
/// reported values and the resulting journal — the two observable outputs the
/// differential checks compare.
fn finish_from_image(
    image: &chidori_js::replay::RuntimeImage,
    journal: &[u8],
    effects: &[&str],
    op_id: u64,
) -> (Vec<Json>, Vec<u8>) {
    let mut rt = ReplayRuntime::from_image(image, ASYNC_BUNDLE, journal, effects).unwrap();
    let mut reported = Vec::new();
    let mut handler = |name: &str, args: &Json| -> Option<Result<Json, String>> {
        match name {
            "fetchValue" => Some(Ok(json!(32))),
            "report" => {
                reported.push(args[0].clone());
                Some(Ok(json!(null)))
            }
            _ => Some(Ok(json!(null))),
        }
    };
    rt.provide_and_drive(op_id, Ok(json!(10)), &mut handler as Handler)
        .unwrap();
    let j = rt.journal_bytes();
    (reported, j)
}

#[test]
fn a_primed_restore_is_identical_to_a_cold_one() {
    let effects = &["fetchValue", "report"];
    let (rt, op_id, journal) = record_until_suspended(ASYNC_BUNDLE, effects);
    let image = rt.to_image().unwrap();
    drop(rt);

    ReplayRuntime::clear_image_restore_pool();
    let cold = finish_from_image(&image, &journal, effects, op_id);

    ReplayRuntime::prime_image_restore(effects);
    let primed = finish_from_image(&image, &journal, effects, op_id);

    assert_eq!(cold.0, primed.0, "reported values differ");
    assert_eq!(
        String::from_utf8_lossy(&cold.1),
        String::from_utf8_lossy(&primed.1),
        "journals differ"
    );
}

#[test]
fn a_primed_target_is_consumed_exactly_once() {
    // `restore_image` mutates the VM it lands in, so the pooled target must be
    // removed when taken. The restore after a single prime has to fall back to
    // building its own — and still produce the same answer.
    let effects = &["fetchValue", "report"];
    let (rt, op_id, journal) = record_until_suspended(ASYNC_BUNDLE, effects);
    let image = rt.to_image().unwrap();
    drop(rt);

    ReplayRuntime::clear_image_restore_pool();
    ReplayRuntime::prime_image_restore(effects);

    let first = finish_from_image(&image, &journal, effects, op_id);
    let second = finish_from_image(&image, &journal, effects, op_id);
    let third = finish_from_image(&image, &journal, effects, op_id);

    assert_eq!(first.0, second.0);
    assert_eq!(second.0, third.0);
    assert_eq!(
        String::from_utf8_lossy(&first.1),
        String::from_utf8_lossy(&third.1),
        "a reused pool slot changed the journal"
    );
}

#[test]
fn a_primed_target_is_never_used_for_a_different_effect_set() {
    // Two effect lists build two different baselines. Priming one must not
    // supply the other — the pool is keyed by the effect list, and the baseline
    // digest stands behind it if that key were ever wrong.
    let recorded = &["fetchValue", "report"];
    let (rt, op_id, journal) = record_until_suspended(ASYNC_BUNDLE, recorded);
    let image = rt.to_image().unwrap();
    drop(rt);

    ReplayRuntime::clear_image_restore_pool();
    // Prime a *different* host surface than the image was taken against.
    ReplayRuntime::prime_image_restore(&["fetchValue", "report", "extraEffect"]);

    // The matching restore still succeeds: it must not pick up the primed
    // target built for the other surface.
    let ok = finish_from_image(&image, &journal, recorded, op_id);
    assert_eq!(ok.0.len(), 1, "matching restore should have reported once");

    // And restoring against the mismatched surface is still refused by the
    // digest, primed target present or not.
    let err = match ReplayRuntime::from_image(
        &image,
        ASYNC_BUNDLE,
        &journal,
        &["fetchValue", "report", "extraEffect"],
    ) {
        Err(e) => e,
        Ok(_) => panic!("a different host surface must not silently restore"),
    };
    assert!(err.contains("baseline"), "got: {err}");
}

#[test]
fn priming_is_idempotent_and_clearable() {
    let effects = &["fetchValue", "report"];
    ReplayRuntime::clear_image_restore_pool();
    ReplayRuntime::prime_image_restore(effects);
    ReplayRuntime::prime_image_restore(effects);
    ReplayRuntime::clear_image_restore_pool();

    // Clearing leaves the restore path working, just cold again.
    let (rt, op_id, journal) = record_until_suspended(ASYNC_BUNDLE, effects);
    let image = rt.to_image().unwrap();
    drop(rt);
    let out = finish_from_image(&image, &journal, effects, op_id);
    assert_eq!(out.0.len(), 1);
}

// ---------------------------------------------------------------------------
// The blob envelope and the lifetime of a runtime.
// ---------------------------------------------------------------------------

#[test]
fn a_legacy_blob_with_a_byte_array_journal_still_restores() {
    // Blobs written before the inline-journal change carried the journal as
    // raw JSON bytes — serialized by serde_json as an array of numbers. The
    // untagged decoder must keep accepting that shape forever; artifacts on
    // disk do not get rewritten.
    let (rt, _op, journal) = record_until_suspended(ASYNC_BUNDLE, &["fetchValue", "report"]);
    drop(rt);

    let legacy = serde_json::json!({
        "bundle": ASYNC_BUNDLE,
        "effects": ["fetchValue", "report"],
        "journal": journal, // Vec<u8> → JSON array of numbers, the old wire shape
    });
    let bytes = serde_json::to_vec(&legacy).unwrap();

    let (mut rt2, path) = ReplayRuntime::from_blob_reporting(&bytes)
        .expect("the legacy journal shape must keep parsing");
    assert_eq!(path, RestorePath::Replay { reason: None });

    let mut suspend = suspend_at_first_fetch;
    let op_id = match rt2.drive(&mut suspend).unwrap() {
        DriveOutcome::Suspended { op_id, .. } => op_id,
        DriveOutcome::Completed => panic!("expected a suspension"),
    };
    let mut reported = Vec::new();
    let mut handler = |name: &str, args: &Json| -> Option<Result<Json, String>> {
        if name == "report" {
            reported.push(args[0].clone());
        }
        Some(Ok(json!(32)))
    };
    rt2.provide_and_drive(op_id, Ok(json!(10)), &mut handler as Handler)
        .unwrap();
    assert_eq!(reported, vec![json!({"total": 42, "seen": ["a", "b"]})]);
}

#[test]
fn dropping_a_runtime_releases_its_realm() {
    // The realm graph is full of Rc cycles reference counting cannot reclaim;
    // ReplayRuntime's Drop breaks them via Vm::dispose. Hold a weak handle to
    // the global object across the drop: if the realm leaked, the cycle keeps
    // the global alive and the upgrade succeeds.
    let (rt, _op, journal) = record_until_suspended(ASYNC_BUNDLE, &["fetchValue", "report"]);
    let blob = rt.to_blob(&["fetchValue", "report"]);
    let weak_recorder = std::rc::Rc::downgrade(&rt.vm.realm.global.0);
    drop(rt);
    assert!(
        weak_recorder.upgrade().is_none(),
        "the recording runtime leaked its realm"
    );
    let _ = journal;

    // Same check for the image-restore path — the restored heap is grafted
    // onto the realm, so a leak here would also pin every restored object.
    let (rt2, _) = ReplayRuntime::from_blob_reporting(&blob).unwrap();
    let weak_restored = std::rc::Rc::downgrade(&rt2.vm.realm.global.0);
    drop(rt2);
    assert!(
        weak_restored.upgrade().is_none(),
        "the image-restored runtime leaked its realm"
    );
}
