//! Cycle-collector tests: garbage cycles are reclaimed, reachable state
//! survives, and `dispose` breaks even orphaned cycles.

use chidori_js::value::Value;
use chidori_js::Engine;

/// Plain self-referential object cycles become unreachable garbage that
/// `collect_cycles` sweeps.
#[test]
fn collects_simple_object_cycles() {
    let mut e = Engine::new();
    e.eval(
        r#"
        (function () {
          for (let i = 0; i < 200; i++) {
            const o = { i };
            o.self = o;            // o -> o
            const a = [o];
            o.arr = a;             // o -> a -> o
          }
        })();
        "#,
    )
    .unwrap();
    let before = e.vm.gc_tracked_live();
    let swept = e.vm.collect_cycles();
    assert!(swept >= 400, "expected >= 400 swept, got {swept}");
    let after = e.vm.gc_tracked_live();
    assert!(
        after < before,
        "live count should drop: {before} -> {after}"
    );
    // The realm is intact: execution still works after a collection.
    let v = e.eval("1 + 1").unwrap();
    assert!(matches!(v, Value::Number(n) if n == 2.0));
}

/// Closure <-> upvalue-cell cycles (the most common real-world leak) are
/// collected when dropped, and survive while still reachable.
#[test]
fn collects_closure_cycles_keeps_reachable_ones() {
    let mut e = Engine::new();
    e.eval(
        r#"
        globalThis.make = function () {
          const x = { tag: "x" };
          x.f = () => x;           // closure captures x; x holds the closure
          return x.f;
        };
        globalThis.kept = make();
        for (let i = 0; i < 100; i++) make(); // garbage cycles
        "#,
    )
    .unwrap();
    let swept = e.vm.collect_cycles();
    assert!(
        swept >= 100,
        "expected the dropped cycles swept, got {swept}"
    );
    // The kept closure still resolves its captured cycle.
    let v = e.eval("kept().f === kept && kept().tag").unwrap();
    assert!(matches!(&v, Value::String(s) if s.as_str() == "x"), "{v:?}");
}

/// A suspended async frame reachable through a live resolver survives
/// collection and resumes correctly afterwards.
#[test]
fn suspended_async_frame_survives_collection() {
    let mut e = Engine::new();
    e.eval(
        r#"
        globalThis.resolveIt = null;
        globalThis.result = null;
        (async () => {
          const v = await new Promise(r => { globalThis.resolveIt = r; });
          globalThis.result = v.x;
        })();
        "#,
    )
    .unwrap();
    e.vm.collect_cycles();
    e.eval("resolveIt({ x: 42 })").unwrap();
    let v = e.eval("result").unwrap();
    assert!(matches!(v, Value::Number(n) if n == 42.0), "{v:?}");
}

/// Abandoned pending-forever async frames (promise <-> reaction <-> frame
/// cycles) are collected once nothing outside can reach them.
#[test]
fn collects_abandoned_async_frames() {
    let mut e = Engine::new();
    e.eval(
        r#"
        (function () {
          for (let i = 0; i < 50; i++) {
            const p = new Promise(() => {});  // never settles
            (async () => { await p; })();      // frame parked on p forever
          }
        })();
        "#,
    )
    .unwrap();
    let swept = e.vm.collect_cycles();
    assert!(swept > 0, "abandoned async state should be swept");
}

/// A suspended generator survives collection while reachable and resumes.
#[test]
fn suspended_generator_survives_collection() {
    let mut e = Engine::new();
    e.eval(
        r#"
        globalThis.g = (function* () {
          const big = { n: 1 };
          yield big;
          yield big.n + 1;
        })();
        g.next();
        "#,
    )
    .unwrap();
    e.vm.collect_cycles();
    let v = e.eval("g.next().value").unwrap();
    assert!(matches!(v, Value::Number(n) if n == 2.0), "{v:?}");
}

/// Repeated collect cycles on a busy heap keep the live count bounded
/// (allocation-heavy loop does not grow the tracked set monotonically).
#[test]
fn live_count_bounded_under_churn() {
    let mut e = Engine::new();
    let mut peak_after_collect = 0usize;
    for round in 0..5 {
        e.eval(
            r#"
            (function () {
              for (let i = 0; i < 500; i++) {
                const o = {};
                o.self = o;
                o.fn = () => o;
              }
            })();
            "#,
        )
        .unwrap();
        e.vm.collect_cycles();
        let live = e.vm.gc_tracked_live();
        if round == 0 {
            peak_after_collect = live;
        } else {
            // Allow slack for caches, but no monotonic growth.
            assert!(
                live <= peak_after_collect + 50,
                "round {round}: live {live} grew past {peak_after_collect}"
            );
        }
    }
}

/// `dispose` reclaims ORPHANED cycles too (ones no longer connected to the
/// realm roots), which the old realm-walk teardown missed.
#[test]
fn dispose_breaks_orphaned_cycles() {
    let mut e = Engine::new();
    let v = e.eval("const o = { name: 'cyc' }; o.self = o; o").unwrap();
    let weak = match &v {
        Value::Object(o) => std::rc::Rc::downgrade(&o.0),
        other => panic!("expected object, got {other:?}"),
    };
    drop(v);
    assert!(
        weak.upgrade().is_some(),
        "cycle keeps itself alive pre-dispose"
    );
    e.vm.dispose();
    assert!(
        weak.upgrade().is_none(),
        "dispose must break orphaned cycles so Rc frees them"
    );
}
