//! Iterator helpers (ES2025 27.1): the lazy adapters, eager consumers,
//! `Iterator.from`, the abstract `Iterator` constructor, and the close /
//! re-entrancy semantics the spec pins.

use chidori_js::Engine;

fn eval_console(src: &str) -> Vec<String> {
    let mut e = Engine::new();
    match e.eval(src) {
        Ok(_) => e.console().to_vec(),
        Err(err) => panic!("eval failed: {err}\nconsole: {:?}", e.console()),
    }
}

#[test]
fn lazy_adapters_produce_spec_sequences() {
    let out = eval_console(
        r#"
        function* nums() { for (let i = 1; i <= 10; i++) yield i; }
        console.log(nums().map(x => x * 2).toArray().join(","));
        console.log(nums().filter(x => x % 2).take(3).toArray().join(","));
        console.log(nums().drop(7).toArray().join(","));
        console.log(nums().flatMap(x => [x, -x]).take(5).toArray().join(","));
        console.log([...nums().map((x, i) => x + "@" + i).take(2)].join(" "));
        "#,
    );
    assert_eq!(
        out,
        [
            "2,4,6,8,10,12,14,16,18,20",
            "1,3,5",
            "8,9,10",
            "1,-1,2,-2,3",
            "1@0 2@1"
        ]
    );
}

#[test]
fn eager_consumers() {
    let out = eval_console(
        r#"
        function* nums() { for (let i = 1; i <= 10; i++) yield i; }
        console.log(nums().reduce((a, b) => a + b));
        console.log(nums().reduce((a, b) => a + b, 100));
        console.log(nums().some(x => x > 9), nums().every(x => x > 0), nums().find(x => x % 7 === 0));
        console.log(nums().every(x => x < 5), nums().some(x => x > 99), nums().find(x => x > 99));
        let seen = [];
        nums().take(3).forEach((v, i) => seen.push(v + ":" + i));
        console.log(seen.join(","));
        try { [].values().reduce((a, b) => a + b); } catch (e) { console.log(e instanceof TypeError); }
        "#,
    );
    assert_eq!(
        out,
        [
            "55",
            "155",
            "true true 7",
            "false false undefined",
            "1:0,2:1,3:2",
            "true"
        ]
    );
}

#[test]
fn early_exit_and_callback_throw_close_the_underlying_iterator() {
    let out = eval_console(
        r#"
        let closes = 0;
        function* tracked() { try { for (let i = 1;; i++) yield i; } finally { closes++; } }
        console.log(tracked().some(x => x === 3), closes);
        console.log(tracked().find(x => x === 2), closes);
        console.log(tracked().take(2).toArray().join(","), closes);
        try { tracked().map(() => { throw new Error("boom"); }).next(); }
        catch (e) { console.log(e.message, closes); }
        // return() closes a started helper's underlying iterator.
        const h = tracked().map(x => x);
        h.next();
        console.log(JSON.stringify(h.return()), closes);
        console.log(JSON.stringify(h.next()));
        "#,
    );
    assert_eq!(
        out,
        [
            "true 1",
            "2 2",
            "1,2 3",
            "boom 4",
            "{\"done\":true} 5",
            "{\"done\":true}"
        ]
    );
}

#[test]
fn take_drop_limit_validation_closes_on_bad_limit() {
    let out = eval_console(
        r#"
        // A never-started generator's finally must NOT run on close (spec:
        // suspendedStart return completes without entering the body), so
        // track the close through an explicit return method instead.
        let closes = 0;
        const bare = () => ({
            next() { return { value: 1, done: false }; },
            return() { closes++; return { done: true }; },
            [Symbol.iterator]() { return this; },
        });
        for (const bad of [NaN, -1]) {
            try { Iterator.from(bare()).take(bad); } catch (e) { console.log(e instanceof RangeError, closes); }
        }
        try { Iterator.from(bare()).drop(-2); } catch (e) { console.log(e instanceof RangeError, closes); }
        // Infinity is a legal limit.
        console.log([].values().drop(Infinity).toArray === undefined ? "?" : "fn");
        console.log([1,2,3].values().take(Infinity).toArray().join(","));
        "#,
    );
    assert_eq!(out, ["true 1", "true 2", "true 3", "fn", "1,2,3"]);
}

#[test]
fn iterator_from_wraps_and_passes_through() {
    let out = eval_console(
        r#"
        // Array iterators are already Iterator instances: pass through.
        const ai = [1,2].values();
        console.log(Iterator.from(ai) === ai);
        // Strings iterate.
        console.log(Iterator.from("ab").toArray().join("|"));
        // A bare iterator (no @@iterator) gets wrapped and forwards next/return.
        let ret = 0;
        const bare = { next() { return { value: 7, done: false }; }, return() { ret++; return { done: true }; } };
        const w = Iterator.from(bare);
        console.log(w !== bare, w.next().value, w instanceof Iterator);
        w.return();
        console.log(ret);
        try { Iterator.from(5); } catch (e) { console.log(e instanceof TypeError); }
        "#,
    );
    assert_eq!(out, ["true", "a|b", "true 7 true", "1", "true"]);
}

#[test]
fn abstract_constructor_and_prototype_accessors() {
    let out = eval_console(
        r#"
        try { new Iterator(); } catch (e) { console.log(e instanceof TypeError); }
        try { Iterator(); } catch (e) { console.log(e instanceof TypeError); }
        class MyIt extends Iterator { next() { return { done: true }; } }
        const m = new MyIt();
        console.log(m instanceof Iterator, m instanceof MyIt);
        // Subclass instances get helper methods from Iterator.prototype.
        console.log(typeof m.map, typeof m.toArray);
        // constructor / @@toStringTag are accessor pairs: assignment on the
        // prototype itself throws, on an instance defines an own property.
        console.log(Iterator.prototype.constructor === Iterator);
        try { Iterator.prototype.constructor = 5; } catch (e) { console.log(e instanceof TypeError); }
        const inst = new MyIt();
        inst.constructor = 42;
        console.log(inst.constructor, Iterator.prototype.constructor === Iterator);
        console.log(Iterator.prototype[Symbol.toStringTag]);
        "#,
    );
    assert_eq!(
        out,
        [
            "true",
            "true",
            "true true",
            "function function",
            "true",
            "true",
            "42 true",
            "Iterator"
        ]
    );
}

#[test]
fn helper_reentrancy_and_flatmap_primitives() {
    let out = eval_console(
        r#"
        // Re-entering a helper from its own callback throws.
        let err = null;
        const h = [1,2,3].values().map(function (x) { try { h.next(); } catch (e) { err = e; } return x; });
        console.log(h.next().value, err instanceof TypeError);
        // flatMap rejects primitive mapper results (strings included).
        let closes = 0;
        function* tracked() { try { yield 1; } finally { closes++; } }
        try { tracked().flatMap(x => "ab").next(); } catch (e) { console.log(e instanceof TypeError, closes); }
        // flatMap accepts iterables and bare iterators.
        console.log([1,2].values().flatMap(x => [x, x * 10]).toArray().join(","));
        "#,
    );
    assert_eq!(out, ["1 true", "true 1", "1,10,2,20"]);
}

#[test]
fn helper_objects_have_the_spec_surface() {
    let out = eval_console(
        r#"
        const h = [1].values().map(x => x);
        console.log(Object.prototype.toString.call(h));
        const proto = Object.getPrototypeOf(h);
        // Chained helpers share one %IteratorHelperPrototype%...
        console.log(proto === Object.getPrototypeOf([1].values().filter(x => x)));
        // ...which chains to %Iterator.prototype%.
        console.log(Object.getPrototypeOf(proto) === Iterator.prototype);
        console.log(typeof proto.next, typeof proto.return);
        // Chained helpers work off the helper prototype itself.
        console.log([1,2,3,4].values().map(x => x + 1).filter(x => x % 2 === 0).toArray().join(","));
        "#,
    );
    assert_eq!(
        out,
        [
            "[object Iterator Helper]",
            "true",
            "true",
            "function function",
            "2,4"
        ]
    );
}
