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
///
/// This is the only caller that applies the `setrlimit` floor, because the
/// limits are applied to the *current process* — sound only when that process is
/// a dedicated worker. The in-process [`serve`] path (used by tests) must never
/// self-limit, or it would mutate the limits of whatever process is hosting it.
pub fn run() -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    serve_inner(stdin.lock(), stdout.lock(), true)
}

/// Run one agent to completion over an arbitrary reader/writer pair *without*
/// applying any per-process resource limits. Factored out of [`run`] so tests
/// can drive the worker over an in-process socket without a subprocess — and
/// without the worker's `setrlimit` floor leaking onto the test process.
pub fn serve<R: Read + 'static, W: Write + 'static>(reader: R, writer: W) -> io::Result<()> {
    serve_inner(reader, writer, false)
}

/// Shared worker body. `apply_limits` gates the per-process `setrlimit` floor —
/// see [`run`] vs [`serve`].
fn serve_inner<R: Read + 'static, W: Write + 'static>(
    reader: R,
    writer: W,
    apply_limits: bool,
) -> io::Result<()> {
    let io = Rc::new(RefCell::new(WorkerIo { reader, writer }));

    let init: FromParent = {
        let mut guard = io.borrow_mut();
        read_frame(&mut guard.reader)?
    };
    let (entry_path, entry_source, fallback_export, input, prelude, limits) = match init {
        FromParent::Init {
            entry_path,
            entry_source,
            fallback_export,
            input,
            prelude,
            limits,
        } => (
            entry_path,
            entry_source,
            fallback_export,
            input,
            prelude,
            limits,
        ),
        FromParent::Reply(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "isolate worker: expected Init as the first frame",
            ));
        }
    };

    // Slam the resource floor shut before any agent code runs — but only in a
    // real worker process (see `serve_inner`'s `apply_limits`). Best-effort: a
    // limit that can't be set degrades isolation but never fails the run.
    if apply_limits {
        limits.apply_to_self();
    } else {
        let _ = &limits;
    }

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
