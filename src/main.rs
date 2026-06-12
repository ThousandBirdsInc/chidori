mod acp;
mod mcp;
mod mem_guard;
mod policy;
mod providers;
mod recipes;
mod runtime;
mod scheduler;
mod server;
mod storage;
mod tools;

use std::path::PathBuf;
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
        #[arg(long)]
        untrusted: bool,
    },

    /// Validate a TypeScript agent file without running it
    Check {
        /// Path to the agent .ts file
        file: PathBuf,
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

        /// Print host function calls to stderr during execution
        #[arg(short, long)]
        verbose: bool,

        /// Serve under the built-in deny-by-default `untrusted` policy profile:
        /// gated effects (http, workspace mutations) are refused unless
        /// allowlisted. Equivalent to CHIDORI_POLICY_PROFILE=untrusted, but
        /// takes precedence over all CHIDORI_POLICY* env vars.
        #[arg(long)]
        untrusted: bool,
    },
}

fn main() {
    let cli = Cli::parse();

    // Commands that only do parsing/validation return exit code 2 on failure;
    // everything else returns 1. Success is 0.
    let (result, parse_only) = match cli.command {
        Commands::Run {
            file,
            input,
            trace,
            verbose,
            tools,
            stream,
            untrusted,
        } => {
            let result = if stream {
                cmd_run_stream(&file, &input, verbose, &tools, untrusted)
            } else {
                cmd_run(&file, &input, trace, verbose, &tools, untrusted)
            };
            (result, false)
        }
        Commands::Demo => (cmd_demo(), false),
        Commands::Check { file } => (cmd_check(&file), true),
        Commands::Tools { dir } => (cmd_tools(&dir), false),
        Commands::Stats { dir } => (cmd_stats(dir.as_deref()), false),
        Commands::Resume { file, run_id, dir } => {
            (cmd_resume(&file, &run_id, dir.as_deref()), false)
        }
        Commands::Trace { run_id, dir } => (cmd_trace(&run_id, dir.as_deref()), false),
        Commands::Snapshot { run_id, dir } => (cmd_snapshot(&run_id, dir.as_deref()), false),
        Commands::Serve {
            file,
            port,
            verbose,
            untrusted,
        } => (cmd_serve(&file, port, verbose, untrusted), false),
    };

    // Flush any buffered OTLP spans before the process exits. No-op when
    // OTEL_EXPORTER_OTLP_ENDPOINT wasn't set.
    crate::runtime::otel::shutdown_on_exit();

    match result {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("Error: {e:#}");
            std::process::exit(if parse_only { 2 } else { 1 });
        }
    }
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

    if demo.requires_provider && !has_llm_provider() {
        println!();
        println!("This demo needs an LLM provider. Set one of:");
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
            if *stream {
                cmd_run_stream(&file, &inputs, false, &tool_dirs, false)
            } else {
                cmd_run(&file, &inputs, *trace, false, &tool_dirs, false)
            }
        }
        DemoAction::Serve { file, port } => {
            if !confirm_start_server(*port)? {
                return Ok(());
            }
            cmd_serve(&PathBuf::from(file), *port, false, false)
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
}

/// Resolve the permission policy for a CLI invocation. `--untrusted` selects
/// the built-in deny-by-default profile and wins over every CHIDORI_POLICY*
/// env var (an explicit flag beats ambient configuration); otherwise the
/// usual env-driven resolution applies.
fn cli_policy(untrusted: bool) -> Arc<policy::PolicyConfig> {
    if untrusted {
        Arc::new(policy::builtin_profile("untrusted").expect("built-in untrusted profile exists"))
    } else {
        policy::PolicyConfig::from_env()
    }
}

fn cmd_run(
    file: &PathBuf,
    inputs: &[String],
    trace: bool,
    verbose: bool,
    extra_tool_dirs: &[PathBuf],
    untrusted: bool,
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

    // Resolve the project base directory.
    let base_dir = file
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();

    // Build the runtime.
    let providers = Arc::new(ProviderRegistry::from_env());
    let template_engine = Arc::new(TemplateEngine::new(&base_dir));
    let tokio_rt =
        Arc::new(tokio::runtime::Runtime::new().context("Failed to create tokio runtime")?);

    // Auto-discover tools from `<project>/tools/` plus any `--tools` dirs.
    let mut tool_dirs: Vec<PathBuf> = vec![base_dir.join("tools")];
    tool_dirs.extend(extra_tool_dirs.iter().cloned());
    let tools =
        Arc::new(ToolRegistry::load_from_dirs(&tool_dirs).unwrap_or_else(|_| ToolRegistry::new()));

    let engine = Engine::new(providers, template_engine, tokio_rt)
        .with_tools(tools)
        .with_policy(cli_policy(untrusted))
        .with_persist_base(base_dir.join(".chidori").join("runs"));

    // Run the agent.
    let result = engine.run(file, &input_value)?;

    // A `chidori.signal(name)` listen point with an empty mailbox pauses the run
    // (there is no stdin fallback for signals, unlike `input()`). The engine has
    // already persisted the durable pause scaffold under `.chidori/runs/<run_id>`;
    // tell the user the run is awaiting a signal and how to deliver one rather
    // than printing a bare `null` output. See `docs/signals.md`.
    if let Some(signal) = &result.paused_signal {
        eprintln!(
            "Run {} paused, awaiting signal '{}'.",
            result.run_id, signal.name
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
    file: &PathBuf,
    inputs: &[String],
    verbose: bool,
    extra_tool_dirs: &[PathBuf],
    untrusted: bool,
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
        Arc::new(tokio::runtime::Runtime::new().context("Failed to create tokio runtime")?);

    let mut tool_dirs: Vec<PathBuf> = vec![base_dir.join("tools")];
    tool_dirs.extend(extra_tool_dirs.iter().cloned());
    let tools =
        Arc::new(ToolRegistry::load_from_dirs(&tool_dirs).unwrap_or_else(|_| ToolRegistry::new()));

    let engine = Engine::new(providers, template_engine, tokio_rt)
        .with_tools(tools)
        .with_policy(cli_policy(untrusted));

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
            let line = serde_json::json!({
                "type": "done",
                "status": "failed",
                "error": format!("{e:#}"),
            });
            println!("{line}");
            Err(e)
        }
    }
}

fn cmd_check(file: &PathBuf) -> Result<()> {
    let providers = Arc::new(ProviderRegistry::new());
    let template_engine = Arc::new(TemplateEngine::new("."));
    let tokio_rt =
        Arc::new(tokio::runtime::Runtime::new().context("Failed to create tokio runtime")?);

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

fn cmd_resume(file: &PathBuf, run_id: &str, dir: Option<&std::path::Path>) -> Result<()> {
    let base_dir = dir
        .map(|d| d.to_path_buf())
        .or_else(|| file.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));

    let run_dir = base_dir.join(".chidori").join("runs").join(run_id);
    let checkpoint_path = run_dir.join("checkpoint.json");
    let input_path = run_dir.join("input.json");

    if !checkpoint_path.exists() {
        anyhow::bail!("No checkpoint found at {}", checkpoint_path.display());
    }

    let records: Vec<crate::runtime::call_log::CallRecord> = {
        let text = std::fs::read_to_string(&checkpoint_path)
            .with_context(|| format!("Failed to read {}", checkpoint_path.display()))?;
        serde_json::from_str(&text).context("Failed to parse checkpoint.json")?
    };

    let input_value: Value = if input_path.exists() {
        let text = std::fs::read_to_string(&input_path)?;
        serde_json::from_str(&text).unwrap_or(Value::Object(Default::default()))
    } else {
        Value::Object(Default::default())
    };

    let providers = Arc::new(ProviderRegistry::from_env());
    let template_engine = Arc::new(TemplateEngine::new(&base_dir));
    let tokio_rt =
        Arc::new(tokio::runtime::Runtime::new().context("Failed to create tokio runtime")?);
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
        result.call_log.total_duration_ms()
    );
    Ok(())
}

fn cmd_trace(run_id: &str, dir: Option<&std::path::Path>) -> Result<()> {
    let base_dir = dir
        .map(|d| d.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let run_dir = base_dir.join(".chidori").join("runs").join(run_id);
    let checkpoint_path = run_dir.join("checkpoint.json");

    if !checkpoint_path.exists() {
        anyhow::bail!("No checkpoint found at {}", checkpoint_path.display());
    }

    let text = std::fs::read_to_string(&checkpoint_path)
        .with_context(|| format!("Failed to read {}", checkpoint_path.display()))?;
    let records: Vec<crate::runtime::call_log::CallRecord> =
        serde_json::from_str(&text).context("Failed to parse checkpoint.json")?;

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
        let checkpoint_path = entry.path().join("checkpoint.json");
        if !checkpoint_path.exists() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&checkpoint_path) else {
            continue;
        };
        let Ok(records): Result<Vec<crate::runtime::call_log::CallRecord>, _> =
            serde_json::from_str(&text)
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

fn cmd_serve(file: &PathBuf, port: u16, verbose: bool, untrusted: bool) -> Result<()> {
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
            tokio::runtime::Runtime::new().context("Failed to create validation runtime")?,
        );
        let engine = Engine::new(providers.clone(), template_engine.clone(), rt);
        engine.check(file).context("Agent file validation failed")?;
    }

    eprintln!("Agent: {}", file.display());

    let tokio_rt = tokio::runtime::Runtime::new().context("Failed to create server runtime")?;
    tokio_rt.block_on(server::serve(
        providers,
        template_engine,
        file.clone(),
        port,
        cli_policy(untrusted),
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
