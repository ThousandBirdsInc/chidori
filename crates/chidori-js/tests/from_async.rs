//! `Array.fromAsync` (ES2024) — the behaviours the self-hosted body pins down:
//! the three input shapes (async iterable / sync iterable / array-like), value
//! awaiting, the mapping function, constructor `this` handling, and the abrupt
//! paths (rejection + iterator close).

use chidori_js::Engine;

/// Run `src`, drain the microtask queue to quiescence, and return the console
/// output. `Engine::eval` drains once; a `fromAsync` chain settles across
/// several rounds of reactions, so drain again until nothing is left.
fn eval_console(src: &str) -> String {
    let mut e = Engine::new();
    if let Err(err) = e.eval(src) {
        return format!("ERR: {err}");
    }
    let _ = e.vm.run_jobs_until_blocked();
    e.console().join("\n")
}

#[test]
fn function_properties() {
    assert_eq!(
        eval_console(
            r#"
            const d = Object.getOwnPropertyDescriptor(Array, "fromAsync");
            console.log(Array.fromAsync.length, Array.fromAsync.name);
            console.log(d.writable, d.enumerable, d.configurable);
            console.log(Object.getPrototypeOf(Array.fromAsync) === Function.prototype);
            console.log(Object.getOwnPropertyDescriptor(Array.fromAsync, "prototype") === undefined);
            let ctorThrew = false;
            try { new Array.fromAsync([]); } catch (e) { ctorThrew = e instanceof TypeError; }
            console.log(ctorThrew);
        "#
        ),
        "1 fromAsync\ntrue false true\ntrue\ntrue\ntrue"
    );
}

#[test]
fn returns_a_promise_synchronously() {
    // Even a rejecting call returns (never throws) a native promise.
    assert_eq!(
        eval_console(
            r#"
            const p = Array.fromAsync(null);
            console.log(p instanceof Promise);
            p.catch(e => console.log('rejected', e.constructor.name));
        "#
        ),
        "true\nrejected TypeError"
    );
}

#[test]
fn async_iterable_input() {
    assert_eq!(
        eval_console(
            r#"
            async function* gen() { yield 1; yield 2; yield 3; }
            Array.fromAsync(gen()).then(a => console.log(a.join(',')));
        "#
        ),
        "1,2,3"
    );
}

#[test]
fn async_iterable_values_are_not_awaited() {
    // The async-iterator path yields values through as-is: a promise stays a
    // promise (only the sync-iterable and array-like paths await elements).
    assert_eq!(
        eval_console(
            r#"
            const p = Promise.resolve('inner');
            const items = {
              [Symbol.asyncIterator]() {
                let done = false;
                return { async next() { if (done) return { done: true };
                                        done = true; return { value: p, done: false }; } };
              }
            };
            Array.fromAsync(items).then(a => console.log(a.length, a[0] === p));
        "#
        ),
        "1 true"
    );
}

#[test]
fn sync_iterable_awaits_its_values() {
    assert_eq!(
        eval_console(
            r#"
            const input = [0, Promise.resolve(1), { then(res) { res(2); } }].values();
            Array.fromAsync(input).then(a => console.log(a.join(',')));
        "#
        ),
        "0,1,2"
    );
    // A plain string is a sync iterable (code points), not an array-like.
    assert_eq!(
        eval_console(r#"Array.fromAsync("abc").then(a => console.log(a.join('|')));"#),
        "a|b|c"
    );
}

#[test]
fn sync_iterators_are_not_async_iterable() {
    // %IteratorPrototype% must not carry @@asyncIterator, or every sync
    // iterable would take the (non-awaiting) async path.
    assert_eq!(
        eval_console(
            r#"
            function* g() {}
            console.log(g()[Symbol.asyncIterator], [].values()[Symbol.asyncIterator]);
            async function* ag() {}
            console.log(typeof ag()[Symbol.asyncIterator]);
        "#
        ),
        "undefined undefined\nfunction"
    );
}

#[test]
fn array_like_input() {
    assert_eq!(
        eval_console(
            r#"
            const input = { length: 3, 0: 'a', 1: Promise.resolve('b'), 2: { then(r) { r('c'); } } };
            Array.fromAsync(input).then(a => console.log(a.length, a.join(',')));
        "#
        ),
        "3 a,b,c"
    );
    // Elements are read in index order, after the two iterator symbols.
    assert_eq!(
        eval_console(
            r#"
            const seen = [];
            const input = new Proxy({ length: 2, 0: 'x', 1: 'y' },
              { get(t, k) { seen.push(String(k)); return t[k]; } });
            Array.fromAsync(input).then(a => console.log(a.join(','), '|', seen.join(' ')));
        "#
        ),
        "x,y | Symbol(Symbol.asyncIterator) Symbol(Symbol.iterator) length 0 1"
    );
}

#[test]
fn mapfn() {
    assert_eq!(
        eval_console(
            r#"
            Array.fromAsync([1, 2, 3], (v, i) => v * 10 + i).then(a => console.log(a.join(',')));
        "#
        ),
        "10,21,32"
    );
    // An async mapfn's result is awaited, and thisArg is honoured.
    assert_eq!(
        eval_console(
            r#"
            const ctx = { factor: 3 };
            Array.fromAsync([1, 2], async function (v) { return v * this.factor; }, ctx)
              .then(a => console.log(a.join(',')));
        "#
        ),
        "3,6"
    );
    // A non-callable mapfn rejects (it does not throw synchronously).
    assert_eq!(
        eval_console(
            r#"
            let sync = 'no sync throw';
            Array.fromAsync([1], 42).catch(e => console.log(sync, e.constructor.name));
        "#
        ),
        "no sync throw TypeError"
    );
}

#[test]
fn subclass_constructor() {
    assert_eq!(
        eval_console(
            r#"
            class MyArray extends Array {}
            Array.fromAsync.call(MyArray, [1, 2]).then(a => {
              console.log(a instanceof MyArray, Array.isArray(a), a.length, a.join(','));
            });
        "#
        ),
        "true true 2 1,2"
    );
    // Order of user-visible operations on a custom `this` and its instance.
    assert_eq!(
        eval_console(
            r#"
            const ops = [];
            function MyArray() {
              ops.push('construct');
              return new Proxy(Object.create(null), {
                set(t, k, v) { ops.push('set ' + String(k)); return Reflect.set(t, k, v); },
                defineProperty(t, k, d) { ops.push('define ' + String(k)); return Reflect.defineProperty(t, k, d); },
              });
            }
            Array.fromAsync.call(MyArray, [7, 8]).then(() => console.log(ops.join(', ')));
        "#
        ),
        "construct, define 0, define 1, set length"
    );
}

#[test]
fn errors_reject_and_close_iterators() {
    // A throwing mapfn closes the sync iterator and rejects.
    assert_eq!(
        eval_console(
            r#"
            let closed = false;
            const iterator = {
              next() { return { value: 1, done: false }; },
              return() { closed = true; return { done: true }; },
              [Symbol.iterator]() { return this; },
            };
            Array.fromAsync(iterator, () => { throw new Error('boom'); })
              .catch(e => console.log(e.message, closed));
        "#
        ),
        "boom true"
    );
    // A rejecting element of a sync iterable closes it too.
    assert_eq!(
        eval_console(
            r#"
            let closed = 0;
            function* g() { try { yield Promise.reject(new Error('nope')); } finally { closed++; } }
            Array.fromAsync(g()).catch(e => console.log(e.message, closed));
        "#
        ),
        "nope 1"
    );
    // A throwing async iterator rejects the promise.
    assert_eq!(
        eval_console(
            r#"
            async function* g() { yield 1; throw new Error('mid'); }
            Array.fromAsync(g()).catch(e => console.log('rejected', e.message));
        "#
        ),
        "rejected mid"
    );
    // A non-callable @@asyncIterator is a TypeError; @@iterator is not probed.
    assert_eq!(
        eval_console(
            r#"
            let probed = false;
            const items = { [Symbol.asyncIterator]: 1,
                            get [Symbol.iterator]() { probed = true; } };
            Array.fromAsync(items).catch(e => console.log(e.constructor.name, probed));
        "#
        ),
        "TypeError false"
    );
}

#[test]
fn captured_intrinsics_resist_tampering() {
    // The body holds the well-known symbols and its spec-operation shims from
    // install time, so replacing the user-reachable globals cannot redirect it.
    assert_eq!(
        eval_console(
            r#"
            globalThis.Symbol = { iterator: Symbol('fake'), asyncIterator: Symbol('fake') };
            Object.defineProperty = function () { throw new Error('nope'); };
            Reflect.apply = function () { throw new Error('nope'); };
            Array.fromAsync({ length: 2, 0: 'a', 1: 'b' }).then(
              a => console.log(a.join(',')),
              e => console.log('unexpected', e.message));
        "#
        ),
        "a,b"
    );
}
