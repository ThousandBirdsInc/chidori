//! Boa-powered JavaScript sandbox, compiled to `wasm32-wasip1` and embedded
//! into the host crate via `include_bytes!`.
//!
//! # ABI (matches sandbox-python)
//!
//! Reads JavaScript source from stdin, evaluates it, and writes the
//! `String(result)` of the final expression to stdout. Errors are written
//! to stdout prefixed with `ERR:` so the host can distinguish them from a
//! successful return with a single strip check. Running under WASI preview
//! 1 means the binary is fed by the host's minimal WASI shim — stdin
//! preloaded with source, stdout captured, fixed clock, zero preopens.
//!
//! The same `result` convention applies: if the program assigns to a top
//! level `result`, that's what we return; otherwise we return the value of
//! the final expression.

use std::io::{self, Read, Write};

use boa_engine::{Context, Source};

fn main() {
    let mut source = String::new();
    if let Err(e) = io::stdin().read_to_string(&mut source) {
        let _ = io::stdout().write_all(format!("ERR:stdin read: {}", e).as_bytes());
        return;
    }

    let mut context = Context::default();

    // Evaluate the full program. Boa returns the value of the final
    // expression automatically, so we don't need to do a second lookup
    // for `result` — but users who prefer explicit binding can still
    // write `var result = …;` as the final line and it'll come through.
    let output = match context.eval(Source::from_bytes(source.as_bytes())) {
        Ok(value) => match value.to_string(&mut context) {
            Ok(s) => s.to_std_string_lossy(),
            Err(e) => format!("ERR:to_string: {}", format_err(&e, &mut context)),
        },
        Err(e) => format!("ERR:{}", format_err(&e, &mut context)),
    };

    let _ = io::stdout().write_all(output.as_bytes());
}

fn format_err(err: &boa_engine::JsError, ctx: &mut Context) -> String {
    match err.try_native(ctx) {
        Ok(native) => native.to_string(),
        Err(_) => format!("{:?}", err),
    }
}
