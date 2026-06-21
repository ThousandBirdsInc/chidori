//! The supervisor half of OS isolation — the parent-side broker.
//!
//! [`run_agent_isolated`] spawns the worker (`chidori __run-worker`), ships it
//! the [`FromParent::Init`] handoff, then services every host op the child sends
//! by routing it through the *same* [`route_host_op`] the in-process host uses.
//! The durable call log, policy, MCP, providers, and OTEL all stay here in the
//! trusted parent; the child only computes JavaScript.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

use crate::runtime::rust_engine::{build_sync_native_dispatch, route_host_op, rust_engine_prelude};
use crate::runtime::typescript::bindings::HostBindingBackend;

use super::protocol::{read_frame, write_frame, FromChild, FromParent, Outcome};

/// Run `source` (the agent at `path`) in a sandboxed child process, brokering its
/// host effects back through `backend`. Returns the agent's output, or an error
/// if the worker failed, crashed, or was killed.
pub(crate) fn run_agent_isolated(
    path: &Path,
    source: &str,
    input: &Value,
    backend: &HostBindingBackend,
) -> Result<Value> {
    let exe = std::env::current_exe().context("locating the chidori worker binary")?;
    let mut child = Command::new(&exe)
        .arg("__run-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        // The child must not re-enter isolation (it runs the agent directly); make
        // that impossible regardless of how this process's env was configured.
        .env_remove("CHIDORI_ISOLATE")
        .spawn()
        .with_context(|| format!("spawning isolate worker `{} __run-worker`", exe.display()))?;

    let mut to_child = child.stdin.take().expect("worker stdin was piped");
    let mut from_child = child.stdout.take().expect("worker stdout was piped");

    let init = FromParent::Init {
        entry_path: path.to_string_lossy().into_owned(),
        entry_source: source.to_string(),
        fallback_export: "agent".to_string(),
        input: input.clone(),
        prelude: backend.runtime_policy().map(|p| rust_engine_prelude(&p)),
    };

    let result = broker(&mut from_child, &mut to_child, backend, init);

    // Drop our pipe ends so the worker sees EOF, then reap it to avoid a zombie.
    // The `Done` frame (or its absence) is authoritative for the run's outcome;
    // the exit status is only used to enrich a missing-result error.
    drop(to_child);
    drop(from_child);
    let status = child.wait();
    match result {
        Ok(v) => Ok(v),
        Err(e) => match status {
            Ok(s) if !s.success() => {
                Err(e.context(format!("isolate worker exited with status {s}")))
            }
            _ => Err(e),
        },
    }
}

/// The broker loop: send `init`, then service `Call` frames until the worker
/// reports `Done`. Generic over the transport so tests can drive it over an
/// in-process socket pair. `pub(crate)` for that reason.
pub(crate) fn broker<R: Read, W: Write>(
    from_child: &mut R,
    to_child: &mut W,
    backend: &HostBindingBackend,
    init: FromParent,
) -> Result<Value> {
    // The captured-effect native dispatch (VFS / crypto / timers), built once and
    // shared across every brokered op — identical to what the in-process host
    // constructs, so brokered and inline runs hit the same handlers.
    let sync = match (backend.runtime_policy(), backend.runtime_ctx()) {
        (Some(policy), Some(ctx)) => Some(build_sync_native_dispatch(ctx.clone(), policy)),
        _ => None,
    };

    write_frame(to_child, &init).context("sending Init to the isolate worker")?;

    loop {
        let msg: FromChild = read_frame(from_child)
            .context("isolate worker terminated before returning a result")?;
        match msg {
            FromChild::Call { op, args } => {
                let outcome: Outcome = route_host_op(backend, sync.as_ref(), &op, &args).into();
                write_frame(to_child, &FromParent::Reply(outcome))
                    .context("replying to the isolate worker")?;
            }
            FromChild::Done { outcome } => {
                return Result::<Value, String>::from(outcome).map_err(|e| anyhow!(e));
            }
        }
    }
}
