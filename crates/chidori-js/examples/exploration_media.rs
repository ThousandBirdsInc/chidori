//! Drives a real UI-exploration session and emits per-frame JSON on stdout, for
//! the media generator (`docs/media/render_media.py`). Everything here is real:
//! the DOM is built through the runtime, the interactions are dispatched events,
//! the "fork" is a second engine replaying the recorded event journal against
//! edited code, and the determinism frame re-runs and compares.
//!
//!   cargo run -p chidori-js --example exploration_media > docs/media/frames.json

use chidori_js::dom::DomHandle;
use chidori_js::Engine;
use serde::Serialize;

#[derive(Serialize)]
struct Frame {
    caption: String,
    note: String,
    html: String,
    /// Optional second panel (the forked variant), shown side by side.
    html_b: Option<String>,
    /// Journal lines produced by this step (event markers + mutations).
    journal: Vec<String>,
}

const SHELL: &str = r#"
    globalThis.build = (theme) => {
        const app = document.createElement('div');
        app.className = 'app ' + theme;
        const h = document.createElement('h1');
        h.textContent = 'Tasks';
        app.appendChild(h);
        const ul = document.createElement('ul');
        ul.className = 'list';
        ul.id = 'list';
        app.appendChild(ul);
        const add = document.createElement('button');
        add.className = 'add';
        add.textContent = '+ Add task';
        app.appendChild(add);
        document.body.appendChild(app);
    };
    globalThis.addTask = (id, text) => {
        const li = document.createElement('li');
        li.className = 'task';
        li.id = id;
        const box = document.createElement('span');
        box.className = 'box';
        box.textContent = '[ ]';
        const label = document.createElement('span');
        label.className = 'label';
        label.id = 'lbl-' + id;
        label.textContent = text;
        const del = document.createElement('span');
        del.className = 'del';
        del.id = 'del-' + id;
        del.textContent = 'x';
        li.appendChild(box);
        li.appendChild(label);
        li.appendChild(del);
        label.addEventListener('click', () => {
            li.classList.toggle('done');
            box.textContent = li.classList.contains('done') ? '[x]' : '[ ]';
        });
        del.addEventListener('click', () => li.remove());
        document.getElementById('list').appendChild(li);
    };
"#;

fn drain(dom: &DomHandle) -> Vec<String> {
    dom.drain_mutations()
        .iter()
        .map(|m| serde_json::to_string(m).unwrap())
        .collect()
}

fn main() {
    let mut frames: Vec<Frame> = Vec::new();

    // --- A live session we record. ---
    let mut engine = Engine::new();
    let dom = engine.install_dom();
    engine.eval(SHELL).unwrap();

    // Frame 1: the shell.
    engine.eval("build('theme-light');").unwrap();
    frames.push(Frame {
        caption: "1 · Agent builds the shell".into(),
        note: "document.createElement → mutation journal".into(),
        html: dom.render_html(),
        html_b: None,
        journal: drain(&dom),
    });

    // Frame 2: agent adds three tasks.
    engine
        .eval("addTask('t1','Write design doc'); addTask('t2','Wire up the DOM'); addTask('t3','Ship the demo');")
        .unwrap();
    frames.push(Frame {
        caption: "2 · Agent adds three tasks".into(),
        note: "each node + listener recorded deterministically".into(),
        html: dom.render_html(),
        html_b: None,
        journal: drain(&dom),
    });

    // Frame 3: user clicks task 2's label → toggles done (a real dispatched event).
    let lbl2 = dom.element_by_id("lbl-t2").unwrap();
    let mut j = vec!["● click  →  #lbl-t2".to_string()];
    dom.dispatch_event(&mut engine.vm, lbl2, "click", serde_json::json!({}))
        .unwrap();
    j.extend(drain(&dom));
    frames.push(Frame {
        caption: "3 · User completes “Wire up the DOM”".into(),
        note: "click handler flips class + checkbox — captured as input".into(),
        html: dom.render_html(),
        html_b: None,
        journal: j,
    });

    // Frame 4: user deletes task 1 (another real event).
    let del1 = dom.element_by_id("del-t1").unwrap();
    let mut j = vec!["● click  →  #del-t1".to_string()];
    dom.dispatch_event(&mut engine.vm, del1, "click", serde_json::json!({}))
        .unwrap();
    j.extend(drain(&dom));
    let live_html = dom.render_html();
    frames.push(Frame {
        caption: "4 · User deletes “Write design doc”".into(),
        note: "subtree removed; render is a pure function of the journal".into(),
        html: live_html.clone(),
        html_b: None,
        journal: j,
    });

    // The recorded non-deterministic input journal so far.
    let recorded = dom.journal();

    // --- Fork: a second engine replays the SAME events against EDITED code. ---
    let mut fork = Engine::new();
    let fdom = fork.install_dom();
    fork.eval(SHELL).unwrap();
    // The "edit": a dark theme + a different heading, same tasks.
    fork.eval("build('theme-dark');").unwrap();
    fork.eval("addTask('t1','Write design doc'); addTask('t2','Wire up the DOM'); addTask('t3','Ship the demo');")
        .unwrap();
    fork.eval("document.querySelector('h1').textContent = 'Tasks · dark';")
        .unwrap();
    let _ = fdom.drain_mutations();
    // Replay the recorded clicks — same inputs, different code.
    fdom.replay(&mut fork.vm, &recorded.events).unwrap();
    frames.push(Frame {
        caption: "5 · Fork → edit theme → replay the SAME events".into(),
        note: "edit-and-rerun: explore a variant; LLM/host work replays for free".into(),
        html: live_html.clone(),
        html_b: Some(fdom.render_html()),
        journal: vec![
            format!("replayed {} recorded events", recorded.events.len()),
            "left = original   ·   right = forked + edited".into(),
        ],
    });

    // --- Determinism: replay the journal into a fresh identical build. ---
    let mut check = Engine::new();
    let cdom = check.install_dom();
    check.eval(SHELL).unwrap();
    check.eval("build('theme-light');").unwrap();
    check.eval("addTask('t1','Write design doc'); addTask('t2','Wire up the DOM'); addTask('t3','Ship the demo');")
        .unwrap();
    let _ = cdom.drain_mutations();
    cdom.replay(&mut check.vm, &recorded.events).unwrap();
    let identical = cdom.render_html() == live_html;
    frames.push(Frame {
        caption: "6 · Record == replay".into(),
        note: if identical {
            "same program + same journal → byte-identical UI ✓".into()
        } else {
            "MISMATCH".into()
        },
        html: cdom.render_html(),
        html_b: None,
        journal: vec![
            format!("events: {}", recorded.events.len()),
            format!("render identical to live: {}", identical),
            "time-travel · replay-as-test · fork-and-edit".into(),
        ],
    });

    println!("{}", serde_json::to_string_pretty(&frames).unwrap());
}
