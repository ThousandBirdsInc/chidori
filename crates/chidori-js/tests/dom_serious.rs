//! Tests for the serious DOM implementation: full W3C event dispatch, captured
//! measurement record/replay, the broader DOM API surface, and the no-leak
//! lifetime guarantee.

use chidori_js::dom::{MeasuredNode, MeasurementProvider, SessionJournal};
use chidori_js::Engine;
use std::rc::Rc;

fn eval_str(engine: &mut Engine, src: &str) -> String {
    let v = engine.eval(src).expect("eval");
    engine.vm.to_string_lossy(&v)
}

// ---------------------------------------------------------------------------
// Lifetime / no-leak (the GC gap)
// ---------------------------------------------------------------------------

#[test]
fn document_arena_is_not_leaked_into_a_cycle() {
    let mut engine = Engine::new();
    let dom = engine.install_dom();
    // Build a real tree with many cached wrappers, listeners, and a global ref.
    engine
        .eval(
            r#"
            globalThis.kept = [];
            for (let i = 0; i < 25; i++) {
                const el = document.createElement('div');
                el.id = 'n' + i;
                el.textContent = 'node ' + i;
                el.addEventListener('click', () => {});
                document.body.appendChild(el);
                globalThis.kept.push(el);   // keep JS-side references alive
            }
        "#,
        )
        .unwrap();
    // The wrapper closures hold Weak refs, so despite all those live wrappers
    // (referenced from JS *and* cached on the nodes) the only strong owner of
    // the arena is this handle. If closures captured a strong Rc, this would be
    // far greater than 1.
    assert_eq!(dom.strong_count(), 1, "arena leaked strong references");
}

// ---------------------------------------------------------------------------
// Full event model
// ---------------------------------------------------------------------------

fn three_level_app(engine: &mut Engine) -> chidori_js::dom::DomHandle {
    let dom = engine.install_dom();
    engine
        .eval(
            r#"
            globalThis.hits = [];
            const outer = document.createElement('div'); outer.id = 'outer';
            const inner = document.createElement('button'); inner.id = 'inner';
            outer.appendChild(inner);
            document.body.appendChild(outer);
        "#,
        )
        .unwrap();
    dom
}

#[test]
fn capture_target_bubble_ordering() {
    let mut engine = Engine::new();
    let dom = three_level_app(&mut engine);
    engine
        .eval(
            r#"
            const outer = document.getElementById('outer');
            const inner = document.getElementById('inner');
            outer.addEventListener('click', () => hits.push('outer-capture'), true);
            outer.addEventListener('click', () => hits.push('outer-bubble'));
            inner.addEventListener('click', () => hits.push('inner-capture'), { capture: true });
            inner.addEventListener('click', () => hits.push('inner-bubble'));
        "#,
        )
        .unwrap();
    let inner = dom.element_by_id("inner").unwrap();
    dom.dispatch_event(&mut engine.vm, inner, "click", serde_json::json!({}))
        .unwrap();
    // Capture descends to target; at target both listeners fire in registration
    // order; then bubble ascends.
    assert_eq!(
        eval_str(&mut engine, "hits.join(',')"),
        "outer-capture,inner-capture,inner-bubble,outer-bubble"
    );
}

#[test]
fn stop_propagation_halts_bubble() {
    let mut engine = Engine::new();
    let dom = three_level_app(&mut engine);
    engine
        .eval(
            r#"
            const outer = document.getElementById('outer');
            const inner = document.getElementById('inner');
            outer.addEventListener('click', () => hits.push('outer-capture'), true);
            outer.addEventListener('click', () => hits.push('outer-bubble'));
            inner.addEventListener('click', (e) => { hits.push('inner'); e.stopPropagation(); });
        "#,
        )
        .unwrap();
    let inner = dom.element_by_id("inner").unwrap();
    dom.dispatch_event(&mut engine.vm, inner, "click", serde_json::json!({}))
        .unwrap();
    assert_eq!(
        eval_str(&mut engine, "hits.join(',')"),
        "outer-capture,inner"
    );
}

#[test]
fn stop_immediate_propagation_halts_remaining_listeners() {
    let mut engine = Engine::new();
    let dom = three_level_app(&mut engine);
    engine
        .eval(
            r#"
            const inner = document.getElementById('inner');
            inner.addEventListener('click', (e) => { hits.push('first'); e.stopImmediatePropagation(); });
            inner.addEventListener('click', () => hits.push('second'));
        "#,
        )
        .unwrap();
    let inner = dom.element_by_id("inner").unwrap();
    dom.dispatch_event(&mut engine.vm, inner, "click", serde_json::json!({}))
        .unwrap();
    assert_eq!(eval_str(&mut engine, "hits.join(',')"), "first");
}

#[test]
fn prevent_default_is_reported() {
    let mut engine = Engine::new();
    let dom = three_level_app(&mut engine);
    engine
        .eval(
            r#"
            const inner = document.getElementById('inner');
            inner.addEventListener('click', (e) => { e.preventDefault(); });
        "#,
        )
        .unwrap();
    let inner = dom.element_by_id("inner").unwrap();
    let not_cancelled = dom
        .dispatch_event(&mut engine.vm, inner, "click", serde_json::json!({}))
        .unwrap();
    assert!(
        !not_cancelled,
        "dispatch should report the default was prevented"
    );
}

#[test]
fn once_listener_fires_a_single_time() {
    let mut engine = Engine::new();
    let dom = three_level_app(&mut engine);
    engine
        .eval(
            r#"
            const inner = document.getElementById('inner');
            inner.addEventListener('click', () => hits.push('x'), { once: true });
        "#,
        )
        .unwrap();
    let inner = dom.element_by_id("inner").unwrap();
    dom.dispatch_event(&mut engine.vm, inner, "click", serde_json::json!({}))
        .unwrap();
    dom.dispatch_event(&mut engine.vm, inner, "click", serde_json::json!({}))
        .unwrap();
    assert_eq!(eval_str(&mut engine, "hits.join(',')"), "x");
}

#[test]
fn remove_event_listener_works() {
    let mut engine = Engine::new();
    let dom = three_level_app(&mut engine);
    engine
        .eval(
            r#"
            const inner = document.getElementById('inner');
            globalThis.h = () => hits.push('x');
            inner.addEventListener('click', globalThis.h);
            inner.removeEventListener('click', globalThis.h);
        "#,
        )
        .unwrap();
    let inner = dom.element_by_id("inner").unwrap();
    dom.dispatch_event(&mut engine.vm, inner, "click", serde_json::json!({}))
        .unwrap();
    assert_eq!(eval_str(&mut engine, "hits.length"), "0");
}

#[test]
fn js_side_dispatch_event_returns_not_cancelled() {
    let mut engine = Engine::new();
    let _dom = three_level_app(&mut engine);
    let out = eval_str(
        &mut engine,
        r#"
        const inner = document.getElementById('inner');
        inner.addEventListener('click', (e) => e.preventDefault());
        const ev = { type: 'click', detail: {}, bubbles: true, cancelable: true };
        inner.dispatchEvent(ev);   // -> false because preventDefault was called
        "#,
    );
    assert_eq!(out, "false");
}

// ---------------------------------------------------------------------------
// Captured measurement reads (record / replay)
// ---------------------------------------------------------------------------

/// A deterministic fake layout engine: width is proportional to text length.
struct FakeLayout;
impl MeasurementProvider for FakeLayout {
    fn measure(&self, kind: &str, node: &MeasuredNode) -> serde_json::Value {
        let w = node.text.chars().count() as i64 * 7;
        match kind {
            "getBoundingClientRect" => serde_json::json!({
                "x": 0, "y": 0, "width": w, "height": 16,
                "top": 0, "left": 0, "right": w, "bottom": 16
            }),
            _ => serde_json::json!(w),
        }
    }
}

const MEASURE_APP: &str = r#"
    const box = document.createElement('div');
    box.id = 'box';
    box.textContent = 'hello world';   // 11 chars -> width 77
    document.body.appendChild(box);
    globalThis.w = box.offsetWidth;
    globalThis.rectW = box.getBoundingClientRect().width;
"#;

#[test]
fn measurements_are_captured_in_record_mode() {
    let mut engine = Engine::new();
    let dom = engine.install_dom();
    dom.set_measurement_provider(Rc::new(FakeLayout));
    engine.eval(MEASURE_APP).unwrap();

    assert_eq!(eval_str(&mut engine, "String(w)"), "77");
    assert_eq!(eval_str(&mut engine, "String(rectW)"), "77");

    let journal = dom.journal();
    assert_eq!(journal.measurements.len(), 2, "two captured reads expected");
    assert!(journal.measurements.iter().any(|m| m.kind == "offsetWidth"));
    assert!(journal
        .measurements
        .iter()
        .any(|m| m.kind == "getBoundingClientRect"));
}

#[test]
fn measurements_replay_from_journal_without_a_provider() {
    // Record.
    let mut e1 = Engine::new();
    let d1 = e1.install_dom();
    d1.set_measurement_provider(Rc::new(FakeLayout));
    e1.eval(MEASURE_APP).unwrap();
    let recorded_w = eval_str(&mut e1, "String(w)");
    let journal = d1.journal();

    // Replay: no provider installed; values must come from the journal, keyed by
    // the (deterministic) node id + kind + seq.
    let mut e2 = Engine::new();
    let d2 = e2.install_dom();
    d2.load_journal_for_replay(&journal);
    e2.eval(MEASURE_APP).unwrap();
    let replayed_w = eval_str(&mut e2, "String(w)");

    assert_eq!(recorded_w, "77");
    assert_eq!(replayed_w, recorded_w);
    // In replay mode nothing new is journaled.
    assert!(d2.journal().measurements.is_empty());
}

#[test]
fn full_session_journal_round_trips_as_json() {
    let mut e = Engine::new();
    let dom = e.install_dom();
    dom.set_measurement_provider(Rc::new(FakeLayout));
    e.eval(MEASURE_APP).unwrap();
    let btn = {
        // add an event too so the journal has both kinds
        e.eval(
            "const b = document.createElement('button'); b.id='b'; document.body.appendChild(b);",
        )
        .unwrap();
        dom.element_by_id("b").unwrap()
    };
    e.eval("document.getElementById('b').addEventListener('click', () => {});")
        .unwrap();
    dom.dispatch_event(&mut e.vm, btn, "click", serde_json::json!({"k": 1}))
        .unwrap();

    let journal = dom.journal();
    let json = serde_json::to_string(&journal).unwrap();
    let back: SessionJournal = serde_json::from_str(&json).unwrap();
    assert_eq!(journal, back);
    assert_eq!(back.events.len(), 1);
    assert_eq!(back.measurements.len(), 2);
}

// ---------------------------------------------------------------------------
// Broader DOM API surface
// ---------------------------------------------------------------------------

#[test]
fn class_list_operations() {
    let mut engine = Engine::new();
    let _dom = engine.install_dom();
    let out = eval_str(
        &mut engine,
        r#"
        const el = document.createElement('div');
        el.classList.add('a');
        el.classList.add('b');
        el.classList.add('a');           // no-op dup
        el.classList.toggle('c');        // add -> true
        el.classList.toggle('b');        // remove -> false
        el.classList.remove('a');
        [el.className, el.classList.contains('c'), el.classList.contains('a')].join('|')
        "#,
    );
    assert_eq!(out, "c|true|false");
}

#[test]
fn query_selectors() {
    let mut engine = Engine::new();
    let _dom = engine.install_dom();
    let out = eval_str(
        &mut engine,
        r#"
        document.body.innerHTML;  // ensure body exists
        const root = document.createElement('section');
        root.id = 'root';
        root.innerHTML;  // no-op
        document.body.appendChild(root);
        for (const t of ['alpha', 'beta', 'alpha']) {
            const p = document.createElement('p');
            p.className = t;
            p.textContent = t;
            root.appendChild(p);
        }
        const byClass = root.querySelectorAll('.alpha').length;
        const byTag = root.getElementsByTagName('p').length;
        const first = root.querySelector('p.beta').textContent;
        const byId = document.querySelector('#root').tagName;
        [byClass, byTag, first, byId].join('|')
        "#,
    );
    assert_eq!(out, "2|3|beta|SECTION");
}

#[test]
fn clone_node_deep_copies_subtree_not_listeners() {
    let mut engine = Engine::new();
    let _dom = engine.install_dom();
    let out = eval_str(
        &mut engine,
        r#"
        const tpl = document.createElement('div');
        tpl.id = 'tpl';
        const child = document.createElement('span');
        child.textContent = 'hi';
        tpl.appendChild(child);
        const deep = tpl.cloneNode(true);
        const shallow = tpl.cloneNode(false);
        [deep.childNodes.length, shallow.childNodes.length, deep.textContent].join('|')
        "#,
    );
    assert_eq!(out, "1|0|hi");
}

#[test]
fn tree_navigation_accessors() {
    let mut engine = Engine::new();
    let _dom = engine.install_dom();
    let out = eval_str(
        &mut engine,
        r#"
        const ul = document.createElement('ul');
        const a = document.createElement('li'); a.textContent = 'a';
        const b = document.createElement('li'); b.textContent = 'b';
        const c = document.createElement('li'); c.textContent = 'c';
        ul.appendChild(a); ul.appendChild(b); ul.appendChild(c);
        [
            ul.firstChild.textContent,
            ul.lastChild.textContent,
            b.nextSibling.textContent,
            b.previousSibling.textContent,
            a.nextSibling === b,
            ul.children.length,
            a.nodeType,
        ].join('|')
        "#,
    );
    assert_eq!(out, "a|c|c|a|true|3|1");
}

#[test]
fn replace_child_and_inner_outer_html() {
    let mut engine = Engine::new();
    let _dom = engine.install_dom();
    let out = eval_str(
        &mut engine,
        r#"
        const box = document.createElement('div');
        box.id = 'box';
        const a = document.createElement('b'); a.textContent = 'A';
        const c = document.createElement('i'); c.textContent = 'C';
        box.appendChild(a);
        box.replaceChild(c, a);   // c replaces a
        document.body.appendChild(box);
        box.innerHTML + '||' + box.outerHTML
        "#,
    );
    assert_eq!(out, "<i>C</i>||<div id=\"box\"><i>C</i></div>");
}

#[test]
fn hierarchy_request_error_on_cycle() {
    let mut engine = Engine::new();
    let _dom = engine.install_dom();
    // Appending an ancestor into its descendant must throw.
    let out = eval_str(
        &mut engine,
        r#"
        const a = document.createElement('div');
        const b = document.createElement('div');
        a.appendChild(b);
        let err = 'none';
        try { b.appendChild(a); } catch (e) { err = 'threw'; }
        err
        "#,
    );
    assert_eq!(out, "threw");
}

#[test]
fn inner_html_parses_into_journaled_dom_and_is_queryable() {
    let mut engine = Engine::new();
    let _dom = engine.install_dom();
    let out = eval_str(
        &mut engine,
        r#"
        const root = document.createElement('div');
        root.innerHTML =
            '<section class="card"><h2>Pro</h2><ul>' +
            '<li class="feat">A</li><li class="feat">B</li></ul>' +
            '<button>Buy &amp; go</button><img src="x"/></section>';
        document.body.appendChild(root);
        [
            root.querySelector('h2').textContent,
            root.querySelectorAll('.feat').length,
            root.querySelector('button').textContent,
            root.getElementsByTagName('img').length,
        ].join('|')
        "#,
    );
    assert_eq!(out, "Pro|2|Buy & go|1");
}

#[test]
fn advanced_css_selectors() {
    let mut engine = Engine::new();
    let _dom = engine.install_dom();
    let out = eval_str(
        &mut engine,
        r#"
        const root = document.createElement('div');
        root.innerHTML =
            '<nav><ul class="menu">' +
            '<li class="item active" data-id="1"><a href="/a">A</a></li>' +
            '<li class="item" data-id="2"><a href="/b">B</a></li>' +
            '<li class="item" data-id="3"><a href="/c">C</a></li>' +
            '</ul></nav>';
        document.body.appendChild(root);
        [
            root.querySelectorAll('ul.menu > li').length,        // child combinator
            root.querySelectorAll('nav li a').length,            // descendant chain
            root.querySelector('li.active').getAttribute('data-id'),  // compound class
            root.querySelectorAll('[data-id]').length,           // attribute exists
            root.querySelector('[data-id="2"] a').textContent,   // attr equals + descendant
            root.querySelector('li:first-child').getAttribute('data-id'),
            root.querySelector('li:last-child').getAttribute('data-id'),
            root.querySelector('li:nth-child(2)').getAttribute('data-id'),
            root.querySelectorAll('a[href^="/"]').length,        // prefix match
        ].join('|')
        "#,
    );
    assert_eq!(out, "3|3|1|3|B|1|3|2|3");
}

#[test]
fn selector_list_groups() {
    let mut engine = Engine::new();
    let _dom = engine.install_dom();
    let out = eval_str(
        &mut engine,
        r#"
        const root = document.createElement('div');
        root.innerHTML = '<h1>t</h1><h2>u</h2><p>v</p>';
        document.body.appendChild(root);
        root.querySelectorAll('h1, h2').length
        "#,
    );
    assert_eq!(out, "2");
}

#[test]
fn insert_adjacent_html_positions() {
    let mut engine = Engine::new();
    let _dom = engine.install_dom();
    let out = eval_str(
        &mut engine,
        r#"
        const box = document.createElement('div');
        box.id = 'box';
        box.innerHTML = '<span id="mid">mid</span>';
        document.body.appendChild(box);
        const mid = document.getElementById('mid');
        mid.insertAdjacentHTML('beforebegin', '<b>before</b>');
        mid.insertAdjacentHTML('afterend', '<i>after</i>');
        box.insertAdjacentHTML('afterbegin', '<u>first</u>');
        box.insertAdjacentHTML('beforeend', '<s>last</s>');
        box.innerHTML
        "#,
    );
    assert_eq!(
        out,
        "<u>first</u><b>before</b><span id=\"mid\">mid</span><i>after</i><s>last</s>"
    );
}

#[test]
fn normalize_merges_adjacent_text() {
    let mut engine = Engine::new();
    let _dom = engine.install_dom();
    let out = eval_str(
        &mut engine,
        r#"
        const p = document.createElement('p');
        p.appendChild(document.createTextNode('Hello, '));
        p.appendChild(document.createTextNode('world'));
        p.appendChild(document.createTextNode(''));
        const before = p.childNodes.length;
        p.normalize();
        [before, p.childNodes.length, p.textContent].join('|')
        "#,
    );
    assert_eq!(out, "3|1|Hello, world");
}

#[test]
fn session_journal_is_versioned() {
    use chidori_js::dom::PROTOCOL_VERSION;
    let mut engine = Engine::new();
    let dom = engine.install_dom();
    engine
        .eval("document.body.appendChild(document.createElement('div'));")
        .unwrap();
    let batch = dom.drain_render_batch();
    assert_eq!(batch.version, PROTOCOL_VERSION);
    assert!(!batch.mutations.is_empty());
    let j = dom.journal();
    assert_eq!(j.version, PROTOCOL_VERSION);
    // round-trips with the version field
    let s = serde_json::to_string(&j).unwrap();
    assert!(s.contains("\"version\""));
}
