//! A runnable demonstration of the deterministic virtual DOM.
//!
//! Run with:  `cargo run -p chidori-js --example dom_session`
//!
//! It builds a tiny counter UI in DOM-shaped JavaScript, drives it with a few
//! synthetic click events, and prints the two journals that fall out of the
//! design: the mutation stream (the render protocol / output journal) and the
//! event stream (the input journal). Then it replays the event journal against a
//! freshly built document and shows the rendered HTML is identical — the
//! record/replay property that powers time-travel and fork-and-edit-rerun.

use chidori_js::Engine;

const APP: &str = r#"
    globalThis.count = 0;
    const btn = document.createElement('button');
    btn.id = 'btn';
    btn.textContent = 'increment';
    document.body.appendChild(btn);

    const label = document.createElement('div');
    label.id = 'label';
    label.textContent = 'count: 0';
    document.body.appendChild(label);

    btn.addEventListener('click', () => {
        globalThis.count += 1;
        document.getElementById('label').textContent = 'count: ' + globalThis.count;
    });
"#;

fn main() {
    // --- Record a session -------------------------------------------------
    let mut engine = Engine::new();
    let dom = engine.install_dom();
    engine.eval(APP).expect("app evaluates");

    let btn = dom.element_by_id("btn").expect("button");
    for _ in 0..3 {
        dom.dispatch_event(&mut engine.vm, btn, "click", serde_json::json!({}))
            .expect("click");
    }

    println!("=== Mutation journal (render protocol) ===");
    for m in dom.mutations() {
        println!("  {}", serde_json::to_string(&m).unwrap());
    }

    println!("\n=== Event journal (input) ===");
    let events = dom.events();
    for e in &events {
        println!("  {}", serde_json::to_string(e).unwrap());
    }

    let recorded_html = dom.render_html();
    println!("\n=== Rendered HTML after recording ===\n  {recorded_html}");

    // --- Replay the event journal into a fresh document -------------------
    let mut engine2 = Engine::new();
    let dom2 = engine2.install_dom();
    engine2.eval(APP).expect("app evaluates");
    dom2.replay_events(&mut engine2.vm, &events).expect("replay");
    let replayed_html = dom2.render_html();

    println!("\n=== Rendered HTML after replay ===\n  {replayed_html}");
    println!(
        "\nrecord == replay : {}",
        if recorded_html == replayed_html { "YES ✓" } else { "NO ✗" }
    );
}
