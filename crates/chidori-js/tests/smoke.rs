use chidori_js::Engine;

fn run(src: &str) -> String {
    let mut e = Engine::new();
    match e.eval(src) {
        Ok(v) => e.vm.to_string_lossy(&v),
        Err(err) => format!("ERR: {err}"),
    }
}
fn console(src: &str) -> String {
    let mut e = Engine::new();
    if let Err(err) = e.eval(src) {
        return format!("ERR: {err}");
    }
    e.console().join("\n")
}

#[test]
fn arithmetic() {
    assert_eq!(run("1 + 2 * 3"), "7");
    assert_eq!(run("(1 + 2) * 3"), "9");
    assert_eq!(run("10 % 3"), "1");
    assert_eq!(run("2 ** 10"), "1024");
    assert_eq!(run("'a' + 'b' + 'c'"), "abc");
    assert_eq!(run("1 + '2'"), "12");
    assert_eq!(run("'5' * 2"), "10");
}

#[test]
fn string_growth_is_bounded() {
    // A doubling loop must hit the string-length cap and throw RangeError rather
    // than allocating without bound (sandbox heap-DoS guard). Both the `+`
    // operator and template-literal joining are capped.
    let out = run("let s='x'; for (let i=0;i<40;i++) s += s; s.length");
    assert!(out.contains("RangeError"), "expected RangeError, got: {out}");
    let out = run("let t='y'; for (let i=0;i<40;i++) t = `${t}${t}`; t.length");
    assert!(out.contains("RangeError"), "expected RangeError, got: {out}");
    // A legitimately large-but-bounded string is still fine.
    assert_eq!(run("'ab'.repeat(1000).length"), "2000");
}

#[test]
fn variables_and_scope() {
    assert_eq!(run("let x = 5; x = x + 1; x"), "6");
    assert_eq!(run("const a = 1, b = 2; a + b"), "3");
    assert_eq!(run("let x = 1; { let x = 2; } x"), "1");
    assert_eq!(run("var x = 1; { var x = 2; } x"), "2");
}

#[test]
fn functions_and_closures() {
    assert_eq!(run("function add(a, b) { return a + b; } add(3, 4)"), "7");
    assert_eq!(run("const f = (x) => x * 2; f(21)"), "42");
    assert_eq!(run("function counter() { let n = 0; return () => ++n; } const c = counter(); c(); c(); c()"), "3");
    assert_eq!(run("const fib = n => n < 2 ? n : fib(n-1) + fib(n-2); fib(10)"), "55");
}

#[test]
fn closures_in_loops() {
    assert_eq!(run("let fns = []; for (let i = 0; i < 3; i++) { fns.push(() => i); } fns[0]() + ',' + fns[1]() + ',' + fns[2]()"), "0,1,2");
}

#[test]
fn control_flow() {
    assert_eq!(run("let s = 0; for (let i = 1; i <= 10; i++) s += i; s"), "55");
    assert_eq!(run("let s = 0; let i = 0; while (i < 5) { s += i; i++; } s"), "10");
    assert_eq!(run("let x = 3; let r; if (x > 2) r = 'big'; else r = 'small'; r"), "big");
    assert_eq!(run("let s = 0; for (const x of [1,2,3,4]) s += x; s"), "10");
}

#[test]
fn objects_and_arrays() {
    assert_eq!(run("const o = { a: 1, b: 2 }; o.a + o.b"), "3");
    assert_eq!(run("const a = [1,2,3]; a.map(x => x * 2).join(',')"), "2,4,6");
    assert_eq!(run("[1,2,3,4,5].filter(x => x % 2 === 0).reduce((a,b) => a+b, 0)"), "6");
    assert_eq!(run("const { x, y } = { x: 10, y: 20 }; x + y"), "30");
    assert_eq!(run("const [a, ...rest] = [1,2,3,4]; rest.join('-')"), "2-3-4");
    assert_eq!(run("JSON.stringify({a: 1, b: [2, 3]})"), r#"{"a":1,"b":[2,3]}"#);
    assert_eq!(run("JSON.parse('{\"x\": 42}').x"), "42");
}

#[test]
fn subclassing_native_collections() {
    // `class X extends Set/Map {}` — super() initializes the exotic internal slot.
    assert_eq!(
        run("class S extends Set {} const s=new S([1,2,2,3]); s instanceof Set && s instanceof S ? s.size : -1"),
        "3"
    );
    assert_eq!(
        run("class M extends Map {} const m=new M([['a',1]]); m instanceof Map && m.get('a')"),
        "1"
    );
}

#[test]
fn regexp_d_flag_indices() {
    // The `d` flag adds `.indices` (start/end pairs) to the match result.
    assert_eq!(run("/b(c)/d.exec('abcd').indices[0].join(',')"), "1,3");
    assert_eq!(run("/b(c)/d.exec('abcd').indices[1].join(',')"), "2,3");
    assert_eq!(run("/x/.exec('x').indices"), "undefined"); // no `d` flag
    // Named groups land in `.indices.groups`.
    assert_eq!(
        run("/(?<g>c)/d.exec('abc').indices.groups.g.join(',')"),
        "2,3"
    );
}

#[test]
fn regexp_escape_static() {
    assert_eq!(run("RegExp.escape('a.b')"), "\\x61\\.b"); // leading 'a' → \x61, '.' → \.
    assert_eq!(run("RegExp.escape('.b')"), "\\.b");
    assert_eq!(run("RegExp.escape('(x)')"), "\\(x\\)");
    assert_eq!(run("RegExp.escape('a b').includes('\\\\x20')"), "true"); // space hex-escaped
    assert_eq!(run("typeof RegExp.escape"), "function");
    assert_eq!(
        run("try { RegExp.escape(42); 'no-throw' } catch(e){ e.constructor.name }"),
        "TypeError"
    );
}

#[test]
fn private_method_call_brand_checks() {
    // Calling a private method on an instance works.
    assert_eq!(
        run("class C { #m(){ return 42; } go(){ return this.#m(); } } new C().go()"),
        "42"
    );
    // Calling it on a foreign object throws a TypeError (brand check).
    assert_eq!(
        run("class C { #m(){ return 1; } probe(o){ try { o.#m(); return 'no-throw'; } catch(e){ return e.constructor.name; } } } new C().probe({})"),
        "TypeError"
    );
}

#[test]
fn classes() {
    assert_eq!(run("class Point { constructor(x, y) { this.x = x; this.y = y; } sum() { return this.x + this.y; } } new Point(3, 4).sum()"), "7");
    assert_eq!(run("class A { greet() { return 'hi'; } } class B extends A { } new B().greet()"), "hi");
    assert_eq!(run("class Animal { constructor(n) { this.name = n; } } class Dog extends Animal { constructor(n) { super(n); this.kind = 'dog'; } } const d = new Dog('Rex'); d.name + ':' + d.kind"), "Rex:dog");
}

#[test]
fn strings_and_templates() {
    assert_eq!(run("const n = 42; `the answer is ${n}`"), "the answer is 42");
    assert_eq!(run("'hello world'.split(' ').map(s => s.toUpperCase()).join('_')"), "HELLO_WORLD");
    assert_eq!(run("'abc'.repeat(3)"), "abcabcabc");
}

#[test]
fn exceptions() {
    assert_eq!(run("try { throw new Error('boom'); } catch (e) { e.message }"), "boom");
    assert_eq!(run("let r = ''; try { throw 1; } catch (e) { r = 'caught'; } finally { r += '+fin'; } r"), "caught+fin");
    assert_eq!(run("function f() { try { return 'try'; } finally { } } f()"), "try");
}

#[test]
fn finally_runs_on_nonlocal_completion() {
    // `return` through finally runs the finalizer.
    assert_eq!(
        run("let r=''; function f(){ try { return 'v'; } finally { r+='F'; } } let v=f(); r+v"),
        "Fv"
    );
    // A `return` in finally overrides the try's return.
    assert_eq!(run("function g(){ try { return 1; } finally { return 2; } } g()"), "2");
    // Nested finallys all run, innermost first, on return.
    assert_eq!(
        run("let o=''; function h(){ try { try { return 'z'; } finally { o+='IN '; } } finally { o+='OUT'; } } let z=h(); o+'|'+z"),
        "IN OUT|z"
    );
    // `break` through finally runs it.
    assert_eq!(
        run("let r=''; for (let i=0;i<3;i++){ try { if(i===1) break; r+='b'+i; } finally { r+='F'+i; } } r"),
        "b0F0F1"
    );
    // `continue` through finally runs it but keeps looping.
    assert_eq!(
        run("let r=''; for (let i=0;i<3;i++){ try { if(i===1) continue; r+='x'+i; } finally { r+='C'+i; } } r"),
        "x0C0C1x2C2"
    );
    // Exception through finally still propagates to an outer catch.
    assert_eq!(
        run("let r=''; try { try { throw 'e'; } finally { r+='F'; } } catch(e){ r+='C'+e; } r"),
        "FCe"
    );
}

#[test]
fn array_destructuring_closes_iterator() {
    // Pattern with fewer targets than the iterable yields → IteratorClose runs.
    assert_eq!(
        run("let log=''; const it={ [Symbol.iterator](){ let n=0; return { next(){ return {value:n++,done:false}; }, return(){ log+='R'; return {done:true}; } }; } }; \
             const [a,b]=it; log+':'+a+b"),
        "R:01"
    );
    // A throw during a default expression closes the iterator (the iterator
    // yields `undefined` so the default actually evaluates).
    assert_eq!(
        run("let log=''; const it={ [Symbol.iterator](){ return { next(){ return {value:undefined,done:false}; }, return(){ log+='R'; return {done:true}; } }; } }; \
             try { const [x, y = (()=>{throw 'e'})()] = it; } catch(e){ log+='C'+e; } log"),
        "RCe"
    );
    // A rest element consumes to done → no close.
    assert_eq!(
        run("let log=''; const it={ [Symbol.iterator](){ let n=0; return { next(){ return n<2?{value:n++,done:false}:{value:undefined,done:true}; }, return(){ log+='R'; return {done:true}; } }; } }; \
             const [first, ...rest]=it; log+':'+first+':'+rest.join(',')"),
        ":0:1"
    );
    // Assignment-form destructuring (and `for ([a] of …)`) also closes.
    assert_eq!(
        run("let log=''; let a,b; const it={ [Symbol.iterator](){ let n=0; return { next(){ return {value:n++,done:false}; }, return(){ log+='R'; return {done:true}; } }; } }; \
             [a,b]=it; log+':'+a+b"),
        "R:01"
    );
}

#[test]
fn generator_return_runs_finally() {
    // `.return()` on a generator suspended inside try/finally runs the finalizer.
    assert_eq!(
        run("let log=''; function* g(){ try { yield 1; yield 2; } finally { log+='F'; } } \
             const it=g(); it.next(); const r=it.return(99); log+':'+r.value+':'+r.done"),
        "F:99:true"
    );
    // A `yield` in the finally traps the return (re-suspends), then completes.
    assert_eq!(
        run("function* g(){ try { yield 1; } finally { yield 2; } } \
             const it=g(); it.next(); const a=it.return(7); const b=it.next(); \
             a.value+','+a.done+'|'+b.value+','+b.done"),
        "2,false|7,true"
    );
    // `.return()` before the body starts just completes (no finally to run).
    assert_eq!(
        run("let log=''; function* g(){ try { yield 1; } finally { log+='F'; } } \
             const it=g(); const r=it.return(5); log+':'+r.value+':'+r.done"),
        ":5:true"
    );
}

#[test]
fn for_of_closes_iterator_on_abrupt_exit() {
    // `break` out of for-of calls the iterator's return().
    assert_eq!(
        run(
            "let log=''; const it={ [Symbol.iterator](){ let n=0; return { next(){ return {value:n++,done:false}; }, return(){ log+='R'; return {done:true}; } }; } }; \
             for (const x of it){ log+=x; if(x===1) break; } log"
        ),
        "01R"
    );
    // Normal exhaustion does NOT call return() (iterator closed itself).
    assert_eq!(
        run(
            "let log=''; const it={ [Symbol.iterator](){ let n=0; return { next(){ return n<2?{value:n++,done:false}:{value:undefined,done:true}; }, return(){ log+='R'; return {done:true}; } }; } }; \
             for (const x of it){ log+=x; } log"
        ),
        "01"
    );
    // `return` out of for-of closes the iterator.
    assert_eq!(
        run(
            "let log=''; const it={ [Symbol.iterator](){ let n=0; return { next(){ return {value:n++,done:false}; }, return(){ log+='R'; return {done:true}; } }; } }; \
             function f(){ for (const x of it){ log+=x; if(x===0) return; } } f(); log"
        ),
        "0R"
    );
}

#[test]
fn higher_order_and_spread() {
    assert_eq!(run("Math.max(...[3, 1, 4, 1, 5, 9, 2, 6])"), "9");
    assert_eq!(run("const o = {...{a:1}, ...{b:2}}; JSON.stringify(o)"), r#"{"a":1,"b":2}"#);
    assert_eq!(run("[...[1,2], ...[3,4]].join(',')"), "1,2,3,4");
}

#[test]
fn console_output() {
    assert_eq!(console("console.log('hello', 42, true)"), "hello 42 true");
    assert_eq!(console("console.log([1, 2, 3])"), "[ 1, 2, 3 ]");
    assert_eq!(console("console.log({a: 1, b: 'two'})"), "{ a: 1, b: 'two' }");
}
