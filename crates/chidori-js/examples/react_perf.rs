//! React render latency on chidori-js — the "fast enough for a browser use
//! case?" measurement.
//!
//! The browser model under test is server-authoritative (LiveView-style, per
//! `docs/dom-runtime-prototype.md`): every user event triggers a full React
//! `renderToStaticMarkup` pass on the engine, and the output (or its diff)
//! ships to a dumb renderer. So the number that decides interactivity is the
//! warm per-render latency at realistic tree sizes, plus the one-time costs a
//! page pays on load (engine build, React bundle eval, first render).
//!
//! The workload is the js-framework-benchmark table shape: N rows of
//! 4 cells, one selected row, every 10th label mutated per tick — a full
//! re-render per tick, no memoization.
//!
//! Run: `cargo run -q --release --example react_perf -p chidori-js`
//! Numbers vary run to run on shared hardware; per-render figures are
//! amortized over enough iterations to sit well above timer noise.

use std::time::Instant;

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

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn main() {
    // Profile mode: `react_perf <rows> <iters>` runs only the tick loop —
    // sized for callgrind (`valgrind --tool=callgrind ... react_perf 100 10`).
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [rows, iters] = args.as_slice() {
        let (rows, iters): (usize, u32) = (rows.parse().unwrap(), iters.parse().unwrap());
        let (mut e, _dom) = load(true);
        e.eval_cached(&format!("__setup({rows})")).unwrap();
        for _ in 0..iters {
            e.eval_cached("__tick()").unwrap();
        }
        return;
    }
    full_report();
}

fn load(quiet: bool) -> (chidori_js::Engine, chidori_js::dom::DomHandle) {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/react_assets");
    let react = std::fs::read_to_string(format!("{dir}/react.js")).expect("react.js vendored");
    let server =
        std::fs::read_to_string(format!("{dir}/react-dom-server.js")).expect("server vendored");

    // -- One-time page-load costs ------------------------------------------
    let t0 = Instant::now();
    let mut e = chidori_js::Engine::new();
    let t_engine = t0.elapsed();

    let dom = e.install_dom();
    e.eval("globalThis.self=globalThis; globalThis.global=globalThis;")
        .unwrap();
    let t1 = Instant::now();
    e.eval(&react).expect("react evaluates");
    let t_react = t1.elapsed();
    let t2 = Instant::now();
    e.eval(&server).expect("react-dom/server evaluates");
    let t_server = t2.elapsed();
    let t3 = Instant::now();
    e.eval(APP_JS).expect("app evaluates");
    let t_app = t3.elapsed();

    if !quiet {
        println!("== one-time costs ==");
        println!("engine_new:            {:8.2} ms", ms(t_engine));
        println!("eval react.js:         {:8.2} ms", ms(t_react));
        println!("eval react-dom-server: {:8.2} ms", ms(t_server));
        println!("eval app:              {:8.2} ms", ms(t_app));
    }
    (e, dom)
}

fn full_report() {
    let (mut e, dom) = load(false);

    // -- Render latency by tree size ---------------------------------------
    println!();
    println!("== full re-render per user event (renderToStaticMarkup) ==");
    println!(
        "{:>6} {:>10} {:>12} {:>12} {:>12}",
        "rows", "markup", "first (ms)", "warm avg", "warm min"
    );
    for &n in &[10usize, 100, 500, 1000] {
        e.eval_cached(&format!("__setup({n})")).unwrap();
        let tf = Instant::now();
        let len = e.eval_cached("__tick()").unwrap();
        let first = tf.elapsed();
        let markup_len = e.vm.to_string_lossy(&len);

        let iters: u32 = if n >= 500 { 20 } else { 60 };
        // Amortized loop: one eval per event, like a host delivering events.
        let mut best = f64::MAX;
        let tw = Instant::now();
        for _ in 0..iters {
            let ti = Instant::now();
            e.eval_cached("__tick()").unwrap();
            let d = ms(ti.elapsed());
            if d < best {
                best = d;
            }
        }
        let avg = ms(tw.elapsed()) / f64::from(iters);
        println!(
            "{n:>6} {markup_len:>10} {:>12.2} {avg:>12.2} {best:>12.2}",
            ms(first)
        );
    }

    // -- Mount into the journaled DOM (the full browser pipeline) ----------
    let tm = Instant::now();
    e.eval_cached(
        "__setup(100); const root = document.createElement('div');\
         document.body.appendChild(root); root.innerHTML = __renderOnce();\
         root.querySelectorAll('tr').length",
    )
    .unwrap();
    let mount = tm.elapsed();
    let muts = dom.drain_mutations().len();
    println!();
    println!(
        "mount 100-row markup into journaled DOM: {:.2} ms ({muts} mutations)",
        ms(mount)
    );
}
