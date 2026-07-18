mod acp;
mod init;
mod mcp;
mod mem_guard;
mod pkg;
mod policy;
mod providers;
mod recipes;
mod runtime;
mod scheduler;
mod server;
mod storage;
mod tools;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::Value;

use crate::providers::ProviderRegistry;
use crate::runtime::engine::Engine;

/// Track live heap usage process-wide so the rust-engine watchdog can enforce a
/// per-run memory ceiling (see `mem_guard` and `runtime::rust_engine`). The
/// overhead is one relaxed atomic per allocation.
#[global_allocator]
static GLOBAL: mem_guard::CountingAllocator = mem_guard::CountingAllocator;
use crate::runtime::template::TemplateEngine;
use crate::tools::ToolRegistry;

#[derive(Parser)]
#[command(
    name = "chidori",
    version,
    about = "AI agent framework powered by TypeScript agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Pick from an interactive list of example agents to run
    Demo,

    /// Sign in with OpenRouter (browser OAuth) so agents can call an LLM
    /// without setting a provider API key. The key is saved to
    /// `~/.chidori/credentials.json` and used automatically as a fallback
    /// whenever no `ANTHROPIC_API_KEY`/`OPENAI_API_KEY` is configured.
    ModelLogin,

    /// Run a TypeScript agent file
    Run {
        /// Path to the agent .ts file
        file: PathBuf,

        /// Input as key=value pairs or a JSON string.
        /// Use @filename to read value from a file.
        #[arg(short, long)]
        input: Vec<String>,

        /// Output the execution trace as JSON
        #[arg(long)]
        trace: bool,

        /// Print host function calls to stderr during execution
        #[arg(short, long)]
        verbose: bool,

        /// Default model for prompts that don't set one in code (equivalent
        /// to CHIDORI_MODEL). Any model name your configured provider
        /// accepts, e.g. `claude-sonnet-4-6`, `gpt-4o`, `deepseek-chat`.
        #[arg(long)]
        model: Option<String>,

        /// Stream each host-function call as a newline-delimited JSON event to
        /// stdout as it executes. Each line is either:
        ///   {"type":"call","record":{...}}
        ///   {"type":"done","status":"completed","output":{...}}
        ///   {"type":"done","status":"failed","error":"..."}
        ///
        /// When set, --trace is ignored (the call log is implicit in the stream).
        #[arg(long)]
        stream: bool,

        /// Run under the built-in deny-by-default `untrusted` policy profile:
        /// gated effects (http, workspace mutations) are refused unless
        /// allowlisted. Equivalent to CHIDORI_POLICY_PROFILE=untrusted, but
        /// takes precedence over all CHIDORI_POLICY* env vars.
        #[arg(long, conflicts_with = "trusted")]
        untrusted: bool,

        /// Opt out of the ask-before-powerful-effects default: with no
        /// CHIDORI_POLICY* configuration, gated effects (http, workspace
        /// mutations, tools) run without prompts. Explicit CHIDORI_POLICY*
        /// configuration still applies. Use for agents you wrote yourself.
        #[arg(long)]
        trusted: bool,

        /// Run the agent in an isolated child process, brokering its host
        /// effects back over a pipe (see docs/os-isolation-plan.md). Equivalent
        /// to CHIDORI_ISOLATE=process. This is the default on Unix; the flag
        /// remains as an explicit override of CHIDORI_ISOLATE=off.
        #[arg(long, conflicts_with = "no_isolate")]
        isolate: bool,

        /// Run the agent in-process, without the isolated worker sandbox.
        /// Equivalent to CHIDORI_ISOLATE=off.
        #[arg(long)]
        no_isolate: bool,
    },

    /// Internal: the isolate worker. Runs one agent over a stdin/stdout frame
    /// protocol on behalf of a parent supervisor; not meant to be invoked
    /// directly. See `crate::runtime::isolate`.
    #[command(name = "__run-worker", hide = true)]
    RunWorker,

    /// Validate a TypeScript agent file without running it
    Check {
        /// Path to the agent .ts file
        file: PathBuf,
    },

    /// Add npm packages to package.json and install them into node_modules.
    /// Packages come from the npm registry (or CHIDORI_NPM_REGISTRY), are
    /// verified against their SHA-512 integrity, cached once per machine in a
    /// content-addressed store (~/.chidori/cache/packages), and hardlinked
    /// into the project. Lifecycle scripts never run.
    Add {
        /// Packages to add: `name`, `name@1.2.3`, `name@^2`, `@scope/name@tag`
        packages: Vec<String>,

        /// Add to devDependencies instead of dependencies
        #[arg(short = 'D', long)]
        dev: bool,

        /// Project directory (defaults to the current directory)
        #[arg(long)]
        dir: Option<PathBuf>,
    },

    /// Install dependencies from chidori.lock.jsonl (or resolve them from
    /// package.json when the lockfile is missing or out of date). Warm
    /// installs are fully offline: every package materializes from the
    /// content-addressed store by hardlink.
    Install {
        /// Fail instead of re-resolving when the lockfile is missing or out
        /// of sync with package.json (for CI).
        #[arg(long)]
        frozen: bool,

        /// Project directory (defaults to the current directory)
        #[arg(long)]
        dir: Option<PathBuf>,
    },

    /// Remove npm packages from package.json, the lockfile, and node_modules.
    Remove {
        /// Package names to remove
        packages: Vec<String>,

        /// Project directory (defaults to the current directory)
        #[arg(long)]
        dir: Option<PathBuf>,
    },

    /// Scaffold a new agent project from a starter template.
    Init {
        /// Directory to scaffold into (defaults to the current directory).
        dir: Option<PathBuf>,

        /// Template to use: `docs`, `chat`, or `worker`. Omit to pick interactively.
        #[arg(short, long)]
        template: Option<String>,
    },

    /// Start an interactive multi-turn chat. With no AGENT it chats with the
    /// model directly (no agent file); pass a conversational agent file to chat
    /// through it. Each turn is a durable host call journaled under
    /// `.chidori/runs/<session_id>`, so the conversation survives crashes, is
    /// inspectable with `chidori trace`, and continues with `--resume`; prior
    /// turns replay for free, so only your newest message hits the provider.
    Chat {
        /// Optional conversational agent .ts file to chat through. It must accept
        /// `{ messages, system?, model?, tools? }` and return `{ transcript }`
        /// (or `{ history }`) — see the `chat` init template.
        agent: Option<PathBuf>,

        /// System prompt for the assistant.
        #[arg(short, long)]
        system: Option<String>,

        /// Model override (otherwise the provider default).
        #[arg(short, long)]
        model: Option<String>,

        /// Continue a previous chat session by its session id (printed when the
        /// session starts, and again at exit). Prior turns replay from the
        /// journal for $0; only new messages reach the provider.
        #[arg(long, value_name = "SESSION_ID")]
        resume: Option<String>,

        /// Run under the built-in deny-by-default `untrusted` policy profile.
        #[arg(long, conflicts_with = "trusted")]
        untrusted: bool,

        /// Opt out of the ask-before-powerful-effects default (see `run --trusted`).
        #[arg(long)]
        trusted: bool,
    },

    /// Replay a persisted run from its checkpoint. Re-runs the agent with
    /// the saved input and call log; LLM calls and other side effects return
    /// cached results instead of executing.
    Resume {
        /// Agent .ts file (same one the run was created from)
        file: PathBuf,

        /// Run id (subdirectory name under `.chidori/runs/`)
        run_id: String,

        /// Project dir containing `.chidori/runs/` (defaults to agent file's parent)
        #[arg(short, long)]
        dir: Option<PathBuf>,

        /// Time travel: replay only the records with seq <= N, then continue
        /// live from that frontier — re-driving the run's logic from an
        /// earlier point in its history (`docs/durable-storage.md`).
        #[arg(long)]
        until_seq: Option<u64>,

        /// Repair a failed run: strip the trailing failed record(s) — and any
        /// nested effects the failed call consumed — from the journal, replay
        /// everything before the failure from cache, then re-execute the
        /// failed call live. Errors if the run's journal has no trailing
        /// failure (a completed run needs nothing; a paused run wants plain
        /// `resume`). Mutually exclusive with `--until-seq`.
        #[arg(long, conflicts_with = "until_seq")]
        retry_failed: bool,

        /// Edit-and-resume: proceed even though the agent source changed
        /// since this run was recorded. Recorded calls replay positionally
        /// against the edited code; an edit that touches already-replayed
        /// calls fails loudly as a divergence, an edit past the pause point
        /// resumes cleanly.
        #[arg(long)]
        allow_source_change: bool,

        /// Default model for prompts executed live past the replay frontier.
        /// Defaults to the model recorded in the run's manifest, so a run
        /// started with `--model` resumes under the same model with no extra
        /// flags. Already-recorded prompts keep their recorded model; a
        /// recorded prompt whose model would change is a divergence and
        /// fails loudly.
        #[arg(long)]
        model: Option<String>,

        /// Deny gated effects (tool calls, network, workspace writes) that
        /// live continuation past the replay frontier would perform.
        #[arg(long, conflicts_with = "trusted")]
        untrusted: bool,

        /// Allow gated effects without asking during live continuation — the
        /// same trust the original `chidori run --trusted` had. Without it, a
        /// crash-resumed run re-asks at the terminal (and fails closed in
        /// scripts), even though the original run was trusted.
        #[arg(long)]
        trusted: bool,
    },

    /// Replay a recorded run as a deterministic test: re-run the agent with
    /// every host call served from the journal, with NO provider configured
    /// and NO writes to the run directory (top-level workspace effects
    /// re-materialize their recorded artifacts, byte-identical), and assert
    /// the run completes with output identical to the recorded one. Exit 0 on pass; non-zero with a
    /// diagnosis on drift (changed source refuses, a diverging call fails
    /// loudly, a run that tries to execute anything live fails). Commit a run
    /// directory to git and run this in CI — a full integration test that
    /// costs $0 and takes milliseconds.
    Verify {
        /// Agent .ts file (same one the run was created from)
        file: PathBuf,

        /// Run id (subdirectory name under `.chidori/runs/`)
        run_id: String,

        /// Project dir containing `.chidori/runs/` (defaults to agent file's parent)
        #[arg(short, long)]
        dir: Option<PathBuf>,
    },

    /// List a run's persisted `chidori.branch` sub-runs and their states.
    Branches {
        /// Run id (subdirectory name under `.chidori/runs/`)
        run_id: String,

        /// Project dir containing `.chidori/runs/` (defaults to current dir)
        #[arg(short, long)]
        dir: Option<PathBuf>,
    },

    /// Resume a paused branch sub-run by answering its pending input prompt.
    /// The branch replays its checkpoint with the response and continues to
    /// its next outcome; the parent run's history is untouched.
    BranchResume {
        /// Run id (subdirectory name under `.chidori/runs/`)
        run_id: String,

        /// Branch id, as reported in the branch outcome / `chidori branches`
        branch_id: String,

        /// The response to the branch's pending input prompt
        #[arg(short, long)]
        value: String,

        /// Project dir containing `.chidori/runs/` (defaults to current dir)
        #[arg(short, long)]
        dir: Option<PathBuf>,

        /// Default model for the branch's live prompts. Defaults to the model
        /// recorded in the parent run's manifest.
        #[arg(long)]
        model: Option<String>,

        /// Deny gated effects (tool calls, network, workspace writes).
        #[arg(long, conflicts_with = "trusted")]
        untrusted: bool,

        /// Allow gated effects without asking.
        #[arg(long)]
        trusted: bool,
    },

    /// Re-run a branch sub-run fresh from its parent anchor, using its stored
    /// (editable) `source.ts`. Edit the file under
    /// `.chidori/runs/<run>/branches/.../source.ts`, then re-run: only that
    /// strategy changes while the anchored state stays identical.
    BranchRerun {
        /// Run id (subdirectory name under `.chidori/runs/`)
        run_id: String,

        /// Branch id, as reported in the branch outcome / `chidori branches`
        branch_id: String,

        /// Project dir containing `.chidori/runs/` (defaults to current dir)
        #[arg(short, long)]
        dir: Option<PathBuf>,

        /// Default model for the branch's live prompts. Defaults to the model
        /// recorded in the parent run's manifest.
        #[arg(long)]
        model: Option<String>,

        /// Deny gated effects (tool calls, network, workspace writes).
        #[arg(long, conflicts_with = "trusted")]
        untrusted: bool,

        /// Allow gated effects without asking.
        #[arg(long)]
        trusted: bool,
    },

    /// Pretty-print a persisted run's call log.
    Trace {
        /// Run id (subdirectory name under `.chidori/runs/`)
        run_id: String,

        /// Project dir containing `.chidori/runs/` (defaults to current dir)
        #[arg(short, long)]
        dir: Option<PathBuf>,
    },

    /// Pretty-print a persisted run's runtime snapshot manifest.
    Snapshot {
        /// Run id (subdirectory name under `.chidori/runs/`)
        run_id: String,

        /// Project dir containing `.chidori/runs/` (defaults to current dir)
        #[arg(short, long)]
        dir: Option<PathBuf>,
    },

    /// Aggregate run history: total runs, tokens, est. cost, per-model breakdown.
    /// Reads `.chidori/runs/<id>/checkpoint.json` in the given directory.
    Stats {
        /// Directory containing agent runs (defaults to current dir)
        #[arg(short, long)]
        dir: Option<PathBuf>,
    },

    /// Serve an agent as an HTTP server.
    /// Every incoming request is passed to agent(event) as a structured event dict.
    Serve {
        /// Path to the agent .ts file. Optional: without it the server hosts
        /// only the detached-agent fleet (re-armed from `.chidori/runs/` in
        /// the current directory) — sessions must then name an agent per
        /// request via the `agent` field.
        file: Option<PathBuf>,

        /// Port to listen on
        #[arg(short, long, default_value = "8080")]
        port: u16,

        /// Address to bind. Defaults to loopback (127.0.0.1) so the server —
        /// which executes agent code — is not reachable from the network
        /// unless you opt in. Pass `--host 0.0.0.0` (or set CHIDORI_HOST) to
        /// expose it; a non-loopback bind requires CHIDORI_API_KEY to be set
        /// unless CHIDORI_ALLOW_UNAUTHENTICATED=1 explicitly opts out. The
        /// server speaks plain HTTP either way — terminate TLS in front of it.
        #[arg(long)]
        host: Option<String>,

        /// Print host function calls to stderr during execution
        #[arg(short, long)]
        verbose: bool,

        /// Default model for prompts that don't set one in code (equivalent
        /// to CHIDORI_MODEL), applied to every session this server runs.
        #[arg(long)]
        model: Option<String>,

        /// Serve under the built-in deny-by-default `untrusted` policy profile:
        /// gated effects (http, workspace mutations) are refused unless
        /// allowlisted. Equivalent to CHIDORI_POLICY_PROFILE=untrusted, but
        /// takes precedence over all CHIDORI_POLICY* env vars.
        ///
        /// This is also the server's default posture when no CHIDORI_POLICY*
        /// configuration is present; pass --trusted to opt back into the
        /// permissive allow-all default.
        #[arg(long, conflicts_with = "trusted")]
        untrusted: bool,

        /// Opt out of the server's deny-by-default posture: with no
        /// CHIDORI_POLICY* configuration, gated effects (http, workspace
        /// mutations) run without restriction. Explicit CHIDORI_POLICY*
        /// configuration still applies.
        #[arg(long)]
        trusted: bool,

        /// Run each request in an isolated child process, brokering its host
        /// effects back over a pipe (see docs/os-isolation-plan.md). Equivalent
        /// to CHIDORI_ISOLATE=process. This is the default on Unix; the flag
        /// remains as an explicit override of CHIDORI_ISOLATE=off. Composes
        /// with --untrusted.
        #[arg(long, conflicts_with = "no_isolate")]
        isolate: bool,

        /// Serve requests in-process, without the isolated worker sandbox.
        /// Equivalent to CHIDORI_ISOLATE=off.
        #[arg(long)]
        no_isolate: bool,
    },
}

fn main() {
    let cli = Cli::parse();

    // The isolate worker speaks a binary frame protocol over stdout, so it must
    // short-circuit before any of the normal startup path can write there.
    if let Commands::RunWorker = cli.command {
        std::process::exit(match on_js_stack(crate::runtime::isolate::worker::run) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("isolate worker error: {e}");
                1
            }
        });
    }

    // OS isolation is default-on for the CLI on platforms with a worker
    // sandbox: when CHIDORI_ISOLATE is unset, agent-running commands spawn a
    // confined child process per run. Explicit env values and the
    // --isolate/--no-isolate flags (handled per command below) always win.
    crate::runtime::isolate::default_on_if_unset();

    // Confine error-report source reads (snippets, stack-frame remaps) to the
    // entry agent's workspace root. The root lives in a THREAD-local (tests
    // need per-thread isolation), and error display spans two threads: the
    // command thread emits `--stream` failure events, while `report_cli_error`
    // below renders on this main thread — so the root must be stamped on
    // both, or the main-thread reporter silently falls back to the current
    // directory and absolute-path invocations lose their remap and snippet.
    let display_root = display_project_root_of(&cli.command);
    if let Some(root) = &display_root {
        crate::runtime::rust_engine::set_display_project_root(root.clone());
    }

    // Commands that only do parsing/validation return exit code 2 on failure;
    // everything else returns 1. Success is 0.
    let (result, parse_only) = on_js_stack(move || {
        if let Some(root) = display_root {
            crate::runtime::rust_engine::set_display_project_root(root);
        }
        dispatch_command(cli.command)
    });

    // Flush any buffered OTLP spans before the process exits. No-op when
    // OTEL_EXPORTER_OTLP_ENDPOINT wasn't set.
    crate::runtime::otel::shutdown_on_exit();

    match result {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            report_cli_error(&e);
            std::process::exit(if parse_only { 2 } else { 1 });
        }
    }
}

/// Run `f` on a thread with [`scheduler::JS_THREAD_STACK_BYTES`] of stack.
/// The interpreter recurses on the native stack (its depth guard allows 2000
/// JS frames), and the default main-thread stack aborts the whole process on
/// deep-but-legal recursion instead of letting the guard throw its catchable
/// RangeError — so every command body (and the isolate worker, whose agent
/// also runs on its process main thread) executes on one big-stack thread.
/// One thread per process: the thread-local compile/transpile caches stay
/// warm for the command's whole lifetime.
fn on_js_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .name("chidori-cmd".to_string())
        .stack_size(scheduler::JS_THREAD_STACK_BYTES)
        .spawn(f)
        .expect("spawning the command thread")
        .join()
        .expect("command thread panicked")
}

/// Dispatch one parsed CLI command to its handler, returning its result and
/// whether it is a parse/validation-only command (exit code 2 on failure).
/// The workspace root error display is confined to (see
/// `rust_engine::read_project_source`): a run/check names a `.ts` file, whose
/// workspace root is where its modules live. `None` for commands that run no
/// agent file.
fn display_project_root_of(command: &Commands) -> Option<PathBuf> {
    let file = match command {
        Commands::Run { file, .. }
        | Commands::Check { file }
        | Commands::Resume { file, .. }
        | Commands::Verify { file, .. } => file.clone(),
        Commands::Serve { file, .. } => file.clone()?,
        Commands::Chat { agent, .. } => agent.clone()?,
        _ => return None,
    };
    Some(crate::runtime::typescript::transpile::find_workspace_root(
        &file,
    ))
}

fn dispatch_command(command: Commands) -> (Result<()>, bool) {
    match command {
        Commands::Run {
            file,
            input,
            trace,
            verbose,
            model,
            stream,
            untrusted,
            trusted,
            isolate,
            no_isolate,
        } => {
            // `run_agent` reads this env var to decide whether to spawn a worker;
            // setting it here keeps the isolation decision in one place.
            if isolate {
                crate::runtime::isolate::enable();
            } else if no_isolate {
                crate::runtime::isolate::disable();
            }
            // The runtime (and any isolate worker child) resolves the default
            // model from CHIDORI_MODEL; the flag is a spelling of the env var.
            if let Some(ref model) = model {
                std::env::set_var("CHIDORI_MODEL", model);
            }
            // Propagate verbosity to the isolate worker child so its sandbox
            // degradation notes surface under -v.
            if verbose {
                std::env::set_var("CHIDORI_VERBOSE", "1");
            }
            crate::runtime::isolate::warn_if_untrusted_without_isolation(untrusted);
            let result = if stream {
                cmd_run_stream(&file, &input, verbose, untrusted, trusted)
            } else {
                cmd_run(&file, &input, trace, verbose, untrusted, trusted)
            };
            (result, false)
        }
        Commands::RunWorker => unreachable!("handled before the dispatch match"),
        Commands::Demo => (cmd_demo(), false),
        Commands::ModelLogin => (cmd_login(), false),
        Commands::Add { packages, dev, dir } => (
            pkg::cmd_add(&dir.unwrap_or_else(|| PathBuf::from(".")), &packages, dev),
            false,
        ),
        Commands::Install { frozen, dir } => (
            pkg::cmd_install(&dir.unwrap_or_else(|| PathBuf::from(".")), frozen),
            false,
        ),
        Commands::Remove { packages, dir } => (
            pkg::cmd_remove(&dir.unwrap_or_else(|| PathBuf::from(".")), &packages),
            false,
        ),
        Commands::Init { dir, template } => (
            init::run(
                &dir.unwrap_or_else(|| PathBuf::from(".")),
                template.as_deref(),
            ),
            false,
        ),
        Commands::Chat {
            agent,
            system,
            model,
            resume,
            untrusted,
            trusted,
        } => (
            cmd_chat(agent.as_deref(), system, model, resume, untrusted, trusted),
            false,
        ),
        Commands::Check { file } => (cmd_check(&file), true),
        Commands::Stats { dir } => (cmd_stats(dir.as_deref()), false),
        Commands::Resume {
            file,
            run_id,
            dir,
            until_seq,
            retry_failed,
            allow_source_change,
            model,
            untrusted,
            trusted,
        } => (
            {
                if let Some(ref model) = model {
                    std::env::set_var("CHIDORI_MODEL", model);
                }
                cmd_resume(
                    &file,
                    &run_id,
                    dir.as_deref(),
                    until_seq,
                    retry_failed,
                    allow_source_change,
                    model,
                    untrusted,
                    trusted,
                )
            },
            false,
        ),
        Commands::Verify { file, run_id, dir } => {
            (cmd_verify(&file, &run_id, dir.as_deref()), false)
        }
        Commands::Branches { run_id, dir } => (cmd_branches(&run_id, dir.as_deref()), false),
        Commands::BranchResume {
            run_id,
            branch_id,
            value,
            dir,
            model,
            untrusted,
            trusted,
        } => (
            cmd_branch_resume(
                &run_id,
                &branch_id,
                &value,
                dir.as_deref(),
                model,
                untrusted,
                trusted,
            ),
            false,
        ),
        Commands::BranchRerun {
            run_id,
            branch_id,
            dir,
            model,
            untrusted,
            trusted,
        } => (
            cmd_branch_rerun(
                &run_id,
                &branch_id,
                dir.as_deref(),
                model,
                untrusted,
                trusted,
            ),
            false,
        ),
        Commands::Trace { run_id, dir } => (cmd_trace(&run_id, dir.as_deref()), false),
        Commands::Snapshot { run_id, dir } => (cmd_snapshot(&run_id, dir.as_deref()), false),
        Commands::Serve {
            file,
            port,
            host,
            verbose,
            model,
            untrusted,
            trusted,
            isolate,
            no_isolate,
        } => {
            if isolate {
                crate::runtime::isolate::enable();
            } else if no_isolate {
                crate::runtime::isolate::disable();
            }
            if let Some(ref model) = model {
                std::env::set_var("CHIDORI_MODEL", model);
            }
            (
                cmd_serve(
                    file.as_deref(),
                    host.as_deref(),
                    port,
                    verbose,
                    untrusted,
                    trusted,
                ),
                false,
            )
        }
    }
}

/// Print a failed command's error to stderr. An uncaught JavaScript exception
/// (the `JavaScript exception:` framing from `runtime::rust_engine`, carrying
/// the stack frames recorded on the thrown error's `.stack`, already remapped
/// to original-source coordinates) renders through miette's graphical report
/// handler — the same presentation TypeScript parse errors already get. The
/// innermost frames that live in a readable source file additionally render
/// as a labeled snippet of that file, one caret per frame, the way rustc
/// points at code. Every other error keeps the plain anyhow context chain.
/// This is presentation only: the compact `JavaScript exception: …` string is
/// what the durable records, `--stream` events, and server responses carry.
fn report_cli_error(e: &anyhow::Error) {
    use crate::runtime::rust_engine::parse_stack_frame;
    use oxc::diagnostics::{
        GraphicalReportHandler, GraphicalTheme, LabeledSpan, NamedSource, OxcDiagnostic,
    };

    let text = format!("{e:#}");
    let Some(idx) = text.find("JavaScript exception: ") else {
        eprintln!("Error: {text}");
        return;
    };
    // `{:#}` prints outer contexts first, so everything before the marker is
    // context ("resume refused: …") and everything after it is the thrown
    // error's `Name: message` line plus the recorded `    at …` frames. The
    // frames arrive in transpiled-bundle coordinates; remap them to the
    // original TypeScript here, at the single display boundary.
    let body = crate::runtime::rust_engine::remap_stack_frames(
        &text[idx + "JavaScript exception: ".len()..],
    );
    let body = body.as_str();
    let context = text[..idx].trim_end().trim_end_matches(':');

    // Snippet: the innermost frame with a readable file anchors it, and every
    // frame in that same file becomes a labeled caret (capped so a deep
    // same-file recursion stays readable).
    const MAX_SNIPPET_LABELS: usize = 6;
    let frames: Vec<_> = body.lines().skip(1).filter_map(parse_stack_frame).collect();
    let snippet_source = frames.iter().find_map(|f| {
        let file = f.file?;
        // Confined to the project root — see `read_project_source`. A frame's
        // file is agent-controlled (via `.stack`); never render a snippet of
        // something outside the project the operator is running.
        Some((
            file,
            crate::runtime::rust_engine::read_project_source(file)?,
        ))
    });
    let mut diagnostic = OxcDiagnostic::error(body.to_string());
    if let Some((file, source)) = &snippet_source {
        let mut seen = std::collections::HashSet::new();
        let labels: Vec<LabeledSpan> = frames
            .iter()
            .filter(|f| f.file == Some(file))
            .filter_map(|f| {
                let offset = byte_offset_of(source, f.line, f.col)?;
                seen.insert(offset).then(|| {
                    LabeledSpan::new(
                        Some(format!("at {}", f.name)),
                        offset,
                        identifier_len_at(source, offset).max(1),
                    )
                })
            })
            .take(MAX_SNIPPET_LABELS)
            .collect();
        if !labels.is_empty() {
            diagnostic = diagnostic.with_labels(labels);
        }
    }

    let handler = GraphicalReportHandler::new_themed(GraphicalTheme::unicode_nocolor());
    let mut rendered = String::new();
    let ok = match snippet_source {
        Some((file, source)) if diagnostic.labels.is_some() => {
            let report = diagnostic.with_source_code(NamedSource::new(file, source));
            handler
                .render_report(&mut rendered, report.as_ref())
                .is_ok()
        }
        _ => handler.render_report(&mut rendered, &diagnostic).is_ok(),
    };
    if !ok {
        eprintln!("Error: {text}");
        return;
    }
    if context.is_empty() {
        eprintln!("Error: uncaught JavaScript exception{rendered}");
    } else {
        eprintln!("Error: {context}: uncaught JavaScript exception{rendered}");
    }
}

/// Byte offset of a 1-based (line, character-column) position in `src`.
fn byte_offset_of(src: &str, line: u32, col: u32) -> Option<usize> {
    let mut offset = 0usize;
    for (i, l) in src.split_inclusive('\n').enumerate() {
        if i + 1 == line as usize {
            let mut bytes = 0usize;
            for (n, c) in l.chars().enumerate() {
                if n + 1 >= col as usize {
                    break;
                }
                bytes += c.len_utf8();
            }
            return Some(offset + bytes);
        }
        offset += l.len();
    }
    None
}

/// Length in bytes of the identifier starting at `offset` (0 when the byte
/// there doesn't start one) — so a frame label underlines the function name
/// it points at rather than a single character.
fn identifier_len_at(src: &str, offset: usize) -> usize {
    src[offset..]
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
        .map(char::len_utf8)
        .sum()
}

struct DemoExample {
    title: &'static str,
    description: &'static str,
    command: &'static str,
    requires_provider: bool,
    action: DemoAction,
}

enum DemoAction {
    Run {
        file: &'static str,
        input: &'static [&'static str],
        trace: bool,
        stream: bool,
    },
    Serve {
        file: &'static str,
        port: u16,
    },
}

fn demo_examples() -> Vec<DemoExample> {
    vec![
        DemoExample {
            title: "Hello agent",
            description: "Runs a minimal TypeScript agent and records a durable log.",
            command: "chidori run examples/agents/hello.ts --input name=Colton",
            requires_provider: false,
            action: DemoAction::Run {
                file: "examples/agents/hello.ts",
                input: &["name=Colton"],
                trace: false,
                stream: false,
            },
        },
        DemoExample {
            title: "Tool call",
            description: "Defines a tool inline with defineTool and calls it from an agent.",
            command: "chidori run examples/agents/tool_use.ts --input query=chidori",
            requires_provider: false,
            action: DemoAction::Run {
                file: "examples/agents/tool_use.ts",
                input: &["query=chidori"],
                trace: false,
                stream: false,
            },
        },
        DemoExample {
            title: "Summarizer with trace",
            description: "Calls an LLM and prints the host-call trace after the run.",
            command: "chidori run examples/agents/summarizer.ts --input document=\"Rust is great.\" --trace",
            requires_provider: true,
            action: DemoAction::Run {
                file: "examples/agents/summarizer.ts",
                input: &["document=Rust is great."],
                trace: true,
                stream: false,
            },
        },
        DemoExample {
            title: "Parallel prompts",
            description: "Runs two prompt branches concurrently inside one agent.",
            command: "chidori run examples/agents/parallel.ts --input '{\"topic\":\"runtime snapshots\"}'",
            requires_provider: true,
            action: DemoAction::Run {
                file: "examples/agents/parallel.ts",
                input: &["{\"topic\":\"runtime snapshots\"}"],
                trace: false,
                stream: false,
            },
        },
        DemoExample {
            title: "Streaming progress",
            description: "Emits newline-delimited runtime events while prompt work runs.",
            command: "chidori run examples/agents/streaming_progress.ts --input topic=\"runtime snapshots\" --stream",
            requires_provider: true,
            action: DemoAction::Run {
                file: "examples/agents/streaming_progress.ts",
                input: &["topic=runtime snapshots"],
                trace: false,
                stream: true,
            },
        },
        DemoExample {
            title: "Human input server",
            description: "Starts the session server for the input/resume example.",
            command: "chidori serve examples/agents/input_pause.ts --port 8080",
            requires_provider: false,
            action: DemoAction::Serve {
                file: "examples/agents/input_pause.ts",
                port: 8080,
            },
        },
    ]
}

fn cmd_demo() -> Result<()> {
    let demos = demo_examples();

    println!("Chidori demos");
    println!();
    for (idx, demo) in demos.iter().enumerate() {
        let provider_note = if demo.requires_provider {
            " (requires an LLM provider)"
        } else {
            ""
        };
        println!("  {}. {}{}", idx + 1, demo.title, provider_note);
        println!("     {}", demo.description);
    }
    println!();

    let Some(choice) = prompt_demo_choice(demos.len())? else {
        return Ok(());
    };
    let demo = &demos[choice];

    println!();
    println!("Running: {}", demo.command);

    if demo.requires_provider && !ensure_llm_provider_interactive() {
        println!();
        println!("This demo needs an LLM provider. Either sign in with OpenRouter:");
        println!("  chidori model-login");
        println!("or set one of:");
        println!("  export ANTHROPIC_API_KEY=sk-ant-...");
        println!("  export OPENAI_API_KEY=sk-...");
        println!("  # any OpenAI-compatible endpoint (DeepSeek, Groq, Ollama, vLLM, LiteLLM...):");
        println!("  export CHIDORI_OPENAI_COMPAT_URL=https://api.deepseek.com");
        println!("  export CHIDORI_OPENAI_COMPAT_KEY=sk-...");
        return Ok(());
    }

    match &demo.action {
        DemoAction::Run {
            file,
            input,
            trace,
            stream,
        } => {
            let file = PathBuf::from(file);
            let inputs = input
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>();
            // The demo runs the repo's own example agents on the developer's
            // machine — the trusted posture, like `run --trusted`.
            if *stream {
                cmd_run_stream(&file, &inputs, false, false, true)
            } else {
                cmd_run(&file, &inputs, *trace, false, false, true)
            }
        }
        DemoAction::Serve { file, port } => {
            if !confirm_start_server(*port)? {
                return Ok(());
            }
            // The demo serves the developer's own example agent on their own
            // machine — the trusted posture, like `chidori run`, on the
            // default loopback bind.
            cmd_serve(Some(&PathBuf::from(file)), None, *port, false, false, true)
        }
    }
}

fn prompt_demo_choice(max: usize) -> Result<Option<usize>> {
    use std::io::Write;

    loop {
        print!("Choose a demo [1-{max}] or q to quit: ");
        std::io::stdout().flush()?;

        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            return Ok(None);
        }

        let value = line.trim();
        if value.eq_ignore_ascii_case("q") || value.eq_ignore_ascii_case("quit") {
            return Ok(None);
        }

        if let Ok(choice) = value.parse::<usize>() {
            if (1..=max).contains(&choice) {
                return Ok(Some(choice - 1));
            }
        }

        eprintln!("Enter a number from 1 to {max}, or q to quit.");
    }
}

fn confirm_start_server(port: u16) -> Result<bool> {
    use std::io::Write;

    println!();
    println!("This starts a server on http://localhost:{port} and runs until Ctrl-C.");
    print!("Start it now? [y/N] ");
    std::io::stdout().flush()?;

    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 {
        return Ok(false);
    }

    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn has_llm_provider() -> bool {
    std::env::var_os("ANTHROPIC_API_KEY").is_some()
        || std::env::var_os("OPENAI_API_KEY").is_some()
        || std::env::var_os("CHIDORI_OPENAI_COMPAT_URL").is_some()
        || std::env::var_os("LITELLM_API_URL").is_some()
        || providers::openrouter::saved_api_key().is_some()
}

/// Explicit `chidori model-login`: run the OpenRouter OAuth flow and save the key.
fn cmd_login() -> Result<()> {
    // An explicit env key already wins over any saved credential, so a browser
    // sign-in would be pointless — respect it and bow out.
    if std::env::var_os("OPENROUTER_API_KEY").is_some() {
        println!(
            "OPENROUTER_API_KEY is already set — using it. Unset it to sign in with OAuth instead."
        );
        return Ok(());
    }
    if providers::openrouter::credentials_path()
        .map(|p| p.exists())
        .unwrap_or(false)
    {
        println!(
            "Already signed in to OpenRouter — re-running the browser sign-in to refresh the key…"
        );
    }
    providers::openrouter::login_and_save()?;
    Ok(())
}

/// Shared fallback for the interactive "try it out" surfaces (`demo`, `chat`,
/// interactive `run`): if no provider is configured, offer an OpenRouter OAuth
/// sign-in. Returns `true` when a provider is available afterwards.
///
/// Non-interactive callers (no TTY, e.g. piped/scripted runs) never block on a
/// prompt — they just report `false` so the caller can surface the usual
/// "set a key" guidance instead of hanging.
fn ensure_llm_provider_interactive() -> bool {
    use std::io::IsTerminal;

    if has_llm_provider() {
        return true;
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return false;
    }

    println!();
    println!(
        "No LLM provider key found (ANTHROPIC_API_KEY / OPENAI_API_KEY / \
         CHIDORI_OPENAI_COMPAT_URL)."
    );
    println!("You can sign in with OpenRouter to try this out — no API key setup needed.");
    if !providers::openrouter::confirm_login() {
        return false;
    }
    match providers::openrouter::login_and_save() {
        Ok(_) => true,
        Err(err) => {
            eprintln!("OpenRouter sign-in failed: {err}");
            false
        }
    }
}

/// Resolve the permission policy for a CLI invocation. Precedence:
///   1. `--untrusted` — deny-by-default, wins over all CHIDORI_POLICY* env
///      (an explicit flag beats ambient configuration).
///   2. `--trusted` — the historical permissive resolution: env-driven,
///      allow-all when nothing is configured.
///   3. Explicit, valid CHIDORI_POLICY* configuration — as configured.
///   4. Nothing configured — ask-before-powerful-effects
///      ([`policy::run_default_profile`]): the operator approves gated
///      effects at a terminal prompt, and non-interactive runs fail closed
///      with a reason naming `--trusted` and the env knobs.
fn cli_policy(untrusted: bool, trusted: bool) -> Arc<policy::PolicyConfig> {
    if untrusted {
        return Arc::new(
            policy::builtin_profile("untrusted").expect("built-in untrusted profile exists"),
        );
    }
    if trusted {
        return policy::PolicyConfig::from_env();
    }
    policy::PolicyConfig::from_env_configured()
        .unwrap_or_else(|| Arc::new(policy::run_default_profile()))
}

/// Resolve the permission policy for `chidori serve`. Unlike `chidori run`
/// (trusted, developer-authored code on the developer's own machine), the
/// server is the surface untrusted callers reach, so when the operator has
/// said nothing it is deny-by-default. Precedence:
///   1. `--untrusted` — deny-by-default, wins over all CHIDORI_POLICY* env.
///   2. `--trusted` — the permissive `chidori run` resolution (env-driven,
///      allow-all when nothing is configured).
///   3. Explicit, valid CHIDORI_POLICY* configuration — as configured.
///   4. Nothing configured (or only malformed configuration, which fails
///      closed) — the deny-by-default serve profile.
///
/// Returns the policy plus a posture label for the startup banner.
fn serve_policy(untrusted: bool, trusted: bool) -> (Arc<policy::PolicyConfig>, String) {
    if untrusted {
        return (
            Arc::new(
                policy::builtin_profile("untrusted").expect("built-in untrusted profile exists"),
            ),
            "deny-by-default (--untrusted)".to_string(),
        );
    }
    if trusted {
        return (
            policy::PolicyConfig::from_env(),
            "trusted (--trusted; CHIDORI_POLICY* env still applies)".to_string(),
        );
    }
    match policy::PolicyConfig::from_env_configured() {
        Some(cfg) => (cfg, "from CHIDORI_POLICY* configuration".to_string()),
        None => (
            Arc::new(policy::serve_default_profile()),
            "deny-by-default (no policy configured; pass --trusted or set CHIDORI_POLICY* to relax)"
                .to_string(),
        ),
    }
}

/// Resolve a project base directory to an absolute path so the workspace root
/// stays stable even if the process later changes its current directory. Falls
/// back to joining the CWD when the path can't be canonicalized (e.g. it's
/// relative and some component doesn't exist yet).
fn abs_dir(dir: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(dir).unwrap_or_else(|_| {
        std::env::current_dir()
            .map(|cwd| cwd.join(dir))
            .unwrap_or_else(|_| dir.to_path_buf())
    })
}

/// Spawn a stderr progress listener for plain (non `--stream`) runs: one line
/// per live prompt call, so a long model call shows a sign of life instead of
/// dead air until the run ends. Reuses the runtime's existing event channel —
/// the returned sender is handed to the engine's `*_streaming` entry point and
/// the drain thread prints only `PromptStart`/`PromptEnd` (per-record `Call`
/// and per-token `PromptDelta` events flow on the same channel and are
/// ignored). Replayed and locally-cached prompt calls short-circuit in
/// `host_core` before the provider-request path that emits `PromptStart`, so
/// a resume never prints phantom "started" lines for calls it served from the
/// journal. Stdout is untouched: it stays reserved for the agent's output.
///
/// Returns `None` when `CHIDORI_QUIET` is set (to anything but `0`/empty),
/// the opt-out for scripts that want the old fully-silent stderr; the caller
/// then runs without an event sender attached, exactly as before.
fn spawn_prompt_progress_listener() -> Option<(
    tokio::sync::mpsc::UnboundedSender<crate::runtime::context::RuntimeEvent>,
    std::thread::JoinHandle<()>,
)> {
    if std::env::var_os("CHIDORI_QUIET").is_some_and(|v| !v.is_empty() && v != "0") {
        return None;
    }
    let (event_tx, event_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::runtime::context::RuntimeEvent>();
    let drain = std::thread::spawn(move || {
        use crate::runtime::context::RuntimeEvent;
        use std::collections::HashMap;
        use std::time::Instant;

        let mut rx = event_rx;
        let mut started: HashMap<String, Instant> = HashMap::new();
        while let Some(event) = rx.blocking_recv() {
            match event {
                RuntimeEvent::PromptStart {
                    stream_id,
                    seq,
                    model,
                    ..
                } => {
                    started.insert(stream_id, Instant::now());
                    eprintln!("seq {seq}: prompt started ({model})");
                }
                RuntimeEvent::PromptEnd {
                    stream_id,
                    seq,
                    error,
                    ..
                } => {
                    // A failed prompt surfaces through the run's own error
                    // path; the progress line only marks successful finishes.
                    let elapsed = started.remove(&stream_id);
                    if error.is_none() {
                        if let Some(t0) = elapsed {
                            eprintln!(
                                "seq {seq}: prompt finished ({:.1}s)",
                                t0.elapsed().as_secs_f64()
                            );
                        }
                    }
                }
                RuntimeEvent::Call(_) | RuntimeEvent::PromptDelta { .. } => {}
            }
        }
    });
    Some((event_tx, drain))
}

fn cmd_run(
    file: &Path,
    inputs: &[String],
    trace: bool,
    verbose: bool,
    untrusted: bool,
    trusted: bool,
) -> Result<()> {
    // Set up tracing.
    if verbose {
        tracing_subscriber::fmt()
            .with_env_filter("info")
            .with_target(false)
            .with_writer(std::io::stderr)
            .init();
    }

    // Parse inputs into a JSON object.
    let input_value = parse_inputs(inputs)?;

    // The durable defaults pin the clock to the epoch and seed Math.random()
    // so replay is byte-identical — powerful, but invisible: 1970 timestamps
    // and repeating "random" values look like bugs to a first-time author.
    // Say it once, only when the defaults are in effect.
    // Terminal-only: interactive authors get the hint, scripts and CI stay quiet.
    use std::io::IsTerminal;
    if std::io::stderr().is_terminal()
        && std::env::var_os("CHIDORI_TS_DATE").is_none()
        && std::env::var_os("CHIDORI_TS_RANDOM").is_none()
    {
        eprintln!(
            "determinism: clock pinned to epoch, Math.random() seeded (replay-safe defaults; \
             override with CHIDORI_TS_DATE / CHIDORI_TS_RANDOM, see docs/replay.md)"
        );
    }

    // Resolve the project base directory.
    let base_dir = file
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();

    // Build the runtime.
    let providers = Arc::new(ProviderRegistry::from_env());
    let template_engine = Arc::new(TemplateEngine::new(&base_dir));
    let tokio_rt =
        Arc::new(scheduler::new_tokio_runtime().context("Failed to create tokio runtime")?);

    // Agent tools are defined in-VM with `defineTool`; the registry is for
    // externally-sourced tools only (MCP), unused on the plain CLI path.
    let tools = Arc::new(ToolRegistry::new());

    let engine = Engine::new(providers, template_engine, tokio_rt)
        .with_tools(tools)
        .with_policy(cli_policy(untrusted, trusted))
        .with_persist_base(base_dir.join(".chidori").join("runs"))
        .with_workspace_root(abs_dir(&base_dir));

    // Run the agent.
    // Announce the run id up front (stderr): after a crash — where buffered
    // stdout is lost — the id `chidori resume` needs is already on record.
    // With the progress listener attached (the default), each live prompt
    // call also gets a one-line stderr note — long model calls are otherwise
    // total silence on the plain path. CHIDORI_QUIET=1 restores that silence.
    let result = match spawn_prompt_progress_listener() {
        Some((event_tx, drain)) => {
            let result = engine.run_streaming_announced(file, &input_value, event_tx);
            // The sender moved into the engine and drops when the run
            // returns; join so the last progress lines land before output.
            drain.join().ok();
            result?
        }
        None => engine.run_announced(file, &input_value)?,
    };

    // A `chidori.signal(name)` listen point with an empty mailbox pauses the run
    // (there is no stdin fallback for signals, unlike `input()`). The engine has
    // already persisted the durable pause scaffold under `.chidori/runs/<run_id>`;
    // tell the user the run is awaiting a signal and how to deliver one rather
    // than printing a bare `null` output. See `docs/signals.md`.
    if let Some(signal) = &result.paused_signal {
        let names = signal.listen_names();
        eprintln!(
            "Run {} paused, awaiting signal{} '{}'.",
            result.run_id,
            if names.len() > 1 { " (any of)" } else { "" },
            names.join("', '")
        );
        eprintln!(
            "Deliver it with: POST /sessions/{{id}}/signal \
             {{\"name\":\"{}\",\"payload\":...,\"from\":...}} \
             (or resume the run server-side).",
            signal.name
        );
        return Ok(());
    }

    // Print the output.
    let output_str = serde_json::to_string_pretty(&result.output)?;
    println!("{output_str}");

    // Print trace if requested.
    if trace {
        let trace_json = result.call_log.to_json()?;
        eprintln!("\n--- Trace ---");
        eprintln!("{trace_json}");

        let (input_tokens, output_tokens) = result.call_log.total_tokens();
        if input_tokens > 0 || output_tokens > 0 {
            eprintln!(
                "\nTokens: {} input, {} output, {} total",
                input_tokens,
                output_tokens,
                input_tokens + output_tokens
            );
            let cost = result.call_log.total_cost_usd();
            if cost > 0.0 {
                eprintln!("Est. cost: ${:.6}", cost);
            }
        }
        eprintln!("Duration: {}ms", result.call_log.total_duration_ms());
    }

    Ok(())
}

/// Like `cmd_run` but emits each `CallRecord` as a newline-delimited JSON
/// event to stdout as the agent executes, then a final `done` event. Used by
/// the builder server's SSE streaming bridge.
fn cmd_run_stream(
    file: &Path,
    inputs: &[String],
    verbose: bool,
    untrusted: bool,
    trusted: bool,
) -> Result<()> {
    use tokio::sync::mpsc;

    if verbose {
        tracing_subscriber::fmt()
            .with_env_filter("info")
            .with_target(false)
            .with_writer(std::io::stderr)
            .init();
    }

    let input_value = parse_inputs(inputs)?;
    let base_dir = file
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();

    let providers = Arc::new(ProviderRegistry::from_env());
    let template_engine = Arc::new(TemplateEngine::new(&base_dir));
    let tokio_rt =
        Arc::new(scheduler::new_tokio_runtime().context("Failed to create tokio runtime")?);

    let tools = Arc::new(ToolRegistry::new());

    // Same posture as the plain `run` path: the agent's project directory is
    // the implicit workspace root, and the run journals under
    // `.chidori/runs/<run_id>` — `--stream` changes how progress is reported,
    // never what the runtime can do or what survives a crash.
    let engine = Engine::new(providers, template_engine, tokio_rt)
        .with_tools(tools)
        .with_policy(cli_policy(untrusted, trusted))
        .with_persist_base(base_dir.join(".chidori").join("runs"))
        .with_workspace_root(abs_dir(&base_dir));

    let (event_tx, event_rx) = mpsc::unbounded_channel::<crate::runtime::context::RuntimeEvent>();

    // Drain thread: reads events from the channel and writes NDJSON to stdout
    // concurrently with the engine's execution.
    let drain_handle = std::thread::spawn(move || {
        use crate::runtime::context::RuntimeEvent;
        let mut rx = event_rx;
        while let Some(evt) = rx.blocking_recv() {
            let line = match evt {
                RuntimeEvent::Call(record) => {
                    serde_json::json!({ "type": "call", "record": record })
                }
                RuntimeEvent::PromptStart {
                    stream_id,
                    seq,
                    prompt_type,
                    model,
                } => serde_json::json!({
                    "type": "prompt_start",
                    "stream_id": stream_id,
                    "seq": seq,
                    "prompt_type": prompt_type,
                    "model": model,
                }),
                RuntimeEvent::PromptDelta {
                    stream_id,
                    seq,
                    prompt_type,
                    delta,
                } => serde_json::json!({
                    "type": "prompt_delta",
                    "stream_id": stream_id,
                    "seq": seq,
                    "prompt_type": prompt_type,
                    "delta": delta,
                }),
                RuntimeEvent::PromptEnd {
                    stream_id,
                    seq,
                    prompt_type,
                    error,
                } => serde_json::json!({
                    "type": "prompt_end",
                    "stream_id": stream_id,
                    "seq": seq,
                    "prompt_type": prompt_type,
                    "error": error,
                }),
            };
            println!("{line}");
        }
    });

    let result = engine.run_streaming_announced(file, &input_value, event_tx);

    // event_tx was moved into the engine; it is dropped when run_streaming
    // returns, which causes blocking_recv() in the drain thread to return None.
    drain_handle.join().ok();

    match result {
        Ok(r) => {
            // A `chidori.signal(...)` listen point with an empty mailbox pauses
            // the run; the persisted scaffold is resumable exactly like the
            // plain-run case, so report `paused` (with the pending names)
            // rather than a `completed` with a null output.
            let line = if let Some(signal) = &r.paused_signal {
                serde_json::json!({
                    "type": "done",
                    "status": "paused",
                    "run_id": r.run_id,
                    "pending_signal": signal.listen_names(),
                })
            } else {
                serde_json::json!({
                    "type": "done",
                    "status": "completed",
                    "run_id": r.run_id,
                    "output": r.output,
                })
            };
            println!("{line}");
            Ok(())
        }
        Err(e) => {
            // Frames arrive in transpiled coordinates; the stream consumer
            // sees the same original-TypeScript positions the CLI reporter
            // shows. The returned error stays raw — report_cli_error remaps
            // it once at its own display boundary.
            let line = serde_json::json!({
                "type": "done",
                "status": "failed",
                "error": crate::runtime::rust_engine::remap_stack_frames(&format!("{e:#}")),
            });
            println!("{line}");
            Err(e)
        }
    }
}

/// Interactive multi-turn chat REPL. Owns the loop in Rust so all terminal I/O
/// is single-threaded (no streaming/stdin races): each turn appends the user's
/// line, re-runs the conversational agent with the prior call log replayed
/// (prior turns are free), streams the newest assistant reply, and carries the
/// merged call log forward.
///
/// The whole session is one durable run: every turn journals into
/// `.chidori/runs/<session_id>` under the agent's directory (the cwd for the
/// built-in agent), the run's `input.json` always holds the full dialogue
/// state, and `--resume <session_id>` replays the journal — restoring the
/// transcript for $0 — and continues the conversation in place. A crash mid-
/// generation loses at most the reply being streamed; `--resume` completes it
/// live.
///
/// With no `agent`, a built-in conversational agent (`init::CHAT_AGENT_SRC`) is
/// written to a temp file. With an `agent`, that file is used instead; it must
/// follow the same contract — accept `{ messages, system?, model?, tools? }` and
/// return `{ transcript }` or `{ history }` of `{ role, text }` turns.
fn cmd_chat(
    agent: Option<&std::path::Path>,
    mut system: Option<String>,
    mut model: Option<String>,
    resume: Option<String>,
    untrusted: bool,
    trusted: bool,
) -> Result<()> {
    use crate::runtime::context::RuntimeEvent;
    use std::io::Write;

    // Resolve the agent file and the base directory for tool/template discovery.
    // A built-in agent goes to a temp file (so it works from an installed binary
    // with no source tree); a provided agent runs in place.
    let mut temp_dir: Option<PathBuf> = None;
    let (agent_path, base_dir) = match agent {
        Some(path) => (
            path.to_path_buf(),
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| std::path::Path::new("."))
                .to_path_buf(),
        ),
        None => {
            let dir = std::env::temp_dir().join(format!("chidori-chat-{}", std::process::id()));
            std::fs::create_dir_all(&dir).context("Failed to create chat temp dir")?;
            let path = dir.join("chat_agent.ts");
            std::fs::write(&path, init::CHAT_AGENT_SRC).context("Failed to write chat agent")?;
            temp_dir = Some(dir);
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            (path, cwd)
        }
    };

    // A chat session is an ordinary durable run: every turn journals into
    // `.chidori/runs/<session_id>` next to the agent (the cwd for the built-in
    // agent), so the conversation survives crashes, is inspectable with
    // `chidori trace`/`verify`, and can be continued with `--resume`.
    let run_base = base_dir.join(".chidori").join("runs");
    let factory = crate::runtime::store::RunStoreFactory::shared(&run_base);
    let lease_owner = format!("chidori-chat-{}", std::process::id());
    let mut messages: Vec<String> = Vec::new();
    let mut call_log: Vec<crate::runtime::call_log::CallRecord> = Vec::new();
    let session_id = match &resume {
        Some(session_id) => {
            let run_dir = run_base.join(session_id);
            // Load through the run store: hydrates from a durable mirror when
            // configured, and unions the last checkpoint with any
            // crash-stranded `records.jsonl` tail — same path as `resume`.
            let _ = factory.hydrate(session_id);
            call_log = factory
                .store_for(session_id)
                .load_call_log()?
                .ok_or_else(|| {
                    anyhow::anyhow!("no chat session found under {}", run_dir.display())
                })?;
            // One driver per session journal, same guard (and same
            // unrenewed-lease limitation) as `chidori resume`.
            match crate::runtime::store::acquire_lease(
                factory.store_for(session_id).as_ref(),
                &lease_owner,
                chrono::Duration::minutes(10),
            ) {
                Ok(Ok(_)) => {}
                Ok(Err(holder)) => anyhow::bail!(
                    "chat session {session_id} is already being driven by another process \
                     (lease holder `{}`, expires {}). Two concurrent drivers would corrupt \
                     the journal — close the other chat, or delete {} if the holder is dead.",
                    holder.owner,
                    holder.expires_at,
                    run_dir.join("lease.json").display()
                ),
                Err(err) => {
                    eprintln!("warning: could not take the session lease: {err}");
                }
            }
            // Each turn rewrites the run's `input.json` with the full driven
            // input, so it is the durable record of the dialogue state:
            // restore the message list, and (unless overridden by flags) the
            // session's system prompt and model.
            if let Ok(text) = std::fs::read_to_string(run_dir.join("input.json")) {
                if let Ok(saved) = serde_json::from_str::<Value>(&text) {
                    if let Some(saved_messages) = saved.get("messages").and_then(Value::as_array) {
                        messages = saved_messages
                            .iter()
                            .filter_map(Value::as_str)
                            .map(String::from)
                            .collect();
                    }
                    if system.is_none() {
                        system = saved
                            .get("system")
                            .and_then(Value::as_str)
                            .map(String::from);
                    }
                    if model.is_none() {
                        model = saved.get("model").and_then(Value::as_str).map(String::from);
                    }
                }
            }
            // An explicit agent file must still match the recorded source
            // fingerprints, exactly like `chidori resume`. The built-in agent
            // is a compiled-in constant written to a fresh temp path each
            // process, so path-keyed validation cannot apply to it.
            if agent.is_some() {
                crate::runtime::snapshot::validate_manifest_for_resume(
                    &run_base,
                    Some(session_id),
                    &agent_path,
                    false,
                )
                .context(
                    "chat --resume refused: the agent source no longer matches this \
                     session's journal",
                )?;
            }
            session_id.clone()
        }
        None => uuid::Uuid::new_v4().to_string(),
    };

    // Chat always calls the model, so offer an OpenRouter sign-in up front when
    // no provider key is configured — building the registry after so it picks
    // up a freshly saved key.
    let _ = ensure_llm_provider_interactive();
    let providers = Arc::new(ProviderRegistry::from_env());
    let template_engine = Arc::new(TemplateEngine::new(&base_dir));
    let tokio_rt =
        Arc::new(scheduler::new_tokio_runtime().context("Failed to create tokio runtime")?);

    let tools = Arc::new(ToolRegistry::new());
    let tool_names: Vec<String> = tools.list().iter().map(|t| t.name.clone()).collect();

    let engine = Engine::new(providers, template_engine, tokio_rt)
        .with_tools(tools)
        .with_policy(cli_policy(untrusted, trusted))
        .with_persist_base(run_base.clone())
        .with_workspace_root(abs_dir(&base_dir));

    eprintln!("chidori chat — type a message and press enter. Type 'exit' or Ctrl-D to quit.");
    eprintln!(
        "session {session_id}{}",
        if resume.is_some() {
            format!(" resumed with {} prior message(s)", messages.len())
        } else {
            String::new()
        }
    );
    if !tool_names.is_empty() {
        eprintln!("tools available: {}", tool_names.join(", "));
    }

    let stdin = std::io::stdin();

    let build_input = |messages: &[String]| {
        let mut input_value = serde_json::json!({ "messages": messages });
        if let Some(system) = &system {
            input_value["system"] = Value::String(system.clone());
        }
        if let Some(model) = &model {
            input_value["model"] = Value::String(model.clone());
        }
        if !tool_names.is_empty() {
            input_value["tools"] = serde_json::json!(tool_names);
        }
        input_value
    };

    // On `--resume`, re-drive the restored dialogue against the journal before
    // reading new input: prior turns replay silently for $0, a final turn that
    // was interrupted mid-generation completes live, and the transcript prints
    // once, in order, so the human sees the conversation they are rejoining.
    if resume.is_some() && !messages.is_empty() {
        let input_value = build_input(&messages);
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<RuntimeEvent>();
        // Discard deltas: the transcript dump below shows the whole dialogue,
        // so streaming a completing tail turn here would print it twice.
        let drain = std::thread::spawn(move || {
            let mut rx = event_rx;
            while rx.blocking_recv().is_some() {}
        });
        let result = engine.resume_run_streaming(
            &agent_path,
            &input_value,
            call_log.clone(),
            &session_id,
            event_tx,
        );
        drain.join().ok();
        match result {
            Ok(result) => {
                if let Some(turns) = result
                    .output
                    .get("transcript")
                    .or_else(|| result.output.get("history"))
                    .and_then(Value::as_array)
                {
                    for turn in turns {
                        let text = turn.get("text").and_then(Value::as_str).unwrap_or("");
                        match turn.get("role").and_then(Value::as_str) {
                            Some("user") => println!("\nyou> {text}"),
                            _ => println!("assistant> {text}"),
                        }
                    }
                }
                call_log = result.call_log.into_records();
            }
            Err(e) => {
                // The journal on disk is untouched; the session can still
                // continue (new turns replay the loaded log in memory).
                eprintln!("\nerror: could not restore the session transcript: {e:#}");
            }
        }
    }

    loop {
        print!("\nyou> ");
        std::io::stdout().flush().ok();

        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            eprintln!("\nbye");
            break;
        }
        let message = line.trim_end_matches(&['\r', '\n'][..]).trim().to_string();
        if message.is_empty() {
            continue;
        }
        if matches!(message.to_lowercase().as_str(), "exit" | "quit" | ":q") {
            eprintln!("bye");
            break;
        }

        messages.push(message);
        let input_value = build_input(&messages);

        // Stream just the new turn's reply. The drain thread prints token
        // deltas while the engine runs; joining it before the next prompt is a
        // barrier, so all terminal output stays serialized (no stdin/stdout
        // race). Prior turns replay silently and emit no deltas.
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<RuntimeEvent>();
        let drain = std::thread::spawn(move || {
            let mut rx = event_rx;
            let mut out = std::io::stdout();
            let mut streamed = false;
            while let Some(evt) = rx.blocking_recv() {
                if let RuntimeEvent::PromptDelta { delta, .. } = evt {
                    // Mark where the reply starts (mirrors the `you> ` prompt)
                    // so scrollback distinguishes the two speakers.
                    if !streamed {
                        print!("assistant> ");
                    }
                    print!("{delta}");
                    out.flush().ok();
                    streamed = true;
                }
            }
            streamed
        });

        let result = engine.resume_run_streaming(
            &agent_path,
            &input_value,
            call_log.clone(),
            &session_id,
            event_tx,
        );
        // event_tx was moved into the engine and is dropped when the run
        // returns, ending the drain loop; join flushes every queued delta
        // before we print anything else.
        let streamed = drain.join().unwrap_or(false);

        match result {
            Ok(result) => {
                // Fallback for non-streaming providers (no deltas emitted):
                // print the newest assistant turn from the returned transcript.
                if !streamed {
                    let reply = result
                        .output
                        .get("transcript")
                        .or_else(|| result.output.get("history"))
                        .and_then(Value::as_array)
                        .and_then(|turns| {
                            turns.iter().rev().find(|turn| {
                                turn.get("role").and_then(Value::as_str) == Some("assistant")
                            })
                        })
                        .and_then(|turn| turn.get("text").and_then(Value::as_str))
                        .unwrap_or("");
                    print!("assistant> {reply}");
                }
                println!();
                call_log = result.call_log.into_records();
            }
            Err(e) => {
                // Drop the failed turn so the next message starts clean, and
                // keep the prior call log. The failed attempt may have
                // journaled partial records on disk; the persister's
                // monotonic floor lets the next successful turn rewrite the
                // journal once its log grows past them.
                messages.pop();
                eprintln!("\nerror: {e:#}");
            }
        }
    }

    if resume.is_some() {
        let _ = crate::runtime::store::release_lease(
            factory.store_for(&session_id).as_ref(),
            &lease_owner,
        );
    }
    if !call_log.is_empty() {
        print_chat_session_summary(&run_base, &session_id, messages.len());
        let agent_arg = agent
            .map(|p| format!("{} ", p.display()))
            .unwrap_or_default();
        eprintln!(
            "session saved — continue with: chidori chat {agent_arg}--resume {session_id} \
             (inspect: chidori trace {session_id})"
        );
    }

    if let Some(temp_dir) = temp_dir {
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
    Ok(())
}

/// One-line usage/cost summary printed when a chat session ends. Reads the
/// session's journaled records from its run dir and prices them exactly like
/// `chidori stats` (same record parsing, same journaled-pricing fallback, and
/// the same "unknown, not $0" treatment for unpriced models).
fn print_chat_session_summary(run_base: &Path, session_id: &str, turns: usize) {
    use crate::runtime::cost::{estimate_cost_usd_with_cache, is_priced_model};
    use crate::runtime::store::RunStore as _;

    let run_dir = run_base.join(session_id);
    let Ok(Some(records)) = crate::runtime::store::FsRunStore::new(&run_dir).load_call_log() else {
        return;
    };
    // Price under the pricing table recorded in the session's manifest, same
    // as `stats` (a live CHIDORI_PRICING still wins inside the cost module).
    if let Ok(manifest) = crate::runtime::snapshot::SnapshotStore::new(&run_dir).load_manifest() {
        if let Some(ref pricing) = manifest.pricing {
            crate::runtime::cost::install_journaled_pricing(pricing);
        }
    }

    let mut prompt_calls: u64 = 0;
    let mut input_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;
    let mut cache_read: u64 = 0;
    let mut cost: f64 = 0.0;
    let mut any_unpriced = false;
    for r in &records {
        if r.function != "prompt" {
            continue;
        }
        prompt_calls += 1;
        let model = r
            .args
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        if !is_priced_model(model) {
            any_unpriced = true;
        }
        if let Some(ref usage) = r.token_usage {
            input_tokens += usage.input_tokens;
            output_tokens += usage.output_tokens;
            let read = usage.cache_read_tokens.unwrap_or(0);
            let write = usage.cache_creation_tokens.unwrap_or(0);
            cache_read += read;
            cost += estimate_cost_usd_with_cache(
                model,
                usage.input_tokens,
                usage.output_tokens,
                write,
                read,
            );
        }
    }
    if prompt_calls == 0 {
        return;
    }

    let cache_note = if cache_read > 0 {
        format!(", {cache_read} cache reads")
    } else {
        String::new()
    };
    // Same distinction as `stats`: an unpriced model's cost is unknown, not $0.
    let cost_note = if !any_unpriced {
        format!("est. cost: ${cost:.6}")
    } else if cost > 0.0 {
        format!(
            "est. cost: ${cost:.6} + unknown (unpriced model; supply rates via CHIDORI_PRICING)"
        )
    } else {
        "est. cost: unknown (unpriced model; supply rates via CHIDORI_PRICING)".to_string()
    };
    eprintln!(
        "session usage: {turns} turn(s), {prompt_calls} prompt call(s), {} tokens \
         ({input_tokens} in / {output_tokens} out{cache_note}), {cost_note}",
        input_tokens + output_tokens
    );
}

fn cmd_check(file: &Path) -> Result<()> {
    let providers = Arc::new(ProviderRegistry::new());
    let template_engine = Arc::new(TemplateEngine::new("."));
    let tokio_rt =
        Arc::new(scheduler::new_tokio_runtime().context("Failed to create tokio runtime")?);

    let engine = Engine::new(providers, template_engine, tokio_rt);
    engine.check(file)?;
    println!("OK: {}", file.display());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_resume(
    file: &Path,
    run_id: &str,
    dir: Option<&std::path::Path>,
    until_seq: Option<u64>,
    retry_failed: bool,
    allow_source_change: bool,
    model: Option<String>,
    untrusted: bool,
    trusted: bool,
) -> Result<()> {
    let base_dir = dir
        .map(|d| d.to_path_buf())
        .or_else(|| file.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));

    let run_base = base_dir.join(".chidori").join("runs");
    let run_dir = run_base.join(run_id);
    let input_path = run_dir.join("input.json");

    // Load through the run store: hydrates the run dir from a configured
    // durable mirror when this machine has never seen the run, and unions the
    // last checkpoint with any crash-stranded `records.jsonl` tail.
    let factory = crate::runtime::store::RunStoreFactory::shared(&run_base);
    let _ = factory.hydrate(run_id);
    let mut records = factory
        .store_for(run_id)
        .load_call_log()?
        .ok_or_else(|| anyhow::anyhow!("No checkpoint found under {}", run_dir.display()))?;

    // `--retry-failed`: first-class repair for a failed run. The trailing
    // failed record(s) — the crash frontier — are stripped with the exact
    // fixpoint the actor `restart: "resume"` path and the detached-agent
    // supervisor use (`strip_crash_frontier`): pop trailing failed records,
    // then sweep out every record whose parent is stripped, so a failed
    // call's nested effects re-execute live too. Divergence scoping falls out
    // of the strip: the surviving prefix still replays under the normal
    // strict rules (nothing here loosens them, and `--allow-source-change`
    // keeps its usual meaning), while the retried tail has no records left to
    // diverge against — it is ordinary live execution, so a different
    // args/result on the retry needs no opt-in.
    if retry_failed {
        if records.last().is_none_or(|r| r.error.is_none()) {
            let store = factory.store_for(run_id);
            let state = if store.get_blob("output.json").ok().flatten().is_some() {
                "completed — it already has a recorded output, so there is nothing to \
                 retry (use `chidori verify` to re-check it, or `--until-seq` to \
                 time-travel into its history)"
                    .to_string()
            } else if store
                .get_blob(crate::runtime::snapshot::PENDING_HOST_OPERATION_FILE)
                .ok()
                .flatten()
                .is_some()
            {
                "paused on a pending operation, not failed — continue it with a plain \
                 `chidori resume` (or deliver its input/signal through `chidori serve`)"
                    .to_string()
            } else {
                format!(
                    "not in a failed state: its journal's last record ({}) completed, so \
                     there is no failure frontier to retry",
                    records
                        .last()
                        .map(|r| format!("seq {} `{}`", r.seq, r.function))
                        .unwrap_or_else(|| "empty journal".to_string())
                )
            };
            anyhow::bail!("--retry-failed: run {run_id} is {state}.");
        }
        let before_seqs: Vec<u64> = records.iter().map(|r| r.seq).collect();
        records = crate::runtime::host_actor::strip_crash_frontier(records);
        let kept: std::collections::HashSet<u64> = records.iter().map(|r| r.seq).collect();
        let removed: Vec<u64> = before_seqs
            .into_iter()
            .filter(|seq| !kept.contains(seq))
            .collect();
        let low = removed.iter().min().copied().unwrap_or_default();
        let high = removed.iter().max().copied().unwrap_or_default();
        eprintln!(
            "retry-failed: stripped {} failed record(s) (seqs {low}..{high}), \
             replaying {} records then executing live",
            removed.len(),
            records.len()
        );
    }

    // Time travel: truncate the journal at the requested frontier; replay
    // serves everything up to it from cache and the run continues live there.
    if let Some(until) = until_seq {
        let before = records.len();
        records.retain(|r| r.seq <= until);
        eprintln!(
            "Time travel: replaying {} of {} records (seq <= {})",
            records.len(),
            before,
            until
        );
    }

    let input_value: Value = if input_path.exists() {
        let text = std::fs::read_to_string(&input_path)?;
        serde_json::from_str(&text).unwrap_or(Value::Object(Default::default()))
    } else {
        Value::Object(Default::default())
    };

    // Replay is positional: verify the agent code on disk still matches the
    // source fingerprints recorded in the run's snapshot manifest, exactly as
    // the server resume routes do, so cached results are never paired with
    // changed code. (Runs persisted before manifests existed skip with a
    // warning; `--allow-source-change` is the edit-and-resume opt-in.)
    crate::runtime::snapshot::validate_manifest_for_resume(
        &run_base,
        Some(run_id),
        file,
        allow_source_change,
    )
    .context("resume refused: the agent source no longer matches this run's checkpoint")?;

    // One driver per run dir: two concurrent resumes of the same run would
    // interleave writes into one journal. The same lease file detached agents
    // use guards the CLI; a dead holder's lease expires on its own.
    let cli_lease_owner = format!("chidori-cli-{}", std::process::id());
    match crate::runtime::store::acquire_lease(
        factory.store_for(run_id).as_ref(),
        &cli_lease_owner,
        chrono::Duration::minutes(10),
    ) {
        Ok(Ok(_)) => {}
        Ok(Err(holder)) => anyhow::bail!(
            "run {run_id} is already being driven by another process (lease holder \
             `{}`, expires {}). Two concurrent drivers would corrupt the journal — \
             wait for it to finish, or delete {} if the holder is dead.",
            holder.owner,
            holder.expires_at,
            run_dir.join("lease.json").display()
        ),
        Err(err) => {
            eprintln!("warning: could not take the run lease: {err}");
        }
    }

    // The run's model travels with it: an explicit `--model` (or a
    // pre-existing CHIDORI_MODEL) wins, then the model recorded in the run's
    // manifest — so the README's bare `chidori resume agent.ts <run-id>`
    // replays a `--model`-started run without re-deriving flags.
    let manifest_model = crate::runtime::snapshot::SnapshotStore::new(run_dir.clone())
        .load_manifest()
        .ok()
        .and_then(|manifest| manifest.default_model);
    let default_model = model
        .or_else(|| std::env::var("CHIDORI_MODEL").ok())
        .or(manifest_model);

    let providers = Arc::new(ProviderRegistry::from_env());
    let template_engine = Arc::new(TemplateEngine::new(&base_dir));
    let tokio_rt =
        Arc::new(scheduler::new_tokio_runtime().context("Failed to create tokio runtime")?);
    let tools = Arc::new(ToolRegistry::new());
    // Same implicit workspace root as `chidori run`: a run that wrote
    // workspace files must replay/resume without extra configuration.
    // CHIDORI_WORKSPACE_ROOT still takes precedence inside the runtime.
    // Policy mirrors `run` (`--trusted`/`--untrusted`), and persistence stays
    // enabled under the run's ORIGINAL id so live continuation past the
    // frontier journals into the same run directory.
    let engine = Engine::new(providers, template_engine, tokio_rt)
        .with_tools(tools)
        .with_policy(cli_policy(untrusted, trusted))
        .with_persist_base(run_base.clone())
        .with_default_model(default_model)
        // Both `--until-seq` and `--retry-failed` intentionally hand the
        // engine a journal SHORTER than the durable one; without the opt-in
        // the shorter-log floor would refuse to compact the repaired history
        // (e.g. a retry that settles in fewer records than the failed attempt
        // journaled), leaving stale failed records behind for `verify` to
        // trip over.
        .with_history_rewrite_allowed(until_seq.is_some() || retry_failed)
        .with_workspace_root(abs_dir(&base_dir));

    // Journaled top-level workspace records re-execute on every replay by
    // design (the workspace is real disk state, re-materialized rather than
    // served from the journal) — count them up front so the summary below can
    // report them as what they are instead of folding them into "executed
    // live", which reads as a re-fired side effect.
    let journaled_workspace = records
        .iter()
        .filter(|r| r.function == "workspace" && r.parent_seq.is_none())
        .count() as u64;
    // Same one-line-per-prompt stderr progress as plain `run`, and only for
    // calls executed live past the replay frontier: replayed records
    // short-circuit before the provider path that emits PromptStart, so the
    // replayed prefix stays silent. CHIDORI_QUIET=1 opts out.
    let result = match spawn_prompt_progress_listener() {
        Some((event_tx, drain)) => {
            let result = engine.resume_run_streaming(file, &input_value, records, run_id, event_tx);
            drain.join().ok();
            result
        }
        None => engine.resume_run(file, &input_value, records, run_id),
    };
    let _ =
        crate::runtime::store::release_lease(factory.store_for(run_id).as_ref(), &cli_lease_owner);
    let result = result?;

    // A resume that lands back on a `chidori.signal(...)` listen point has no
    // stdin fallback: report the pause and how to deliver, exactly like
    // `chidori run` does, instead of printing a bare `null` that reads as a
    // completed run.
    if let Some(signal) = &result.paused_signal {
        let names = signal.listen_names();
        eprintln!(
            "Run {run_id} replayed to its pause and is still awaiting signal{} '{}'.",
            if names.len() > 1 { " (any of)" } else { "" },
            names.join("', '")
        );
        eprintln!(
            "Deliver it with: POST /sessions/{{id}}/signal \
             {{\"name\":\"{}\",\"payload\":...,\"from\":...}} against a `chidori serve` \
             session for this run. (Signal delivery and `timeoutMs` deadlines are \
             server-side — the bare CLI can neither deliver nor time out a signal.)",
            signal.name
        );
        return Ok(());
    }

    let output_str = serde_json::to_string_pretty(&result.output)?;
    println!("{output_str}");
    // Report the replayed / re-materialized / live split — the total alone
    // reads as "everything was replayed", and folding workspace
    // re-materializations into "executed live" reads as a re-fired side
    // effect. In-flight work at a crash re-executes by design
    // (at-least-once), so the live count is the honest recovery cost.
    let total = result.call_log.records().len() as u64;
    let live = total.saturating_sub(result.replayed_calls);
    let rematerialized = live.min(journaled_workspace);
    let live_new = live.saturating_sub(rematerialized);
    let remat_clause = if rematerialized > 0 {
        format!(", {rematerialized} workspace re-materialization(s)")
    } else {
        String::new()
    };
    eprintln!(
        "\nResumed from {run_id} ({} recorded calls replayed{remat_clause}, {live_new} executed live)",
        result.replayed_calls,
    );
    Ok(())
}

/// `chidori verify` — checkpoint-as-test as a first-class command. Replays a
/// recorded run with no provider configured, a deny-all policy, and no
/// persistence (the run directory is never written), then asserts the run
/// completed with byte-identical output. Every drift mode fails loudly:
/// changed source refuses via the manifest check, a diverging recorded call
/// errors positionally, a run that reaches for anything live has no provider
/// (and no allowed gated effects) to reach.
fn cmd_verify(file: &Path, run_id: &str, dir: Option<&std::path::Path>) -> Result<()> {
    let base_dir = dir
        .map(|d| d.to_path_buf())
        .or_else(|| file.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    let run_base = base_dir.join(".chidori").join("runs");
    let run_dir = run_base.join(run_id);

    use crate::runtime::store::RunStore as _;
    let store = crate::runtime::store::FsRunStore::new(run_dir.clone());
    let records = store
        .load_call_log()?
        .ok_or_else(|| anyhow::anyhow!("No checkpoint found under {}", run_dir.display()))?;
    let recorded_output: Option<Value> = store
        .get_blob("output.json")?
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());

    let input_path = run_dir.join("input.json");
    let input_value: Value = if input_path.exists() {
        serde_json::from_str(&std::fs::read_to_string(&input_path)?)
            .unwrap_or(Value::Object(Default::default()))
    } else {
        Value::Object(Default::default())
    };

    // Drift gate 1: the agent source must match the recorded fingerprints.
    // No `--allow-source-change` escape here — a verify against edited code
    // is not a verification.
    crate::runtime::snapshot::validate_manifest_for_resume(&run_base, Some(run_id), file, false)
        .context("verify refused: the agent source no longer matches this run's checkpoint")?;

    // No providers, deny-all policy, no persistence: the replay must be able
    // to answer EVERY effect from the journal or fail.
    let providers = Arc::new(ProviderRegistry::new());
    let template_engine = Arc::new(TemplateEngine::new(&base_dir));
    let tokio_rt =
        Arc::new(scheduler::new_tokio_runtime().context("Failed to create tokio runtime")?);
    let tools = Arc::new(ToolRegistry::new());
    let manifest_model = crate::runtime::snapshot::SnapshotStore::new(run_dir.clone())
        .load_manifest()
        .ok()
        .and_then(|manifest| manifest.default_model);
    let engine = Engine::new(providers, template_engine, tokio_rt)
        .with_tools(tools)
        .with_policy(Arc::new(
            policy::builtin_profile("untrusted").expect("built-in untrusted profile exists"),
        ))
        .with_default_model(manifest_model)
        .with_workspace_root(abs_dir(&base_dir));

    let journal_len = records.len() as u64;
    let result = engine
        .resume_run(file, &input_value, records, run_id)
        .context("verify FAILED: the recorded run did not replay cleanly")?;

    if result.paused.is_some() || result.paused_approval.is_some() || result.paused_signal.is_some()
    {
        anyhow::bail!(
            "verify FAILED: the run replayed to a pause instead of completing — \
             only completed runs can be verified"
        );
    }
    if let Some(recorded) = recorded_output {
        if recorded != result.output {
            anyhow::bail!(
                "verify FAILED: replayed output differs from the recorded output.\n\
                 recorded: {}\n\
                 replayed: {}",
                serde_json::to_string(&recorded).unwrap_or_default(),
                serde_json::to_string(&result.output).unwrap_or_default()
            );
        }
    } else {
        eprintln!(
            "chidori: warning: no recorded output.json under {} — verified replay \
             consistency only, not output identity",
            run_dir.display()
        );
    }
    let records = result.call_log.records();
    let total = records.len() as u64;
    let live = total.saturating_sub(result.replayed_calls);
    // Workspace effects re-execute by design on every replay (the workspace
    // is real disk state, re-materialized rather than served from the
    // journal; nested ones replay inside their container's subtree). Only
    // top-level workspace records are expected live — anything else live
    // means the replay reached past the journal.
    let expected_live = records
        .iter()
        .filter(|r| r.function == "workspace" && r.parent_seq.is_none())
        .count() as u64;
    if live > expected_live {
        anyhow::bail!(
            "verify FAILED: {} call(s) executed live beyond the expected {expected_live} \
             workspace re-materialization(s) ({} of {journal_len} journal records replayed)",
            live - expected_live,
            result.replayed_calls
        );
    }
    println!(
        "verified: {} calls replayed, {live} workspace re-materialization(s), \
         output identical — $0",
        result.replayed_calls
    );
    Ok(())
}

/// Resolve `<base>/.chidori/runs/<run_id>` for the branch commands.
fn branch_run_dir(run_id: &str, dir: Option<&std::path::Path>) -> Result<PathBuf> {
    let base_dir = dir
        .map(|d| d.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let run_dir = base_dir.join(".chidori").join("runs").join(run_id);
    if !run_dir.is_dir() {
        anyhow::bail!("No persisted run at {}", run_dir.display());
    }
    Ok(run_dir)
}

/// The engine for out-of-band branch operations, wired like `cmd_resume`'s:
/// providers from env, `--trusted`/`--untrusted` policy, tools from
/// `<base>/tools`, and the parent run's recorded model as the default
/// (`--model` / CHIDORI_MODEL still win).
fn branch_engine(
    run_dir: &std::path::Path,
    dir: Option<&std::path::Path>,
    model: Option<String>,
    untrusted: bool,
    trusted: bool,
) -> Result<Engine> {
    let base_dir = dir
        .map(|d| d.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let manifest_model = crate::runtime::snapshot::SnapshotStore::new(run_dir.to_path_buf())
        .load_manifest()
        .ok()
        .and_then(|manifest| manifest.default_model);
    let default_model = model
        .or_else(|| std::env::var("CHIDORI_MODEL").ok())
        .or(manifest_model);
    let providers = Arc::new(ProviderRegistry::from_env());
    let template_engine = Arc::new(TemplateEngine::new(&base_dir));
    let tokio_rt =
        Arc::new(scheduler::new_tokio_runtime().context("Failed to create tokio runtime")?);
    let tools = Arc::new(ToolRegistry::new());
    Ok(Engine::new(providers, template_engine, tokio_rt)
        .with_tools(tools)
        .with_policy(cli_policy(untrusted, trusted))
        .with_default_model(default_model)
        .with_workspace_root(abs_dir(&base_dir)))
}

fn cmd_branches(run_id: &str, dir: Option<&std::path::Path>) -> Result<()> {
    let run_dir = branch_run_dir(run_id, dir)?;
    let branches = Engine::list_branches(&run_dir)?;
    if branches.is_empty() {
        eprintln!("No persisted branches under {}", run_dir.display());
        return Ok(());
    }
    println!("{}", serde_json::to_string_pretty(&branches)?);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_branch_resume(
    run_id: &str,
    branch_id: &str,
    value: &str,
    dir: Option<&std::path::Path>,
    model: Option<String>,
    untrusted: bool,
    trusted: bool,
) -> Result<()> {
    let run_dir = branch_run_dir(run_id, dir)?;
    let engine = branch_engine(&run_dir, dir, model, untrusted, trusted)?;
    let outcome = engine.resume_branch(&run_dir, branch_id, value)?;
    println!("{}", serde_json::to_string_pretty(&outcome)?);
    Ok(())
}

fn cmd_branch_rerun(
    run_id: &str,
    branch_id: &str,
    dir: Option<&std::path::Path>,
    model: Option<String>,
    untrusted: bool,
    trusted: bool,
) -> Result<()> {
    let run_dir = branch_run_dir(run_id, dir)?;
    let engine = branch_engine(&run_dir, dir, model, untrusted, trusted)?;
    let outcome = engine.rerun_branch(&run_dir, branch_id)?;
    println!("{}", serde_json::to_string_pretty(&outcome)?);
    Ok(())
}

/// Label every record in a multi-process trace with its owner: `main` for the
/// run's own records, the actor's registered name (or pid) for records folded
/// in at a `join_actor`/`stop_actor`, and the branch variant's label for
/// records under a `branch` fan-out. Ownership is derived from the
/// `parent_seq` chain — a record with no chain belongs to the run itself,
/// even when the fold advanced its seq into a reserved high range.
fn trace_owner_label(
    r: &crate::runtime::call_log::CallRecord,
    by_seq: &std::collections::HashMap<u64, &crate::runtime::call_log::CallRecord>,
    actor_names: &std::collections::HashMap<String, String>,
) -> String {
    let mut anchor = r;
    let mut hops = 0;
    while let Some(parent) = anchor.parent_seq.and_then(|p| by_seq.get(&p)) {
        anchor = parent;
        hops += 1;
        if hops > 128 {
            break;
        }
    }
    if anchor.seq == r.seq {
        return "main".to_string();
    }
    match anchor.function.as_str() {
        "join_actor" | "stop_actor" => {
            let pid = anchor
                .args
                .get("pid")
                .and_then(|v| v.as_str())
                .unwrap_or("actor");
            match actor_names.get(pid) {
                Some(name) => name.clone(),
                None => pid.to_string(),
            }
        }
        "branch" => {
            // Branch k occupies [base + k·width, base + (k+1)·width) where
            // base is the slot boundary — recover k to name the variant.
            let variants = anchor.args.get("variants").and_then(|v| v.as_array());
            let count = variants.map(|v| v.len() as u64).unwrap_or(1).max(1);
            let width = 10_000u64;
            let base = (r.seq / (width * count)) * (width * count);
            let k = ((r.seq.saturating_sub(base)) / width).min(count.saturating_sub(1));
            variants
                .and_then(|v| v.get(k as usize))
                .and_then(|v| v.get("label"))
                .and_then(|v| v.as_str())
                .map(|label| format!("branch:{label}"))
                .unwrap_or_else(|| format!("branch-{k}"))
        }
        _ => "main".to_string(),
    }
}

fn cmd_trace(run_id: &str, dir: Option<&std::path::Path>) -> Result<()> {
    let base_dir = dir
        .map(|d| d.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let run_base = base_dir.join(".chidori").join("runs");
    let run_dir = run_base.join(run_id);

    let factory = crate::runtime::store::RunStoreFactory::shared(&run_base);
    let _ = factory.hydrate(run_id);
    let records = factory
        .store_for(run_id)
        .load_call_log()?
        .ok_or_else(|| anyhow::anyhow!("No checkpoint found under {}", run_dir.display()))?;

    // The run's manifest carries the CHIDORI_PRICING table that was live when
    // it executed — install it as the cost fallback so the trace prices
    // correctly in a shell that doesn't have the env var set.
    if let Ok(manifest) = crate::runtime::snapshot::SnapshotStore::new(&run_dir).load_manifest() {
        if let Some(ref pricing) = manifest.pricing {
            crate::runtime::cost::install_journaled_pricing(pricing);
        }
    }

    println!("Run: {}", run_id);
    println!("Calls: {}", records.len());

    let by_seq: std::collections::HashMap<u64, &crate::runtime::call_log::CallRecord> =
        records.iter().map(|r| (r.seq, r)).collect();
    let mut actor_names: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for r in &records {
        if r.function == "spawn_actor" {
            if let (Some(pid), Some(name)) = (
                r.result.get("pid").and_then(|v| v.as_str()),
                r.result.get("name").and_then(|v| v.as_str()),
            ) {
                actor_names.insert(pid.to_string(), format!("{name} ({pid})"));
            }
        }
    }
    let labels: Vec<String> = records
        .iter()
        .map(|r| trace_owner_label(r, &by_seq, &actor_names))
        .collect();
    // Announce the cast when the trace has more than the main run in it.
    {
        let mut owners: Vec<&String> = labels.iter().filter(|l| *l != "main").collect();
        owners.sort();
        owners.dedup();
        if !owners.is_empty() {
            println!(
                "Owners: main, {}",
                owners
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    println!();

    let owner_width = labels.iter().map(|l| l.len()).max().unwrap_or(4).max(4);
    let mut total_in = 0u64;
    let mut total_out = 0u64;
    let mut total_cache_read = 0u64;
    let mut total_cache_write = 0u64;
    let mut total_ms = 0u64;
    let mut total_cost = 0.0;
    let mut unpriced_models: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for (r, label) in records.iter().zip(&labels) {
        let args_str = serde_json::to_string(&r.args).unwrap_or_default();
        let args_short = if args_str.len() > 100 {
            format!("{}…", &args_str[..100])
        } else {
            args_str
        };
        let err_tag = r
            .error
            .as_ref()
            .map(|e| format!(" ERROR: {e}"))
            .unwrap_or_default();
        let token_tag = r
            .token_usage
            .as_ref()
            .map(|u| {
                let cache = match (
                    u.cache_read_tokens.unwrap_or(0),
                    u.cache_creation_tokens.unwrap_or(0),
                ) {
                    (0, 0) => String::new(),
                    (read, 0) => format!(", {read} cache-read"),
                    (0, write) => format!(", {write} cache-write"),
                    (read, write) => format!(", {read} cache-read, {write} cache-write"),
                };
                format!(" [{}→{} tok{}]", u.input_tokens, u.output_tokens, cache)
            })
            .unwrap_or_default();
        // Records folded in from actors/branches live in reserved high seq
        // ranges; print the offset within the range (`·N`) instead of a
        // 13-digit absolute for anything that has a named owner.
        let seq_disp = if label == "main" && r.seq < 1_000_000_000_000 {
            format!("#{}", r.seq)
        } else if label.starts_with("branch") {
            format!("#…{}", r.seq % 10_000)
        } else if label == "main" {
            format!("#{}", r.seq)
        } else {
            format!("·{}", r.seq % 1_000_000_000_000)
        };
        // Signals carry the interesting half — who answered, with what — in
        // the RESULT (`{name, payload, from}`), which the generic args column
        // never shows. Render it inline so `trace` is the multiplayer audit
        // trail the signals docs promise, not just a list of listen points.
        let signal_tag = if matches!(r.function.as_str(), "signal" | "signal_any" | "poll_signal") {
            if r.result.is_null() {
                "  ← empty (no queued signal)".to_string()
            } else if r.result.get("timedOut").and_then(|v| v.as_bool()) == Some(true) {
                "  ← timed out (sentinel)".to_string()
            } else {
                let name = r.result.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let from = match r.result.get("from") {
                    Some(serde_json::Value::Object(f)) => format!(
                        "{}:{}",
                        f.get("kind").and_then(|v| v.as_str()).unwrap_or("?"),
                        f.get("id").and_then(|v| v.as_str()).unwrap_or("?")
                    ),
                    _ => "unattributed".to_string(),
                };
                let payload = r
                    .result
                    .get("payload")
                    .map(|p| serde_json::to_string(p).unwrap_or_default())
                    .unwrap_or_else(|| "null".to_string());
                let payload_short = if payload.chars().count() > 80 {
                    let head: String = payload.chars().take(80).collect();
                    format!("{head}…")
                } else {
                    payload
                };
                format!("  ← {name} from {from}: {payload_short}")
            }
        } else {
            String::new()
        };
        println!(
            "  {:<owner_width$}  {:<8} {:>6}ms  {}  {}{}{}{}",
            label, seq_disp, r.duration_ms, r.function, args_short, token_tag, signal_tag, err_tag
        );
        if let Some(ref u) = r.token_usage {
            total_in += u.input_tokens;
            total_out += u.output_tokens;
            total_cache_read += u.cache_read_tokens.unwrap_or(0);
            total_cache_write += u.cache_creation_tokens.unwrap_or(0);
            if r.function == "prompt" {
                let model = r.args.get("model").and_then(|v| v.as_str()).unwrap_or("");
                if crate::runtime::cost::is_priced_model(model) {
                    total_cost += crate::runtime::cost::estimate_cost_usd_with_cache(
                        model,
                        u.input_tokens,
                        u.output_tokens,
                        u.cache_creation_tokens.unwrap_or(0),
                        u.cache_read_tokens.unwrap_or(0),
                    );
                } else {
                    unpriced_models.insert(model.to_string());
                }
            }
        }
        total_ms += r.duration_ms;
    }

    println!();
    if total_in > 0 || total_out > 0 {
        println!("Tokens:   {} in / {} out", total_in, total_out);
        if total_cache_read > 0 || total_cache_write > 0 {
            println!(
                "Cache:    {} read / {} written (prompt-cache tokens)",
                total_cache_read, total_cache_write
            );
        }
        // "$0.000000" for a model missing from the pricing table would read
        // as "free"; say "unknown" instead and name the unpriced models.
        if unpriced_models.is_empty() {
            println!("Est cost: ${:.6}", total_cost);
        } else {
            let names = unpriced_models.into_iter().collect::<Vec<_>>().join(", ");
            if total_cost > 0.0 {
                println!(
                    "Est cost: ${:.6} + unknown (no pricing data for: {}; supply rates via \
                     CHIDORI_PRICING)",
                    total_cost, names
                );
            } else {
                println!(
                    "Est cost: unknown (no pricing data for: {}; supply rates via \
                     CHIDORI_PRICING)",
                    names
                );
            }
        }
    }
    println!("Duration: {} ms", total_ms);
    Ok(())
}

fn cmd_snapshot(run_id: &str, dir: Option<&std::path::Path>) -> Result<()> {
    let base_dir = dir
        .map(|d| d.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let run_dir = base_dir.join(".chidori").join("runs").join(run_id);
    let store = crate::runtime::snapshot::SnapshotStore::new(&run_dir);
    let manifest = store.load_manifest()?;

    println!("{}", serde_json::to_string_pretty(&manifest)?);
    Ok(())
}

fn cmd_stats(dir: Option<&std::path::Path>) -> Result<()> {
    use crate::runtime::call_log::CallLog;
    use crate::runtime::cost::estimate_cost_usd_with_cache;
    use std::collections::BTreeMap;

    let runs_dir = dir
        .map(|d| d.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".chidori")
        .join("runs");

    if !runs_dir.exists() {
        println!("No runs found at {}", runs_dir.display());
        return Ok(());
    }

    let mut run_count: u64 = 0;
    let mut prompt_count: u64 = 0;
    let mut tool_count: u64 = 0;
    let mut total_input_tokens: u64 = 0;
    let mut total_output_tokens: u64 = 0;
    let mut total_cache_read: u64 = 0;
    let mut total_cache_write: u64 = 0;
    let mut total_duration_ms: u64 = 0;
    let mut total_cost: f64 = 0.0;

    #[derive(Default)]
    struct ModelStats {
        calls: u64,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cost_usd: f64,
    }
    let mut per_model: BTreeMap<String, ModelStats> = BTreeMap::new();

    for entry in std::fs::read_dir(&runs_dir)? {
        let entry = entry?;
        // Union the last checkpoint with the append-only tail: mid-run and
        // crashed runs have records in `records.jsonl` that the checkpoint —
        // rewritten only at compaction points — doesn't carry yet.
        use crate::runtime::store::RunStore as _;
        let Ok(Some(records)) =
            crate::runtime::store::FsRunStore::new(entry.path()).load_call_log()
        else {
            continue;
        };

        // Price this run under the pricing table recorded in its manifest
        // (env-set CHIDORI_PRICING still wins inside the cost module).
        if let Ok(manifest) =
            crate::runtime::snapshot::SnapshotStore::new(entry.path()).load_manifest()
        {
            if let Some(ref pricing) = manifest.pricing {
                crate::runtime::cost::install_journaled_pricing(pricing);
            }
        }

        run_count += 1;
        let mut log = CallLog::new();
        for r in records {
            if r.function == "prompt" {
                prompt_count += 1;
                // Count the call under its model even when the record carries
                // no token usage (e.g. a locally-cache-served or zero-usage
                // prompt) — otherwise the top-line "Prompt calls" and the
                // per-model rows silently disagree.
                let model = r
                    .args
                    .get("model")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let ms = per_model.entry(model.clone()).or_default();
                ms.calls += 1;
                if let Some(ref usage) = r.token_usage {
                    total_input_tokens += usage.input_tokens;
                    total_output_tokens += usage.output_tokens;
                    let cache_read = usage.cache_read_tokens.unwrap_or(0);
                    let cache_write = usage.cache_creation_tokens.unwrap_or(0);
                    total_cache_read += cache_read;
                    total_cache_write += cache_write;
                    let cost = estimate_cost_usd_with_cache(
                        &model,
                        usage.input_tokens,
                        usage.output_tokens,
                        cache_write,
                        cache_read,
                    );
                    total_cost += cost;
                    let ms = per_model.entry(model).or_default();
                    ms.input_tokens += usage.input_tokens;
                    ms.output_tokens += usage.output_tokens;
                    ms.cache_read_tokens += cache_read;
                    ms.cost_usd += cost;
                }
            } else if r.function == "tool" {
                // Registry (MCP / Rust-native) tools dispatched by name.
                tool_count += 1;
            } else if r.function == "mark"
                && r.args
                    .get("label")
                    .and_then(|v| v.as_str())
                    .is_some_and(|l| l.starts_with("tool:"))
            {
                // In-VM `defineTool` invocations journal as `mark("tool:<name>")`
                // records — the common case for single-file agents. Leaving them
                // out reported "Tool calls: 0" for agents that made dozens.
                tool_count += 1;
            }
            total_duration_ms += r.duration_ms;
            log.push(r);
        }
    }

    println!("Runs:              {}", run_count);
    println!("Prompt calls:      {}", prompt_count);
    println!("Tool calls:        {}", tool_count);
    println!(
        "Tokens:            {} in / {} out / {} total",
        total_input_tokens,
        total_output_tokens,
        total_input_tokens + total_output_tokens
    );
    let unpriced: Vec<&String> = per_model
        .keys()
        .filter(|m| !crate::runtime::cost::is_priced_model(m))
        .collect();
    if total_cache_read > 0 || total_cache_write > 0 {
        println!(
            "Prompt cache:      {} read / {} written",
            total_cache_read, total_cache_write
        );
    }
    if unpriced.is_empty() {
        println!("Est. cost:         ${:.6}", total_cost);
    } else if total_cost > 0.0 {
        println!(
            "Est. cost:         ${:.6} + unknown (unpriced models below; supply rates via \
             CHIDORI_PRICING)",
            total_cost
        );
    } else {
        println!(
            "Est. cost:         unknown (unpriced models below; supply rates via CHIDORI_PRICING)"
        );
    }
    println!("Total duration:    {} ms", total_duration_ms);

    if !per_model.is_empty() {
        println!("\nPer model:");
        for (model, s) in &per_model {
            let cost = if crate::runtime::cost::is_priced_model(model) {
                format!("${:.6}", s.cost_usd)
            } else {
                "cost unknown (no pricing data)".to_string()
            };
            let cache = if s.cache_read_tokens > 0 {
                format!("  {:>8} cached", s.cache_read_tokens)
            } else {
                String::new()
            };
            println!(
                "  {:<24} {:>4} calls  {:>8} in  {:>8} out{}  {}",
                model, s.calls, s.input_tokens, s.output_tokens, cache, cost
            );
        }
    }

    Ok(())
}

fn cmd_serve(
    file: Option<&Path>,
    host: Option<&str>,
    port: u16,
    verbose: bool,
    untrusted: bool,
    trusted: bool,
) -> Result<()> {
    if verbose {
        // Isolate worker children read this to decide whether to print
        // sandbox degradation notes.
        std::env::set_var("CHIDORI_VERBOSE", "1");
        tracing_subscriber::fmt()
            .with_env_filter("info")
            .with_target(false)
            .with_writer(std::io::stderr)
            .init();
    }

    let base_dir = match file {
        Some(file) => file
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf(),
        None => PathBuf::from("."),
    };

    let providers = Arc::new(ProviderRegistry::from_env());
    let template_engine = Arc::new(TemplateEngine::new(&base_dir));

    // Validate the agent file before starting the server.
    if let Some(file) = file {
        let rt = Arc::new(
            scheduler::new_tokio_runtime().context("Failed to create validation runtime")?,
        );
        let engine = Engine::new(providers.clone(), template_engine.clone(), rt);
        engine.check(file).context("Agent file validation failed")?;
    }

    match file {
        Some(file) => eprintln!("Agent: {}", file.display()),
        None => eprintln!(
            "Agent: none — fleet-only server (detached agents re-armed from the registry; \
             sessions must name an agent via the `agent` field)"
        ),
    }
    eprintln!("Isolation: {}", crate::runtime::isolate::describe());
    // The server is deny-by-default unless explicitly trusted; if it is confining
    // callers by policy but not by process, point at --isolate.
    crate::runtime::isolate::warn_if_untrusted_without_isolation(!trusted);

    // Bind-address precedence: --host flag, then CHIDORI_HOST, then the safe
    // loopback default (the server refuses non-loopback binds without auth —
    // see server::serve).
    let host = host
        .map(str::to_owned)
        .or_else(|| std::env::var("CHIDORI_HOST").ok())
        .unwrap_or_else(|| "127.0.0.1".to_string());

    let (policy, policy_posture) = serve_policy(untrusted, trusted);
    let tokio_rt = scheduler::new_tokio_runtime().context("Failed to create server runtime")?;
    tokio_rt.block_on(server::serve(
        providers,
        template_engine,
        file.map(|f| f.to_path_buf()),
        host,
        port,
        policy,
        policy_posture,
    ))?;

    Ok(())
}

/// Parse CLI input args into a JSON object.
///
/// Supports:
///   --input key=value         → {"key": "value"}
///   --input key=@file.txt     → {"key": "<file contents>"}
///   --input '{"key": "val"}'  → {"key": "val"}
fn parse_inputs(inputs: &[String]) -> Result<Value> {
    let mut map = serde_json::Map::new();

    for input in inputs {
        // Top-level `@/path/to/input.json` — read the entire input object from
        // a file. Useful when the JSON payload is too large to fit in argv
        // (the kernel's ARG_MAX is hit quickly by big prompts or catalogs).
        if let Some(path) = input.strip_prefix('@') {
            let content = std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read input file: {path}"))?;
            let val: Value = serde_json::from_str(&content)
                .with_context(|| format!("Failed to parse JSON input from {path}"))?;
            if let Value::Object(obj) = val {
                map.extend(obj);
                continue;
            }
            anyhow::bail!("Input file {path} must contain a JSON object");
        }

        // Try parsing as raw JSON first.
        if input.starts_with('{') {
            let val: Value = serde_json::from_str(input)
                .with_context(|| format!("Failed to parse JSON input: {input}"))?;
            if let Value::Object(obj) = val {
                map.extend(obj);
                continue;
            }
        }

        // Parse as key=value (with optional per-value @file).
        if let Some((key, value)) = input.split_once('=') {
            let value = if let Some(path) = value.strip_prefix('@') {
                std::fs::read_to_string(path)
                    .with_context(|| format!("Failed to read input file: {path}"))?
            } else {
                value.to_string()
            };
            map.insert(key.to_string(), Value::String(value));
        } else {
            anyhow::bail!("Invalid input format: '{input}'. Use key=value, JSON, or @path.");
        }
    }

    Ok(Value::Object(map))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Decision;
    use serde_json::json;

    // serve_policy reads CHIDORI_POLICY* env vars only on its non-flag paths;
    // the flag-driven branches below are deterministic regardless of ambient
    // configuration. Nothing in this test binary sets those vars in-process.

    #[test]
    fn serve_policy_untrusted_flag_is_deny_by_default() {
        let (cfg, posture) = serve_policy(true, false);
        let (decision, _) = cfg.decide("http", &json!({}));
        assert_eq!(decision, Decision::NeverAllow);
        assert!(posture.contains("--untrusted"));
    }

    #[test]
    fn serve_policy_default_denies_and_names_the_opt_out() {
        // No flags and (in the test environment) no CHIDORI_POLICY* vars:
        // the server posture is deny-by-default with an actionable reason.
        if std::env::var_os("CHIDORI_POLICY_FILE").is_some()
            || std::env::var_os("CHIDORI_POLICY").is_some()
            || std::env::var_os("CHIDORI_POLICY_PROFILE").is_some()
        {
            return; // ambient configuration would legitimately change the result
        }
        let (cfg, posture) = serve_policy(false, false);
        let (decision, reason) = cfg.decide("http", &json!({}));
        assert_eq!(decision, Decision::NeverAllow);
        assert!(reason.unwrap_or_default().contains("--trusted"));
        assert!(posture.contains("deny-by-default"));

        // The read-only workspace allowlist still applies.
        let (decision, _) = cfg.decide("workspace:read", &json!({}));
        assert_eq!(decision, Decision::AlwaysAllow);
    }

    #[test]
    fn serve_policy_trusted_flag_restores_the_permissive_default() {
        if std::env::var_os("CHIDORI_POLICY_FILE").is_some()
            || std::env::var_os("CHIDORI_POLICY").is_some()
            || std::env::var_os("CHIDORI_POLICY_PROFILE").is_some()
        {
            return;
        }
        let (cfg, posture) = serve_policy(false, true);
        let (decision, _) = cfg.decide("http", &json!({}));
        assert_eq!(decision, Decision::AlwaysAllow);
        assert!(posture.contains("--trusted"));
    }
}
