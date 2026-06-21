//! The worker half of OS isolation — the code that runs in the sandboxed child.
//!
//! The child holds *only* the JavaScript engine. It reads an [`FromParent::Init`]
//! handoff, runs the agent through the ordinary [`run_module`] path, and routes
//! every host op (`chidori.*` effect, captured native, DOM flush, module load)
//! back to the parent over the pipe via [`BrokeredHost`]. It never touches the
//! filesystem, the network, or a clock of its own — those live behind the seam.
//!
//! Phase 1 wires the broker but applies no sandbox yet; the seccomp / namespace
//! confinement lands in a later phase (see `docs/os-isolation-plan.md`).

use std::cell::RefCell;
use std::io::{self, Read, Write};
use std::path::Path;
use std::rc::Rc;

use serde_json::Value;

use crate::runtime::rust_engine::{run_module, RunHost};

use super::protocol::{read_frame, write_frame, FromChild, FromParent, Outcome};

/// The duplex the worker speaks over: replies/Init arrive on `reader`, calls/Done
/// go out on `writer`. Wrapped in a single cell so the run thread can borrow both
/// for one request/response exchange.
struct WorkerIo<R: Read, W: Write> {
    reader: R,
    writer: W,
}

/// A [`RunHost`] that satisfies every op by a blocking round trip to the parent.
/// Mirrors the engine's existing synchronous dispatch, so to the VM this is
/// indistinguishable from the in-process host.
struct BrokeredHost<R: Read, W: Write> {
    io: Rc<RefCell<WorkerIo<R, W>>>,
    prelude: Option<String>,
}

impl<R: Read, W: Write> RunHost for BrokeredHost<R, W> {
    fn call(&self, op: &str, args: &Value) -> Result<Value, String> {
        let mut io = self.io.borrow_mut();
        let io = &mut *io;
        write_frame(
            &mut io.writer,
            &FromChild::Call {
                op: op.to_string(),
                args: args.clone(),
            },
        )
        .map_err(|e| format!("isolate worker: writing host call `{op}`: {e}"))?;
        let reply: FromParent = read_frame(&mut io.reader)
            .map_err(|e| format!("isolate worker: reading reply for `{op}`: {e}"))?;
        match reply {
            FromParent::Reply(outcome) => outcome.into(),
            FromParent::Init { .. } => {
                Err("isolate worker: unexpected Init while awaiting a reply".to_string())
            }
        }
    }

    fn prelude(&self) -> Option<String> {
        self.prelude.clone()
    }
}

/// Entry point for the hidden `chidori __run-worker` subcommand: drive the
/// protocol over this process's stdin/stdout. stderr is left untouched for
/// diagnostics — nothing but frames may go to stdout or the stream desyncs.
pub fn run() -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    serve(stdin.lock(), stdout.lock())
}

/// Run one agent to completion over an arbitrary reader/writer pair. Factored out
/// of [`run`] so tests can drive it over an in-process socket without spawning a
/// subprocess. Always reports the run outcome as a final [`FromChild::Done`];
/// protocol/IO failures (a dead parent) surface as the returned `io::Result`.
pub fn serve<R: Read + 'static, W: Write + 'static>(reader: R, writer: W) -> io::Result<()> {
    let io = Rc::new(RefCell::new(WorkerIo { reader, writer }));

    let init: FromParent = {
        let mut guard = io.borrow_mut();
        read_frame(&mut guard.reader)?
    };
    let (entry_path, entry_source, fallback_export, input, prelude) = match init {
        FromParent::Init {
            entry_path,
            entry_source,
            fallback_export,
            input,
            prelude,
        } => (entry_path, entry_source, fallback_export, input, prelude),
        FromParent::Reply(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "isolate worker: expected Init as the first frame",
            ));
        }
    };

    let host: Rc<dyn RunHost> = Rc::new(BrokeredHost {
        io: io.clone(),
        prelude,
    });
    // `run_module` already contains the opcode-budget guard and a `catch_unwind`
    // boundary, so an interpreter panic comes back here as `Err`, not an unwind.
    let outcome: Outcome = match run_module(
        Path::new(&entry_path),
        &entry_source,
        &fallback_export,
        &input,
        host,
    ) {
        Ok(value) => Outcome::Ok(value),
        Err(e) => Outcome::Err(e.to_string()),
    };

    let mut guard = io.borrow_mut();
    write_frame(&mut guard.writer, &FromChild::Done { outcome })
}
