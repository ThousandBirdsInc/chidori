use anyhow::Result;
use chrono::Utc;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::providers::{
    CacheTtl, ContentBlock, LlmRequest, LlmResponse, ProviderRegistry, TokenSink, ToolCall,
};
use crate::runtime::call_log::{CallRecord, TokenUsage};
use crate::runtime::context::{
    ActorSignalWait, InputMode, PendingInput, PendingSignal, RuntimeContext, WarmInputWait,
};
use crate::runtime::errors::RunInterrupt;
use crate::runtime::memory::execute_memory_action;
use crate::runtime::snapshot::{HostPromiseState, PendingHostOperationKind};
use crate::runtime::template::TemplateEngine;
use crate::tools::ToolRegistry;

pub fn execute_durable_json_call(
    ctx: &RuntimeContext,
    function: &str,
    args: Value,
    live: impl FnOnce() -> Result<Value>,
) -> Result<Value> {
    let seq = ctx.next_seq();
    execute_durable_json_call_at_seq(ctx, seq, function, args, live)
}

pub fn execute_durable_json_call_at_seq(
    ctx: &RuntimeContext,
    seq: u64,
    function: &str,
    args: Value,
    live: impl FnOnce() -> Result<Value>,
) -> Result<Value> {
    if let Some(record) = ctx
        .try_replay_checked(seq, function, &args)
        .map_err(|err| anyhow::anyhow!(err))?
    {
        // A replayed call's `live()` is skipped. If it was a container (a tool
        // or call_agent whose body made its own host calls), those nested calls
        // burned sequence numbers that won't be re-consumed now — absorb the
        // recorded subtree so the outer sequence stays aligned and the nested
        // records survive in the trace. No-op for leaf calls.
        ctx.absorb_replayed_subtree(seq);
        return Ok(record.result);
    }

    let operation_kind = host_operation_kind(function);
    if let Some(kind) = operation_kind.clone() {
        if let Some(result) = replay_completed_host_operation(ctx, seq, function, kind, &args)? {
            return Ok(result);
        }
    }

    // Strict-durability gate: once a journal write has failed, refuse to run
    // further live side effects — the run would otherwise keep acting on the
    // world without a recording of it (`docs/durable-storage.md`).
    if let Some(failure) = ctx.persist_failure() {
        anyhow::bail!(
            "refusing live `{function}`: durable journal write failed earlier \
             under CHIDORI_DURABILITY=strict: {failure}"
        );
    }

    let host_operation = operation_kind.map(|kind| {
        ctx.begin_host_operation_with_function(seq, kind, Some(function.to_string()), args.clone())
    });
    if let Some(id) = host_operation {
        ctx.run_host_operation_safepoint(id)?;
    }
    let started = Utc::now();
    // Mark this call as executing so any calls made inside `live()` (a
    // sub-agent's host calls, when `function == "call_agent"`) nest under it.
    ctx.enter_call(seq);
    let result = live();
    ctx.exit_call(seq);
    let duration_ms = Utc::now()
        .signed_duration_since(started)
        .num_milliseconds()
        .max(0) as u64;

    match result {
        Ok(result) => {
            if let Some(id) = host_operation {
                ctx.resolve_host_operation(id, result.clone())?;
            }
            ctx.record_call(CallRecord {
                seq,
                parent_seq: None,
                function: function.to_string(),
                args,
                result: result.clone(),
                duration_ms,
                token_usage: None,
                timestamp: started,
                error: None,
            });
            if let Some(id) = host_operation {
                ctx.run_host_operation_completion_safepoint(id)?;
            }
            Ok(result)
        }
        Err(err) => {
            // A pause is control flow, not a failure: don't record or reject
            // anything, just keep unwinding. Re-type it here so everything
            // upstream can downcast — the interrupt may arrive as a plain
            // string when the effect ran on the other side of the JS engine.
            if let Some(interrupt) = RunInterrupt::from_error(&err) {
                return Err(anyhow::Error::new(interrupt));
            }
            let message = err.to_string();
            if let Some(id) = host_operation {
                ctx.reject_host_operation(id, message.clone())?;
            }
            ctx.record_call(CallRecord {
                seq,
                parent_seq: None,
                function: function.to_string(),
                args,
                result: Value::Null,
                duration_ms,
                token_usage: None,
                timestamp: started,
                error: Some(message.clone()),
            });
            if let Some(id) = host_operation {
                ctx.run_host_operation_completion_safepoint(id)?;
            }
            Err(anyhow::anyhow!(message))
        }
    }
}

fn replay_completed_host_operation(
    ctx: &RuntimeContext,
    seq: u64,
    function: &str,
    kind: PendingHostOperationKind,
    args: &Value,
) -> Result<Option<Value>> {
    let Some(record) = ctx.completed_host_operation(seq, kind) else {
        return Ok(None);
    };

    // A completed operation exists at this (seq, kind) but was recorded with
    // different arguments: the agent code (or its inputs) changed since the
    // recording. Historically this silently fell through to a live
    // re-execution of the side effect — surface it as a divergence instead,
    // unless the operator explicitly opted into the lax behavior.
    if !crate::runtime::snapshot::completed_args_match(&record.operation.args, args) {
        if crate::runtime::context::replay_lax() {
            tracing::warn!(
                "replay divergence at seq {seq} tolerated (CHIDORI_REPLAY_LAX=1): completed \
                 `{function}` was recorded with different arguments; re-executing live"
            );
            return Ok(None);
        }
        anyhow::bail!(
            "Replay divergence at seq {}: completed `{}` was recorded with arguments {} but the \
             agent now calls it with {}.{} The agent code (or its inputs/configuration) changed \
             since the checkpoint was saved — re-run without replay to regenerate, or set \
             CHIDORI_REPLAY_LAX=1 to tolerate argument drift and re-execute the effect live.",
            seq,
            function,
            crate::runtime::context::truncate_json_for_error(&record.operation.args),
            crate::runtime::context::truncate_json_for_error(args),
            crate::runtime::context::describe_args_divergence(&record.operation.args, args)
        );
    }

    match record.state {
        HostPromiseState::Resolved {
            value,
            completed_at,
        } => {
            ctx.record_call(CallRecord {
                seq,
                parent_seq: None,
                function: function.to_string(),
                args: args.clone(),
                result: value.clone(),
                duration_ms: 0,
                token_usage: None,
                timestamp: completed_at,
                error: None,
            });
            Ok(Some(value))
        }
        HostPromiseState::Rejected {
            error,
            completed_at,
        } => {
            ctx.record_call(CallRecord {
                seq,
                parent_seq: None,
                function: function.to_string(),
                args: args.clone(),
                result: Value::Null,
                duration_ms: 0,
                token_usage: None,
                timestamp: completed_at,
                error: Some(error.clone()),
            });
            Err(anyhow::anyhow!(error))
        }
        HostPromiseState::Pending => Ok(None),
    }
}

pub fn execute_log(args: &Value) -> Result<Value> {
    let message = args
        .get("message")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("log requires string message"))?;
    let fields = args.get("fields").and_then(Value::as_object);

    match fields {
        Some(fields) if !fields.is_empty() => {
            let fields_str = serde_json::to_string(&Value::Object(fields.clone()))?;
            tracing::info!(message = message, fields = %fields_str);
        }
        _ => tracing::info!("{}", message),
    }

    Ok(Value::Null)
}

fn host_operation_kind(function: &str) -> Option<PendingHostOperationKind> {
    match function {
        "prompt" => Some(PendingHostOperationKind::Prompt),
        "input" => Some(PendingHostOperationKind::Input),
        "tool" => Some(PendingHostOperationKind::Tool),
        "call_agent" => Some(PendingHostOperationKind::CallAgent),
        "http" => Some(PendingHostOperationKind::Http),
        "template" => Some(PendingHostOperationKind::Template),
        "memory" => Some(PendingHostOperationKind::Memory),
        "mark" => Some(PendingHostOperationKind::Checkpoint),
        "log" => Some(PendingHostOperationKind::Log),
        "signal" | "poll_signal" | "signal_any" => Some(PendingHostOperationKind::Signal),
        _ => None,
    }
}

pub fn execute_input(ctx: &RuntimeContext, args: &Value) -> Result<Value> {
    let prompt = args
        .get("prompt")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("input requires string prompt"))?
        .to_string();
    // Author-declared options (`type`, `choices`, `default`, `details`).
    // Behavioral only: the durable shapes below stay normalized to
    // `{"prompt"}` so existing checkpoints and pending-operation records
    // replay unchanged.
    let default_answer = args
        .get("opts")
        .and_then(|o| o.get("default"))
        .and_then(Value::as_str)
        .map(str::to_string);
    // `details` is the artifact under review — a draft, a diff, a report —
    // shown to the human alongside the question so an approval gate is never
    // blind. Rendered by the CLI and carried on the server's pending state;
    // never part of the durable record shape.
    let details = args
        .get("opts")
        .and_then(|o| o.get("details"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let normalized = json!({ "prompt": prompt });
    let seq = ctx.next_seq();
    // Both the live paths and the server's synthetic resume record store the
    // normalized `{"prompt"}` shape, so that is what divergence compares.
    if let Some(record) = ctx
        .try_replay_checked(seq, "input", &normalized)
        .map_err(|err| anyhow::anyhow!(err))?
    {
        return Ok(record.result);
    }

    if let Some(result) = replay_completed_host_operation(
        ctx,
        seq,
        "input",
        PendingHostOperationKind::Input,
        &normalized,
    )? {
        return Ok(result);
    }

    let host_operation = ctx.begin_host_operation_with_function(
        seq,
        PendingHostOperationKind::Input,
        Some("input".to_string()),
        normalized.clone(),
    );
    ctx.run_host_operation_safepoint(host_operation)?;
    match ctx.input_mode() {
        InputMode::Stdin => {
            if let Some(ref details) = details {
                eprintln!("--- details ---\n{details}\n---------------");
            }
            eprintln!("{}", prompt);
            let mut line = String::new();
            let bytes_read = std::io::stdin().read_line(&mut line)?;
            let mut response = line.trim_end_matches(&['\r', '\n'][..]).to_string();
            // An empty answer — blank enter, or EOF in a non-interactive run —
            // takes the author-declared default. EOF with no default is an
            // error: silently proceeding with "" would send the agent down a
            // branch the author never chose.
            if response.is_empty() {
                if let Some(default) = default_answer {
                    response = default;
                } else if bytes_read == 0 {
                    anyhow::bail!(
                        "input() reached end-of-file on stdin with no `default` declared \
                         (prompt: {prompt:?}). Provide an answer on stdin, declare a \
                         `default` in the input options, or run under `chidori serve` \
                         where input() pauses the session instead."
                    );
                }
            }
            ctx.resolve_host_operation(host_operation, Value::String(response.clone()))?;
            ctx.record_call(CallRecord {
                seq,
                parent_seq: None,
                function: "input".to_string(),
                args: json!({ "prompt": prompt }),
                result: Value::String(response.clone()),
                duration_ms: 0,
                token_usage: None,
                timestamp: Utc::now(),
                error: None,
            });
            ctx.run_host_operation_completion_safepoint(host_operation)?;
            Ok(Value::String(response))
        }
        InputMode::Pause => {
            // Warm resume (server flow): keep the LIVE VM parked on this
            // thread and wait for the response instead of unwinding — the
            // continuation costs O(1) rather than an O(history) replay
            // re-execution. Durability is unchanged: the pending op is
            // already on disk (begin + safepoint above), so a crash while
            // parked resumes by replay exactly as an unwound pause would,
            // and `Park` (eviction / capacity / no supervisor) falls back
            // to that path immediately. The record written here is the
            // same one the replay path's synthetic injection produces.
            if let Some(bridge) = ctx.warm_input_bridge() {
                let pending = PendingInput {
                    seq,
                    prompt: prompt.clone(),
                    details: details.clone(),
                };
                match bridge.wait(ctx, &pending) {
                    WarmInputWait::Delivered(response) => {
                        let result = Value::String(response);
                        ctx.resolve_host_operation(host_operation, result.clone())?;
                        ctx.record_call(CallRecord {
                            seq,
                            parent_seq: None,
                            function: "input".to_string(),
                            args: json!({ "prompt": prompt }),
                            result: result.clone(),
                            duration_ms: 0,
                            token_usage: None,
                            timestamp: Utc::now(),
                            error: None,
                        });
                        ctx.run_host_operation_completion_safepoint(host_operation)?;
                        return Ok(result);
                    }
                    WarmInputWait::Park => {}
                }
            }
            ctx.set_pending_input(PendingInput {
                seq,
                prompt: prompt.clone(),
                details,
            });
            Err(anyhow::Error::new(RunInterrupt::Input { prompt }))
        }
    }
}

/// Pull the required `name` string from a signal binding's args
/// (`{ "name": <string>, "opts": <json|null> }`), with a clear host-side error
/// when it is missing or not a string. Shared by `execute_signal` /
/// `execute_poll_signal`.
fn signal_name(function: &str, args: &Value) -> Result<String> {
    args.get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("chidori.{function} requires a string name"))
}

/// Pull the required non-empty `names` string array from a `signalAny`
/// binding's args (`{ "names": [<string>...], "opts": <json|null> }`).
fn signal_names(args: &Value) -> Result<Vec<String>> {
    let names: Vec<String> = args
        .get("names")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|v| v.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
        })
        .ok_or_else(|| anyhow::anyhow!("chidori.signal fan-in requires an array of names"))?
        .ok_or_else(|| anyhow::anyhow!("chidori.signal fan-in names must all be strings"))?;
    if names.is_empty() {
        anyhow::bail!("chidori.signal fan-in requires at least one name");
    }
    Ok(names)
}

/// Optional positive `timeoutMs` from a signal binding's opts
/// (`{ ..., "opts": { "timeoutMs": <number> } }`). The runtime only records it
/// on the pause; enforcement (resolving the pause with the
/// [`signal_timeout_sentinel`] after the deadline) is the supervising server's
/// job, since a paused run is not a live task.
fn signal_timeout_ms(args: &Value) -> Option<u64> {
    args.get("opts")
        .and_then(|opts| opts.get("timeoutMs"))
        .and_then(Value::as_u64)
        .filter(|ms| *ms > 0)
}

/// The result a timed-out signal listen point resolves to (`docs/signals.md`
/// §16, pinned: resolve-to-sentinel rather than reject). Shaped like a signal
/// with `payload`/`from` nulled and `timedOut: true`, so agent code
/// discriminates with `"timedOut" in result`. `name` is the single awaited
/// name, or `null` for a multi-name `signalAny` (no name fired).
pub fn signal_timeout_sentinel(names: &[String]) -> Value {
    let name = match names {
        [single] => Value::String(single.clone()),
        _ => Value::Null,
    };
    json!({
        "name": name,
        "payload": Value::Null,
        "from": Value::Null,
        "timedOut": true,
    })
}

/// `chidori.signal(name, opts)` — the blocking, named, externally-deliverable
/// flavor of `input`, fronted by a durable per-run mailbox. See
/// `docs/signals.md` §5/§8.3. Order:
///   1. replay short-circuit (`try_replay_checked`) — a recorded result wins,
///      so a replay never reads the inbox;
///   2. completed-host-op check (`(seq, Signal, {name})`) — a delivered-via-
///      resume signal that post-dates the journal;
///   3. mailbox drain (`take_queued_signal`) — consume a pre-arrived signal
///      WITHOUT pausing, recording the `{name,payload,from}` result;
///   4. otherwise PAUSE: set `PendingSignal` and bail with `PAUSE_MARKER` (the
///      pause *type* is distinguished from `input` by which pending slot is set).
///
/// The durable match key is `{ "name": name }` only — the payload is unknown at
/// pause time and rides in the result.
///
/// Resolve a signal-family listen point in place: mark the host op resolved,
/// append the journal record (the same shape a queued drain, a server-side
/// synthetic resolution, or a timeout sentinel produces), and run the
/// completion safepoint. Shared by the queued-inbox hit, the actor inline
/// wait, and the inline timeout.
fn settle_signal_listen(
    ctx: &RuntimeContext,
    host_operation: crate::runtime::snapshot::HostOperationId,
    seq: u64,
    function: &str,
    match_args: Value,
    result: Value,
) -> Result<Value> {
    ctx.resolve_host_operation(host_operation, result.clone())?;
    ctx.record_call(CallRecord {
        seq,
        parent_seq: None,
        function: function.to_string(),
        args: match_args,
        result: result.clone(),
        duration_ms: 0,
        token_usage: None,
        timestamp: Utc::now(),
        error: None,
    });
    ctx.run_host_operation_completion_safepoint(host_operation)?;
    Ok(result)
}

/// Actor fast path for a listen point whose inbox is empty: block in place
/// for the next matching delivery on the actor's shared mailbox instead of
/// parking the actor and re-executing its whole history per message (the
/// O(messages²) supervision loop). Returns `Some(result)` when the wait
/// settled the listen point inline (message or timeout sentinel); `None` when
/// the actor should park through the ordinary pause path (stop/idle), or when
/// no waiter is installed (non-actor contexts).
fn actor_inline_signal_wait(
    ctx: &RuntimeContext,
    host_operation: crate::runtime::snapshot::HostOperationId,
    seq: u64,
    function: &str,
    names: &[String],
    timeout_ms: Option<u64>,
    match_args: &Value,
) -> Result<Option<Value>> {
    let Some(waiter) = ctx.actor_signal_waiter() else {
        return Ok(None);
    };
    match waiter.wait(names, timeout_ms) {
        ActorSignalWait::Delivered => {
            // The waiter observed a matching message in the shared mailbox;
            // pump it into the run-level inbox and drain through the same
            // path a pre-queued signal takes (ordering + durable-inbox
            // semantics included).
            crate::runtime::host_actor::pump_own_mailbox(ctx);
            if let Some(queued) = ctx.take_queued_signal_any(names) {
                let result = json!({
                    "name": queued.name,
                    "payload": queued.payload,
                    "from": queued.from,
                });
                return settle_signal_listen(
                    ctx,
                    host_operation,
                    seq,
                    function,
                    match_args.clone(),
                    result,
                )
                .map(Some);
            }
            // Defensive: the drain missed (should not happen — the actor
            // thread is the mailbox's only consumer). Park via the pause
            // path, which handles it exactly as an empty mailbox.
            Ok(None)
        }
        ActorSignalWait::TimedOut => settle_signal_listen(
            ctx,
            host_operation,
            seq,
            function,
            match_args.clone(),
            signal_timeout_sentinel(names),
        )
        .map(Some),
        ActorSignalWait::Park => Ok(None),
    }
}

pub fn execute_signal(ctx: &RuntimeContext, args: &Value) -> Result<Value> {
    let name = signal_name("signal", args)?;
    let match_args = json!({ "name": name });
    let seq = ctx.next_seq();

    if let Some(record) = ctx
        .try_replay_checked(seq, "signal", &match_args)
        .map_err(|err| anyhow::anyhow!(err))?
    {
        return Ok(record.result);
    }

    if let Some(result) = replay_completed_host_operation(
        ctx,
        seq,
        "signal",
        PendingHostOperationKind::Signal,
        &match_args,
    )? {
        return Ok(result);
    }

    let host_operation = ctx.begin_host_operation_with_function(
        seq,
        PendingHostOperationKind::Signal,
        Some("signal".to_string()),
        match_args.clone(),
    );
    ctx.run_host_operation_safepoint(host_operation)?;

    if let Some(queued) = ctx.take_queued_signal(&name) {
        let result = json!({
            "name": queued.name,
            "payload": queued.payload,
            "from": queued.from,
        });
        ctx.resolve_host_operation(host_operation, result.clone())?;
        ctx.record_call(CallRecord {
            seq,
            parent_seq: None,
            function: "signal".to_string(),
            args: match_args,
            result: result.clone(),
            duration_ms: 0,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        });
        ctx.run_host_operation_completion_safepoint(host_operation)?;
        return Ok(result);
    }

    let names = vec![name.clone()];
    if let Some(result) = actor_inline_signal_wait(
        ctx,
        host_operation,
        seq,
        "signal",
        &names,
        signal_timeout_ms(args),
        &match_args,
    )? {
        return Ok(result);
    }

    ctx.set_pending_signal(PendingSignal {
        seq,
        name: name.clone(),
        names,
        timeout_ms: signal_timeout_ms(args),
        id: host_operation,
    });
    Err(anyhow::Error::new(RunInterrupt::Signal { name }))
}

/// `chidori.signal(names[], opts)` — the fan-in listen point (`docs/signals.md`
/// §6.1): pause until ANY of the named signals is delivered (or one is already
/// queued). Same shape as `execute_signal` with the match key `{ "names":
/// [...] }` and function name `"signal_any"`. The result is the bare consumed
/// signal `{name, payload, from}` — its `name` says which fired (§16, pinned).
/// The mailbox drain takes the lowest-`delivery_seq` entry across the whole
/// name set, freezing arrival order into the recorded result.
pub fn execute_signal_any(ctx: &RuntimeContext, args: &Value) -> Result<Value> {
    let names = signal_names(args)?;
    let match_args = json!({ "names": names });
    let seq = ctx.next_seq();

    if let Some(record) = ctx
        .try_replay_checked(seq, "signal_any", &match_args)
        .map_err(|err| anyhow::anyhow!(err))?
    {
        return Ok(record.result);
    }

    if let Some(result) = replay_completed_host_operation(
        ctx,
        seq,
        "signal_any",
        PendingHostOperationKind::Signal,
        &match_args,
    )? {
        return Ok(result);
    }

    let host_operation = ctx.begin_host_operation_with_function(
        seq,
        PendingHostOperationKind::Signal,
        Some("signal_any".to_string()),
        match_args.clone(),
    );
    ctx.run_host_operation_safepoint(host_operation)?;

    if let Some(queued) = ctx.take_queued_signal_any(&names) {
        let result = json!({
            "name": queued.name,
            "payload": queued.payload,
            "from": queued.from,
        });
        ctx.resolve_host_operation(host_operation, result.clone())?;
        ctx.record_call(CallRecord {
            seq,
            parent_seq: None,
            function: "signal_any".to_string(),
            args: match_args,
            result: result.clone(),
            duration_ms: 0,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        });
        ctx.run_host_operation_completion_safepoint(host_operation)?;
        return Ok(result);
    }

    if let Some(result) = actor_inline_signal_wait(
        ctx,
        host_operation,
        seq,
        "signal_any",
        &names,
        signal_timeout_ms(args),
        &match_args,
    )? {
        return Ok(result);
    }

    ctx.set_pending_signal(PendingSignal {
        seq,
        name: names[0].clone(),
        names: names.clone(),
        timeout_ms: signal_timeout_ms(args),
        id: host_operation,
    });
    Err(anyhow::Error::new(RunInterrupt::SignalAny { names }))
}

/// `chidori.pollSignal(name)` — non-blocking signal consumption (`docs/signals.md`
/// §6.1). Same replay/completed-host-op checks as `execute_signal` (function
/// name `"poll_signal"`, kind `Signal`, match key `{name}`), then a single
/// mailbox drain: records the `{name,payload,from}` object if a matching signal
/// is queued, or JSON `null` if not. The result is ALWAYS recorded at this seq —
/// so a poll that found nothing replays as `null` deterministically — and it
/// never pauses.
pub fn execute_poll_signal(ctx: &RuntimeContext, args: &Value) -> Result<Value> {
    let name = signal_name("pollSignal", args)?;
    let match_args = json!({ "name": name });
    let seq = ctx.next_seq();

    if let Some(record) = ctx
        .try_replay_checked(seq, "poll_signal", &match_args)
        .map_err(|err| anyhow::anyhow!(err))?
    {
        return Ok(record.result);
    }

    if let Some(result) = replay_completed_host_operation(
        ctx,
        seq,
        "poll_signal",
        PendingHostOperationKind::Signal,
        &match_args,
    )? {
        return Ok(result);
    }

    let host_operation = ctx.begin_host_operation_with_function(
        seq,
        PendingHostOperationKind::Signal,
        Some("poll_signal".to_string()),
        match_args.clone(),
    );
    ctx.run_host_operation_safepoint(host_operation)?;

    let result = match ctx.take_queued_signal(&name) {
        Some(queued) => json!({
            "name": queued.name,
            "payload": queued.payload,
            "from": queued.from,
        }),
        None => Value::Null,
    };
    ctx.resolve_host_operation(host_operation, result.clone())?;
    ctx.record_call(CallRecord {
        seq,
        parent_seq: None,
        function: "poll_signal".to_string(),
        args: match_args,
        result: result.clone(),
        duration_ms: 0,
        token_usage: None,
        timestamp: Utc::now(),
        error: None,
    });
    ctx.run_host_operation_completion_safepoint(host_operation)?;
    Ok(result)
}

/// First half of `chidori.step(name, fn)` — the durable value checkpoint
/// (`docs/value-checkpoints.md`). The VM-side binding calls this *before*
/// running the callback:
///   1. replay short-circuit (`try_replay_checked(seq, "step")`) — a recorded
///      result (or recorded error) is returned as `{cached: true, ...}` and the
///      callback is never run. The recorded `name` must match, else the agent
///      code moved/renamed steps before the resume point (fail-loud divergence,
///      same contract as `try_replay_checked`'s function-name check).
///   2. otherwise mark the step live (`begin_step`) and return
///      `{cached: false}` — the binding runs the callback and reports its
///      result through [`execute_step_end`], which writes the record at this
///      same `seq`.
///
/// While the step is live, every other host effect is refused (the dispatchers
/// check `active_step_name`), so the callback is guaranteed pure compute and
/// skipping it on replay cannot desynchronize the journal. Steps never pause
/// and need no pending host operation: a crash between begin and end simply
/// re-runs the (deterministic) callback on resume.
pub fn execute_step_begin(ctx: &RuntimeContext, args: &Value) -> Result<Value> {
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("chidori.step requires a string name"))?
        .to_string();
    if let Some(active) = ctx.active_step_name() {
        anyhow::bail!(
            "chidori.step(\"{name}\") cannot start inside chidori.step(\"{active}\"): \
             step callbacks must be pure, synchronous computation"
        );
    }
    let seq = ctx.next_seq();

    if let Some(record) = ctx
        .try_replay_checked(seq, "step", &json!({ "name": name }))
        .map_err(|err| anyhow::anyhow!(err))?
    {
        let recorded_name = record
            .args
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if recorded_name != name {
            anyhow::bail!(
                "Replay divergence at seq {seq}: checkpoint has step \"{recorded_name}\" \
                 but agent called step \"{name}\". The agent code changed since the \
                 checkpoint was saved — re-run without replay to regenerate."
            );
        }
        return Ok(match record.error {
            Some(error) => json!({ "cached": true, "error": error }),
            None => json!({ "cached": true, "value": record.result }),
        });
    }

    ctx.begin_step(seq, &name);
    Ok(json!({ "cached": false }))
}

/// Second half of `chidori.step(name, fn)` — record the callback's outcome at
/// the seq [`execute_step_begin`] reserved. `args` carries either `value` (the
/// callback's JSON-serialized return value) or `error` (its thrown message).
/// Returns the canonical (JSON round-tripped) value so live and replayed runs
/// observe byte-identical results.
pub fn execute_step_end(ctx: &RuntimeContext, args: &Value) -> Result<Value> {
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("chidori.step requires a string name"))?;
    let Some(step) = ctx.take_active_step() else {
        anyhow::bail!("chidori.step internal error: step_end(\"{name}\") without an active step");
    };
    if step.name != name {
        anyhow::bail!(
            "chidori.step internal error: step_end(\"{name}\") while step \"{}\" is active",
            step.name
        );
    }
    let duration_ms = Utc::now()
        .signed_duration_since(step.started)
        .num_milliseconds()
        .max(0) as u64;
    let error = args.get("error").and_then(Value::as_str);
    let result = match error {
        Some(_) => Value::Null,
        None => args.get("value").cloned().unwrap_or(Value::Null),
    };
    ctx.record_call(CallRecord {
        seq: step.seq,
        parent_seq: None,
        function: "step".to_string(),
        args: json!({ "name": name }),
        result: result.clone(),
        duration_ms,
        token_usage: None,
        timestamp: step.started,
        error: error.map(str::to_string),
    });
    Ok(result)
}

/// Whether (and how long) a prompt request should mark cacheable prefix
/// boundaries. The default is on with the 5-minute TTL — caching is a billing
/// optimization that never changes a response, so it is safe by default; a
/// single call opts out with `cache: false` in its prompt options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePosture {
    Auto(CacheTtl),
    Disabled,
}

impl Default for CachePosture {
    fn default() -> Self {
        CachePosture::Auto(CacheTtl::FiveMinutes)
    }
}

/// Parse the cache posture from prompt options: `cache: false` disables,
/// `cache: true` / absent uses the default 5m TTL, `cache: "1h"` or
/// `cache: { ttl: "1h" }` selects the extended TTL.
pub fn cache_posture_from_options(options: &Value) -> CachePosture {
    let Some(cache) = options.get("cache") else {
        return CachePosture::default();
    };
    let ttl_str = |s: &str| match s {
        "1h" => Some(CacheTtl::OneHour),
        "5m" => Some(CacheTtl::FiveMinutes),
        _ => None,
    };
    match cache {
        Value::Bool(false) => CachePosture::Disabled,
        Value::Bool(true) | Value::Null => CachePosture::default(),
        Value::String(s) => CachePosture::Auto(ttl_str(s).unwrap_or(CacheTtl::FiveMinutes)),
        Value::Object(map) => CachePosture::Auto(
            map.get("ttl")
                .and_then(Value::as_str)
                .and_then(ttl_str)
                .unwrap_or(CacheTtl::FiveMinutes),
        ),
        _ => CachePosture::default(),
    }
}

/// Default auto-marking of cacheable prefix boundaries (the zero-author win):
/// the system block and the tool schemas are stable for a whole run/loop, so
/// they are always marked; the conversation head (the latest message) is
/// marked when a follow-up request sharing the prefix is plausible — a
/// tool-use loop is coming (`tools` non-empty) or the request is already
/// multi-turn (a context/conversation being extended). Explicit marks placed
/// by the author are never overridden. A pure function of the request shape,
/// so the same request always produces the same layout (replay-stable).
pub fn auto_mark_prompt_cache(request: &mut LlmRequest, posture: CachePosture) {
    let CachePosture::Auto(ttl) = posture else {
        return;
    };
    if request.system.is_some() && request.cache.system.is_none() {
        request.cache.system = Some(ttl);
    }
    if !request.tools.is_empty() && request.cache.tools.is_none() {
        request.cache.tools = Some(ttl);
    }
    let multi_turn = request.messages.len() >= 2 || !request.tools.is_empty();
    if multi_turn {
        if let Some(last) = request
            .messages
            .last_mut()
            .filter(|m| m.cache_control.is_none())
        {
            last.cache_control = Some(ttl);
        }
    }
}

/// Content digest of the fully assembled prompt request (model, system, tools,
/// messages, cache layout). Recorded in the prompt `CallRecord.args` so the
/// durable log is self-describing — replay still matches on `(seq, function)`
/// and ignores it. Computed over canonical JSON (serde_json sorts object keys)
/// and versioned so a future canonicalization change can't silently collide.
pub fn prompt_request_digest(request: &LlmRequest) -> String {
    let canonical = json!({
        "v": 1,
        "model": request.model,
        "system": request.system,
        "messages": request.messages,
        "tools": request.tools,
        "cache": {
            "system": request.cache.system,
            "tools": request.cache.tools,
        },
        "max_tokens": request.max_tokens,
        "temperature": request.temperature,
    });
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string().as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Apply the runtime context's model override (Pi-style save point) to an
/// outgoing prompt request. A no-op unless the host installed an override hook
/// that currently yields a model. This is the single point where a mid-run
/// model change takes effect for every prompt path — the native agent loop and
/// the TypeScript interactive engine both call the prompt bindings below.
fn apply_model_override(ctx: &RuntimeContext, request: &mut LlmRequest) {
    if let Some(model) = ctx.resolve_model_override() {
        request.model = model;
    }
}

/// Look up a prompt in the opt-in local content-addressed cache
/// (`prompt_cache`, Phase 3 of `docs/context-management.md`). Consulted on
/// the live path only, after the replay short-circuit and completed-operation
/// replay have both declined, so it can never shadow the call log. The digest
/// is recomputed here because `apply_model_override` may have changed the
/// model after the caller stamped `request_digest` into its args — the cache
/// must key on the request actually sent.
fn local_prompt_cache_lookup(request: &LlmRequest) -> Option<LlmResponse> {
    if !crate::runtime::prompt_cache::enabled() {
        return None;
    }
    crate::runtime::prompt_cache::lookup(&prompt_request_digest(request))
        .and_then(|value| llm_response_from_json(&value))
}

/// Complete a prompt host operation with a locally cached response: identical
/// begin/safepoint/resolve/record/completion sequence to a live provider
/// success, with the same `result` the provider would have produced.
/// `token_usage` stays `None` because this run paid no tokens for it.
fn complete_prompt_from_local_cache(
    ctx: &RuntimeContext,
    seq: u64,
    args: Value,
    result: Value,
) -> Result<()> {
    let host_operation = ctx.begin_host_operation_with_function(
        seq,
        PendingHostOperationKind::Prompt,
        Some("prompt".to_string()),
        args.clone(),
    );
    ctx.run_host_operation_safepoint(host_operation)?;
    ctx.resolve_host_operation(host_operation, result.clone())?;
    ctx.record_call(CallRecord {
        seq,
        parent_seq: None,
        function: "prompt".to_string(),
        args,
        result,
        duration_ms: 0,
        token_usage: None,
        timestamp: Utc::now(),
        error: None,
    });
    ctx.run_host_operation_completion_safepoint(host_operation)?;
    Ok(())
}

/// Surface a hard-to-see failure mode: a response cut off by the output-token
/// cap. `chidori.prompt()` returns a bare string, so without this the
/// truncation is invisible — the cut-off text flows on as if complete.
/// Reasoning models make this likely: hidden reasoning spends the same
/// `maxTokens` budget before any visible output.
fn warn_if_truncated(response: &LlmResponse, seq: u64, max_tokens: u64) {
    if matches!(response.stop_reason.as_str(), "length" | "max_tokens") {
        eprintln!(
            "chidori: warning: prompt (seq {seq}) hit the {max_tokens}-token output cap \
             (stop reason `{}`) — the response is truncated mid-generation. Raise \
             `maxTokens` in the prompt options; reasoning models also spend this budget \
             on hidden reasoning before visible output.",
            response.stop_reason
        );
        tracing::warn!(
            seq,
            max_tokens,
            stop_reason = %response.stop_reason,
            "prompt response truncated at the output-token cap"
        );
    }
}

pub fn execute_prompt_text(
    ctx: &RuntimeContext,
    providers: &ProviderRegistry,
    tokio_rt: &tokio::runtime::Runtime,
    mut request: LlmRequest,
    args: Value,
    prompt_type: Option<String>,
) -> Result<Value> {
    apply_model_override(ctx, &mut request);
    let seq = ctx.next_seq();
    if let Some(record) = ctx
        .try_replay_checked(seq, "prompt", &args)
        .map_err(|err| anyhow::anyhow!(err))?
    {
        return Ok(record.result);
    }

    if let Some(result) = replay_completed_host_operation(
        ctx,
        seq,
        "prompt",
        PendingHostOperationKind::Prompt,
        &args,
    )? {
        return Ok(result);
    }

    if let Some(cached) = local_prompt_cache_lookup(&request) {
        let result = Value::String(cached.content);
        complete_prompt_from_local_cache(ctx, seq, args, result.clone())?;
        return Ok(result);
    }

    let host_operation = ctx.begin_host_operation_with_function(
        seq,
        PendingHostOperationKind::Prompt,
        Some("prompt".to_string()),
        args.clone(),
    );
    ctx.run_host_operation_safepoint(host_operation)?;
    let started = Utc::now();
    let response = send_prompt_request(ctx, providers, tokio_rt, seq, &request, prompt_type)?;
    let duration_ms = Utc::now()
        .signed_duration_since(started)
        .num_milliseconds()
        .max(0) as u64;

    match response {
        Ok(response) => {
            warn_if_truncated(&response, seq, request.max_tokens);
            if crate::runtime::prompt_cache::enabled() {
                crate::runtime::prompt_cache::store(
                    &prompt_request_digest(&request),
                    &llm_response_to_json(&response),
                );
            }
            let result = Value::String(response.content.clone());
            ctx.resolve_host_operation(host_operation, result.clone())?;
            ctx.record_call(CallRecord {
                seq,
                parent_seq: None,
                function: "prompt".to_string(),
                args,
                result: result.clone(),
                duration_ms,
                token_usage: Some(TokenUsage::from_response(&response)),
                timestamp: started,
                error: None,
            });
            ctx.run_host_operation_completion_safepoint(host_operation)?;
            Ok(result)
        }
        Err(err) => {
            let message = err.to_string();
            ctx.reject_host_operation(host_operation, message.clone())?;
            ctx.record_call(CallRecord {
                seq,
                parent_seq: None,
                function: "prompt".to_string(),
                args,
                result: Value::Null,
                duration_ms,
                token_usage: None,
                timestamp: started,
                error: Some(message.clone()),
            });
            ctx.run_host_operation_completion_safepoint(host_operation)?;
            Err(anyhow::anyhow!(message))
        }
    }
}

pub fn execute_prompt_response(
    ctx: &RuntimeContext,
    providers: &ProviderRegistry,
    tokio_rt: &tokio::runtime::Runtime,
    mut request: LlmRequest,
    args: Value,
    prompt_type: Option<String>,
) -> Result<LlmResponse> {
    apply_model_override(ctx, &mut request);
    let seq = ctx.next_seq();
    if let Some(record) = ctx
        .try_replay_checked(seq, "prompt", &args)
        .map_err(|err| anyhow::anyhow!(err))?
    {
        return llm_response_from_json(&record.result).ok_or_else(|| {
            anyhow::anyhow!("cached prompt record at seq {seq} is not a tool-use turn")
        });
    }

    if let Some(result) = replay_completed_host_operation(
        ctx,
        seq,
        "prompt",
        PendingHostOperationKind::Prompt,
        &args,
    )? {
        return llm_response_from_json(&result).ok_or_else(|| {
            anyhow::anyhow!("completed prompt host operation at seq {seq} is not a tool-use turn")
        });
    }

    if let Some(cached) = local_prompt_cache_lookup(&request) {
        complete_prompt_from_local_cache(ctx, seq, args, llm_response_to_json(&cached))?;
        return Ok(cached);
    }

    let host_operation = ctx.begin_host_operation_with_function(
        seq,
        PendingHostOperationKind::Prompt,
        Some("prompt".to_string()),
        args.clone(),
    );
    ctx.run_host_operation_safepoint(host_operation)?;
    let started = Utc::now();
    let response = send_prompt_request(ctx, providers, tokio_rt, seq, &request, prompt_type)?;
    let duration_ms = Utc::now()
        .signed_duration_since(started)
        .num_milliseconds()
        .max(0) as u64;

    match response {
        Ok(response) => {
            warn_if_truncated(&response, seq, request.max_tokens);
            let result = llm_response_to_json(&response);
            if crate::runtime::prompt_cache::enabled() {
                crate::runtime::prompt_cache::store(&prompt_request_digest(&request), &result);
            }
            ctx.resolve_host_operation(host_operation, result.clone())?;
            ctx.record_call(CallRecord {
                seq,
                parent_seq: None,
                function: "prompt".to_string(),
                args,
                result,
                duration_ms,
                token_usage: Some(TokenUsage::from_response(&response)),
                timestamp: started,
                error: None,
            });
            ctx.run_host_operation_completion_safepoint(host_operation)?;
            Ok(response)
        }
        Err(err) => {
            let message = err.to_string();
            ctx.reject_host_operation(host_operation, message.clone())?;
            ctx.record_call(CallRecord {
                seq,
                parent_seq: None,
                function: "prompt".to_string(),
                args,
                result: Value::Null,
                duration_ms,
                token_usage: None,
                timestamp: started,
                error: Some(message.clone()),
            });
            ctx.run_host_operation_completion_safepoint(host_operation)?;
            Err(anyhow::anyhow!(message))
        }
    }
}

fn send_prompt_request(
    ctx: &RuntimeContext,
    providers: &ProviderRegistry,
    tokio_rt: &tokio::runtime::Runtime,
    seq: u64,
    request: &LlmRequest,
    prompt_type: Option<String>,
) -> Result<anyhow::Result<LlmResponse>> {
    if ctx.has_event_sender() {
        let stream_id = ctx.begin_prompt_stream(seq, prompt_type.clone(), request.model.clone());
        let ctx_for_cb = ctx.clone();
        let stream_id_for_cb = stream_id.clone();
        let prompt_type_for_cb = prompt_type.clone();
        let mut sink: TokenSink = Box::new(move |delta: &str| {
            if let Some(stream_id) = stream_id_for_cb.clone() {
                ctx_for_cb.emit_prompt_delta(
                    stream_id,
                    seq,
                    prompt_type_for_cb.clone(),
                    delta.to_string(),
                );
            }
        });
        let response = tokio_rt.block_on(async { providers.stream(request, &mut sink).await });
        if let Some(stream_id) = stream_id {
            ctx.end_prompt_stream(
                stream_id,
                seq,
                prompt_type,
                response.as_ref().err().map(|err| err.to_string()),
            );
        }
        Ok(response)
    } else {
        Ok(tokio_rt.block_on(async { providers.send(request).await }))
    }
}

pub fn llm_response_to_json(response: &LlmResponse) -> Value {
    let mut value = json!({
        "content": response.content,
        "blocks": response.blocks,
        "toolCalls": response.tool_calls.iter().map(|call| json!({
            "id": call.id,
            "name": call.name,
            "input": call.input,
        })).collect::<Vec<_>>(),
        "stopReason": response.stop_reason,
        "inputTokens": response.input_tokens,
        "outputTokens": response.output_tokens,
        "cacheCreationTokens": response.cache_creation_tokens,
        "cacheReadTokens": response.cache_read_tokens,
    });
    // Only present for reasoning models; omitted otherwise so existing
    // checkpoints and consumers see an unchanged shape.
    if let Some(ref reasoning) = response.reasoning {
        value["reasoning"] = json!(reasoning);
    }
    value
}

pub fn llm_response_from_json(value: &Value) -> Option<LlmResponse> {
    Some(LlmResponse {
        content: value.get("content")?.as_str()?.to_string(),
        blocks: serde_json::from_value::<Vec<ContentBlock>>(value.get("blocks")?.clone()).ok()?,
        tool_calls: value
            .get("toolCalls")?
            .as_array()?
            .iter()
            .filter_map(|call| {
                Some(ToolCall {
                    id: call.get("id")?.as_str()?.to_string(),
                    name: call.get("name")?.as_str()?.to_string(),
                    input: call.get("input").cloned().unwrap_or(Value::Null),
                })
            })
            .collect(),
        stop_reason: value.get("stopReason")?.as_str()?.to_string(),
        input_tokens: value.get("inputTokens")?.as_u64()?,
        output_tokens: value.get("outputTokens")?.as_u64()?,
        cache_creation_tokens: value
            .get("cacheCreationTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_read_tokens: value
            .get("cacheReadTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        reasoning: value
            .get("reasoning")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

pub fn execute_memory(args: &Value) -> Result<Value> {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    let namespace = args
        .get("namespace")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let key = args.get("key").and_then(Value::as_str);
    let value = args.get("value").filter(|value| !value.is_null());
    let prefix = args.get("prefix").and_then(Value::as_str).unwrap_or("");

    execute_memory_action(action, namespace, key, value, prefix)
}

pub fn execute_template(template_engine: &TemplateEngine, args: &Value) -> Result<Value> {
    let template = args
        .get("template")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("template requires string template"))?;
    let vars = args.get("vars").cloned().unwrap_or_else(|| json!({}));
    template_engine.render(template, &vars).map(Value::String)
}

pub fn execute_tool_call(
    ctx: &RuntimeContext,
    name: &str,
    kwargs: Value,
    live: impl FnOnce() -> Result<Value>,
) -> Result<Value> {
    execute_durable_json_call(
        ctx,
        "tool",
        json!({
            "name": name,
            "kwargs": kwargs,
        }),
        live,
    )
}

pub fn execute_tool_call_at_seq(
    ctx: &RuntimeContext,
    seq: u64,
    name: &str,
    kwargs: Value,
    live: impl FnOnce() -> Result<Value>,
) -> Result<Value> {
    execute_durable_json_call_at_seq(
        ctx,
        seq,
        "tool",
        json!({
            "name": name,
            "kwargs": kwargs,
        }),
        live,
    )
}

#[allow(dead_code)]
pub fn execute_native_tool_call(
    ctx: &RuntimeContext,
    registry: &ToolRegistry,
    name: &str,
    kwargs: Value,
) -> Result<Value> {
    execute_tool_call(ctx, name, kwargs.clone(), || {
        registry.dispatch_native(name, kwargs)
    })
}

#[allow(dead_code)]
pub fn execute_native_tool_call_at_seq(
    ctx: &RuntimeContext,
    seq: u64,
    registry: &ToolRegistry,
    name: &str,
    kwargs: Value,
) -> Result<Value> {
    execute_tool_call_at_seq(ctx, seq, name, kwargs.clone(), || {
        registry.dispatch_native(name, kwargs)
    })
}

pub fn execute_call_agent(
    ctx: &RuntimeContext,
    args: Value,
    live: impl FnOnce() -> Result<Value>,
) -> Result<Value> {
    execute_durable_json_call(ctx, "call_agent", args, live)
}

/// User-Agent that identifies chidori-issued requests to the wider internet.
/// Hosts like Wikimedia reject the bare `reqwest/X.Y` default with a 403 and a
/// link to their robot policy, so we ship a UA that names the runtime, its
/// version, and a contact URL by default. Callers can still override it by
/// including a `User-Agent` header in the http args.
const DEFAULT_USER_AGENT: &str = concat!(
    "chidori/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/ThousandBirdsInc/chidori)",
);

/// The process-wide HTTP client for the `http`/`fetch` host effect. Building
/// a `reqwest::Client` loads TLS roots and allocates a fresh connection pool
/// (~7 ms measured in `benches/runtime.rs`), and a per-call client also means
/// a fresh TCP+TLS handshake for every fetch — so it is built once and
/// shared; repeat fetches to the same host reuse pooled connections. The
/// default User-Agent is applied per request (not here) so callers can still
/// override it.
fn http_client() -> Result<reqwest::Client> {
    use std::sync::OnceLock;

    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    // `gzip(true)` (with the reqwest `gzip` feature) sends Accept-Encoding
    // and transparently decompresses gzipped response bodies, so tools get
    // readable text instead of a binary blob.
    //
    // The SSRF guard is baked into the client: the guarded DNS resolver
    // filters non-public addresses at resolution time (so DNS rebinding and
    // redirect hops are covered by the same check the connector dials from),
    // and the redirect policy re-validates each hop's scheme and IP-literal
    // host. See `runtime::ssrf`.
    let built = reqwest::Client::builder()
        .gzip(true)
        .dns_resolver(crate::runtime::ssrf::dns_resolver())
        .redirect(crate::runtime::ssrf::redirect_policy())
        .build()?;
    Ok(CLIENT.get_or_init(|| built).clone())
}

/// Rewrite `url` to the per-agent Mock Gateway when its host is listed in the
/// `CHIDORI_INTEGRATION_BASE_URLS` env map (`{host: "http://127.0.0.1:port/__mock/<id>"}`),
/// preserving path and query. Returns `None` (no rewrite) when the env var is
/// unset/invalid or the host is not mapped — the production default.
///
/// The map is parsed once per process. Each agent run is its own subprocess
/// (`chidori run`/`serve`) with its own env, so process-global caching is correct.
fn apply_base_url_override(url: &str) -> Option<String> {
    use std::collections::HashMap;
    use std::sync::OnceLock;

    static MAP: OnceLock<Option<HashMap<String, String>>> = OnceLock::new();
    let map = MAP
        .get_or_init(|| {
            let raw = std::env::var("CHIDORI_INTEGRATION_BASE_URLS").ok()?;
            serde_json::from_str::<HashMap<String, String>>(&raw).ok()
        })
        .as_ref()?;
    rewrite_to_override(url, map)
}

/// Pure rewrite: swap `url`'s origin to the mapped base (keyed by host),
/// preserving path + query. Split from [`apply_base_url_override`] so it is
/// testable without the process-global env cache.
fn rewrite_to_override(
    url: &str,
    map: &std::collections::HashMap<String, String>,
) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let replacement = map.get(host)?;
    let query = parsed.query().map(|q| format!("?{q}")).unwrap_or_default();
    Some(format!(
        "{}{}{}",
        replacement.trim_end_matches('/'),
        parsed.path(),
        query
    ))
}

/// Render an error with its full source chain. reqwest's `Display` truncates
/// the chain (a blocked destination shows up as just "error sending request"),
/// but the guest needs the root cause — e.g. the SSRF guard's policy message —
/// to act on the failure.
fn error_chain_string(err: &(dyn std::error::Error + 'static)) -> String {
    let mut rendered = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !rendered.contains(&text) {
            rendered.push_str(": ");
            rendered.push_str(&text);
        }
        source = cause.source();
    }
    rendered
}

pub fn execute_http(tokio_rt: &tokio::runtime::Runtime, args: &Value) -> Result<Value> {
    execute_http_with_secrets(
        tokio_rt,
        args,
        crate::runtime::secret_env::SecretStore::global(),
    )
}

/// Perform an app-data write/query on behalf of a guest agent (the generative-UI
/// agent-run write tool, docs/design/chidori-handoff.md §3.2). Reads the
/// host-only `CHIDORI_APP_DATA` binding; absent → a structured `no_cluster`
/// error so a clusterless agent gets a clear message rather than a silent no-op.
///
/// Option A: the write is issued as a loopback HTTP POST to agent-builder
/// (which wraps `AppDataPlane::execute_write`), so it rides `execute_http` — the
/// same host chokepoint that `fetch`/`node:http` route through. That gives the bearer-placeholder →
/// real-token substitution and the host allowlist for free, and chidori never
/// holds a libpq credential.
///
/// Returns a value in all cases (never throws): `{ rowsAffected }` / `{ rows }`
/// on success, or `{ appDataError: { kind, message } }` on failure — so the
/// call journals deterministically and replays byte-identically.
pub fn execute_app_data(tokio_rt: &tokio::runtime::Runtime, args: &Value) -> Result<Value> {
    match crate::runtime::app_data::AppDataConfig::from_env() {
        Some(cfg) => execute_app_data_with_config(tokio_rt, args, &cfg),
        None => Ok(crate::runtime::app_data::app_data_error(
            "no_cluster",
            "no app data cluster is bound to this run",
        )),
    }
}

/// `execute_app_data` with an explicit config — split out so tests can drive it
/// against a local endpoint without touching the process-wide `CHIDORI_APP_DATA`.
fn execute_app_data_with_config(
    tokio_rt: &tokio::runtime::Runtime,
    args: &Value,
    cfg: &crate::runtime::app_data::AppDataConfig,
) -> Result<Value> {
    use crate::runtime::app_data::app_data_error;

    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("write");
    let sql = args.get("sql").and_then(Value::as_str).unwrap_or("").trim();
    if sql.is_empty() {
        return Ok(app_data_error("sql", "empty SQL statement"));
    }
    let params = args
        .get("params")
        .filter(|value| !value.is_null())
        .cloned()
        .unwrap_or_else(|| json!([]));

    // The endpoint is host-injected config (typically loopback agent-builder),
    // so exempt its host from the SSRF guard before issuing the request.
    if let Some(host) = url::Url::parse(&cfg.endpoint)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
    {
        crate::runtime::ssrf::trust_host(&host);
    }

    // The bearer is a placeholder; the secret broker substitutes the real
    // per-run token inside `execute_http`, locked to the endpoint's host.
    let http_args = json!({
        "url": cfg.endpoint,
        "method": "POST",
        "headers": {
            "Authorization": format!("Bearer {}", cfg.token),
            "Content-Type": "application/json",
        },
        "body": { "op": action, "sql": sql, "params": params },
    });

    let resp = execute_http(tokio_rt, &http_args)?;
    if let Some(err) = resp.get("error").and_then(Value::as_str) {
        return Ok(app_data_error("transport", err));
    }
    let status = resp.get("status").and_then(Value::as_u64).unwrap_or(0);
    let body = resp.get("body").cloned().unwrap_or(Value::Null);
    if (200..300).contains(&status) {
        // Success: the endpoint returns `{ rowsAffected }` (write) or
        // `{ rows }` (query). Pass the body straight back to the guest.
        Ok(body)
    } else {
        // A bad statement is the agent's fault (surface as `sql`); anything
        // else is the transport/endpoint's.
        let message = body
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("app-data endpoint returned status {status}"));
        let kind = if matches!(status, 400 | 409 | 422) {
            "sql"
        } else {
            "transport"
        };
        Ok(app_data_error(kind, message))
    }
}

/// `execute_http` with an explicit secret store — split out so tests can
/// inject a store without touching the process-wide one.
fn execute_http_with_secrets(
    tokio_rt: &tokio::runtime::Runtime,
    args: &Value,
    secrets: &crate::runtime::secret_env::SecretStore,
) -> Result<Value> {
    let method = args
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("GET")
        .to_string();
    let mut url = args
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("http requires string url"))?
        .to_string();
    let mut headers = args.get("headers").and_then(|value| match value {
        Value::Object(map) => Some(map.clone()),
        _ => None,
    });
    let mut body = args.get("body").filter(|value| !value.is_null()).cloned();
    let mut params = args.get("params").and_then(|value| match value {
        Value::Object(map) => Some(map.clone()),
        _ => None,
    });

    // Test-mode base-URL override (runs before secret substitution). When the
    // harness injects CHIDORI_INTEGRATION_BASE_URLS (host → mock gateway base),
    // any request to a mapped host is transparently rewritten to the per-agent
    // Mock Gateway, keeping the original path + query. This is the single seam
    // that makes "test mode" work for every integration regardless of the npm
    // package versions a generated agent installs. In production the env var is
    // absent and this is a no-op. After rewriting, the host becomes
    // 127.0.0.1:<port>, which is on no secret allowlist — so secrets never
    // substitute into a mock request (and in test mode none are injected
    // anyway). See app-agent-builder docs/design/mock-integrations-test-mode.md.
    if let Some(rewritten) = apply_base_url_override(&url) {
        // The rewrite target is host-injected config (the loopback mock
        // gateway), so exempt it from the SSRF guard before requesting.
        if let Some(host) = url::Url::parse(&rewritten)
            .ok()
            .and_then(|parsed| parsed.host_str().map(str::to_owned))
        {
            crate::runtime::ssrf::trust_host(&host);
        }
        url = rewritten;
    }

    // Secret broker: guest code only ever holds opaque placeholder tokens
    // (its `process.env` is built that way by the harness); the real values
    // are substituted here — after the durable call log captured the args in
    // token form — and only for hosts the secret's allowlist permits. The
    // substitution happens on the local copies above, so recorded args,
    // traces, and anything the guest can observe keep the token form.
    if !secrets.is_empty() {
        let host = url::Url::parse(&url)
            .ok()
            .and_then(|parsed| parsed.host_str().map(str::to_owned));
        match host {
            Some(host) => {
                let deny = |err: String| anyhow::anyhow!("http secret substitution: {err}");
                url = secrets.substitute_str(&url, &host).map_err(deny)?;
                if let Some(map) = headers.as_mut() {
                    for (_, value) in map.iter_mut() {
                        secrets.substitute_value(value, &host).map_err(deny)?;
                    }
                }
                if let Some(map) = params.as_mut() {
                    for (_, value) in map.iter_mut() {
                        secrets.substitute_value(value, &host).map_err(deny)?;
                    }
                }
                if let Some(value) = body.as_mut() {
                    secrets.substitute_value(value, &host).map_err(deny)?;
                }
            }
            None => {
                // Unparseable URL: only an error if the request references a
                // secret token; otherwise let reqwest produce its usual error.
                let mentions_token =
                    crate::runtime::secret_env::SecretStore::looks_like_token(&args.to_string());
                if mentions_token {
                    anyhow::bail!(
                        "http secret substitution: cannot determine request host from url"
                    );
                }
            }
        }
    }

    // SSRF pre-flight: reject non-http(s) schemes and blocked IP-literal
    // destinations up front with a policy error (hostname destinations are
    // checked by the guarded resolver at connect time, against the exact
    // addresses the connector dials — see `runtime::ssrf`). An unparseable
    // URL is left for reqwest to reject with its usual builder error.
    if let Ok(parsed) = url::Url::parse(&url) {
        crate::runtime::ssrf::preflight(&parsed).map_err(|message| anyhow::anyhow!(message))?;
    }

    let caller_set_user_agent = headers
        .as_ref()
        .is_some_and(|map| map.keys().any(|key| key.eq_ignore_ascii_case("user-agent")));

    tokio_rt.block_on(async move {
        let client = http_client()?;
        let request_method =
            reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
        let mut req = client.request(request_method, &url);
        // Only install the default UA when the caller hasn't named their own —
        // reqwest's `header()` appends rather than replaces, so setting both
        // would put two User-Agent headers on the wire.
        if !caller_set_user_agent {
            req = req.header(reqwest::header::USER_AGENT, DEFAULT_USER_AGENT);
        }

        if let Some(headers) = headers {
            for (name, value) in headers {
                if let Some(value) = value.as_str() {
                    req = req.header(name, value);
                }
            }
        }

        if let Some(params) = params {
            let pairs: Vec<(String, String)> = params
                .into_iter()
                .map(|(key, value)| {
                    let value = value
                        .as_str()
                        .map(ToOwned::to_owned)
                        .unwrap_or_else(|| value.to_string());
                    (key, value)
                })
                .collect();
            req = req.query(&pairs);
        }

        if let Some(body) = body {
            // A string body goes on the wire verbatim (so `fetch`/`node:http`
            // callers that pre-serialize with `JSON.stringify` aren't double-
            // encoded, and they keep control of `Content-Type` via headers).
            // Any other JSON value is sent as a JSON body with reqwest setting
            // `Content-Type: application/json` — the object-body convenience the
            // `node:http` shim relies on when a caller passes a non-string body.
            match body {
                Value::String(text) => {
                    req = req.body(text);
                }
                other => {
                    req = req.json(&other);
                }
            }
        }

        // Everything returned from here flows into the durable call log and
        // OTEL export, so secret values must never appear: transport errors
        // can embed the full (substituted) URL, and APIs may echo credentials
        // back in bodies or headers. `redact` maps them to [REDACTED:<KEY>].
        let resp = match req.send().await {
            Ok(resp) => resp,
            Err(err) => {
                if err.is_builder() {
                    return Err(anyhow::anyhow!(secrets.redact(&error_chain_string(&err))));
                }
                return Ok(json!({
                    "status": 0,
                    "headers": {},
                    "body": null,
                    "error": secrets.redact(&error_chain_string(&err)),
                }));
            }
        };
        let status = resp.status().as_u16();
        let mut response_headers = serde_json::Map::new();
        for (name, value) in resp.headers() {
            if let Ok(value) = value.to_str() {
                response_headers.insert(
                    name.as_str().to_string(),
                    Value::String(secrets.redact(value)),
                );
            }
        }
        let bytes = match resp.bytes().await {
            Ok(bytes) => bytes,
            Err(err) => {
                return Ok(json!({
                    "status": status,
                    "headers": response_headers,
                    "body": null,
                    "error": secrets.redact(&err.to_string()),
                }));
            }
        };
        let text = secrets.redact(&String::from_utf8_lossy(&bytes));
        let body = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));

        Ok(json!({
            "status": status,
            "headers": response_headers,
            "body": body,
        }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Message as LlmMessage;
    use crate::runtime::context::{
        HostOperationCompletionSafepoint, HostOperationSafepoint, RuntimeContext,
    };
    use crate::runtime::snapshot::{
        HostOperationId, HostPromiseRecord, HostPromiseState, PendingHostOperation, QueuedSignal,
        PENDING_HOST_OPERATION_FILE,
    };

    /// A one-shot canned HTTP/1.1 endpoint on an ephemeral port. Reads the
    /// request, replies with `status` + JSON `body`, closes. Returns the bound
    /// address so the test can point `AppDataConfig::endpoint` at it.
    fn canned_http_endpoint(status: &'static str, body: &'static str) -> std::net::SocketAddr {
        use std::io::{Read, Write};
        // Tests exercise the real http effect against loopback fixtures, which
        // the SSRF guard would otherwise block.
        crate::runtime::ssrf::trust_host("127.0.0.1");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 8192];
                let _ = stream.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        addr
    }

    #[test]
    fn app_data_no_cluster_when_env_absent() {
        // CHIDORI_APP_DATA is unset in the test process, so the binding is
        // absent and the guest gets a structured `no_cluster` error.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let out = execute_app_data(
            &rt,
            &json!({ "action": "write", "sql": "INSERT INTO t VALUES (1)" }),
        )
        .unwrap();
        assert_eq!(out["appDataError"]["kind"], "no_cluster");
    }

    #[test]
    fn app_data_empty_sql_is_a_sql_error() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cfg = crate::runtime::app_data::AppDataConfig {
            endpoint: "http://127.0.0.1:1/never".into(),
            token: "__CHIDORI_SECRET__t__".into(),
        };
        let out =
            execute_app_data_with_config(&rt, &json!({ "action": "write", "sql": "   " }), &cfg)
                .unwrap();
        assert_eq!(out["appDataError"]["kind"], "sql");
    }

    #[test]
    fn app_data_write_round_trips_rows_affected() {
        let addr = canned_http_endpoint("200 OK", r#"{"rowsAffected":1}"#);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cfg = crate::runtime::app_data::AppDataConfig {
            endpoint: format!("http://{addr}/internal/app-data/write"),
            token: "__CHIDORI_SECRET__t__".into(),
        };
        let args = json!({
            "action": "write",
            "sql": "INSERT INTO notes (body) VALUES ($1)",
            "params": ["hello"],
        });
        let out = execute_app_data_with_config(&rt, &args, &cfg).unwrap();
        assert_eq!(out["rowsAffected"], 1);
        assert!(out.get("appDataError").is_none());
    }

    #[test]
    fn app_data_4xx_maps_to_sql_error_with_message() {
        let addr = canned_http_endpoint(
            "400 Bad Request",
            r#"{"error":"syntax error at or near \"FROM\""}"#,
        );
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cfg = crate::runtime::app_data::AppDataConfig {
            endpoint: format!("http://{addr}/internal/app-data/write"),
            token: "__CHIDORI_SECRET__t__".into(),
        };
        let out = execute_app_data_with_config(
            &rt,
            &json!({ "action": "write", "sql": "INSERT BOGUS" }),
            &cfg,
        )
        .unwrap();
        assert_eq!(out["appDataError"]["kind"], "sql");
        assert!(out["appDataError"]["message"]
            .as_str()
            .unwrap()
            .contains("syntax error"));
    }

    #[test]
    fn base_url_override_rewrites_mapped_host_keeping_path_and_query() {
        let mut map = std::collections::HashMap::new();
        map.insert(
            "slack.com".to_string(),
            "http://127.0.0.1:49874/__mock/slack".to_string(),
        );
        map.insert(
            "gmail.googleapis.com".to_string(),
            "http://127.0.0.1:49874/__mock/gmail/".to_string(), // trailing slash tolerated
        );

        assert_eq!(
            rewrite_to_override("https://slack.com/api/chat.postMessage", &map).as_deref(),
            Some("http://127.0.0.1:49874/__mock/slack/api/chat.postMessage")
        );
        // Path + query preserved; trailing slash on the replacement trimmed.
        assert_eq!(
            rewrite_to_override(
                "https://gmail.googleapis.com/gmail/v1/users/me/messages?q=is:unread",
                &map
            )
            .as_deref(),
            Some("http://127.0.0.1:49874/__mock/gmail/gmail/v1/users/me/messages?q=is:unread")
        );
        // Unmapped host → no rewrite (production passthrough).
        assert_eq!(
            rewrite_to_override("https://api.openai.com/v1/chat", &map),
            None
        );
    }

    #[test]
    fn durable_json_call_replays_without_live_execution() {
        let replay = vec![CallRecord {
            seq: 1,
            parent_seq: None,
            function: "template".to_string(),
            args: json!({ "template": "{{ value }}", "vars": {} }),
            result: json!("cached"),
            duration_ms: 1,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        }];
        let ctx = RuntimeContext::with_replay(replay);

        let result = execute_durable_json_call(
            &ctx,
            "template",
            json!({ "template": "{{ value }}", "vars": {} }),
            || anyhow::bail!("live path should not run"),
        )
        .unwrap();

        assert_eq!(result, json!("cached"));
        assert_eq!(ctx.call_log().into_records().len(), 1);
    }

    #[test]
    fn durable_json_call_arg_mismatch_is_a_divergence_error() {
        // Same function name, different args: historically the cached result
        // was served anyway (the divergence check compared name only). That
        // silently paired cached results with changed code — it must now be a
        // hard replay-divergence error.
        let replay = vec![CallRecord {
            seq: 1,
            parent_seq: None,
            function: "template".to_string(),
            args: json!({ "template": "recorded", "vars": {} }),
            result: json!("cached"),
            duration_ms: 1,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        }];
        let ctx = RuntimeContext::with_replay(replay);

        let err = execute_durable_json_call(
            &ctx,
            "template",
            json!({ "template": "changed", "vars": {} }),
            || anyhow::bail!("live path should not run on divergence"),
        )
        .unwrap_err();

        let message = err.to_string();
        assert!(
            message.contains("Replay divergence"),
            "expected divergence error, got: {message}"
        );
        assert!(
            message.contains("CHIDORI_REPLAY_LAX"),
            "error should name the escape hatch, got: {message}"
        );
    }

    #[test]
    fn replay_keeps_sequence_aligned_after_nested_host_call() {
        // Record: a container call (call_agent) whose `live()` makes a nested
        // host call (log), followed by an outer call (prompt). The nested log
        // burns a sequence number that sits *between* the container and the
        // next outer call.
        let ctx = RuntimeContext::new();
        let agent_result = execute_durable_json_call(
            &ctx,
            "call_agent",
            json!({ "path": "/child.ts", "input": { "value": 1 } }),
            || {
                // Nested host call inside the container's execution.
                execute_durable_json_call(&ctx, "log", json!({ "message": "inside" }), || {
                    Ok(Value::Null)
                })?;
                Ok(json!({ "child": 2 }))
            },
        )
        .unwrap();
        assert_eq!(agent_result, json!({ "child": 2 }));
        let prompt_result =
            execute_durable_json_call(&ctx, "prompt", json!({ "model": "m" }), || {
                Ok(json!("answer"))
            })
            .unwrap();
        assert_eq!(prompt_result, json!("answer"));

        let records = ctx.call_log().into_records();
        // call_agent(seq 1), nested log(seq 2, parent 1), prompt(seq 3).
        assert_eq!(records.len(), 3);
        assert!(records
            .iter()
            .any(|r| r.function == "log" && r.parent_seq == Some(1)));

        // Replay: the container short-circuits without re-running `live()`, so
        // the nested log's seq is never re-consumed. Before the subtree-absorb
        // fix the prompt would land on the log's seq and diverge.
        let replay_ctx = RuntimeContext::with_replay(records);
        let replayed_agent = execute_durable_json_call(
            &replay_ctx,
            "call_agent",
            json!({ "path": "/child.ts", "input": { "value": 1 } }),
            || anyhow::bail!("container live path must not run on replay"),
        )
        .unwrap();
        assert_eq!(replayed_agent, json!({ "child": 2 }));

        // This is the call that diverged before the fix.
        let replayed_prompt =
            execute_durable_json_call(&replay_ctx, "prompt", json!({ "model": "m" }), || {
                anyhow::bail!("prompt live path must not run on replay")
            })
            .unwrap();
        assert_eq!(replayed_prompt, json!("answer"));

        // The nested record is preserved in the replayed trace.
        let replayed_records = replay_ctx.call_log().into_records();
        assert_eq!(replayed_records.len(), 3);
        assert!(replayed_records
            .iter()
            .any(|r| r.function == "log" && r.parent_seq == Some(1)));
    }

    #[test]
    fn native_tool_call_logs_and_replays_without_callback_execution() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        let calls_for_handler = calls.clone();
        registry.register_native("echo", "Echo input", Vec::new(), move |args| {
            calls_for_handler.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(args)
        });

        let ctx = RuntimeContext::new();
        let result =
            execute_native_tool_call(&ctx, &registry, "echo", json!({ "value": 42 })).unwrap();
        assert_eq!(result, json!({ "value": 42 }));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let records = ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "tool");
        assert_eq!(
            records[0].args,
            json!({ "name": "echo", "kwargs": { "value": 42 } })
        );

        let replay_ctx = RuntimeContext::with_replay(records);
        let replayed =
            execute_native_tool_call(&replay_ctx, &registry, "echo", json!({ "value": 42 }))
                .unwrap();

        assert_eq!(replayed, json!({ "value": 42 }));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(replay_ctx.call_log().into_records().len(), 1);
    }

    #[test]
    fn completed_host_operation_arg_mismatch_is_a_divergence_error() {
        // A completed host operation exists at (seq, kind) but was recorded
        // with different arguments. Historically this silently fell through to
        // a live re-execution of the side effect — it must now surface as a
        // replay-divergence error naming the escape hatch.
        let recorded_args = json!({ "name": "echo", "kwargs": { "value": 42 } });
        let records = vec![HostPromiseRecord {
            operation: PendingHostOperation::new(
                HostOperationId(1),
                1,
                PendingHostOperationKind::Tool,
                recorded_args,
            ),
            state: HostPromiseState::Resolved {
                value: json!({ "value": 42 }),
                completed_at: Utc::now(),
            },
        }];
        let ctx = RuntimeContext::with_replay_and_host_promises(Vec::new(), records);

        let live_ran = std::cell::Cell::new(false);
        let err = execute_durable_json_call(
            &ctx,
            "tool",
            json!({ "name": "echo", "kwargs": { "value": 43 } }),
            || {
                live_ran.set(true);
                Ok(json!({ "value": 43 }))
            },
        )
        .unwrap_err();

        assert!(
            !live_ran.get(),
            "the side effect must not silently re-execute live on arg mismatch"
        );
        let message = err.to_string();
        assert!(
            message.contains("Replay divergence"),
            "expected divergence error, got: {message}"
        );
        assert!(
            message.contains("CHIDORI_REPLAY_LAX"),
            "error should name the escape hatch, got: {message}"
        );
    }

    #[test]
    fn durable_json_call_replays_completed_host_operation_without_live_execution() {
        let args = json!({ "name": "echo", "kwargs": { "value": 42 } });
        let records = vec![HostPromiseRecord {
            operation: PendingHostOperation::new(
                HostOperationId(1),
                1,
                PendingHostOperationKind::Tool,
                args.clone(),
            ),
            state: HostPromiseState::Resolved {
                value: json!({ "value": 42 }),
                completed_at: Utc::now(),
            },
        }];
        let ctx = RuntimeContext::with_replay_and_host_promises(Vec::new(), records);

        let result = execute_durable_json_call(&ctx, "tool", args, || {
            anyhow::bail!("live path should not run after completed host operation")
        })
        .unwrap();

        assert_eq!(result, json!({ "value": 42 }));
        let call_log = ctx.call_log().into_records();
        assert_eq!(call_log.len(), 1);
        assert_eq!(call_log[0].function, "tool");
        assert_eq!(call_log[0].result, json!({ "value": 42 }));
    }

    #[test]
    fn durable_json_call_replays_rejected_host_operation_without_live_execution() {
        let args = json!({ "url": "https://example.invalid" });
        let records = vec![HostPromiseRecord {
            operation: PendingHostOperation::new(
                HostOperationId(1),
                1,
                PendingHostOperationKind::Http,
                args.clone(),
            ),
            state: HostPromiseState::Rejected {
                error: "network failed after persistence".to_string(),
                completed_at: Utc::now(),
            },
        }];
        let ctx = RuntimeContext::with_replay_and_host_promises(Vec::new(), records);

        let err = execute_durable_json_call(&ctx, "http", args, || {
            anyhow::bail!("live path should not run after rejected host operation")
        })
        .unwrap_err();

        assert!(err.to_string().contains("network failed after persistence"));
        let call_log = ctx.call_log().into_records();
        assert_eq!(call_log.len(), 1);
        assert_eq!(call_log[0].function, "http");
        assert_eq!(
            call_log[0].error.as_deref(),
            Some("network failed after persistence")
        );
    }

    #[test]
    fn model_override_swaps_request_model_before_send() {
        use crate::providers::{LlmProvider, LlmResponse, TokenSink};
        use std::sync::{Arc as StdArc, Mutex as StdMutex};

        struct RecordingProvider {
            seen_model: StdArc<StdMutex<Option<String>>>,
        }

        #[async_trait::async_trait]
        impl LlmProvider for RecordingProvider {
            fn supports_model(&self, _model: &str) -> bool {
                true
            }
            async fn send(&self, request: &LlmRequest) -> Result<LlmResponse> {
                *self.seen_model.lock().unwrap() = Some(request.model.clone());
                Ok(LlmResponse {
                    content: "ok".to_string(),
                    blocks: vec![ContentBlock::Text {
                        text: "ok".to_string(),
                    }],
                    input_tokens: 1,
                    output_tokens: 1,
                    ..LlmResponse::default()
                })
            }
            async fn stream(
                &self,
                request: &LlmRequest,
                _on_delta: &mut TokenSink,
            ) -> Result<LlmResponse> {
                self.send(request).await
            }
        }

        let seen_model = StdArc::new(StdMutex::new(None));
        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(RecordingProvider {
            seen_model: StdArc::clone(&seen_model),
        }));
        let tokio_rt = tokio::runtime::Runtime::new().unwrap();

        let ctx = RuntimeContext::new();
        ctx.set_model_override(crate::runtime::context::ModelOverride::new(|| {
            Some("override-model".to_string())
        }));

        let request = LlmRequest {
            model: "request-model".to_string(),
            messages: vec![LlmMessage::user_text("hi".to_string())],
            system: None,
            temperature: 0.0,
            max_tokens: 16,
            tools: Vec::new(),
            cache: crate::providers::CacheLayout::default(),
        };
        let _ =
            execute_prompt_response(&ctx, &providers, &tokio_rt, request, json!({}), None).unwrap();

        assert_eq!(
            seen_model.lock().unwrap().as_deref(),
            Some("override-model"),
            "the provider must receive the overridden model, not the request model"
        );
    }

    #[test]
    fn prompt_text_replays_completed_host_operation_without_provider_call() {
        let args = json!({
            "text": "hello",
            "model": "test-model",
            "type": "progress",
        });
        let records = vec![HostPromiseRecord {
            operation: PendingHostOperation::new(
                HostOperationId(1),
                1,
                PendingHostOperationKind::Prompt,
                args.clone(),
            ),
            state: HostPromiseState::Resolved {
                value: json!("cached response"),
                completed_at: Utc::now(),
            },
        }];
        let ctx = RuntimeContext::with_replay_and_host_promises(Vec::new(), records);
        let tokio_rt = tokio::runtime::Runtime::new().unwrap();

        let result = execute_prompt_text(
            &ctx,
            &ProviderRegistry::new(),
            &tokio_rt,
            LlmRequest {
                model: "test-model".to_string(),
                messages: vec![LlmMessage::user_text("hello".to_string())],
                system: None,
                temperature: 0.0,
                max_tokens: 16,
                tools: Vec::new(),
                cache: crate::providers::CacheLayout::default(),
            },
            args,
            Some("progress".to_string()),
        )
        .unwrap();

        assert_eq!(result, json!("cached response"));
        assert_eq!(ctx.call_log().into_records().len(), 1);
    }

    #[test]
    fn template_call_uses_json_args() {
        let engine = TemplateEngine::new(".");
        let result = execute_template(
            &engine,
            &json!({
                "template": "Hello {{ name }}!",
                "vars": { "name": "core" },
            }),
        )
        .unwrap();

        assert_eq!(result, json!("Hello core!"));
    }

    #[test]
    fn durable_json_call_resolves_host_operation_after_success() {
        let ctx = RuntimeContext::new();
        let result = execute_durable_json_call(
            &ctx,
            "tool",
            json!({ "name": "echo", "kwargs": {} }),
            || Ok(json!({ "ok": true })),
        )
        .unwrap();

        assert_eq!(result, json!({ "ok": true }));
        let records = ctx.host_promise_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].operation.kind, PendingHostOperationKind::Tool);
        assert!(matches!(
            records[0].state,
            HostPromiseState::Resolved { .. }
        ));
    }

    #[test]
    fn durable_json_call_rejects_host_operation_after_error() {
        let ctx = RuntimeContext::new();
        let err = execute_durable_json_call(&ctx, "http", json!({ "url": "bad" }), || {
            anyhow::bail!("network failed")
        })
        .unwrap_err();

        assert!(err.to_string().contains("network failed"));
        let records = ctx.host_promise_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].operation.kind, PendingHostOperationKind::Http);
        assert!(matches!(
            records[0].state,
            HostPromiseState::Rejected { .. }
        ));
    }

    #[test]
    fn execute_http_returns_status_zero_for_transport_error() {
        crate::runtime::ssrf::trust_host("127.0.0.1");
        let tokio_rt = tokio::runtime::Runtime::new().unwrap();
        let result = execute_http(
            &tokio_rt,
            &json!({
                "url": "http://127.0.0.1:9/chidori-connection-refused",
                "method": "GET",
            }),
        )
        .unwrap();

        assert_eq!(result["status"], json!(0));
        assert!(!result["error"].as_str().unwrap_or_default().is_empty());
        assert!(result["headers"].as_object().unwrap().is_empty());
        assert!(result["body"].is_null());
    }

    /// Spin up a one-shot TCP server that captures the raw request bytes from
    /// the first connection. Returns the `(url, JoinHandle<raw_request>)` so
    /// the test can issue a request to `url` and then read what landed on the
    /// wire. The server replies with a minimal 200 response so reqwest doesn't
    /// surface a transport error.
    async fn one_shot_http_capture() -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // Tests exercise the real http effect against loopback fixtures, which
        // the SSRF guard would otherwise block.
        crate::runtime::ssrf::trust_host("127.0.0.1");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/ua-check");
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await;
            let _ = stream.shutdown().await;
            request
        });
        (url, handle)
    }

    fn user_agent_header(request: &str) -> Option<String> {
        request
            .lines()
            .find_map(|line| {
                line.strip_prefix("user-agent: ")
                    .or_else(|| line.strip_prefix("User-Agent: "))
            })
            .map(ToOwned::to_owned)
    }

    #[test]
    fn execute_http_sends_default_chidori_user_agent() {
        let tokio_rt = tokio::runtime::Runtime::new().unwrap();
        let (url, server) = tokio_rt.block_on(one_shot_http_capture());
        execute_http(&tokio_rt, &json!({ "url": url, "method": "GET" })).unwrap();
        let request = tokio_rt.block_on(server).unwrap();
        let ua = user_agent_header(&request).expect("missing User-Agent header");
        assert!(
            ua.starts_with("chidori/"),
            "expected chidori-prefixed UA, got {ua:?}"
        );
        assert!(
            ua.contains("github.com/ThousandBirdsInc/chidori"),
            "default UA should include contact URL, got {ua:?}"
        );
        // Exactly one User-Agent header should be on the wire — Wikimedia
        // rejects requests that send the bare `reqwest/` default *or* two UAs.
        let ua_count = request
            .lines()
            .filter(|line| line.to_ascii_lowercase().starts_with("user-agent:"))
            .count();
        assert_eq!(ua_count, 1, "request had {ua_count} User-Agent headers");
    }

    #[test]
    fn execute_http_caller_user_agent_overrides_default() {
        let tokio_rt = tokio::runtime::Runtime::new().unwrap();
        let (url, server) = tokio_rt.block_on(one_shot_http_capture());
        execute_http(
            &tokio_rt,
            &json!({
                "url": url,
                "method": "GET",
                "headers": { "User-Agent": "my-agent/1.0 (contact@example.com)" },
            }),
        )
        .unwrap();
        let request = tokio_rt.block_on(server).unwrap();
        let ua = user_agent_header(&request).expect("missing User-Agent header");
        assert_eq!(ua, "my-agent/1.0 (contact@example.com)");
        // The default mustn't tag along behind the caller-supplied override.
        let ua_count = request
            .lines()
            .filter(|line| line.to_ascii_lowercase().starts_with("user-agent:"))
            .count();
        assert_eq!(ua_count, 1, "request had {ua_count} User-Agent headers");
    }

    #[test]
    fn execute_http_sends_string_body_verbatim() {
        // A string body must land on the wire unchanged (no JSON quoting), so a
        // `fetch`/`node:http` caller that pre-serialized with `JSON.stringify`
        // isn't double-encoded.
        let tokio_rt = tokio::runtime::Runtime::new().unwrap();
        let (url, server) = tokio_rt.block_on(one_shot_http_capture());
        execute_http(
            &tokio_rt,
            &json!({
                "url": url,
                "method": "POST",
                "headers": { "content-type": "application/json" },
                "body": "{\"a\":1}",
            }),
        )
        .unwrap();
        let request = tokio_rt.block_on(server).unwrap();
        assert!(
            request.ends_with("{\"a\":1}"),
            "string body should be sent verbatim, got request:\n{request}"
        );
    }

    fn secret_test_store() -> crate::runtime::secret_env::SecretStore {
        use crate::runtime::secret_env::{SecretEntry, SECRET_TOKEN_PREFIX};
        crate::runtime::secret_env::SecretStore::for_tests(vec![
            (
                format!("{SECRET_TOKEN_PREFIX}aaaa1111__"),
                SecretEntry {
                    key: "LOCAL_API_KEY".into(),
                    value: "sk-local-secret-value".into(),
                    allowed_hosts: vec!["127.0.0.1".into()],
                    allow_any_host: false,
                },
            ),
            (
                format!("{SECRET_TOKEN_PREFIX}bbbb2222__"),
                SecretEntry {
                    key: "REMOTE_ONLY_KEY".into(),
                    value: "remote-only-value".into(),
                    allowed_hosts: vec!["api.example.com".into()],
                    allow_any_host: false,
                },
            ),
        ])
    }

    #[test]
    fn execute_http_substitutes_secret_for_allowed_host() {
        let tokio_rt = tokio::runtime::Runtime::new().unwrap();
        let (url, server) = tokio_rt.block_on(one_shot_http_capture());
        let token = format!(
            "{}aaaa1111__",
            crate::runtime::secret_env::SECRET_TOKEN_PREFIX
        );
        execute_http_with_secrets(
            &tokio_rt,
            &json!({
                "url": url,
                "method": "GET",
                "headers": { "Authorization": format!("Bearer {token}") },
            }),
            &secret_test_store(),
        )
        .unwrap();
        let request = tokio_rt.block_on(server).unwrap();
        assert!(
            request.contains("Bearer sk-local-secret-value"),
            "wire request should carry the substituted secret, got: {request}"
        );
        assert!(
            !request.contains("__CHIDORI_SECRET__"),
            "placeholder token must not reach the wire: {request}"
        );
    }

    #[test]
    fn execute_http_fails_closed_for_disallowed_host() {
        let tokio_rt = tokio::runtime::Runtime::new().unwrap();
        let (url, server) = tokio_rt.block_on(one_shot_http_capture());
        let token = format!(
            "{}bbbb2222__",
            crate::runtime::secret_env::SECRET_TOKEN_PREFIX
        );
        let err = execute_http_with_secrets(
            &tokio_rt,
            &json!({
                "url": url,
                "method": "GET",
                "headers": { "Authorization": format!("Bearer {token}") },
            }),
            &secret_test_store(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("REMOTE_ONLY_KEY"),
            "error names the key: {err}"
        );
        assert!(err.contains("127.0.0.1"), "error names the host: {err}");
        assert!(
            !err.contains("remote-only-value"),
            "error must not leak the value: {err}"
        );
        // The request never went out: the capture server is still waiting.
        assert!(
            !server.is_finished(),
            "no request should reach the listener"
        );
        server.abort();
    }

    #[test]
    fn execute_http_redacts_echoed_secret_from_response() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        crate::runtime::ssrf::trust_host("127.0.0.1");
        let tokio_rt = tokio::runtime::Runtime::new().unwrap();
        let (url, server) = tokio_rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let url = format!("http://{addr}/echo");
            let handle = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let _ = stream.read(&mut buf).await.unwrap();
                let body = "leaked: sk-local-secret-value";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nX-Echo: sk-local-secret-value\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
            (url, handle)
        });
        let result = execute_http_with_secrets(
            &tokio_rt,
            &json!({ "url": url, "method": "GET" }),
            &secret_test_store(),
        )
        .unwrap();
        tokio_rt.block_on(server).unwrap();
        assert_eq!(
            result["body"],
            json!("leaked: [REDACTED:LOCAL_API_KEY]"),
            "response body must be redacted"
        );
        assert_eq!(
            result["headers"]["x-echo"],
            json!("[REDACTED:LOCAL_API_KEY]"),
            "response headers must be redacted"
        );
    }

    #[test]
    fn execute_http_blocks_metadata_ip_literal() {
        let tokio_rt = tokio::runtime::Runtime::new().unwrap();
        let err = execute_http(
            &tokio_rt,
            &json!({ "url": "http://169.254.169.254/latest/meta-data/", "method": "GET" }),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("SSRF protection"), "{err}");
        assert!(err.contains("CHIDORI_HTTP_ALLOW_HOSTS"), "{err}");
    }

    #[test]
    fn execute_http_blocks_hostname_resolving_to_loopback() {
        // `localhost` resolves to a loopback address; the guarded resolver
        // must refuse it even though `127.0.0.1` is trusted by other tests
        // under its own (distinct) host key.
        let tokio_rt = tokio::runtime::Runtime::new().unwrap();
        let result = execute_http(
            &tokio_rt,
            &json!({ "url": "http://localhost:9/ssrf-check", "method": "GET" }),
        )
        .unwrap();
        assert_eq!(result["status"], json!(0));
        let err = result["error"].as_str().unwrap_or_default();
        assert!(err.contains("SSRF protection"), "{err}");
    }

    #[test]
    fn execute_http_blocks_redirect_to_private_address() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        crate::runtime::ssrf::trust_host("127.0.0.1");
        let tokio_rt = tokio::runtime::Runtime::new().unwrap();
        let url = tokio_rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 302 Found\r\nLocation: http://10.0.0.1/internal\r\nContent-Length: 0\r\n\r\n",
                    )
                    .await;
                let _ = stream.shutdown().await;
            });
            format!("http://{addr}/redirect")
        });
        let result = execute_http(&tokio_rt, &json!({ "url": url, "method": "GET" })).unwrap();
        assert_eq!(result["status"], json!(0));
        let err = result["error"].as_str().unwrap_or_default();
        assert!(err.contains("SSRF protection"), "{err}");
    }

    #[test]
    fn execute_http_rejects_non_http_scheme() {
        let tokio_rt = tokio::runtime::Runtime::new().unwrap();
        let err = execute_http(
            &tokio_rt,
            &json!({ "url": "file:///etc/passwd", "method": "GET" }),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("scheme"), "{err}");
    }

    #[test]
    fn durable_json_call_pause_leaves_pending_host_operation_unrecorded() {
        let ctx = RuntimeContext::new();
        // Raise the pause as a bare wire string, the shape it has after a JS
        // round trip, so this also exercises the `from_message` fallback and
        // the immediate re-typing in the dispatch error path.
        let err =
            execute_durable_json_call(&ctx, "tool", json!({ "name": "ask", "kwargs": {} }), || {
                anyhow::bail!(
                    "{}: approval required",
                    crate::runtime::errors::PAUSE_MARKER
                )
            })
            .unwrap_err();

        assert!(err.downcast_ref::<RunInterrupt>().is_some());
        let pending = ctx.pending_host_operations();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].kind, PendingHostOperationKind::Tool);
        assert_eq!(pending[0].args, json!({ "name": "ask", "kwargs": {} }));
        assert!(ctx.call_log().into_records().is_empty());
    }

    #[test]
    fn durable_json_call_persists_pending_operation_before_live_side_effect() {
        let base = std::env::temp_dir().join(format!(
            "chidori-host-operation-before-side-effect-{}",
            uuid::Uuid::new_v4()
        ));
        let ctx = RuntimeContext::new();
        let run_dir = ctx.enable_persistence(base.clone());
        let pending_path = run_dir.join(PENDING_HOST_OPERATION_FILE);

        execute_durable_json_call(
            &ctx,
            "http",
            json!({ "url": "https://example.test" }),
            || {
                let pending: PendingHostOperation =
                    serde_json::from_slice(&std::fs::read(&pending_path)?)?;
                assert_eq!(pending.kind, PendingHostOperationKind::Http);
                assert_eq!(pending.args, json!({ "url": "https://example.test" }));
                Ok(json!({ "status": 200 }))
            },
        )
        .unwrap();

        assert!(!pending_path.exists());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn durable_json_call_runs_safepoint_after_pending_persist_before_live_side_effect() {
        let base = std::env::temp_dir().join(format!(
            "chidori-host-operation-safepoint-{}",
            uuid::Uuid::new_v4()
        ));
        let ctx = RuntimeContext::new();
        let run_dir = ctx.enable_persistence(base.clone());
        let pending_path = run_dir.join(PENDING_HOST_OPERATION_FILE);
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let safepoint_events = events.clone();
        let safepoint_pending_path = pending_path.clone();
        ctx.set_host_operation_safepoint(HostOperationSafepoint::new(move |operation| {
            let pending: PendingHostOperation =
                serde_json::from_slice(&std::fs::read(&safepoint_pending_path)?)?;
            assert_eq!(pending.id, operation.id);
            assert_eq!(pending.kind, PendingHostOperationKind::Http);
            safepoint_events.lock().unwrap().push("safepoint");
            Ok(())
        }));

        execute_durable_json_call(
            &ctx,
            "http",
            json!({ "url": "https://example.test" }),
            || {
                events.lock().unwrap().push("live");
                Ok(json!({ "status": 200 }))
            },
        )
        .unwrap();

        assert_eq!(*events.lock().unwrap(), vec!["safepoint", "live"]);
        assert!(!pending_path.exists());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn durable_json_call_safepoint_failure_blocks_live_side_effect() {
        let ctx = RuntimeContext::new();
        let live_ran = std::sync::Arc::new(std::sync::Mutex::new(false));
        ctx.set_host_operation_safepoint(HostOperationSafepoint::new(|operation| {
            assert_eq!(operation.kind, PendingHostOperationKind::Http);
            anyhow::bail!("snapshot persistence failed")
        }));

        let live_ran_in_closure = live_ran.clone();
        let err = execute_durable_json_call(
            &ctx,
            "http",
            json!({ "url": "https://example.test" }),
            move || {
                *live_ran_in_closure.lock().unwrap() = true;
                Ok(json!({ "status": 200 }))
            },
        )
        .unwrap_err();

        assert!(err.to_string().contains("snapshot persistence failed"));
        assert!(!*live_ran.lock().unwrap());
        assert_eq!(ctx.pending_host_operations().len(), 1);
        assert!(ctx.call_log().into_records().is_empty());
    }

    #[test]
    fn durable_json_call_runs_completion_safepoint_after_result_record() {
        let base = std::env::temp_dir().join(format!(
            "chidori-host-operation-completion-safepoint-{}",
            uuid::Uuid::new_v4()
        ));
        let ctx = RuntimeContext::new();
        let run_dir = ctx.enable_persistence(base.clone());
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let completion_events = events.clone();
        let completion_run_dir = run_dir.clone();
        ctx.set_host_operation_completion_safepoint(HostOperationCompletionSafepoint::new(
            move |record| {
                // The resolved state must be durable (per-op blob ∪ table)
                // by the time the completion safepoint observes it.
                let store = crate::runtime::store::FsRunStore::new(&completion_run_dir);
                let records = crate::runtime::snapshot::load_host_promise_records(&store)?;
                assert_eq!(records.len(), 1);
                assert_eq!(records[0].operation.id, record.operation.id);
                assert!(matches!(
                    records[0].state,
                    HostPromiseState::Resolved { .. }
                ));
                completion_events.lock().unwrap().push("completion");
                Ok(())
            },
        ));

        execute_durable_json_call(
            &ctx,
            "tool",
            json!({ "name": "echo", "kwargs": {} }),
            || {
                events.lock().unwrap().push("live");
                Ok(json!({ "ok": true }))
            },
        )
        .unwrap();

        assert_eq!(*events.lock().unwrap(), vec!["live", "completion"]);
        let call_log = ctx.call_log().into_records();
        assert_eq!(call_log.len(), 1);
        assert_eq!(call_log[0].function, "tool");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn durable_json_call_completion_safepoint_failure_keeps_completed_result_for_replay() {
        let ctx = RuntimeContext::new();
        ctx.set_host_operation_completion_safepoint(HostOperationCompletionSafepoint::new(
            |record| {
                assert!(matches!(record.state, HostPromiseState::Resolved { .. }));
                anyhow::bail!("snapshot persistence failed after result")
            },
        ));

        let err = execute_durable_json_call(
            &ctx,
            "tool",
            json!({ "name": "echo", "kwargs": { "value": 41 } }),
            || Ok(json!({ "value": 42 })),
        )
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("snapshot persistence failed after result"));
        let records = ctx.host_promise_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].operation.kind, PendingHostOperationKind::Tool);
        assert!(matches!(
            records[0].state,
            HostPromiseState::Resolved { .. }
        ));
        let call_log = ctx.call_log().into_records();
        assert_eq!(call_log.len(), 1);
        assert_eq!(call_log[0].function, "tool");
        assert_eq!(call_log[0].result, json!({ "value": 42 }));
    }

    #[test]
    fn cache_posture_parses_prompt_options() {
        assert_eq!(
            cache_posture_from_options(&json!({})),
            CachePosture::Auto(CacheTtl::FiveMinutes)
        );
        assert_eq!(
            cache_posture_from_options(&json!({ "cache": false })),
            CachePosture::Disabled
        );
        assert_eq!(
            cache_posture_from_options(&json!({ "cache": true })),
            CachePosture::Auto(CacheTtl::FiveMinutes)
        );
        assert_eq!(
            cache_posture_from_options(&json!({ "cache": "1h" })),
            CachePosture::Auto(CacheTtl::OneHour)
        );
        assert_eq!(
            cache_posture_from_options(&json!({ "cache": { "ttl": "1h" } })),
            CachePosture::Auto(CacheTtl::OneHour)
        );
    }

    fn cacheable_request() -> LlmRequest {
        LlmRequest {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![LlmMessage::user_text("q1")],
            system: Some("system".to_string()),
            temperature: 0.0,
            max_tokens: 16,
            tools: Vec::new(),
            cache: crate::providers::CacheLayout::default(),
        }
    }

    #[test]
    fn auto_marking_covers_stable_head_and_respects_disable() {
        // Single-shot text prompt (no tools, one message): system is marked but
        // the lone user turn is not — there is no follow-up to read it.
        let mut single = cacheable_request();
        auto_mark_prompt_cache(&mut single, CachePosture::default());
        assert_eq!(single.cache.system, Some(CacheTtl::FiveMinutes));
        assert!(single.messages[0].cache_control.is_none());

        // Tool-loop shape: tools and the conversation head are marked too.
        let mut looped = cacheable_request();
        looped.tools.push(crate::providers::ToolSchema {
            name: "read".to_string(),
            description: "Read".to_string(),
            input_schema: json!({ "type": "object" }),
        });
        auto_mark_prompt_cache(&mut looped, CachePosture::default());
        assert_eq!(looped.cache.tools, Some(CacheTtl::FiveMinutes));
        assert_eq!(
            looped.messages.last().unwrap().cache_control,
            Some(CacheTtl::FiveMinutes)
        );

        // Multi-turn conversation: head marked even without tools.
        let mut convo = cacheable_request();
        convo.messages.push(LlmMessage::assistant_blocks(vec![]));
        convo.messages.push(LlmMessage::user_text("q2"));
        auto_mark_prompt_cache(&mut convo, CachePosture::default());
        assert_eq!(
            convo.messages.last().unwrap().cache_control,
            Some(CacheTtl::FiveMinutes)
        );

        // Explicit author marks are never overridden, and Disabled is inert.
        let mut explicit = cacheable_request();
        explicit.messages[0].cache_control = Some(CacheTtl::OneHour);
        auto_mark_prompt_cache(&mut explicit, CachePosture::default());
        assert_eq!(explicit.messages[0].cache_control, Some(CacheTtl::OneHour));
        let mut disabled = cacheable_request();
        auto_mark_prompt_cache(&mut disabled, CachePosture::Disabled);
        assert!(disabled.cache.system.is_none());
        assert!(disabled.messages[0].cache_control.is_none());
    }

    #[test]
    fn request_digest_is_stable_and_covers_cache_layout() {
        let request = cacheable_request();
        let digest = prompt_request_digest(&request);
        assert_eq!(digest, prompt_request_digest(&request.clone()));
        assert_eq!(digest.len(), 64);

        let mut different_text = cacheable_request();
        different_text.messages = vec![LlmMessage::user_text("q2")];
        assert_ne!(digest, prompt_request_digest(&different_text));

        let mut different_cache = cacheable_request();
        different_cache.cache.system = Some(CacheTtl::FiveMinutes);
        assert_ne!(digest, prompt_request_digest(&different_cache));
    }

    #[test]
    fn input_pause_leaves_pending_host_operation() {
        let ctx = RuntimeContext::new();
        ctx.set_input_mode(InputMode::Pause);

        let err = execute_input(&ctx, &json!({ "prompt": "Approve?" })).unwrap_err();

        assert_eq!(
            RunInterrupt::from_error(&err),
            Some(RunInterrupt::Input {
                prompt: "Approve?".to_string()
            })
        );
        let pending = ctx.pending_host_operations();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].kind, PendingHostOperationKind::Input);
        assert_eq!(pending[0].args, json!({ "prompt": "Approve?" }));
    }

    fn queued_signal(name: &str, payload: Value, from: Value, delivery_seq: u64) -> QueuedSignal {
        QueuedSignal {
            name: name.to_string(),
            payload,
            from,
            delivery_seq,
            enqueued_at: Utc::now(),
        }
    }

    #[test]
    fn signal_pauses_when_inbox_empty() {
        let ctx = RuntimeContext::new();

        let err = execute_signal(&ctx, &json!({ "name": "review", "opts": null })).unwrap_err();

        assert_eq!(
            RunInterrupt::from_error(&err),
            Some(RunInterrupt::Signal {
                name: "review".to_string()
            })
        );
        // The pending host op carries kind Signal and the name-only match key.
        let pending = ctx.pending_host_operations();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].kind, PendingHostOperationKind::Signal);
        assert_eq!(pending[0].args, json!({ "name": "review" }));
        // The pause slot is set so the engine surfaces a signal pause.
        let pending_signal = ctx.take_pending_signal().expect("pending signal set");
        assert_eq!(pending_signal.name, "review");
        assert_eq!(pending_signal.seq, 1);
        // No call was recorded — the run is suspended, not completed.
        assert!(ctx.call_log().into_records().is_empty());
    }

    #[test]
    fn signal_consumes_queued_without_pausing() {
        let ctx = RuntimeContext::new();
        ctx.set_signal_inbox(vec![queued_signal(
            "review",
            json!({ "decision": "approve" }),
            json!({ "kind": "human", "id": "mara" }),
            7,
        )]);

        let result = execute_signal(&ctx, &json!({ "name": "review", "opts": null })).unwrap();

        assert_eq!(
            result,
            json!({
                "name": "review",
                "payload": { "decision": "approve" },
                "from": { "kind": "human", "id": "mara" },
            })
        );
        // The inbox is drained and a completed call is recorded with the value.
        assert!(ctx.signal_inbox().is_empty());
        let records = ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "signal");
        assert_eq!(records[0].args, json!({ "name": "review" }));
        assert_eq!(records[0].result, result);
        assert!(ctx.take_pending_signal().is_none());
    }

    #[test]
    fn signal_replay_returns_recorded_value_without_touching_inbox() {
        // Record a signal consumption.
        let recorded = vec![CallRecord {
            seq: 1,
            parent_seq: None,
            function: "signal".to_string(),
            args: json!({ "name": "review" }),
            result: json!({
                "name": "review",
                "payload": { "decision": "approve" },
                "from": { "kind": "human", "id": "mara" },
            }),
            duration_ms: 0,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        }];
        // Replay context with a DIFFERENT same-name signal sitting in the inbox.
        // The recorded value must win and the inbox must stay undrained.
        let ctx = RuntimeContext::with_replay(recorded);
        ctx.set_signal_inbox(vec![queued_signal(
            "review",
            json!({ "decision": "changes" }),
            json!({ "kind": "agent", "id": "bot" }),
            99,
        )]);

        let result = execute_signal(&ctx, &json!({ "name": "review", "opts": null })).unwrap();

        assert_eq!(
            result,
            json!({
                "name": "review",
                "payload": { "decision": "approve" },
                "from": { "kind": "human", "id": "mara" },
            })
        );
        // The inbox was never read on replay.
        assert_eq!(ctx.signal_inbox().len(), 1);
        assert_eq!(
            ctx.signal_inbox()[0].payload,
            json!({ "decision": "changes" })
        );
    }

    #[test]
    fn signal_consumes_same_name_in_delivery_seq_order() {
        let ctx = RuntimeContext::new();
        // Two same-name signals enqueued out of delivery_seq order.
        ctx.set_signal_inbox(vec![
            queued_signal("review", json!("second"), json!({ "id": "b" }), 20),
            queued_signal("review", json!("first"), json!({ "id": "a" }), 10),
        ]);

        let first = execute_signal(&ctx, &json!({ "name": "review", "opts": null })).unwrap();
        assert_eq!(first["payload"], json!("first"));
        assert_eq!(ctx.signal_inbox().len(), 1);

        let second = execute_signal(&ctx, &json!({ "name": "review", "opts": null })).unwrap();
        assert_eq!(second["payload"], json!("second"));
        assert!(ctx.signal_inbox().is_empty());

        let records = ctx.call_log().into_records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].result["payload"], json!("first"));
        assert_eq!(records[1].result["payload"], json!("second"));
    }

    #[test]
    fn poll_signal_records_null_when_empty_and_value_when_queued() {
        // Empty inbox → records null, never pauses.
        let ctx = RuntimeContext::new();
        let empty = execute_poll_signal(&ctx, &json!({ "name": "steer" })).unwrap();
        assert_eq!(empty, Value::Null);
        let records = ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "poll_signal");
        assert_eq!(records[0].result, Value::Null);

        // Queued → records the value object.
        let ctx2 = RuntimeContext::new();
        ctx2.set_signal_inbox(vec![queued_signal(
            "steer",
            json!({ "priority": "high" }),
            json!({ "kind": "human", "id": "sam" }),
            3,
        )]);
        let value = execute_poll_signal(&ctx2, &json!({ "name": "steer" })).unwrap();
        assert_eq!(
            value,
            json!({
                "name": "steer",
                "payload": { "priority": "high" },
                "from": { "kind": "human", "id": "sam" },
            })
        );
        assert!(ctx2.signal_inbox().is_empty());
    }

    #[test]
    fn poll_signal_replays_null_and_value_deterministically() {
        // A recorded null poll replays as null without consulting the inbox.
        let null_record = vec![CallRecord {
            seq: 1,
            parent_seq: None,
            function: "poll_signal".to_string(),
            args: json!({ "name": "steer" }),
            result: Value::Null,
            duration_ms: 0,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        }];
        let ctx = RuntimeContext::with_replay(null_record);
        // Even with a queued same-name signal, the recorded null wins.
        ctx.set_signal_inbox(vec![queued_signal(
            "steer",
            json!({ "priority": "high" }),
            json!({ "id": "sam" }),
            5,
        )]);
        let replayed_null = execute_poll_signal(&ctx, &json!({ "name": "steer" })).unwrap();
        assert_eq!(replayed_null, Value::Null);
        assert_eq!(ctx.signal_inbox().len(), 1);

        // A recorded value poll replays the value.
        let value = json!({
            "name": "steer",
            "payload": { "priority": "high" },
            "from": { "id": "sam" },
        });
        let value_record = vec![CallRecord {
            seq: 1,
            parent_seq: None,
            function: "poll_signal".to_string(),
            args: json!({ "name": "steer" }),
            result: value.clone(),
            duration_ms: 0,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        }];
        let ctx2 = RuntimeContext::with_replay(value_record);
        let replayed_value = execute_poll_signal(&ctx2, &json!({ "name": "steer" })).unwrap();
        assert_eq!(replayed_value, value);
    }

    #[test]
    fn signal_requires_string_name() {
        let ctx = RuntimeContext::new();
        let err = execute_signal(&ctx, &json!({ "name": 42, "opts": null })).unwrap_err();
        assert!(err.to_string().contains("requires a string name"));
    }

    #[test]
    fn signal_pause_records_timeout_ms_from_opts() {
        let ctx = RuntimeContext::new();

        let err = execute_signal(
            &ctx,
            &json!({ "name": "review", "opts": { "timeoutMs": 1500 } }),
        )
        .unwrap_err();

        assert!(RunInterrupt::from_error(&err).is_some());
        let pending = ctx.take_pending_signal().expect("pending signal set");
        assert_eq!(pending.timeout_ms, Some(1500));
        assert_eq!(pending.listen_names(), vec!["review".to_string()]);
        // The durable match key stays name-only — timeoutMs must not leak into
        // the args the resume matcher compares.
        let ops = ctx.pending_host_operations();
        assert_eq!(ops[0].args, json!({ "name": "review" }));
    }

    #[test]
    fn signal_any_pauses_with_listen_set_when_inbox_empty() {
        let ctx = RuntimeContext::new();

        let err = execute_signal_any(&ctx, &json!({ "names": ["review", "steer"], "opts": null }))
            .unwrap_err();

        assert_eq!(
            RunInterrupt::from_error(&err),
            Some(RunInterrupt::SignalAny {
                names: vec!["review".to_string(), "steer".to_string()]
            })
        );
        let pending = ctx.pending_host_operations();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].kind, PendingHostOperationKind::Signal);
        assert_eq!(pending[0].function.as_deref(), Some("signal_any"));
        assert_eq!(pending[0].args, json!({ "names": ["review", "steer"] }));
        let pending_signal = ctx.take_pending_signal().expect("pending signal set");
        assert_eq!(
            pending_signal.listen_names(),
            vec!["review".to_string(), "steer".to_string()]
        );
        assert_eq!(pending_signal.name, "review");
        assert!(ctx.call_log().into_records().is_empty());
    }

    #[test]
    fn signal_any_consumes_lowest_delivery_seq_across_name_set() {
        let ctx = RuntimeContext::new();
        // Two candidates with DIFFERENT names; the earlier-arriving one (lower
        // delivery_seq) must win regardless of name order in the listen set.
        ctx.set_signal_inbox(vec![
            queued_signal("review", json!("later"), json!({ "id": "r" }), 12),
            queued_signal("steer", json!("earlier"), json!({ "id": "s" }), 4),
        ]);

        let result =
            execute_signal_any(&ctx, &json!({ "names": ["review", "steer"], "opts": null }))
                .unwrap();

        assert_eq!(result["name"], json!("steer"));
        assert_eq!(result["payload"], json!("earlier"));
        // The non-consumed candidate stays queued for a later listen point.
        assert_eq!(ctx.signal_inbox().len(), 1);
        assert_eq!(ctx.signal_inbox()[0].name, "review");
        let records = ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "signal_any");
        assert_eq!(records[0].args, json!({ "names": ["review", "steer"] }));
    }

    #[test]
    fn signal_any_replays_recorded_value_without_touching_inbox() {
        let recorded = vec![CallRecord {
            seq: 1,
            parent_seq: None,
            function: "signal_any".to_string(),
            args: json!({ "names": ["review", "steer"] }),
            result: json!({
                "name": "steer",
                "payload": { "priority": "high" },
                "from": { "kind": "human", "id": "sam" },
            }),
            duration_ms: 0,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        }];
        let ctx = RuntimeContext::with_replay(recorded);
        ctx.set_signal_inbox(vec![queued_signal(
            "review",
            json!("other"),
            json!({ "id": "x" }),
            1,
        )]);

        let result =
            execute_signal_any(&ctx, &json!({ "names": ["review", "steer"], "opts": null }))
                .unwrap();

        assert_eq!(result["name"], json!("steer"));
        assert_eq!(ctx.signal_inbox().len(), 1);
    }

    #[test]
    fn signal_any_rejects_empty_or_non_string_names() {
        let ctx = RuntimeContext::new();
        let err = execute_signal_any(&ctx, &json!({ "names": [], "opts": null })).unwrap_err();
        assert!(err.to_string().contains("at least one name"));

        let err =
            execute_signal_any(&ctx, &json!({ "names": ["ok", 42], "opts": null })).unwrap_err();
        assert!(err.to_string().contains("must all be strings"));

        let err = execute_signal_any(&ctx, &json!({ "names": null, "opts": null })).unwrap_err();
        assert!(err.to_string().contains("requires an array of names"));
    }

    /// The actor inline wait settles a listen point in place — message
    /// consumed, journal record appended, NO pause — so a signal-driven actor
    /// never re-executes its history per message.
    #[test]
    fn actor_inline_wait_settles_listen_point_without_pausing() {
        let ctx = RuntimeContext::new();
        let waiter_ctx = ctx.clone();
        ctx.set_actor_signal_waiter(crate::runtime::context::ActorSignalWaiter::new(
            move |names, _timeout_ms| {
                // Simulate a delivery landing while the listen point waits.
                waiter_ctx.enqueue_live_signal(&names[0], json!({ "n": 1 }), json!("tester"));
                crate::runtime::context::ActorSignalWait::Delivered
            },
        ));
        let result = execute_signal(&ctx, &json!({ "name": "go" })).unwrap();
        assert_eq!(result["name"], json!("go"));
        assert_eq!(result["payload"], json!({ "n": 1 }));
        assert!(ctx.take_pending_signal().is_none(), "must not pause");
        let log = ctx.call_log().into_records();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].function, "signal");
        assert_eq!(log[0].result, result);
    }

    /// An inline wait that hits the listen point's own timeout resolves with
    /// the sentinel in place — the same record the parked path's synthetic
    /// injection produces, so replay is indistinguishable.
    #[test]
    fn actor_inline_wait_timeout_resolves_sentinel_inline() {
        let ctx = RuntimeContext::new();
        ctx.set_actor_signal_waiter(crate::runtime::context::ActorSignalWaiter::new(
            |_names, timeout_ms| {
                assert_eq!(timeout_ms, Some(5), "listen timeout must reach the waiter");
                crate::runtime::context::ActorSignalWait::TimedOut
            },
        ));
        let result =
            execute_signal(&ctx, &json!({ "name": "go", "opts": { "timeoutMs": 5 } })).unwrap();
        assert_eq!(result, signal_timeout_sentinel(&["go".to_string()]));
        assert!(ctx.take_pending_signal().is_none(), "must not pause");
        let log = ctx.call_log().into_records();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].function, "signal");
    }

    /// `Park` (stop requested / idle cap) falls back to the ordinary pause
    /// path unchanged — pending signal set, PAUSE_MARKER raised.
    #[test]
    fn actor_inline_wait_park_falls_back_to_pause() {
        let ctx = RuntimeContext::new();
        ctx.set_actor_signal_waiter(crate::runtime::context::ActorSignalWaiter::new(
            |_names, _timeout_ms| crate::runtime::context::ActorSignalWait::Park,
        ));
        let err = execute_signal(&ctx, &json!({ "name": "go" })).unwrap_err();
        assert!(RunInterrupt::from_error(&err).is_some());
        let pending = ctx
            .take_pending_signal()
            .expect("pause path must set pending");
        assert_eq!(pending.names, vec!["go".to_string()]);
    }

    /// The fan-in listen point takes the same inline path, draining whichever
    /// name the delivery matched.
    #[test]
    fn actor_inline_wait_settles_signal_any() {
        let ctx = RuntimeContext::new();
        let waiter_ctx = ctx.clone();
        ctx.set_actor_signal_waiter(crate::runtime::context::ActorSignalWaiter::new(
            move |names, _timeout_ms| {
                assert_eq!(names.len(), 2);
                waiter_ctx.enqueue_live_signal(&names[1], json!("payload-b"), json!(null));
                crate::runtime::context::ActorSignalWait::Delivered
            },
        ));
        let result = execute_signal_any(&ctx, &json!({ "names": ["a", "b"] })).unwrap();
        assert_eq!(result["name"], json!("b"));
        assert_eq!(result["payload"], json!("payload-b"));
        assert!(ctx.take_pending_signal().is_none());
        let log = ctx.call_log().into_records();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].function, "signal_any");
    }

    #[test]
    fn signal_timeout_sentinel_shapes() {
        let single = signal_timeout_sentinel(&["review".to_string()]);
        assert_eq!(
            single,
            json!({ "name": "review", "payload": null, "from": null, "timedOut": true })
        );
        let multi = signal_timeout_sentinel(&["a".to_string(), "b".to_string()]);
        assert_eq!(multi["name"], Value::Null);
        assert_eq!(multi["timedOut"], json!(true));
    }

    #[test]
    fn step_begin_and_end_record_one_step_call() {
        let ctx = RuntimeContext::new();
        let begin = execute_step_begin(&ctx, &json!({ "name": "plan" })).unwrap();
        assert_eq!(begin, json!({ "cached": false }));
        assert_eq!(ctx.active_step_name().as_deref(), Some("plan"));

        let result =
            execute_step_end(&ctx, &json!({ "name": "plan", "value": { "total": 42 } })).unwrap();
        assert_eq!(result, json!({ "total": 42 }));
        assert!(ctx.active_step_name().is_none());

        let records = ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "step");
        assert_eq!(records[0].seq, 1);
        assert_eq!(records[0].args, json!({ "name": "plan" }));
        assert_eq!(records[0].result, json!({ "total": 42 }));
        assert!(records[0].error.is_none());
    }

    #[test]
    fn step_begin_replays_recorded_value_and_error_without_running() {
        let records = vec![
            CallRecord {
                seq: 1,
                parent_seq: None,
                function: "step".to_string(),
                args: json!({ "name": "plan" }),
                result: json!({ "total": 42 }),
                duration_ms: 0,
                token_usage: None,
                timestamp: Utc::now(),
                error: None,
            },
            CallRecord {
                seq: 2,
                parent_seq: None,
                function: "step".to_string(),
                args: json!({ "name": "boom" }),
                result: Value::Null,
                duration_ms: 0,
                token_usage: None,
                timestamp: Utc::now(),
                error: Some("Error: bad parse".to_string()),
            },
        ];
        let ctx = RuntimeContext::with_replay(records);

        let hit = execute_step_begin(&ctx, &json!({ "name": "plan" })).unwrap();
        assert_eq!(hit, json!({ "cached": true, "value": { "total": 42 } }));
        // A cache hit leaves no active step — the callback never runs, so no
        // step_end follows.
        assert!(ctx.active_step_name().is_none());

        let err_hit = execute_step_begin(&ctx, &json!({ "name": "boom" })).unwrap();
        assert_eq!(
            err_hit,
            json!({ "cached": true, "error": "Error: bad parse" })
        );
    }

    #[test]
    fn step_begin_diverges_on_renamed_step() {
        let records = vec![CallRecord {
            seq: 1,
            parent_seq: None,
            function: "step".to_string(),
            args: json!({ "name": "plan" }),
            result: json!(1),
            duration_ms: 0,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        }];
        let ctx = RuntimeContext::with_replay(records);
        let err = execute_step_begin(&ctx, &json!({ "name": "renamed" })).unwrap_err();
        assert!(err.to_string().contains("Replay divergence"));
        assert!(err.to_string().contains("\"plan\""));
        assert!(err.to_string().contains("\"renamed\""));
    }

    #[test]
    fn step_rejects_nesting_and_unmatched_end() {
        let ctx = RuntimeContext::new();
        execute_step_begin(&ctx, &json!({ "name": "outer" })).unwrap();
        let nested = execute_step_begin(&ctx, &json!({ "name": "inner" })).unwrap_err();
        assert!(nested
            .to_string()
            .contains("cannot start inside chidori.step(\"outer\")"));

        // Close the outer step, then a stray step_end has nothing to match.
        execute_step_end(&ctx, &json!({ "name": "outer", "value": 1 })).unwrap();
        let stray = execute_step_end(&ctx, &json!({ "name": "outer", "value": 1 })).unwrap_err();
        assert!(stray.to_string().contains("without an active step"));
    }

    #[test]
    fn step_end_records_error_outcome() {
        let ctx = RuntimeContext::new();
        execute_step_begin(&ctx, &json!({ "name": "boom" })).unwrap();
        let result = execute_step_end(
            &ctx,
            &json!({ "name": "boom", "error": "Error: bad parse" }),
        )
        .unwrap();
        assert_eq!(result, Value::Null);

        let records = ctx.call_log().into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].function, "step");
        assert_eq!(records[0].error.as_deref(), Some("Error: bad parse"));
        assert_eq!(records[0].result, Value::Null);
    }
}
