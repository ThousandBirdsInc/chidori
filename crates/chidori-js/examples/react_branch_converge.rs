//! An agent that **branches into experiments, asks the user for feedback, and
//! converges** — all on the chidori-js runtime with real React, and all
//! journaled so the whole session (including the human's answer) replays
//! deterministically.
//!
//!   cargo run -p chidori-js --example react_branch_converge > docs/media/branch_frames.json
//!
//! Flow:
//!   1. Branch: the agent drafts three React variants and renders + tests each
//!      in its own isolated engine ("execution branch").
//!   2. Feedback: it asks the user which direction to ship and for any tweak —
//!      a `chidori.input()` host effect, recorded into the journal.
//!   3. Converge: it refines the chosen variant with the requested tweak and
//!      ships the green result.
//!   4. Replay: re-running the journal (drafts + the human's answer + refine)
//!      reproduces the converged UI byte-for-byte, with zero new model calls and
//!      without asking the human again.

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

#[derive(Serialize, Clone)]
struct Card {
    label: String,
    html: String,
    tests: Vec<Test>,
    features: usize,
    chosen: bool,
}

#[derive(Serialize)]
struct Frame {
    kind: String, // "branches" | "feedback" | "converged" | "replay"
    caption: String,
    note: String,
    cards: Vec<Card>,
    html: String,
    tests: Vec<Test>,
    question: String,
    answer: String,
    model_calls: usize,
    inputs: usize,
    replayed: usize,
}

impl Default for Frame {
    fn default() -> Self {
        Frame {
            kind: String::new(),
            caption: String::new(),
            note: String::new(),
            cards: Vec::new(),
            html: String::new(),
            tests: Vec::new(),
            question: String::new(),
            answer: String::new(),
            model_calls: 0,
            inputs: 0,
            replayed: 0,
        }
    }
}

/// The agent + its durable journal. Both `prompt` (model) and `input` (human)
/// answers are recorded; on replay they are served for free — so a converged
/// session can be reproduced without re-calling the model OR re-asking the user.
struct Host {
    journal: Vec<String>,
    cursor: usize,
    replay: bool,
    model_calls: usize,
    inputs: usize,
    replayed: usize,
}

impl Host {
    fn new() -> Host {
        Host { journal: Vec::new(), cursor: 0, replay: false, model_calls: 0, inputs: 0, replayed: 0 }
    }

    fn respond(&mut self, effect: &str, spec: &str) -> String {
        if self.replay && self.cursor < self.journal.len() {
            let v = self.journal[self.cursor].clone();
            self.cursor += 1;
            self.replayed += 1;
            return v;
        }
        let ans = brain(effect, spec);
        self.journal.push(ans.clone());
        self.cursor += 1;
        if effect == "input" {
            self.inputs += 1;
        } else {
            self.model_calls += 1;
        }
        ans
    }
}

fn card_src(theme: &str, title: &str, price: &str, feats: &[&str], cta: &str) -> String {
    let feat_els: String = feats.iter().map(|f| format!("e('li',null,'{f}'),")).collect();
    format!(
        "globalThis.App=function(){{const e=React.createElement;return \
         e('div',{{className:'card {theme}'}},\
         e('h2',null,'{title}'),\
         e('div',{{className:'price'}},'{price}'),\
         e('ul',null,{feat_els}),\
         e('button',null,'{cta}'));}};"
    )
}

/// The agent's "model" and the simulated human. Canned, but routed and journaled
/// exactly like real `prompt`/`input` host effects.
fn brain(effect: &str, spec: &str) -> String {
    if effect == "input" {
        // The user's feedback: pick variant B, and rename the CTA.
        return r#"{"choice":"B","tweak":"Start free trial"}"#.to_string();
    }
    match spec {
        "variant:A" => card_src("light", "Starter", "$9/mo", &["Email support", "1 project"], "Sign up"),
        "variant:B" => card_src(
            "light",
            "Pro",
            "$29/mo",
            &["Priority support", "Unlimited projects", "Deterministic replay", "Time-travel debugging"],
            "Subscribe",
        ),
        "variant:C" => card_src("dark", "Team", "$99/mo", &["SSO & SAML", "Audit log", "Replay sharing"], "Get started"),
        // Converge: the chosen variant (B) with the user's tweak applied.
        "refine" => card_src(
            "light",
            "Pro",
            "$29/mo",
            &["Priority support", "Unlimited projects", "Deterministic replay", "Time-travel debugging"],
            "Start free trial",
        ),
        _ => card_src("light", "Pro", "$29/mo", &["A", "B"], "Subscribe"),
    }
}

fn setup(engine: &mut Engine, host: &Rc<RefCell<Host>>) -> DomHandle {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/react_assets");
    let react = std::fs::read_to_string(format!("{dir}/react.js")).unwrap();
    let server = std::fs::read_to_string(format!("{dir}/react-dom-server.js")).unwrap();
    let dom = engine.install_dom();
    let h = host.clone();
    engine.install_chidori_effects(Rc::new(move |effect: &str, args: &Value| {
        match effect {
            "prompt" => {
                let spec = args.get("text").and_then(Value::as_str).unwrap_or("");
                Ok(Value::String(h.borrow_mut().respond("prompt", spec)))
            }
            "input" => {
                let spec = args.get("prompt").and_then(Value::as_str).unwrap_or("");
                Ok(Value::String(h.borrow_mut().respond("input", spec)))
            }
            _ => Ok(Value::Null),
        }
    }));
    engine.eval("globalThis.self=globalThis; globalThis.global=globalThis;").unwrap();
    engine.eval(&react).unwrap();
    engine.eval(&server).unwrap();
    engine
        .eval(
            r#"
            globalThis.runTests = function (root) {
                const txt = root.textContent || '';
                const btn = root.querySelector('button');
                return [
                    { name: 'has heading',        pass: !!root.querySelector('h2') },
                    { name: 'shows a $ price',    pass: /\$\d/.test(txt) },
                    { name: 'lists ≥2 features',  pass: root.querySelectorAll('li').length >= 2 },
                    { name: 'has a CTA',          pass: !!btn && btn.textContent.length > 0 },
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

fn prompt(engine: &mut Engine, spec: &str) -> String {
    let v = engine.eval(&format!("chidori.prompt({:?})", spec)).unwrap();
    engine.vm.to_string_lossy(&v)
}

fn ask_user(engine: &mut Engine, question: &str) -> String {
    let v = engine.eval(&format!("chidori.input({:?})", question)).unwrap();
    engine.vm.to_string_lossy(&v)
}

fn render_and_test(engine: &mut Engine, source: &str) -> (String, Vec<Test>) {
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

fn feature_count(html: &str) -> usize {
    html.matches("</li>").count()
}

fn main() {
    let host = Rc::new(RefCell::new(Host::new()));
    let mut frames: Vec<Frame> = Vec::new();
    let variants = [("A", "Starter"), ("B", "Pro"), ("C", "Team")];

    // ---- 1. Branch: three isolated experiments. ----
    eprintln!("agent: branching into {} experiments…", variants.len());
    let mut cards: Vec<Card> = Vec::new();
    for (letter, _) in variants {
        let mut be = Engine::new();
        let _dom = setup(&mut be, &host);
        let src = prompt(&mut be, &format!("variant:{letter}"));
        let (html, tests) = render_and_test(&mut be, &src);
        let pass = tests.iter().filter(|t| t.pass).count();
        eprintln!("  variant {letter}: {}/{} tests pass", pass, tests.len());
        cards.push(Card {
            label: format!("Variant {letter}"),
            features: feature_count(&html),
            html,
            tests,
            chosen: false,
        });
    }
    let (mc, ic) = { let h = host.borrow(); (h.model_calls, h.inputs) };
    frames.push(Frame {
        kind: "branches".into(),
        caption: "1 · Agent branches into 3 experiments".into(),
        note: "each variant rendered (real React) + tested in its own engine".into(),
        cards: cards.clone(),
        model_calls: mc,
        inputs: ic,
        ..Default::default()
    });

    // ---- 2. Feedback: ask the user which to ship + a tweak. ----
    let question = "Which direction should we ship — A, B, or C — and any tweak?";
    eprintln!("agent → user: {question}");
    let mut main_eng = Engine::new();
    let _dom = setup(&mut main_eng, &host);
    let answer = ask_user(&mut main_eng, question);
    eprintln!("user → agent: {answer}");
    let parsed: Value = serde_json::from_str(&answer).unwrap_or(Value::Null);
    let choice = parsed["choice"].as_str().unwrap_or("B").to_string();
    let tweak = parsed["tweak"].as_str().unwrap_or("").to_string();
    let mut fb_cards = cards.clone();
    for c in &mut fb_cards {
        c.chosen = c.label.ends_with(&choice);
    }
    let (mc, ic) = { let h = host.borrow(); (h.model_calls, h.inputs) };
    frames.push(Frame {
        kind: "feedback".into(),
        caption: "2 · Asks the user for feedback".into(),
        note: "the human's answer is a captured host input — recorded in the journal".into(),
        cards: fb_cards,
        question: question.into(),
        answer: format!("ship {choice} · CTA → “{tweak}”"),
        model_calls: mc,
        inputs: ic,
        ..Default::default()
    });

    // ---- 3. Converge: refine the chosen variant with the tweak. ----
    eprintln!("agent: converging on {choice} with tweak “{tweak}”…");
    let refined = prompt(&mut main_eng, "refine");
    let (final_html, final_tests) = render_and_test(&mut main_eng, &refined);
    let passed = final_tests.iter().filter(|t| t.pass).count();
    let (mc, ic) = { let h = host.borrow(); (h.model_calls, h.inputs) };
    frames.push(Frame {
        kind: "converged".into(),
        caption: format!("3 · Converges on {choice} · {}/{} green", passed, final_tests.len()),
        note: format!("applied the user's tweak (CTA → “{tweak}”) and shipped"),
        html: final_html.clone(),
        tests: final_tests.clone(),
        model_calls: mc,
        inputs: ic,
        ..Default::default()
    });

    // ---- 4. Replay the whole session (drafts + the human answer + refine). ----
    {
        let mut h = host.borrow_mut();
        h.replay = true;
        h.cursor = 0;
        h.replayed = 0;
    }
    let mut rep = Engine::new();
    let _dom = setup(&mut rep, &host);
    for (letter, _) in variants {
        let src = prompt(&mut rep, &format!("variant:{letter}"));
        let _ = render_and_test(&mut rep, &src);
    }
    let _ = ask_user(&mut rep, question); // served from journal — user NOT re-asked
    let refined2 = prompt(&mut rep, "refine");
    let (replay_html, replay_tests) = render_and_test(&mut rep, &refined2);
    let identical = replay_html == final_html;
    let (mc, ic, rp) = { let h = host.borrow(); (h.model_calls, h.inputs, h.replayed) };
    eprintln!("replay: identical={identical}, replayed={rp} calls, new model calls=0");
    frames.push(Frame {
        kind: "replay".into(),
        caption: "4 · Record == replay".into(),
        note: if identical {
            format!("replayed {rp} calls incl. the human's answer → byte-identical, 0 new model calls ✓")
        } else {
            "MISMATCH".into()
        },
        html: replay_html,
        tests: replay_tests,
        model_calls: mc,
        inputs: ic,
        replayed: rp,
        ..Default::default()
    });

    println!("{}", serde_json::to_string_pretty(&frames).unwrap());
}
