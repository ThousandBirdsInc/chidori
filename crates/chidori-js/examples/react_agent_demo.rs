//! The killer demo: an agent iterates on a **real React** component on the
//! chidori-js runtime, gated by a DOM test suite, with fork + replay.
//!
//!   cargo run -p chidori-js --example react_agent_demo > docs/media/react_frames.json
//!
//! What's real here:
//!   * React 18 + react-dom/server execute on the pure-Rust engine.
//!   * Each draft is server-rendered, mounted into the journaled virtual DOM
//!     (`root.innerHTML = ...`), and tested via DOM queries.
//!   * The "agent" answers come through the host dispatch boundary and are
//!     journaled — so the fork run replays the prior LLM calls for FREE (0 new
//!     model calls) and only the edit costs a call.

use chidori_js::dom::DomHandle;
use chidori_js::Engine;
use serde::Serialize;
use serde_json::Value;
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Serialize, Clone)]
struct Test {
    name: String,
    pass: bool,
}

#[derive(Serialize)]
struct Frame {
    caption: String,
    note: String,
    html: String,
    html_b: Option<String>,
    tests: Vec<Test>,
    tests_b: Option<Vec<Test>>,
    model_calls: usize,
    replayed: usize,
}

/// The simulated coding agent + its journal. In record mode it "thinks" (a model
/// call) and journals the answer; in replay it serves the recorded answer for
/// free. This is the durable-host contract, at the dispatch boundary.
struct Agent {
    journal: Vec<String>,
    cursor: usize,
    replay: bool,
    calls: usize,
    replayed: usize,
}

impl Agent {
    fn new() -> Agent {
        Agent { journal: Vec::new(), cursor: 0, replay: false, calls: 0, replayed: 0 }
    }

    fn prompt(&mut self, spec: &str) -> String {
        if self.replay && self.cursor < self.journal.len() {
            let v = self.journal[self.cursor].clone();
            self.cursor += 1;
            self.replayed += 1;
            return v;
        }
        let answer = brain(spec);
        self.journal.push(answer.clone());
        self.cursor += 1;
        self.calls += 1;
        answer
    }
}

/// The agent's "model": maps a spec to the next React draft (canned, but routed
/// and journaled exactly like a real LLM call would be).
fn brain(spec: &str) -> String {
    let card = |theme: &str, title: &str, price: Option<&str>, cta: &str| -> String {
        let price_el = match price {
            Some(p) => format!("e('div',{{className:'price'}},'{p}'),"),
            None => String::new(),
        };
        format!(
            "globalThis.App=function(){{const e=React.createElement;return \
             e('div',{{className:'card {theme}'}},\
             e('h2',null,'{title}'),\
             {price_el}\
             e('ul',null,\
               e('li',null,'Unlimited projects'),\
               e('li',null,'Deterministic replay'),\
               e('li',null,'Time-travel debugging')),\
             e('button',null,'{cta}'));}};"
        )
    };
    match spec {
        "draft:0" => card("light", "Pro", None, "Buy"),
        "draft:1" => card("light", "Pro", Some("$29/mo"), "Buy now"),
        "draft:2" => card("light", "Pro", Some("$29/mo"), "Subscribe"),
        "edit" => card("dark", "Pro · Annual", Some("$290/yr"), "Subscribe"),
        _ => card("light", "Pro", Some("$29/mo"), "Subscribe"),
    }
}

fn setup(engine: &mut Engine, agent: &Rc<RefCell<Agent>>) -> DomHandle {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/react_assets");
    let react = std::fs::read_to_string(format!("{dir}/react.js")).unwrap();
    let server = std::fs::read_to_string(format!("{dir}/react-dom-server.js")).unwrap();

    let dom = engine.install_dom();
    let a = agent.clone();
    engine.install_chidori_effects(Rc::new(move |effect: &str, args: &Value| {
        if effect == "prompt" {
            let spec = args.get("text").and_then(Value::as_str).unwrap_or("");
            Ok(Value::String(a.borrow_mut().prompt(spec)))
        } else {
            Ok(Value::Null)
        }
    }));
    // React UMD reads `self`/`global` as the global object.
    engine.eval("globalThis.self=globalThis; globalThis.global=globalThis;").unwrap();
    engine.eval(&react).unwrap();
    engine.eval(&server).unwrap();
    // The agent's acceptance suite — pure DOM-query assertions on the output.
    engine
        .eval(
            r#"
            globalThis.runTests = function (root) {
                const txt = root.textContent || '';
                const btn = root.querySelector('button');
                return [
                    { name: 'renders a heading',   pass: !!root.querySelector('h2') },
                    { name: 'shows a $ price',     pass: /\$\d/.test(txt) },
                    { name: 'lists 3 features',    pass: root.querySelectorAll('li').length === 3 },
                    { name: 'CTA says “Subscribe”', pass: !!btn && btn.textContent === 'Subscribe' },
                ];
            };
            globalThis.renderApp = function () {
                let root = document.getElementById('root');
                if (!root) { root = document.createElement('div'); root.id = 'root'; document.body.appendChild(root); }
                root.innerHTML = ReactDOMServer.renderToStaticMarkup(React.createElement(globalThis.App));
                return JSON.stringify({ html: root.innerHTML, tests: runTests(root) });
            };
        "#,
        )
        .unwrap();
    dom
}

fn render_and_test(engine: &mut Engine, source: &str) -> (String, Vec<Test>) {
    // `source` is the agent's draft; run it (defines globalThis.App), then render
    // it to the journaled DOM and run the DOM-query test suite.
    engine.eval(source).unwrap();
    let v = engine.eval("renderApp()").unwrap();
    let s = engine.vm.to_string_lossy(&v);
    let parsed: Value = serde_json::from_str(&s).unwrap();
    let html = parsed["html"].as_str().unwrap_or("").to_string();
    let tests = parsed["tests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| Test {
            name: t["name"].as_str().unwrap_or("").to_string(),
            pass: t["pass"].as_bool().unwrap_or(false),
        })
        .collect();
    (html, tests)
}

fn main() {
    let agent = Rc::new(RefCell::new(Agent::new()));
    let mut frames: Vec<Frame> = Vec::new();

    // ---- Live session: the agent iterates until the suite is green. ----
    let mut engine = Engine::new();
    let _dom = setup(&mut engine, &agent);

    let mut last_green_source = String::new();
    let mut last_green_html = String::new();
    let mut last_green_tests: Vec<Test> = Vec::new();

    for k in 0..3 {
        let spec = format!("draft:{k}");
        // Ask the agent (through the host boundary) for the next draft — once.
        let source = {
            let v = engine.eval(&format!("chidori.prompt({:?})", spec)).unwrap();
            engine.vm.to_string_lossy(&v)
        };

        let (html, tests) = render_and_test(&mut engine, &source);
        let passed = tests.iter().filter(|t| t.pass).count();
        let total = tests.len();
        let calls = agent.borrow().calls;
        if passed == total {
            last_green_source = source.clone();
            last_green_html = html.clone();
            last_green_tests = tests.clone();
        }
        frames.push(Frame {
            caption: format!("Iteration {} · {}/{} tests pass", k + 1, passed, total),
            note: if passed == total {
                "all green — the agent accepts this draft".into()
            } else {
                "failing tests drive the next revision".into()
            },
            html,
            html_b: None,
            tests,
            tests_b: None,
            model_calls: calls,
            replayed: 0,
        });
    }

    // ---- Fork: a fresh engine REPLAYS the journal (free), then one edit. ----
    {
        let mut a = agent.borrow_mut();
        a.replay = true;
        a.cursor = 0;
        a.replayed = 0;
    }
    let calls_before = agent.borrow().calls;

    let mut fork = Engine::new();
    let _fdom = setup(&mut fork, &agent);
    // Replay the agent's prior drafts to reconstruct the green state — for free.
    for k in 0..3 {
        let v = fork.eval(&format!("chidori.prompt({:?})", format!("draft:{k}"))).unwrap();
        let src = fork.vm.to_string_lossy(&v);
        let _ = render_and_test(&mut fork, &src);
    }
    // The new edit: a dark, annual variant. This is the only fresh model call.
    let edit_src = {
        let v = fork.eval("chidori.prompt(\"edit\")").unwrap();
        fork.vm.to_string_lossy(&v)
    };
    let (variant_html, variant_tests) = render_and_test(&mut fork, &edit_src);
    let replayed = agent.borrow().replayed;
    let new_calls = agent.borrow().calls - calls_before;

    frames.push(Frame {
        caption: "Fork → edit → replay".into(),
        note: format!(
            "replayed {replayed} prior LLM calls for free · {new_calls} new call for the edit"
        ),
        html: last_green_html.clone(),
        html_b: Some(variant_html),
        tests: last_green_tests.clone(),
        tests_b: Some(variant_tests),
        model_calls: agent.borrow().calls,
        replayed,
    });

    // ---- Determinism: replay the journal into a fresh engine; compare. ----
    {
        let mut a = agent.borrow_mut();
        a.replay = true;
        a.cursor = 0;
    }
    let mut check = Engine::new();
    let _cdom = setup(&mut check, &agent);
    let mut check_html = String::new();
    for k in 0..3 {
        let v = check.eval(&format!("chidori.prompt({:?})", format!("draft:{k}"))).unwrap();
        let src = check.vm.to_string_lossy(&v);
        let (h, _) = render_and_test(&mut check, &src);
        check_html = h;
    }
    let identical = check_html == last_green_html && !last_green_source.is_empty();
    frames.push(Frame {
        caption: "Record == replay".into(),
        note: if identical {
            "same drafts + same journal → byte-identical UI ✓".into()
        } else {
            "MISMATCH".into()
        },
        html: check_html,
        html_b: None,
        tests: last_green_tests.clone(),
        tests_b: None,
        model_calls: agent.borrow().calls,
        replayed: agent.borrow().replayed,
    });

    println!("{}", serde_json::to_string_pretty(&frames).unwrap());
}
