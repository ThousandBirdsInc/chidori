//! Proves the headline capability: real React 18 + react-dom/server execute on
//! the pure-Rust engine, and their output mounts into the journaled DOM where it
//! can be tested with ordinary DOM queries. The bundles are vendored under
//! `examples/react_assets/`.

use chidori_js::dom::DomHandle;
use chidori_js::Engine;

#[must_use]
fn load_react(engine: &mut Engine) -> DomHandle {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/react_assets");
    let react = std::fs::read_to_string(format!("{dir}/react.js")).expect("react.js vendored");
    let server =
        std::fs::read_to_string(format!("{dir}/react-dom-server.js")).expect("server vendored");
    let dom = engine.install_dom();
    // UMD bundles resolve the global via `self` / `global`.
    engine.eval("globalThis.self=globalThis; globalThis.global=globalThis;").unwrap();
    engine.eval(&react).expect("react evaluates");
    engine.eval(&server).expect("react-dom/server evaluates");
    dom
}

#[test]
fn react_globals_are_present() {
    let mut e = Engine::new();
    let _dom = load_react(&mut e);
    let ce = e.eval("typeof React.createElement").unwrap();
    assert_eq!(e.vm.to_string_lossy(&ce), "function");
    let rs = e.eval("typeof ReactDOMServer.renderToStaticMarkup").unwrap();
    assert_eq!(e.vm.to_string_lossy(&rs), "function");
}

#[test]
fn react_renders_function_components_to_markup() {
    let mut e = Engine::new();
    let _dom = load_react(&mut e);
    let out = e
        .eval(
            r#"
            const h = React.createElement;
            function Item({ text }) { return h('li', { className: 'feat' }, text); }
            function Card() {
                return h('div', { className: 'card' },
                    h('h2', null, 'Pro'),
                    h('ul', null,
                        h(Item, { text: 'A' }),
                        h(Item, { text: 'B' }),
                        h(Item, { text: 'C' })));
            }
            ReactDOMServer.renderToStaticMarkup(h(Card));
        "#,
        )
        .unwrap();
    assert_eq!(
        e.vm.to_string_lossy(&out),
        "<div class=\"card\"><h2>Pro</h2><ul>\
         <li class=\"feat\">A</li><li class=\"feat\">B</li><li class=\"feat\">C</li></ul></div>"
    );
}

#[test]
fn react_output_mounts_into_dom_and_is_testable() {
    // The agent's loop: render React → mount into the journaled DOM → assert via
    // DOM queries (this is exactly what the react_agent_demo does).
    let mut e = Engine::new();
    let _dom = load_react(&mut e);
    let out = e
        .eval(
            r#"
            const h = React.createElement;
            function App() {
                return h('div', { className: 'card' },
                    h('h2', null, 'Pro'),
                    h('div', { className: 'price' }, '$29/mo'),
                    h('ul', null, h('li', null, 'A'), h('li', null, 'B'), h('li', null, 'C')),
                    h('button', null, 'Subscribe'));
            }
            const root = document.createElement('div');
            document.body.appendChild(root);
            root.innerHTML = ReactDOMServer.renderToStaticMarkup(h(App));
            const txt = root.textContent;
            [
                root.querySelector('h2').textContent,
                /\$\d/.test(txt),
                root.querySelectorAll('li').length,
                root.querySelector('button').textContent === 'Subscribe',
            ].join('|')
        "#,
        )
        .unwrap();
    assert_eq!(e.vm.to_string_lossy(&out), "Pro|true|3|true");
}

#[test]
fn use_state_initial_render_works_in_ssr() {
    // Hooks run on the server for the initial render (no updates, but state init
    // and the returned tree must render).
    let mut e = Engine::new();
    let _dom = load_react(&mut e);
    let out = e
        .eval(
            r#"
            const h = React.createElement;
            function Counter() {
                const [n] = React.useState(7);
                return h('span', null, 'count: ' + n);
            }
            ReactDOMServer.renderToStaticMarkup(h(Counter));
        "#,
        )
        .unwrap();
    assert_eq!(e.vm.to_string_lossy(&out), "<span>count: 7</span>");
}
