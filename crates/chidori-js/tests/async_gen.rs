use chidori_js::Engine;

fn console(src: &str) -> String {
    let mut e = Engine::new();
    if let Err(err) = e.eval(src) { return format!("ERR: {err}"); }
    // Eval already drains microtasks once; drain again for chained reactions.
    let _ = e.vm.run_jobs_until_blocked();
    e.console().join("\n")
}

#[test]
fn generators() {
    assert_eq!(console("function* g(){ yield 1; yield 2; yield 3; } console.log([...g()].join(','))"), "1,2,3");
    assert_eq!(console("function* g(){ let x = yield 1; console.log('got', x); yield x * 2; } const it = g(); console.log(it.next().value); console.log(it.next(10).value);"), "1\ngot 10\n20");
    assert_eq!(console("function* nums(){ for (let i=0;i<3;i++) yield i; } let s=0; for (const n of nums()) s+=n; console.log(s)"), "3");
}

#[test]
fn promises() {
    assert_eq!(console("Promise.resolve(42).then(v => console.log(v))"), "42");
    assert_eq!(console("Promise.reject('boom').catch(e => console.log('caught', e))"), "caught boom");
    assert_eq!(console("Promise.all([Promise.resolve(1), Promise.resolve(2), 3]).then(a => console.log(a.join(',')))"), "1,2,3");
    assert_eq!(console("Promise.resolve(1).then(x => x + 1).then(x => x * 10).then(x => console.log(x))"), "20");
}

#[test]
fn async_await() {
    assert_eq!(console("async function f(){ return 1 + await Promise.resolve(2); } f().then(v => console.log(v))"), "3");
    assert_eq!(console(r#"
        async function chain() {
            const a = await Promise.resolve(10);
            const b = await Promise.resolve(20);
            return a + b;
        }
        chain().then(v => console.log('sum', v));
    "#), "sum 30");
    assert_eq!(console(r#"
        async function f() {
            try { await Promise.reject(new Error('nope')); }
            catch (e) { return 'handled: ' + e.message; }
        }
        f().then(v => console.log(v));
    "#), "handled: nope");
}

#[test]
fn microtask_ordering() {
    assert_eq!(console(r#"
        console.log('start');
        Promise.resolve().then(() => console.log('microtask'));
        console.log('end');
    "#), "start\nend\nmicrotask");
}
