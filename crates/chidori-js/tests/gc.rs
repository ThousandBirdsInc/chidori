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

/// Automatic collection: a long-lived engine that keeps creating garbage
/// cycles must stay bounded WITHOUT the host ever calling `collect_cycles` —
/// the allocation-count trigger fires at the end-of-drain quiescence point.
#[test]
fn auto_collection_bounds_cycle_churn_without_manual_calls() {
    let mut e = Engine::new();
    let mut max_live = 0usize;
    for _ in 0..8 {
        e.eval(
            r#"
            (function () {
              for (let i = 0; i < 3000; i++) {
                const o = { i };
                o.self = o;
                o.fn = () => o;
              }
            })();
            "#,
        )
        .unwrap();
        max_live = max_live.max(e.vm.gc_tracked_live());
    }
    // 8 rounds x 3000 cycles x ~3 objects each ≈ 72k allocations. Unbounded
    // leak would keep them all live; the auto trigger must keep the peak to
    // roughly one inter-collection window.
    assert!(
        max_live < 40_000,
        "auto GC failed to bound cycle churn: peak live {max_live}"
    );
}

/// A mapped-arguments slot ALIASES the frame's parameter binding cell. The
/// collector must count that shared cell's inner reference ONCE, not once per
/// holder: double-subtracting it absorbs a genuine host reference in the
/// accounting, so a host-held object that is otherwise reachable only through
/// a doomed cycle would be treated as garbage and have its edges cleared — a
/// use-after-free-equivalent silent corruption.
#[test]
fn mapped_arguments_cell_counted_once_keeps_host_held_object() {
    let mut e = Engine::new();
    // `build(x)` parks x's binding cell in BOTH a suspended generator frame
    // and the mapped arguments object, then orphans the whole cycle.
    let build = e
        .eval(
            r#"
            (function build(x) {
              function* g(a) {
                const args = arguments;   // mapped: args[0] aliases a's cell
                const self = { args };
                yield self;
                return a;
              }
              let it = g(x);
              let self = it.next().value;
              self.gen = it;              // orphaned cycle: gen -> frame -> self -> gen
            })
            "#,
        )
        .unwrap();
    // Host-held object: its ONLY in-heap references flow through the doomed
    // cycle's shared cell.
    let x = e.eval("({ keep: 'me' })").unwrap();
    e.vm
        .call(build, Value::Undefined, std::slice::from_ref(&x))
        .unwrap();

    let swept = e.vm.collect_cycles();
    assert!(swept > 0, "the orphaned generator cycle should be swept");

    // The host-held object must have survived with its properties intact.
    let getkeep = e.eval("(function (o) { return o.keep })").unwrap();
    let v = e.vm.call(getkeep, Value::Undefined, &[x]).unwrap();
    assert!(
        matches!(&v, Value::String(s) if s.as_str() == "me"),
        "host-held object was corrupted by the sweep: {v:?}"
    );
}

/// WeakMap entries must not keep their keys alive: once nothing else can
/// reach the key, collection reclaims key and value and prunes the entry.
#[test]
fn weakmap_does_not_keep_keys_alive() {
    let mut e = Engine::new();
    e.eval(
        r#"
        globalThis.wm = new WeakMap();
        globalThis.kept = { tag: 'kept' };
        wm.set(kept, { payload: 'for-kept' });
        (function () {
          for (let i = 0; i < 100; i++) {
            wm.set({ i }, { big: 'value-' + i });   // keys die at loop exit
          }
        })();
        "#,
    )
    .unwrap();
    let before = e.vm.gc_tracked_live();
    let swept = e.vm.collect_cycles();
    // 100 dead keys + 100 values must go; the strongly-reachable key stays.
    assert!(swept >= 200, "expected weak entries reclaimed, got {swept}");
    let after = e.vm.gc_tracked_live();
    assert!(
        after < before,
        "live count should drop: {before} -> {after}"
    );
    let v = e.eval("wm.get(kept).payload").unwrap();
    assert!(
        matches!(&v, Value::String(s) if s.as_str() == "for-kept"),
        "{v:?}"
    );
}

/// WeakSet membership must not keep its members alive.
#[test]
fn weakset_does_not_keep_members_alive() {
    let mut e = Engine::new();
    e.eval(
        r#"
        globalThis.ws = new WeakSet();
        globalThis.kept = { tag: 'kept' };
        ws.add(kept);
        (function () {
          for (let i = 0; i < 100; i++) ws.add({ i });
        })();
        "#,
    )
    .unwrap();
    let swept = e.vm.collect_cycles();
    assert!(swept >= 100, "expected weak members reclaimed, got {swept}");
    let v = e.eval("ws.has(kept)").unwrap();
    assert!(matches!(v, Value::Bool(true)), "{v:?}");
}

/// Ephemeron semantics: a WeakMap value is kept alive by a live key, even
/// when the ONLY path to the value is through the WeakMap — and chains of
/// such entries (value of one is key of the next) resolve to fixpoint.
#[test]
fn weakmap_values_live_iff_keys_live() {
    let mut e = Engine::new();
    e.eval(
        r#"
        globalThis.wm = new WeakMap();
        globalThis.k1 = { tag: 'k1' };
        const v1 = { tag: 'v1' };        // reachable ONLY through wm entry
        wm.set(k1, v1);
        wm.set(v1, { tag: 'v2' });       // chain: v1 alive => v2 alive
        "#,
    )
    .unwrap();
    e.vm.collect_cycles();
    let v = e.eval("wm.get(wm.get(k1)).tag").unwrap();
    assert!(
        matches!(&v, Value::String(s) if s.as_str() == "v2"),
        "{v:?}"
    );
    // Now drop the root key: the whole chain becomes collectable.
    e.eval("globalThis.k1 = null").unwrap();
    let swept = e.vm.collect_cycles();
    assert!(swept >= 3, "chain should be reclaimed, got {swept}");
}

/// The classic weak cycle: value references its own key. A strong map leaks
/// it; a WeakMap entry whose key is otherwise unreachable must be collected.
#[test]
fn weakmap_key_value_cycle_is_collected() {
    let mut e = Engine::new();
    e.eval(
        r#"
        globalThis.wm = new WeakMap();
        (function () {
          for (let i = 0; i < 50; i++) {
            const key = { i };
            wm.set(key, { backref: key });  // value -> key cycle through the entry
          }
        })();
        "#,
    )
    .unwrap();
    let swept = e.vm.collect_cycles();
    assert!(
        swept >= 100,
        "key<->value cycles should be reclaimed, got {swept}"
    );
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
