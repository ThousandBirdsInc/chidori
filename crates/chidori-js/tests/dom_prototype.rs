//! Prototype tests for the deterministic virtual DOM (`chidori_js::dom`).
//!
//! These exercise the two properties that make the "DOM behind the host
//! boundary" idea interesting:
//!   1. The mutation stream is a faithful, deterministic render journal.
//!   2. Replaying the recorded event journal reproduces the mutation stream and
//!      rendered HTML byte-for-byte — the basis for time-travel / fork-rerun.

use chidori_js::dom::{DomHandle, EventRecord, Mutation};
use chidori_js::Engine;

/// A self-contained counter UI: a button whose click handler increments a
/// global counter and writes it into a `<div id="label">`. State lives in
/// `globalThis` and the label is re-fetched via `getElementById`, so the test
/// depends on nothing but the DOM + event semantics.
const COUNTER_APP: &str = r#"
    globalThis.count = 0;
    const btn = document.createElement('button');
    btn.id = 'btn';
    btn.textContent = 'increment';
    document.body.appendChild(btn);

    const label = document.createElement('div');
    label.id = 'label';
    label.className = 'count';
    label.textContent = '0';
    document.body.appendChild(label);

    btn.addEventListener('click', () => {
        globalThis.count += 1;
        document.getElementById('label').textContent = String(globalThis.count);
    });
"#;

/// Build the counter app in a fresh engine, click it `clicks` times, and return
/// the resulting (mutation journal, event journal, rendered HTML).
fn run_counter(clicks: usize) -> (Vec<Mutation>, Vec<EventRecord>, String) {
    let mut engine = Engine::new();
    let dom: DomHandle = engine.install_dom();
    engine
        .eval(COUNTER_APP)
        .expect("counter app should evaluate");

    let btn = dom.element_by_id("btn").expect("button should exist");
    for _ in 0..clicks {
        dom.dispatch_event(&mut engine.vm, btn, "click", serde_json::json!({}))
            .expect("click should dispatch");
    }
    (dom.mutations(), dom.events(), dom.render_html())
}

#[test]
fn builds_dom_and_renders() {
    let (_muts, _events, html) = run_counter(0);
    assert_eq!(
        html,
        "<html><body><button id=\"btn\">increment</button>\
         <div id=\"label\" class=\"count\">0</div></body></html>"
    );
}

#[test]
fn clicks_update_rendered_state() {
    let (_muts, events, html) = run_counter(3);
    assert!(
        html.contains("<div id=\"label\" class=\"count\">3</div>"),
        "got: {html}"
    );
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].ty, "click");
}

#[test]
fn mutation_stream_is_deterministic() {
    // Two independent runs of the same program + inputs produce byte-identical
    // mutation journals and HTML. This is the property the whole design rests on.
    let (m1, e1, h1) = run_counter(5);
    let (m2, e2, h2) = run_counter(5);
    assert_eq!(m1, m2, "mutation journals diverged across identical runs");
    assert_eq!(e1, e2, "event journals diverged");
    assert_eq!(h1, h2, "rendered HTML diverged");
}

#[test]
fn replaying_event_journal_reproduces_state() {
    // Record a session, then replay just its event journal against a freshly
    // built DOM. The replayed mutation journal must match the recorded one.
    let (recorded_muts, recorded_events, recorded_html) = run_counter(4);

    let mut engine = Engine::new();
    let dom = engine.install_dom();
    engine.eval(COUNTER_APP).unwrap();
    dom.replay_events(&mut engine.vm, &recorded_events).unwrap();

    assert_eq!(
        dom.mutations(),
        recorded_muts,
        "replay diverged from record"
    );
    assert_eq!(dom.render_html(), recorded_html);
}

#[test]
fn prefix_replay_is_a_time_machine() {
    // Replaying the first k events lands exactly on the state the live session
    // had after k clicks — the time-travel / fork-at-step-k primitive.
    let (_full_muts, full_events, _) = run_counter(6);
    let (state_at_2, _, html_at_2) = run_counter(2);

    let mut engine = Engine::new();
    let dom = engine.install_dom();
    engine.eval(COUNTER_APP).unwrap();
    dom.replay_events(&mut engine.vm, &full_events[..2])
        .unwrap();

    assert_eq!(dom.mutations(), state_at_2);
    assert_eq!(dom.render_html(), html_at_2);
}

#[test]
fn event_journal_serializes_round_trip() {
    // The journals are plain serde types: a recorded session is persistable JSON.
    let (muts, events, _) = run_counter(2);
    let muts_json = serde_json::to_string(&muts).unwrap();
    let events_json = serde_json::to_string(&events).unwrap();
    let muts_back: Vec<Mutation> = serde_json::from_str(&muts_json).unwrap();
    let events_back: Vec<EventRecord> = serde_json::from_str(&events_json).unwrap();
    assert_eq!(muts, muts_back);
    assert_eq!(events, events_back);
    // Spot-check the wire shape of a mutation.
    assert!(muts_json.contains("\"op\":\"create\""), "got: {muts_json}");
}

#[test]
fn dom_mutation_methods_record_and_query() {
    // Exercise attributes, insertBefore, removeChild, textContent get, parentNode.
    let mut engine = Engine::new();
    let dom = engine.install_dom();
    let out = engine
        .eval(
            r#"
            const list = document.createElement('ul');
            list.setAttribute('role', 'list');
            document.body.appendChild(list);
            const a = document.createElement('li'); a.textContent = 'a';
            const b = document.createElement('li'); b.textContent = 'b';
            list.appendChild(b);
            list.insertBefore(a, b);     // a before b
            const c = document.createElement('li'); c.textContent = 'c';
            list.appendChild(c);
            list.removeChild(b);          // drop the middle-ish one
            // assertions evaluated to a result string:
            [list.childNodes.length, a.parentNode === list, list.getAttribute('role'),
             document.body.textContent].join('|')
        "#,
        )
        .unwrap();
    assert_eq!(engine.vm.to_string_lossy(&out), "2|true|list|ac");
    assert_eq!(
        dom.render_html(),
        "<html><body><ul role=\"list\"><li>a</li><li>c</li></ul></body></html>"
    );
}

#[test]
fn event_detail_reaches_handler() {
    // The detail payload crosses the host→JS boundary intact (captured input).
    let mut engine = Engine::new();
    let dom = engine.install_dom();
    engine
        .eval(
            r#"
            globalThis.last = null;
            const input = document.createElement('input');
            input.id = 'in';
            document.body.appendChild(input);
            input.addEventListener('input', (e) => { globalThis.last = e.detail.value; });
        "#,
        )
        .unwrap();
    let input = dom.element_by_id("in").unwrap();
    dom.dispatch_event(
        &mut engine.vm,
        input,
        "input",
        serde_json::json!({"value": "hello"}),
    )
    .unwrap();
    let last = engine.eval("globalThis.last").unwrap();
    assert_eq!(engine.vm.to_string_lossy(&last), "hello");
}

#[test]
fn events_bubble_to_ancestor_listeners() {
    let mut engine = Engine::new();
    let dom = engine.install_dom();
    engine
        .eval(
            r#"
            globalThis.hits = [];
            const outer = document.createElement('div'); outer.id = 'outer';
            const inner = document.createElement('button'); inner.id = 'inner';
            outer.appendChild(inner);
            document.body.appendChild(outer);
            outer.addEventListener('click', () => globalThis.hits.push('outer'));
            inner.addEventListener('click', () => globalThis.hits.push('inner'));
        "#,
        )
        .unwrap();
    let inner = dom.element_by_id("inner").unwrap();
    dom.dispatch_event(&mut engine.vm, inner, "click", serde_json::json!({}))
        .unwrap();
    let hits = engine.eval("globalThis.hits.join(',')").unwrap();
    // target first, then bubble to ancestor.
    assert_eq!(engine.vm.to_string_lossy(&hits), "inner,outer");
}
