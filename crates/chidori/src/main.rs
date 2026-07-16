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

        /// Extra directories to scan for tool files.
        /// Defaults to `<agent file's parent>/tools/` only.
        #[arg(long)]
        tools: Vec<PathBuf>,

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
    /// through it. Each turn is a durable host call; replaying the prior turns
    /// is free, so only your newest message hits the provider.
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

        /// Extra directories to scan for tool files (defaults to ./tools/).
        /// Discovered tools are offered to the model on every turn.
        #[arg(long)]
        tools: Vec<PathBuf>,

        /// Run under the built-in deny-by-default `untrusted` policy profile.
        #[arg(long, conflicts_with = "trusted")]
        untrusted: bool,

        /// Opt out of the ask-before-powerful-effects default (see `run --trusted`).
        #[arg(long)]
        trusted: bool,
    },

    /// List all available tools
    Tools {
        /// Tool directories to search (defaults to ./tools/)
        #[arg(short, long)]
        dir: Vec<PathBuf>,
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

        /// Edit-and-resume: proceed even though the agent source changed
        /// since this run was recorded. Recorded calls replay positionally
        /// against the edited code; an edit that touches already-replayed
        /// calls fails loudly as a divergence, an edit past the pause point
        /// resumes cleanly.
        #[arg(long)]
        allow_source_change: bool,
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
        /// Path to the agent .ts file
        file: PathBuf,

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

    // Commands that only do parsing/validation return exit code 2 on failure;
    // everything else returns 1. Success is 0.
    let (result, parse_only) = on_js_stack(move || dispatch_command(cli.command));

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
fn dispatch_command(command: Commands) -> (Result<()>, bool) {
    // Confine error-report source snippets to the entry agent's workspace root
    // (see `rust_engine::read_project_source`): a run/check names a `.ts` file,
    // whose workspace root is where its modules live.
    let entry_file = match &command {
        Commands::Run { file, .. }
        | Commands::Check { file }
        | Commands::Resume { file, .. }
        | Commands::Serve { file, .. } => Some(file.clone()),
        Commands::Chat { agent, .. } => agent.clone(),
        _ => None,
    };
    if let Some(file) = entry_file {
        crate::runtime::rust_engine::set_display_project_root(
            crate::runtime::typescript::transpile::find_workspace_root(&file),
        );
    }

    match command {
        Commands::Run {
            file,
            input,
            trace,
            verbose,
            tools,
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
            crate::runtime::isolate::warn_if_untrusted_without_isolation(untrusted);
            let result = if stream {
                cmd_run_stream(&file, &input, verbose, &tools, untrusted, trusted)
            } else {
                cmd_run(&file, &input, trace, verbose, &tools, untrusted, trusted)
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
            tools,
            untrusted,
            trusted,
        } => (
            cmd_chat(agent.as_deref(), system, model, &tools, untrusted, trusted),
            false,
        ),
        Commands::Check { file } => (cmd_check(&file), true),
        Commands::Tools { dir } => (cmd_tools(&dir), false),
        Commands::Stats { dir } => (cmd_stats(dir.as_deref()), false),
        Commands::Resume {
            file,
            run_id,
            dir,
            until_seq,
            allow_source_change,
        } => (
            cmd_resume(
                &file,
                &run_id,
                dir.as_deref(),
                until_seq,
                allow_source_change,
            ),
            false,
        ),
        Commands::Branches { run_id, dir } => (cmd_branches(&run_id, dir.as_deref()), false),
        Commands::BranchResume {
            run_id,
            branch_id,
            value,
            dir,
        } => (
            cmd_branch_resume(&run_id, &branch_id, &value, dir.as_deref()),
            false,
        ),
        Commands::BranchRerun {
            run_id,
            branch_id,
            dir,
        } => (cmd_branch_rerun(&run_id, &branch_id, dir.as_deref()), false),
        Commands::Trace { run_id, dir } => (cmd_trace(&run_id, dir.as_deref()), false),
        Commands::Snapshot { run_id, dir } => (cmd_snapshot(&run_id, dir.as_deref()), false),
        Commands::Serve {
            file,
            port,
            host,
            verbose,
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
            (
                cmd_serve(&file, host.as_deref(), port, verbose, untrusted, trusted),
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
        tools: &'static [&'static str],
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
                tools: &[],
            },
        },
        DemoExample {
            title: "Tool call",
            description: "Loads a local TypeScript tool and calls it from an agent.",
            command: "chidori run examples/agents/tool_use.ts --input query=chidori --tools examples/tools",
            requires_provider: false,
            action: DemoAction::Run {
                file: "examples/agents/tool_use.ts",
                input: &["query=chidori"],
                trace: false,
                stream: false,
                tools: &["examples/tools"],
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
                tools: &[],
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
                tools: &[],
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
                tools: &[],
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
        println!("  export LITELLM_API_URL=http://localhost:4401/v1");
        println!("  export LITELLM_API_KEY=sk-litellm-master-key");
        return Ok(());
    }

    match &demo.action {
        DemoAction::Run {
            file,
            input,
            trace,
            stream,
            tools,
        } => {
            let file = PathBuf::from(file);
            let inputs = input
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>();
            let tool_dirs = tools.iter().map(PathBuf::from).collect::<Vec<_>>();
            // The demo runs the repo's own example agents on the developer's
            // machine — the trusted posture, like `run --trusted`.
            if *stream {
                cmd_run_stream(&file, &inputs, false, &tool_dirs, false, true)
            } else {
                cmd_run(&file, &inputs, *trace, false, &tool_dirs, false, true)
            }
        }
        DemoAction::Serve { file, port } => {
            if !confirm_start_server(*port)? {
                return Ok(());
            }
            // The demo serves the developer's own example agent on their own
            // machine — the trusted posture, like `chidori run`, on the
            // default loopback bind.
            cmd_serve(&PathBuf::from(file), None, *port, false, false, true)
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
    println!("No LLM provider key found (ANTHROPIC_API_KEY / OPENAI_API_KEY).");
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

fn cmd_run(
    file: &Path,
    inputs: &[String],
    trace: bool,
    verbose: bool,
    extra_tool_dirs: &[PathBuf],
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

    // Auto-discover tools from `<project>/tools/` plus any `--tools` dirs.
    let mut tool_dirs: Vec<PathBuf> = vec![base_dir.join("tools")];
    tool_dirs.extend(extra_tool_dirs.iter().cloned());
    let tools =
        Arc::new(ToolRegistry::load_from_dirs(&tool_dirs).unwrap_or_else(|_| ToolRegistry::new()));

    let engine = Engine::new(providers, template_engine, tokio_rt)
        .with_tools(tools)
        .with_policy(cli_policy(untrusted, trusted))
        .with_persist_base(base_dir.join(".chidori").join("runs"))
        .with_workspace_root(abs_dir(&base_dir));

    // Run the agent.
    let result = engine.run(file, &input_value)?;

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
    extra_tool_dirs: &[PathBuf],
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

    let mut tool_dirs: Vec<PathBuf> = vec![base_dir.join("tools")];
    tool_dirs.extend(extra_tool_dirs.iter().cloned());
    let tools =
        Arc::new(ToolRegistry::load_from_dirs(&tool_dirs).unwrap_or_else(|_| ToolRegistry::new()));

    let engine = Engine::new(providers, template_engine, tokio_rt)
        .with_tools(tools)
        .with_policy(cli_policy(untrusted, trusted));

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

    let result = engine.run_streaming(file, &input_value, event_tx);

    // event_tx was moved into the engine; it is dropped when run_streaming
    // returns, which causes blocking_recv() in the drain thread to return None.
    drain_handle.join().ok();

    match result {
        Ok(r) => {
            let line = serde_json::json!({
                "type": "done",
                "status": "completed",
                "output": r.output,
            });
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
/// With no `agent`, a built-in conversational agent (`init::CHAT_AGENT_SRC`) is
/// written to a temp file. With an `agent`, that file is used instead; it must
/// follow the same contract — accept `{ messages, system?, model?, tools? }` and
/// return `{ transcript }` or `{ history }` of `{ role, text }` turns.
fn cmd_chat(
    agent: Option<&std::path::Path>,
    system: Option<String>,
    model: Option<String>,
    extra_tool_dirs: &[PathBuf],
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

    // Chat always calls the model, so offer an OpenRouter sign-in up front when
    // no provider key is configured — building the registry after so it picks
    // up a freshly saved key.
    let _ = ensure_llm_provider_interactive();
    let providers = Arc::new(ProviderRegistry::from_env());
    let template_engine = Arc::new(TemplateEngine::new(&base_dir));
    let tokio_rt =
        Arc::new(scheduler::new_tokio_runtime().context("Failed to create tokio runtime")?);

    let mut tool_dirs: Vec<PathBuf> = vec![base_dir.join("tools")];
    tool_dirs.extend(extra_tool_dirs.iter().cloned());
    let tools =
        Arc::new(ToolRegistry::load_from_dirs(&tool_dirs).unwrap_or_else(|_| ToolRegistry::new()));
    let tool_names: Vec<String> = tools.list().iter().map(|t| t.name.clone()).collect();

    let engine = Engine::new(providers, template_engine, tokio_rt)
        .with_tools(tools)
        .with_policy(cli_policy(untrusted, trusted))
        .with_workspace_root(abs_dir(&base_dir));

    eprintln!("chidori chat — type a message and press enter. Type 'exit' or Ctrl-D to quit.");
    if !tool_names.is_empty() {
        eprintln!("tools available: {}", tool_names.join(", "));
    }

    let mut messages: Vec<String> = Vec::new();
    let mut call_log: Vec<crate::runtime::call_log::CallRecord> = Vec::new();
    let stdin = std::io::stdin();

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
                    print!("{delta}");
                    out.flush().ok();
                    streamed = true;
                }
            }
            streamed
        });

        let result =
            engine.run_with_replay_streaming(&agent_path, &input_value, call_log.clone(), event_tx);
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
                    print!("{reply}");
                }
                println!();
                call_log = result.call_log.into_records();
            }
            Err(e) => {
                // Drop the failed turn so the next message starts clean, and keep
                // the prior call log (the failed turn left no durable record).
                messages.pop();
                eprintln!("\nerror: {e:#}");
            }
        }
    }

    if let Some(temp_dir) = temp_dir {
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
    Ok(())
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

fn cmd_tools(dirs: &[PathBuf]) -> Result<()> {
    let dirs = if dirs.is_empty() {
        vec![PathBuf::from("tools")]
    } else {
        dirs.to_vec()
    };

    let registry = ToolRegistry::load_from_dirs(&dirs)?;
    let tools = registry.list();

    if tools.is_empty() {
        println!(
            "No tools found in: {}",
            dirs.iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        return Ok(());
    }

    for tool in tools {
        println!("  {} — {}", tool.name, tool.description);
        for param in &tool.params {
            let req = if param.required { " (required)" } else { "" };
            let default = param
                .default
                .as_ref()
                .map(|d| format!(" [default: {d}]"))
                .unwrap_or_default();
            println!("    {}: {}{}{}", param.name, param.param_type, req, default);
        }
        println!();
    }

    Ok(())
}

fn cmd_resume(
    file: &Path,
    run_id: &str,
    dir: Option<&std::path::Path>,
    until_seq: Option<u64>,
    allow_source_change: bool,
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

    let providers = Arc::new(ProviderRegistry::from_env());
    let template_engine = Arc::new(TemplateEngine::new(&base_dir));
    let tokio_rt =
        Arc::new(scheduler::new_tokio_runtime().context("Failed to create tokio runtime")?);
    let tools_dir = base_dir.join("tools");
    let tools = Arc::new(
        ToolRegistry::load_from_dirs(&[tools_dir]).unwrap_or_else(|_| ToolRegistry::new()),
    );
    let engine = Engine::new(providers, template_engine, tokio_rt).with_tools(tools);

    let result = engine.run_with_replay(file, &input_value, records)?;

    let output_str = serde_json::to_string_pretty(&result.output)?;
    println!("{output_str}");
    eprintln!(
        "\nResumed from {} ({} calls replayed)",
        run_id,
        result.call_log.records().len()
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
/// providers/policy from env, tools from `<base>/tools`.
fn branch_engine(dir: Option<&std::path::Path>) -> Result<Engine> {
    let base_dir = dir
        .map(|d| d.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let providers = Arc::new(ProviderRegistry::from_env());
    let template_engine = Arc::new(TemplateEngine::new(&base_dir));
    let tokio_rt =
        Arc::new(scheduler::new_tokio_runtime().context("Failed to create tokio runtime")?);
    let tools_dir = base_dir.join("tools");
    let tools = Arc::new(
        ToolRegistry::load_from_dirs(&[tools_dir]).unwrap_or_else(|_| ToolRegistry::new()),
    );
    Ok(Engine::new(providers, template_engine, tokio_rt).with_tools(tools))
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

fn cmd_branch_resume(
    run_id: &str,
    branch_id: &str,
    value: &str,
    dir: Option<&std::path::Path>,
) -> Result<()> {
    let run_dir = branch_run_dir(run_id, dir)?;
    let engine = branch_engine(dir)?;
    let outcome = engine.resume_branch(&run_dir, branch_id, value)?;
    println!("{}", serde_json::to_string_pretty(&outcome)?);
    Ok(())
}

fn cmd_branch_rerun(run_id: &str, branch_id: &str, dir: Option<&std::path::Path>) -> Result<()> {
    let run_dir = branch_run_dir(run_id, dir)?;
    let engine = branch_engine(dir)?;
    let outcome = engine.rerun_branch(&run_dir, branch_id)?;
    println!("{}", serde_json::to_string_pretty(&outcome)?);
    Ok(())
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

    println!("Run: {}", run_id);
    println!("Calls: {}", records.len());
    println!();

    let mut total_in = 0u64;
    let mut total_out = 0u64;
    let mut total_ms = 0u64;
    let mut total_cost = 0.0;

    for r in &records {
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
            .map(|u| format!(" [{}→{} tok]", u.input_tokens, u.output_tokens))
            .unwrap_or_default();
        println!(
            "  #{:<3} {:>4}ms  {}  {}{}{}",
            r.seq, r.duration_ms, r.function, args_short, token_tag, err_tag
        );
        if let Some(ref u) = r.token_usage {
            total_in += u.input_tokens;
            total_out += u.output_tokens;
            if r.function == "prompt" {
                let model = r.args.get("model").and_then(|v| v.as_str()).unwrap_or("");
                total_cost +=
                    crate::runtime::cost::estimate_cost_usd(model, u.input_tokens, u.output_tokens);
            }
        }
        total_ms += r.duration_ms;
    }

    println!();
    if total_in > 0 || total_out > 0 {
        println!("Tokens:   {} in / {} out", total_in, total_out);
        println!("Est cost: ${:.6}", total_cost);
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
    use crate::runtime::cost::estimate_cost_usd;
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
    let mut total_duration_ms: u64 = 0;
    let mut total_cost: f64 = 0.0;

    #[derive(Default)]
    struct ModelStats {
        calls: u64,
        input_tokens: u64,
        output_tokens: u64,
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

        run_count += 1;
        let mut log = CallLog::new();
        for r in records {
            if r.function == "prompt" {
                prompt_count += 1;
                if let Some(ref usage) = r.token_usage {
                    total_input_tokens += usage.input_tokens;
                    total_output_tokens += usage.output_tokens;
                    let model = r
                        .args
                        .get("model")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let cost = estimate_cost_usd(&model, usage.input_tokens, usage.output_tokens);
                    total_cost += cost;
                    let ms = per_model.entry(model).or_default();
                    ms.calls += 1;
                    ms.input_tokens += usage.input_tokens;
                    ms.output_tokens += usage.output_tokens;
                    ms.cost_usd += cost;
                }
            } else if r.function == "tool" {
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
    println!("Est. cost:         ${:.6}", total_cost);
    println!("Total duration:    {} ms", total_duration_ms);

    if !per_model.is_empty() {
        println!("\nPer model:");
        for (model, s) in &per_model {
            println!(
                "  {:<24} {:>4} calls  {:>8} in  {:>8} out  ${:.6}",
                model, s.calls, s.input_tokens, s.output_tokens, s.cost_usd
            );
        }
    }

    Ok(())
}

fn cmd_serve(
    file: &Path,
    host: Option<&str>,
    port: u16,
    verbose: bool,
    untrusted: bool,
    trusted: bool,
) -> Result<()> {
    if verbose {
        tracing_subscriber::fmt()
            .with_env_filter("info")
            .with_target(false)
            .with_writer(std::io::stderr)
            .init();
    }

    let base_dir = file
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();

    let providers = Arc::new(ProviderRegistry::from_env());
    let template_engine = Arc::new(TemplateEngine::new(&base_dir));

    // Validate the agent file before starting the server.
    {
        let rt = Arc::new(
            scheduler::new_tokio_runtime().context("Failed to create validation runtime")?,
        );
        let engine = Engine::new(providers.clone(), template_engine.clone(), rt);
        engine.check(file).context("Agent file validation failed")?;
    }

    eprintln!("Agent: {}", file.display());
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
        file.to_path_buf(),
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
