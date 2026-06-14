//! Integration: the DOM and the agent's host effects share ONE causal journal.
//!
//! This wires `install_dom()` alongside `install_chidori_effects(dispatch)` — the
//! engine's existing durable seam — and routes DOM render batches and UI events
//! through the *same* dispatcher that handles `prompt` / `tool`. The result is a
//! single ordered log interleaving the agent's reasoning, the UI mutations it
//! produced, and the user input that came back: the "agent and interface share
//! one journal" property, demonstrated end to end.

use chidori_js::Engine;
use serde_json::json;
use std::cell::RefCell;
use std::rc::Rc;

type Dispatch = Rc<dyn Fn(&str, &serde_json::Value) -> Result<serde_json::Value, String>>;

#[test]
fn agent_effects_and_ui_share_one_journal() {
    // The single causal log for everything the agent does.
    let journal: Rc<RefCell<Vec<(String, serde_json::Value)>>> = Rc::new(RefCell::new(Vec::new()));

    let mut engine = Engine::new();
    let dom = engine.install_dom();

    // One host dispatcher records every effect and serves a canned "LLM" answer.
    let j = journal.clone();
    let dispatch: Dispatch = Rc::new(move |effect: &str, args: &serde_json::Value| {
        j.borrow_mut().push((effect.to_string(), args.clone()));
        match effect {
            // The "model" decides the button's color.
            "prompt" => Ok(json!("crimson")),
            _ => Ok(serde_json::Value::Null),
        }
    });
    engine.install_chidori_effects(dispatch.clone());

    // Agent body: ask the model, then build UI reflecting the answer. At this
    // engine seam the dispatcher answers synchronously, so no await is needed.
    engine
        .eval(
            r#"
            const color = chidori.prompt('pick a button color');
            const btn = document.createElement('button');
            btn.id = 'btn';
            btn.setAttribute('data-color', color);
            btn.textContent = 'Click me';
            document.body.appendChild(btn);
            btn.addEventListener('click', () => chidori.log('clicked'));
        "#,
        )
        .unwrap();

    // Flush the produced UI mutations through the SAME host boundary (the render
    // effect), then deliver a user click as a captured input through it too.
    let batch = dom.drain_mutations();
    dispatch("dom_render", &serde_json::to_value(&batch).unwrap()).unwrap();

    let btn = dom.element_by_id("btn").unwrap();
    dispatch("dom_event", &json!({ "target": btn, "type": "click" })).unwrap();
    dom.dispatch_event(&mut engine.vm, btn, "click", json!({}))
        .unwrap();

    // ---- assertions: one journal, causally ordered ----
    let log = journal.borrow();
    let effects: Vec<&str> = log.iter().map(|(e, _)| e.as_str()).collect();

    // The model was consulted first; the render of its decision came after; the
    // user event after that; and the click handler's own effect (log) last.
    assert_eq!(effects, vec!["prompt", "dom_render", "dom_event", "log"]);

    // The model's decision flowed into the DOM: the render batch carries the
    // setAttribute with the color the prompt returned.
    let (_, render_args) = log.iter().find(|(e, _)| e == "dom_render").unwrap();
    let render_str = render_args.to_string();
    assert!(
        render_str.contains("data-color") && render_str.contains("crimson"),
        "render batch should carry the model's decision: {render_str}"
    );

    // And the handler fired through the shared boundary.
    let (_, log_args) = log.iter().find(|(e, _)| e == "log").unwrap();
    assert_eq!(
        log_args.get("message").and_then(|m| m.as_str()),
        Some("clicked")
    );
}
