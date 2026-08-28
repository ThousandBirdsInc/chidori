//! `chidori-js-jit` — the alternative engine entry point with the Cranelift
//! kernel JIT compiled in and ENABLED (docs/cranelift-jit.md).
//!
//! Same file-runner contract as `examples/run.rs` (evaluate a script, print
//! the console output and the completion value), but built as its own binary
//! behind `required-features = ["jit"]`, so the default `chidori-js` build —
//! and its `forbid(unsafe_code)` guarantee — is untouched. `--no-jit` runs
//! the identical build with the tier switched off, which is the quickest A/B
//! (and byte-identity check) on any script:
//!
//! ```text
//! cargo run -p chidori-js --features jit --bin chidori-js-jit -- script.js
//! cargo run -p chidori-js --features jit --bin chidori-js-jit -- --no-jit script.js
//! ```

use chidori_js::Engine;

fn main() {
    let mut jit = true;
    let mut show_stats = false;
    let mut file: Option<String> = None;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--no-jit" => jit = false,
            "--jit-stats" => show_stats = true,
            _ => file = Some(arg),
        }
    }
    let Some(file) = file else {
        eprintln!("usage: chidori-js-jit [--no-jit] [--jit-stats] <script.js>");
        std::process::exit(2);
    };
    let src = match std::fs::read_to_string(&file) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("chidori-js-jit: cannot read {file}: {err}");
            std::process::exit(2);
        }
    };
    let mut engine = Engine::new();
    engine.vm.jit_enabled = jit;
    let outcome = engine.eval(&src);
    for line in engine.console() {
        println!("{line}");
    }
    match outcome {
        Ok(v) => println!("=> {}", engine.vm.to_string_lossy(&v)),
        Err(err) => println!("ERROR: {err}"),
    }
    if show_stats {
        let s = chidori_js::jit::stats();
        eprintln!(
            "jit: {} kernels compiled ({} int-typed), {} declined, {} native runs, {} element shim calls",
            s.compiled, s.int_typed, s.declined, s.native_runs, s.elem_shim_calls
        );
    }
}
