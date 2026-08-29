//! Shapes-focused corpus (docs/js-object-shapes-design.md §4, Phase 2).
//!
//! Plain objects are born SHAPED (shared key layout in the realm's
//! transition tree) and demote to dictionary mode on the destructive edges.
//! Storage mode must be unobservable: these tests pin the observable
//! surfaces — enumeration order, delete/defineProperty/freeze mid-loop,
//! accessors, proxies over shaped targets, JSON round-trips — across the
//! shaped path, the demotion boundary, and dictionary mode.

use chidori_js::Engine;

fn run(src: &str) -> String {
    let mut e = Engine::new();
    match e.eval(src) {
        Ok(v) => e.vm.to_string_lossy(&v),
        Err(err) => format!("ERR: {err}"),
    }
}

#[test]
fn literal_enumeration_order_is_insertion_order() {
    assert_eq!(run("Object.keys({b: 1, a: 2, c: 3}).join()"), "b,a,c");
    // Same keys, different insertion order: distinct shapes, distinct orders.
    assert_eq!(run("Object.keys({a: 2, b: 1, c: 3}).join()"), "a,b,c");
}

#[test]
fn integer_keys_enumerate_first_ascending() {
    // Spec ordering applies at enumeration time in both storage modes.
    assert_eq!(
        run("Object.keys({b: 0, 2: 0, a: 0, 0: 0}).join()"),
        "0,2,b,a"
    );
    assert_eq!(
        run(r#"
            let o = {x: 1};
            o[1] = 'i'; o.y = 2; o[0] = 'j';
            Object.keys(o).join()
        "#),
        "0,1,x,y"
    );
}

#[test]
fn same_shape_objects_do_not_alias_values() {
    // N same-shape objects share ONE key layout but never values.
    assert_eq!(
        run(r#"
            const mk = (i) => ({x: i, y: i * 2});
            const a = [];
            for (let i = 0; i < 100; i++) a.push(mk(i));
            a[7].x = -1;
            [a[7].x, a[7].y, a[8].x, a[8].y].join()
        "#),
        "-1,14,8,16"
    );
}

#[test]
fn delete_mid_loop_preserves_order_and_semantics() {
    assert_eq!(
        run(r#"
            const out = [];
            for (let i = 0; i < 3; i++) {
                const o = {a: 1, b: 2, c: 3};
                if (i === 1) delete o.b;
                o.d = 4; // append AFTER the demoting delete
                out.push(Object.keys(o).join(''));
            }
            out.join('|')
        "#),
        "abcd|acd|abcd"
    );
    // Delete then re-add: the key moves to the END (insertion order).
    assert_eq!(
        run("const o = {a:1, b:2, c:3}; delete o.b; o.b = 9; Object.keys(o).join()"),
        "a,c,b"
    );
    // Deleting an ABSENT key is not a demoting edge and changes nothing.
    assert_eq!(
        run("const o = {a:1, b:2}; delete o.zzz; Object.keys(o).join() + '=' + o.a"),
        "a,b=1"
    );
}

#[test]
fn define_property_mid_loop() {
    // Attribute changes (non-enumerable, non-writable, accessors) on shaped
    // objects — mid-loop so shaped siblings of the mutated object coexist.
    assert_eq!(
        run(r#"
            const out = [];
            for (let i = 0; i < 3; i++) {
                const o = {a: 1, b: 2};
                if (i === 1) {
                    Object.defineProperty(o, 'b', {enumerable: false});
                    Object.defineProperty(o, 'c', {value: 3, writable: false,
                                                   enumerable: true, configurable: true});
                    o.c = 99; // silently ignored (sloppy): non-writable
                } else {
                    o.c = 3;
                }
                out.push(Object.keys(o).join('') + ':' + o.a + o.b + o.c);
            }
            out.join('|')
        "#),
        "abc:123|ac:123|abc:123"
    );
}

#[test]
fn accessors_on_shaped_objects() {
    assert_eq!(
        run(r#"
            const o = {x: 1};
            let backing = 10;
            Object.defineProperty(o, 'y', {
                get() { return backing; },
                set(v) { backing = v * 2; },
                enumerable: true, configurable: true,
            });
            o.z = 3;               // append after an accessor was defined
            o.y = 21;
            [o.x, o.y, o.z, Object.keys(o).join('')].join('|')
        "#),
        "1|42|3|xyz"
    );
}

#[test]
fn freeze_and_seal_mid_loop() {
    assert_eq!(
        run(r#"
            const out = [];
            for (let i = 0; i < 3; i++) {
                const o = {a: 1, b: 2};
                if (i === 1) Object.freeze(o);
                o.a = 5;    // ignored when frozen (sloppy)
                o.c = 3;    // ignored when frozen
                out.push(Object.keys(o).join('') + ':' + o.a + ':' + (o.c ?? '-')
                         + ':' + Object.isFrozen(o));
            }
            out.join('|')
        "#),
        "abc:5:3:false|ab:1:-:true|abc:5:3:false"
    );
    assert_eq!(
        run(r#"
            const o = {a: 1, b: 2};
            Object.seal(o);
            o.a = 7;          // sealed: existing props stay writable
            delete o.b;       // refused
            o.c = 1;          // refused
            [Object.keys(o).join(''), o.a, o.b, Object.isSealed(o)].join('|')
        "#),
        "ab|7|2|true"
    );
}

#[test]
fn for_in_over_shaped_and_demoted() {
    assert_eq!(
        run(r#"
            const proto = {p: 0};
            const o = Object.create(proto);
            o.a = 1; o.b = 2;
            const seen = [];
            for (const k in o) seen.push(k);
            delete o.a;
            for (const k in o) seen.push(k);
            seen.join()
        "#),
        "a,b,p,b,p"
    );
}

#[test]
fn json_roundtrip_shaped_records() {
    assert_eq!(
        run(r#"
            const src = JSON.stringify(
                Array.from({length: 50}, (_, i) => ({id: i, name: 'n' + i,
                    tags: [i, i + 1], meta: {ok: i % 2 === 0}})));
            const arr = JSON.parse(src);
            JSON.stringify(arr) === src
                ? arr[49].id + ':' + arr[49].meta.ok + ':' + Object.keys(arr[0]).join('')
                : 'MISMATCH'
        "#),
        "49:false:idnametagsmeta"
    );
}

#[test]
fn spread_rest_and_assign_preserve_order() {
    assert_eq!(run("Object.keys({...{b: 1, a: 2}, c: 3}).join()"), "b,a,c");
    assert_eq!(
        run("const {b, ...rest} = {b: 1, a: 2, c: 3}; Object.keys(rest).join()"),
        "a,c"
    );
    assert_eq!(
        run("Object.keys(Object.assign({z: 0}, {b: 1, a: 2})).join()"),
        "z,b,a"
    );
}

#[test]
fn proxy_over_shaped_target() {
    assert_eq!(
        run(r#"
            const target = {a: 1, b: 2};
            const log = [];
            const p = new Proxy(target, {
                get(t, k, r) { if (typeof k === 'string') log.push('g' + k); return Reflect.get(t, k, r); },
                deleteProperty(t, k) { log.push('d' + k); return Reflect.deleteProperty(t, k); },
            });
            p.a; delete p.b; p.c = 3;
            [Object.keys(target).join(''), log.join('')].join('|')
        "#),
        "ac|gadb"
    );
}

#[test]
fn getownpropertydescriptor_across_modes() {
    assert_eq!(
        run(r#"
            const o = {a: 1, b: 2};
            const d1 = Object.getOwnPropertyDescriptor(o, 'a');
            delete o.b; // demote
            const d2 = Object.getOwnPropertyDescriptor(o, 'a');
            [d1.value, d1.writable, d1.enumerable, d1.configurable,
             d2.value, d2.writable, d2.enumerable, d2.configurable].join()
        "#),
        "1,true,true,true,1,true,true,true"
    );
}

#[test]
fn many_index_keys_demote_but_stay_correct() {
    // Integer-key spam on a non-array crosses the shaped→dictionary bound;
    // ordering (indices ascending first) and values must be unaffected.
    assert_eq!(
        run(r#"
            const o = {name: 'grid'};
            for (let i = 0; i < 20; i++) o[i] = i * i;
            Object.keys(o).length + ':' + o[19] + ':' + Object.keys(o)[0]
              + ':' + Object.keys(o)[20]
        "#),
        "21:361:0:name"
    );
}

#[test]
fn wide_objects_use_index_lookup() {
    // Cross the chain-walk → per-shape index threshold (8) and keep going.
    assert_eq!(
        run(r#"
            const o = {};
            for (let i = 0; i < 40; i++) o['k' + i] = i;
            let sum = 0;
            for (let i = 0; i < 40; i++) sum += o['k' + i];
            sum + ':' + Object.keys(o).length + ':' + o.k39
        "#),
        "780:40:39"
    );
}

#[test]
fn shaped_objects_in_maps_and_stringify_of_demoted() {
    assert_eq!(
        run(r#"
            const o = {a: 1, b: 2, c: 3};
            delete o.b;
            o.d = 4;
            JSON.stringify(o)
        "#),
        r#"{"a":1,"c":3,"d":4}"#
    );
}

#[test]
fn prototype_mutation_does_not_disturb_shapes() {
    // Proto changes do NOT demote (the shape holds no proto); lookups after
    // a proto swap must see the new chain.
    assert_eq!(
        run(r#"
            const o = {a: 1};
            const proto1 = {p: 'one'}, proto2 = {p: 'two'};
            Object.setPrototypeOf(o, proto1);
            const before = o.p;
            Object.setPrototypeOf(o, proto2);
            o.b = 2; // still appendable (still shaped or dict — unobservable)
            [before, o.p, Object.keys(o).join('')].join('|')
        "#),
        "one|two|ab"
    );
}

#[test]
fn replay_identical_across_engines() {
    // Two fresh engines running the same shape-heavy program must produce
    // identical output (shapes are derived from program behavior only).
    let src = r#"
        const rows = [];
        for (let i = 0; i < 25; i++) {
            const r = {i, sq: i * i, label: 'r' + i};
            if (i % 5 === 0) delete r.sq;
            if (i % 7 === 0) Object.defineProperty(r, 'hidden', {value: i, enumerable: false});
            rows.push(r);
        }
        JSON.stringify(rows)
    "#;
    let a = run(src);
    let b = run(src);
    assert_eq!(a, b);
    assert!(a.contains("\"label\":\"r24\""), "unexpected output: {a}");
}

#[test]
fn for_in_guard_and_cursor_hint_edges() {
    // The for-in fast path (shape-guarded liveness + the (object, key, slot)
    // cursor hint serving `o[k]` in the body) must be unobservable. Each
    // case here breaks one of its ingredients mid-loop.

    // Delete mid-loop (demotes to dictionary): the deleted-but-unvisited key
    // must be skipped, later keys still enumerate, and `o[k]` still reads
    // the live values.
    assert_eq!(
        run(r#"
            const o = {a: 1, b: 2, c: 3, d: 4};
            const seen = [];
            for (const k in o) {
                if (k === 'a') delete o.c;
                seen.push(k + '=' + o[k]);
            }
            seen.join()
        "#),
        "a=1,b=2,d=4"
    );

    // Add mid-loop (new shape): snapshot keys keep enumerating (the added
    // key is not yielded), and reads keep resolving.
    assert_eq!(
        run(r#"
            const o = {a: 1, b: 2};
            const seen = [];
            for (const k in o) { o.z = 9; seen.push(k + '=' + o[k]); }
            seen.join() + '|' + o.z
        "#),
        "a=1,b=2|9"
    );

    // Overwrite mid-loop: the hint carries a SLOT, not a value — `o[k]`
    // must observe the write that happened after the step.
    assert_eq!(
        run(r#"
            const o = {a: 1, b: 2};
            const seen = [];
            for (const k in o) { o[k] = o[k] * 10; seen.push(k + '=' + o[k]); }
            seen.join()
        "#),
        "a=10,b=20"
    );

    // defineProperty to an accessor mid-loop keeps the SAME shape
    // (attributes live in slots) — the hint must fall back so the getter
    // actually runs.
    assert_eq!(
        run(r#"
            const o = {a: 1, b: 2};
            const seen = [];
            for (const k in o) {
                if (k === 'a') {
                    Object.defineProperty(o, 'b', { get() { return 77; }, enumerable: true });
                }
                seen.push(k + '=' + o[k]);
            }
            seen.join()
        "#),
        "a=1,b=77"
    );

    // The hint is keyed to ONE object: a same-shaped sibling read with the
    // same key must not be served from the enumerated object's slots.
    assert_eq!(
        run(r#"
            const o = {a: 1, b: 2};
            const p = {a: 10, b: 20};
            const seen = [];
            for (const k in o) seen.push(o[k] + '/' + p[k]);
            seen.join()
        "#),
        "1/10,2/20"
    );

    // Nested for-in over the same object: inner enumeration finishing must
    // not perturb the outer one's reads.
    assert_eq!(
        run(r#"
            const o = {a: 1, b: 2};
            const seen = [];
            for (const k in o) {
                for (const j in o) seen.push(k + j);
                seen.push(k + '=' + o[k]);
            }
            seen.join()
        "#),
        "aa,ab,a=1,ba,bb,b=2"
    );

    // Prototype-contributed keys disable the guard (they are not own):
    // deletes on the proto mid-loop must still be honored per the spec's
    // deleted-key skip.
    assert_eq!(
        run(r#"
            const proto = {p: 1, q: 2};
            const o = Object.create(proto);
            o.a = 0;
            const seen = [];
            for (const k in o) {
                if (k === 'a') delete proto.q;
                seen.push(k + '=' + o[k]);
            }
            seen.join()
        "#),
        "a=0,p=1"
    );

    // Index-like keys re-order ahead of names (no guard): order and reads
    // stay correct.
    assert_eq!(
        run(r#"
            const o = {b: 'B', 1: 'one', a: 'A', 0: 'zero'};
            const seen = [];
            for (const k in o) seen.push(k + '=' + o[k]);
            seen.join()
        "#),
        "0=zero,1=one,b=B,a=A"
    );
}

#[test]
fn for_in_plan_cache_respects_per_object_attributes() {
    // The for-in key plan is cached on the SHAPE, but enumerability lives
    // per object (attributes never fork the transition tree). These cases
    // pin that the shared plan can never leak one object's attributes onto
    // a same-shaped sibling.

    // Same shape, different enumerability: the plan must not be applied to
    // the object that hid a key.
    assert_eq!(
        run(r#"
            const a = {x: 1, y: 2, z: 3};
            const b = {x: 1, y: 2, z: 3};
            Object.defineProperty(b, 'y', { enumerable: false });
            const ka = []; for (const k in a) ka.push(k);
            const kb = []; for (const k in b) kb.push(k);
            ka.join('') + '|' + kb.join('')
        "#),
        "xyz|xz"
    );

    // Order matters too: hide first, then enumerate the all-enumerable
    // sibling — whichever object primes the shape's plan, both stay right.
    assert_eq!(
        run(r#"
            const b = {x: 1, y: 2, z: 3};
            Object.defineProperty(b, 'y', { enumerable: false });
            const kb = []; for (const k in b) kb.push(k);
            const a = {x: 1, y: 2, z: 3};
            const ka = []; for (const k in a) ka.push(k);
            kb.join('') + '|' + ka.join('')
        "#),
        "xz|xyz"
    );

    // Re-enabling enumerability mid-life is observed on the next loop.
    assert_eq!(
        run(r#"
            const o = {p: 1, q: 2};
            Object.defineProperty(o, 'q', { enumerable: false });
            const one = []; for (const k in o) one.push(k);
            Object.defineProperty(o, 'q', { enumerable: true });
            const two = []; for (const k in o) two.push(k);
            one.join('') + '|' + two.join('')
        "#),
        "p|pq"
    );

    // Index keys make slot order differ from enumeration order, so the
    // shape carries no plan — the generic ordering must still hold, and
    // the `o[k]` reads must follow it.
    assert_eq!(
        run(r#"
            const o = {b: 'B', 2: 'two', a: 'A', 0: 'zero'};
            const seen = []; for (const k in o) seen.push(k + ':' + o[k]);
            seen.join()
        "#),
        "0:zero,2:two,b:B,a:A"
    );

    // Accessors are enumerated (and must run) even though the plan path
    // only proves enumerability, not data-ness.
    assert_eq!(
        run(r#"
            const o = { a: 1, get b() { return 'g'; }, c: 3 };
            const seen = []; for (const k in o) seen.push(k + '=' + o[k]);
            seen.join()
        "#),
        "a=1,b=g,c=3"
    );

    // A plan primed on a plain object must not be reused across a shape
    // that grew further (different shape node, different plan).
    assert_eq!(
        run(r#"
            const s = {m: 1, n: 2};
            const t = {m: 1, n: 2}; t.o = 3;
            const ks = []; for (const k in s) ks.push(k);
            const kt = []; for (const k in t) kt.push(k);
            ks.join('') + '|' + kt.join('')
        "#),
        "mn|mno"
    );
}
