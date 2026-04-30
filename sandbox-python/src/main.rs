//! RustPython-powered Python sandbox, compiled to `wasm32-wasip1` and
//! embedded into the host crate via `include_bytes!`.
//!
//! # ABI
//!
//! The guest reads Python source from stdin and writes the string repr of
//! the final expression's value to stdout. Errors are written to stdout
//! prefixed with `ERR:`.  The host drives this by creating a fresh wasmer
//! store per call, attaching a WASI env with stdin pre-populated with the
//! source and stdout captured to a pipe, running `_start`, and then reading
//! the pipe.
//!
//! This matches the same "source in, result out" contract as the miniscript
//! runtime — only with real Python instead of postfix arithmetic.

use std::io::{self, Read, Write};

use rustpython_vm::{self as vm, AsObject};

fn main() {
    // Read all of stdin as the Python source.
    let mut source = String::new();
    if let Err(e) = io::stdin().read_to_string(&mut source) {
        let _ = writeln!(io::stdout(), "ERR:stdin read: {}", e);
        return;
    }

    let interpreter = vm::Interpreter::without_stdlib(Default::default());

    let result = interpreter.enter(|vm| {
        let scope = vm.new_scope_with_builtins();
        // Compile the user source as an `exec` unit so multi-statement
        // programs work. We then fish the last bound name out of the scope
        // and print its repr. This matches how Jupyter / Python REPLs
        // present the final value of a multi-line snippet.
        let code = vm
            .compile(
                &source,
                vm::compiler::Mode::Exec,
                "<sandbox>".to_owned(),
            )
            .map_err(|err| vm.new_syntax_error(&err, Some(&source)))?;

        vm.run_code_obj(code, scope.clone())?;

        // Prefer a user-defined `result` variable; fall back to `None`.
        let printable = scope
            .globals
            .get_item_opt("result", vm)?
            .unwrap_or_else(|| vm.ctx.none());
        let repr = printable.repr(vm)?;
        Ok::<String, vm::PyRef<vm::builtins::PyBaseException>>(
            repr.to_str().unwrap_or("").to_string(),
        )
    });

    match result {
        Ok(out) => {
            let _ = io::stdout().write_all(out.as_bytes());
        }
        Err(exc) => {
            // Format the exception as a single line: "TypeName: message".
            let summary = interpreter.enter(|vm| {
                let type_name = exc.as_object().class().name().to_string();
                let args: Vec<String> = exc
                    .args()
                    .as_slice()
                    .iter()
                    .filter_map(|a| a.str(vm).ok().and_then(|s| s.to_str().map(String::from)))
                    .collect();
                if args.is_empty() {
                    format!("ERR:{}", type_name)
                } else {
                    format!("ERR:{}: {}", type_name, args.join(", "))
                }
            });
            let _ = io::stdout().write_all(summary.as_bytes());
        }
    }
}
