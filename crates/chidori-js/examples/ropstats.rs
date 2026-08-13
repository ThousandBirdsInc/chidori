//! Register-tier opcode-frequency analyzer (feature `rop-histogram`).
//!
//! Runs a JS file — or the vendored React render workload with `--react` —
//! and prints the execution-weighted `ROp` histogram plus adjacent-pair
//! counts: the data that drives reg-tier superinstruction selection
//! (the register-tier twin of `examples/opstats.rs`).
//!
//! ```sh
//! cargo run -q --release --example ropstats -p chidori-js \
//!   --features rop-histogram -- crates/chidori-js/benchmarks/workloads/mixed_helpers.js
//! cargo run -q --release --example ropstats -p chidori-js \
//!   --features rop-histogram -- --react
//! ```

fn main() {
    let arg = std::env::args()
        .nth(1)
        .expect("usage: ropstats <file.js>|--react");
    let mut e = chidori_js::Engine::new();
    if arg == "--react" {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/react_assets");
        let react = std::fs::read_to_string(format!("{dir}/react.js")).unwrap();
        let server = std::fs::read_to_string(format!("{dir}/react-dom-server.js")).unwrap();
        e.install_dom();
        e.eval("globalThis.self=globalThis; globalThis.global=globalThis;")
            .unwrap();
        e.eval(&react).unwrap();
        e.eval(&server).unwrap();
        e.eval(APP_JS).unwrap();
        e.eval("__setup(100)").unwrap();
        chidori_js::opstats::reset();
        for _ in 0..10 {
            e.eval("__tick()").unwrap();
        }
    } else {
        let src = std::fs::read_to_string(&arg).unwrap();
        chidori_js::opstats::reset();
        e.eval(&src).unwrap();
    }
    let r = chidori_js::opstats::take();
    println!("total dispatched (both tiers): {}", r.total);
    println!("\n== top ops ==");
    for (op, n) in r.ops.iter().take(40) {
        println!("{n:>12}  {:5.2}%  {op}", *n as f64 / r.total as f64 * 100.0);
    }
    println!("\n== top pairs ==");
    for ((a, b), n) in r.pairs.iter().take(60) {
        println!(
            "{n:>12}  {:5.2}%  {a} -> {b}",
            *n as f64 / r.total as f64 * 100.0
        );
    }
}

// Mirror of `react_perf.rs`'s APP_JS (kept in sync by hand; analysis-only).
const APP_JS: &str = r#"
const h = React.createElement;

// Deterministic js-framework-benchmark-style data.
const ADJ = ['pretty','large','big','small','tall','short','long','handsome',
             'plain','quaint','clean','elegant','easy','angry','crazy','helpful'];
const NOUN = ['table','chair','house','bbq','desk','car','pony','cookie',
              'sandwich','burger','pizza','mouse','keyboard'];
let seed = 42;
function rnd(max) { seed = (seed * 1103515245 + 12345) >>> 0; return seed % max; }
function buildData(n) {
  const out = [];
  for (let i = 0; i < n; i++) {
    out.push({ id: i + 1,
               label: ADJ[rnd(ADJ.length)] + ' ' + NOUN[rnd(NOUN.length)] + ' ' + i });
  }
  return out;
}

function GlyphButton({ id, label }) {
  return h('div', { className: 'col-sm-6 smallpad' },
    h('button', { type: 'button', className: 'btn btn-primary btn-block', id }, label));
}
function Row({ item, selected }) {
  return h('tr', { className: selected ? 'danger' : '' },
    h('td', { className: 'col-md-1' }, String(item.id)),
    h('td', { className: 'col-md-4' }, h('a', { className: 'lbl' }, item.label)),
    h('td', { className: 'col-md-1' },
      h('a', { className: 'remove' },
        h('span', { className: 'glyphicon glyphicon-remove', 'aria-hidden': 'true' }))),
    h('td', { className: 'col-md-6' }));
}
function App({ items, selected }) {
  return h('div', { className: 'container' },
    h('div', { className: 'jumbotron' },
      h('div', { className: 'row' },
        h('div', { className: 'col-md-6' }, h('h1', null, 'chidori-js React')),
        h('div', { className: 'col-md-6' },
          h(GlyphButton, { id: 'run', label: 'Create 1,000 rows' }),
          h(GlyphButton, { id: 'clear', label: 'Clear' })))),
    h('table', { className: 'table table-hover table-striped test-data' },
      h('tbody', null,
        items.map((it) => h(Row, { key: it.id, item: it, selected: it.id === selected })))));
}

let items = [];
let selected = 0;
let tick = 0;
globalThis.__setup = function (n) { items = buildData(n); selected = 0; tick = 0; };
// One user-event-shaped state change + full re-render. Returns markup length
// so the render cannot be dead-code-eliminated anywhere in the pipeline.
globalThis.__tick = function () {
  tick++;
  selected = (tick % items.length) + 1;
  for (let i = 0; i < items.length; i += 10) {
    items[i] = { id: items[i].id, label: items[i].label + ' !!!' };
  }
  return ReactDOMServer.renderToStaticMarkup(h(App, { items, selected })).length;
};
globalThis.__renderOnce = function () {
  return ReactDOMServer.renderToStaticMarkup(h(App, { items, selected }));
};
"#;
