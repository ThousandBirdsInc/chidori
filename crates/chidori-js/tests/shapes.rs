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
