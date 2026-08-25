mod acp;
mod app_manifest;
mod cellstore;
mod deploy;
mod export;
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

    /// Watch an agent and re-run it on every save, replaying recorded calls
    /// from the journal so edits cost zero tokens. The first run records a
    /// journal; each save re-executes the (edited) agent against it. An edit
    /// past the recorded calls continues live; an edit that changes an
    /// already-recorded call is reported with its exact seq and the run
    /// re-records live from that point, so the journal always tracks the
    /// newest code. Exit with Ctrl-C.
    Dev {
        /// Path to the agent .ts file
        file: PathBuf,

        /// Input as key=value pairs or a JSON string.
        /// Use @filename to read value from a file.
        #[arg(short, long)]
        input: Vec<String>,

        /// Default model for prompts that don't set one in code (equivalent
        /// to CHIDORI_MODEL).
        #[arg(long)]
        model: Option<String>,

        /// Run under the built-in deny-by-default `untrusted` policy profile
        /// (see `run --untrusted`).
        #[arg(long, conflicts_with = "trusted")]
        untrusted: bool,

        /// Opt out of the ask-before-powerful-effects default (see `run --trusted`).
        #[arg(long)]
        trusted: bool,
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

        /// CI mode: emit a machine-readable JSON report to stdout and use
        /// stable exit codes — 0 the replay matched the checkpoint exactly
        /// (byte-identical, $0), 3 the replay diverged (the agent's calls or
        /// output no longer match the recording), 1 on any other error.
        /// Designed for `tael eval run --cmd`. A superset of `chidori verify`
        /// (same gates), reported machine-readably.
        #[arg(long)]
        ci: bool,
    },

    /// What is this run holding right now? One view of a run's live
    /// obligations: the pending host operation it is parked on, queued
    /// signals in its inbox, actors it spawned and has not settled, detached
    /// agents it launched (with their registry state), open branches, and
    /// armed compensations.
    Holdings {
        /// Run id (subdirectory name under `.chidori/runs/`)
        run_id: String,

        /// Project dir containing `.chidori/runs/` (defaults to the current
        /// directory)
        #[arg(short, long)]
        dir: Option<PathBuf>,
    },

    /// Run a run's registered compensations in reverse (saga rollback).
    /// `chidori.compensation.register(name, agent, input?)` journals an
    /// inverse action per side effect; this command executes them
    /// newest-first — each as its own ordinary run — for a run that stopped
    /// short (cancelled, failed, or paused-and-abandoned). Refuses a
    /// completed run (its compensations are void) and a second rollback
    /// (inverse actions are not re-fired).
    Rollback {
        /// Run id (subdirectory name under `.chidori/runs/`)
        run_id: String,

        /// Project dir containing `.chidori/runs/` (defaults to the current
        /// directory)
        #[arg(short, long)]
        dir: Option<PathBuf>,

        /// Deny gated effects the compensation agents would perform.
        #[arg(long, conflicts_with = "trusted")]
        untrusted: bool,

        /// Allow gated effects without asking (see `run --trusted`).
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

        /// Read the run from `<runs-dir>/<run_id>` instead of
        /// `<dir>/.chidori/runs/<run_id>` — the consumption side of
        /// `chidori export --fixture`: point this at the committed fixture
        /// directory.
        #[arg(long)]
        runs_dir: Option<PathBuf>,
    },

    /// Export a completed run as a minimal, committable verification fixture:
    /// copies just the artifacts `chidori verify` reads (records.jsonl,
    /// runtime.snapshot.json, output.json, input.json) into `<dest>/<run_id>/`,
    /// leaving the multi-megabyte runtime snapshot blob and resume-only state
    /// behind. Commit the fixture and run
    /// `chidori verify <agent.ts> <run_id> --runs-dir <dest>` in CI.
    Export {
        /// Run id (subdirectory name under `.chidori/runs/`)
        run_id: String,

        /// Destination directory for the fixture; the run's artifacts land in
        /// `<dest>/<run_id>/`.
        #[arg(long)]
        fixture: PathBuf,

        /// Project dir containing `.chidori/runs/` (defaults to current dir)
        #[arg(short, long)]
        dir: Option<PathBuf>,
    },

    /// Operate on persisted run checkpoints as portable artifacts.
    Checkpoint {
        #[command(subcommand)]
        action: CheckpointAction,
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

    /// Show a run's implementation history: the git-like chain of agent
    /// source versions (entry + imported modules, full text) recorded
    /// alongside the execution journal — the version the run started with,
    /// every edit accepted through edit-and-resume, and each branch's own
    /// source chain (fork, edit-and-rerun). Each commit is anchored to the
    /// journal frontier where its code took over, so the listing shows which
    /// recorded calls executed under which version.
    History {
        /// Run id (subdirectory name under `.chidori/runs/`)
        run_id: String,

        /// Project dir containing `.chidori/runs/` (defaults to current dir)
        #[arg(short, long)]
        dir: Option<PathBuf>,

        /// Print the stored source of a commit (all files, or one with
        /// --path). Accepts a full commit id or a unique hex prefix (>= 4
        /// chars).
        #[arg(long, value_name = "COMMIT", conflicts_with = "diff")]
        show: Option<String>,

        /// Unified diff between two recorded versions: `<a>..<b>`, or a
        /// single commit to diff against its parent.
        #[arg(long, value_name = "COMMIT[..COMMIT]")]
        diff: Option<String>,

        /// Restrict --show / --diff to one file path within the commit tree.
        #[arg(long)]
        path: Option<PathBuf>,

        /// Emit machine-readable JSON instead of the human listing.
        #[arg(long)]
        json: bool,
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

        /// Application manifest to boot the server from (detached-agent
        /// fleet, schedules, webhook routes). Defaults to
        /// `chidori.app.yml`/`.yaml`/`.json` next to the agent file (or in
        /// the current directory for a fleet-only server) when one exists;
        /// CHIDORI_APP_MANIFEST also names one.
        #[arg(long, value_name = "MANIFEST")]
        app: Option<PathBuf>,

        /// Strict routing: only the declared routes (sessions API, manifest
        /// webhook routes) and the canonical `/events` entrypoint are served;
        /// every other unknown path is 404 instead of executing agent(event).
        /// Equivalent to CHIDORI_SERVE_ROUTES=strict. The open default
        /// answers ANY /* with agent(event), which is webhook-friendly but
        /// means any reachable path executes the agent.
        #[arg(long)]
        strict_routes: bool,

        /// Server-wide edit-and-resume opt-in: resumes, pause-resolving
        /// signals, and approvals proceed even when the agent source changed
        /// since a run was recorded, as if every request body had set
        /// `allow_source_change: true`. Equivalent to
        /// CHIDORI_ALLOW_SOURCE_CHANGE=1. Replay's positional divergence
        /// checks still guard the already-journaled calls.
        #[arg(long)]
        allow_source_change: bool,
    },

    /// Serve a self-hosted durable run store — the celld model
    /// (github.com/denoland/celld) applied to runs, as an alternative to the
    /// Cloudflare Durable Object relay. Every run is its own SQLite database
    /// (a "cell") on local disk, replicated to an S3-compatible bucket;
    /// object-storage compare-and-swap ensures exactly one node owns a cell
    /// at a time, with no membership protocol or consensus service. Idle
    /// cells hibernate; any node sharing the bucket can restore and resume
    /// them. Speaks the same REST protocol as the Durable Object worker, so
    /// point CHIDORI_RUN_STORE=http://host:9700 at it and nothing else
    /// changes. CHIDORI_RUN_STORE_TOKEN (when set) enforces bearer auth;
    /// bucket credentials come from the same environment as
    /// CHIDORI_RUN_STORE=s3://… (endpoint, region, AWS keys).
    #[command(name = "cell-store")]
    CellStore {
        /// Address to listen on. Defaults to loopback; expose it deliberately
        /// and put TLS in front for anything non-local.
        #[arg(long, default_value = "127.0.0.1:9700")]
        listen: String,

        /// s3://bucket[/prefix] shared by the fleet — the source of truth for
        /// cell state and ownership. Omit to run a single-node store with no
        /// replication (cells live on local disk only).
        #[arg(long)]
        bucket: Option<String>,

        /// Directory for cell databases and the node's index.
        #[arg(long, default_value = ".chidori/cellstore")]
        data_dir: PathBuf,

        /// Stable node identity used in ownership records. Defaults to an id
        /// generated once and persisted in the data directory, so restarts
        /// reclaim their own cells instead of waiting out the lease.
        #[arg(long)]
        node_id: Option<String>,

        /// URL other parties can reach THIS node at (e.g.
        /// http://10.0.0.7:9700). Stamped into the ownership records this
        /// node writes, so a cell owned here answers other nodes' clients
        /// with an address they can follow instead of a bare node id. Omit
        /// on a single-node store, or when clients should stand down rather
        /// than follow.
        #[arg(long, value_name = "URL")]
        advertise: Option<String>,

        /// Cell ownership lease TTL in seconds. Takeover of a dead node's
        /// cells waits this long; node clocks must be sane within a fraction
        /// of it.
        #[arg(long, default_value_t = 30)]
        lease_secs: u64,

        /// Replication cadence in seconds: how often dirty cells snapshot to
        /// the bucket (also the bound on what a lost machine can lose).
        #[arg(long, default_value_t = 2)]
        sync_secs: u64,

        /// Hibernate cells idle longer than this many seconds: final
        /// replication, published unowned, memory dropped.
        #[arg(long, default_value_t = 300)]
        idle_secs: u64,
    },

    /// Deploy an agent to a Chidori Deploy server (like Val Town's `vt`): a
    /// local directory kept in sync with the cloud. With no subcommand, pushes
    /// the current directory as a new live version.
    ///
    ///   chidori deploy                 # push the current directory
    ///   chidori deploy status          # live version + count
    ///   chidori deploy versions        # version history
    ///   chidori deploy rollback        # revert to the previous version
    ///   chidori deploy promote 3       # make v3 live
    ///   chidori deploy pull            # download the live version's tree
    ///
    /// Auth via CHIDORI_API_KEY (or --token); server via CHIDORI_DEPLOY_URL
    /// (or --url; default http://localhost:8090).
    Deploy(deploy::DeployArgs),
}

#[derive(Subcommand)]
enum CheckpointAction {
    /// Archive a persisted run directory (`.chidori/runs/<run_id>/`) as a
    /// .tar.gz — checkpoint, input, snapshot manifest, branch stores — so the
    /// run can be committed to git as a regression fixture or attached to an
    /// eval case in an external system without knowing the runs layout.
    Export {
        /// Run id (subdirectory name under `.chidori/runs/`)
        run_id: String,

        /// Output path (defaults to `<run_id>.chidori-run.tar.gz`)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Project dir containing `.chidori/runs/` (defaults to current dir)
        #[arg(short, long)]
        dir: Option<PathBuf>,
    },

    /// Unpack an exported run archive back under `.chidori/runs/` so it can
    /// be replayed with `chidori resume`.
    Import {
        /// Path to a `.chidori-run.tar.gz` produced by `checkpoint export`
        archive: PathBuf,

        /// Project dir containing `.chidori/runs/` (defaults to current dir)
        #[arg(short, long)]
        dir: Option<PathBuf>,
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
        | Commands::Dev { file, .. }
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
        Commands::Dev {
            file,
            input,
            model,
            untrusted,
            trusted,
        } => {
            if let Some(ref model) = model {
                std::env::set_var("CHIDORI_MODEL", model);
            }
            crate::runtime::isolate::warn_if_untrusted_without_isolation(untrusted);
            (cmd_dev(&file, &input, untrusted, trusted), false)
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
            ci,
        } => {
            if ci {
                // CI mode manages its own exit codes (0 match / 3 diverged / 1
                // error) and always prints a JSON report before exiting.
                let code = cmd_resume_ci(&file, &run_id, dir.as_deref());
                crate::runtime::otel::shutdown_on_exit();
                std::process::exit(code);
            }
            if let Some(ref model) = model {
                std::env::set_var("CHIDORI_MODEL", model);
            }
            (
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
                ),
                false,
            )
        }
        Commands::Holdings { run_id, dir } => (cmd_holdings(&run_id, dir.as_deref()), false),
        Commands::Rollback {
            run_id,
            dir,
            untrusted,
            trusted,
        } => (
            cmd_rollback(&run_id, dir.as_deref(), untrusted, trusted),
            false,
        ),
        Commands::Verify {
            file,
            run_id,
            dir,
            runs_dir,
        } => (
            cmd_verify(&file, &run_id, dir.as_deref(), runs_dir.as_deref()),
            false,
        ),
        Commands::Export {
            run_id,
            fixture,
            dir,
        } => (
            crate::export::cmd_export(&run_id, &fixture, dir.as_deref()),
            false,
        ),
        Commands::Checkpoint { action } => match action {
            CheckpointAction::Export {
                run_id,
                output,
                dir,
            } => (
                cmd_checkpoint_export(&run_id, output.as_deref(), dir.as_deref()),
                false,
            ),
            CheckpointAction::Import { archive, dir } => {
                (cmd_checkpoint_import(&archive, dir.as_deref()), false)
            }
        },
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
        Commands::History {
            run_id,
            dir,
            show,
            diff,
            path,
            json,
        } => (
            cmd_history(
                &run_id,
                dir.as_deref(),
                show.as_deref(),
                diff.as_deref(),
                path.as_deref(),
                json,
            ),
            false,
        ),
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
            app,
            strict_routes,
            allow_source_change,
        } => {
            if isolate {
                crate::runtime::isolate::enable();
            } else if no_isolate {
                crate::runtime::isolate::disable();
            }
            if let Some(ref model) = model {
                std::env::set_var("CHIDORI_MODEL", model);
            }
            // Flags are sugar over the env vars the server reads, matching
            // how --isolate/--model configure their subsystems.
            if strict_routes {
                std::env::set_var("CHIDORI_SERVE_ROUTES", "strict");
            }
            if allow_source_change {
                std::env::set_var("CHIDORI_ALLOW_SOURCE_CHANGE", "1");
            }
            (
                cmd_serve(
                    file.as_deref(),
                    host.as_deref(),
                    port,
                    verbose,
                    untrusted,
                    trusted,
                    app.as_deref(),
                ),
                false,
            )
        }
        Commands::CellStore {
            listen,
            bucket,
            data_dir,
            node_id,
            advertise,
            lease_secs,
            sync_secs,
            idle_secs,
        } => (
            crate::cellstore::cmd_cell_store(
                &listen,
                bucket.as_deref(),
                &data_dir,
                node_id,
                advertise,
                lease_secs,
                sync_secs,
                idle_secs,
            ),
            false,
        ),
        Commands::Deploy(args) => (deploy::run(args), false),
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
                    // oxc-miette 3 narrowed label spans to u32; a project
                    // source file that large can't reach here anyway.
                    LabeledSpan::new(
                        Some(format!("at {}", f.name)),
                        offset as u32,
                        identifier_len_at(source, offset).max(1) as u32,
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
        Some((file, source)) if !diagnostic.labels.is_empty() => {
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
            cmd_serve(
                Some(&PathBuf::from(file)),
                None,
                *port,
                false,
                false,
                true,
                None,
            )
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
/// (which asks at the terminal by default), the server is the surface
/// untrusted callers reach with no operator present, so when the operator
/// has said nothing it is deny-by-default. Precedence:
///   1. `--untrusted` — deny-by-default, wins over all CHIDORI_POLICY* env.
///   2. `--trusted` — env-driven resolution, allow-all when nothing is
///      configured.
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

/// `chidori dev` — the edit-and-replay loop as a first-class mode.
///
/// The first run records a journal like plain `chidori run`. After that the
/// command watches the entry file plus every module the run's snapshot
/// manifest fingerprints, and each save re-executes the (edited) agent with
/// recorded calls replayed from the journal — zero tokens for everything the
/// recording already answers. Three outcomes per save:
///
///   - the edit is past the recorded calls: the prefix replays free and the
///     new tail executes live, extending the journal;
///   - the edit changed an already-recorded call: the divergence is reported
///     with its exact seq, the journal is truncated just before it, and the
///     run re-records live from that point (the dev-mode answer to the
///     fail-loud default `resume` keeps);
///   - the previous iteration failed: the crash frontier is stripped exactly
///     like `resume --retry-failed`, so the failing call re-executes live
///     against the fixed code while everything before it replays from cache.
fn cmd_dev(file: &Path, inputs: &[String], untrusted: bool, trusted: bool) -> Result<()> {
    let input_value = parse_inputs(inputs)?;
    let base_dir = file
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();
    let run_base = base_dir.join(".chidori").join("runs");

    let providers = Arc::new(ProviderRegistry::from_env());
    let template_engine = Arc::new(TemplateEngine::new(&base_dir));
    let tokio_rt =
        Arc::new(scheduler::new_tokio_runtime().context("Failed to create tokio runtime")?);

    // One engine per iteration (the builder methods consume), sharing the
    // expensive parts. `with_history_rewrite_allowed` because a divergence
    // truncation or an edit that removes calls legitimately shortens the
    // journal — dev mode owns this run and rewriting its history is the point.
    let build_engine = || {
        Engine::new(providers.clone(), template_engine.clone(), tokio_rt.clone())
            .with_tools(Arc::new(ToolRegistry::new()))
            .with_policy(cli_policy(untrusted, trusted))
            .with_persist_base(run_base.clone())
            .with_history_rewrite_allowed(true)
            .with_workspace_root(abs_dir(&base_dir))
    };

    // ---- Initial recording run -------------------------------------------
    eprintln!("[dev] recording initial run of {}…", file.display());
    let started = std::time::SystemTime::now();
    // Baseline the entry file BEFORE the initial (longest, all-live) run, so
    // an edit saved while it executes still triggers the first re-run. The
    // imported-module set isn't known until the run's manifest exists; those
    // paths fold into the baseline afterwards.
    let pre_run_signatures = watch_signatures(&[file.to_path_buf()]);
    let mut last_output: Option<Value> = None;
    let mut run_id: Option<String> = match run_engine(&build_engine(), file, &input_value) {
        Ok(result) => {
            report_dev_iteration(&result, None);
            last_output = Some(result.output.clone());
            Some(result.run_id)
        }
        Err(err) => {
            eprintln!("[dev] run failed: {err:#}");
            // The engine announced and persisted the run before failing; adopt
            // its journal so the next save replays the good prefix for free
            // and only re-executes the failing call.
            let adopted = newest_run_dir_since(&run_base, started);
            if adopted.is_none() {
                eprintln!("[dev] no journal recorded — the next save re-runs from scratch");
            }
            adopted
        }
    };

    // ---- Watch loop -------------------------------------------------------
    eprintln!("[dev] watching for changes (Ctrl-C to exit)…");
    let mut signatures = watch_signatures(&watch_set(&run_base, run_id.as_deref(), file));
    // The entry keeps its pre-run signature: if it changed during the
    // initial run, the first loop tick sees the difference and re-runs.
    signatures.extend(pre_run_signatures);
    loop {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let watched = watch_set(&run_base, run_id.as_deref(), file);
        let current = watch_signatures(&watched);
        // A path both maps know about with differing signatures is an edit; a
        // path only `current` knows (a module added by the last iteration)
        // is baseline growth, not a change, and folds in below.
        let changed: Vec<&PathBuf> = watched
            .iter()
            .filter(|p| match (signatures.get(*p), current.get(*p)) {
                (Some(prev), Some(cur)) => prev != cur,
                _ => false,
            })
            .collect();
        if changed.is_empty() {
            signatures = current;
            continue;
        }
        for path in &changed {
            eprintln!("\n[dev] changed: {}", path.display());
        }
        // Debounce: editors write in bursts; settle, then baseline the watch
        // set BEFORE running so an edit made during a long run still triggers
        // the next iteration.
        std::thread::sleep(std::time::Duration::from_millis(200));
        signatures = watch_signatures(&watch_set(&run_base, run_id.as_deref(), file));

        let iteration_started = std::time::SystemTime::now();
        match dev_iteration(
            &build_engine,
            file,
            &input_value,
            run_id.as_deref(),
            &run_base,
        ) {
            Ok(result) => {
                report_dev_iteration(&result, last_output.as_ref());
                last_output = Some(result.output.clone());
                run_id = Some(result.run_id);
            }
            Err(err) => {
                eprintln!("[dev] run failed: {err:#}");
                if run_id.is_none() {
                    run_id = newest_run_dir_since(&run_base, iteration_started);
                }
            }
        }
    }
}

/// One dev-loop re-run: replay the journal against the current source, with
/// crash-frontier stripping for a previously failed run and truncate-and-
/// re-record when the edit diverges from an already-recorded call.
fn dev_iteration(
    build_engine: &dyn Fn() -> Engine,
    file: &Path,
    input_value: &Value,
    run_id: Option<&str>,
    run_base: &Path,
) -> Result<crate::runtime::engine::RunResult> {
    // No journal yet (the very first run never persisted): plain fresh run.
    let Some(run_id) = run_id else {
        return run_engine(&build_engine(), file, input_value);
    };

    // ABI and policy drift stay fatal; source-change refusal is downgraded —
    // dev mode *is* the edit-and-resume opt-in.
    crate::runtime::snapshot::validate_manifest_for_resume(run_base, Some(run_id), file, true)?;

    let factory = crate::runtime::store::RunStoreFactory::shared(run_base);
    let _ = factory.hydrate(run_id);
    let mut records = factory
        .store_for(run_id)
        .load_call_log()?
        .unwrap_or_default();

    // A failed previous iteration left its crash frontier in the journal;
    // strip it (exactly like `resume --retry-failed`) so the fixed code
    // re-executes the failing call live instead of diverging against it.
    if records.last().is_some_and(|r| r.error.is_some()) {
        let before = records.len();
        records = crate::runtime::host_actor::strip_crash_frontier(records);
        eprintln!(
            "[dev] stripped {} failed record(s) from the previous attempt",
            before - records.len()
        );
    }

    let input_value = dev_run_input(run_base, run_id, input_value);

    // Divergence loop: each truncation lands strictly earlier in the journal,
    // so this terminates; the bound is sheer paranoia.
    for _ in 0..32 {
        let result = build_engine().resume_run(file, &input_value, records.clone(), run_id);
        match result {
            Ok(result) => return Ok(result),
            Err(err) => {
                let text = format!("{err:#}");
                let Some(seq) = parse_divergence_seq(&text) else {
                    return Err(err);
                };
                let dropped = records.iter().filter(|r| r.seq >= seq).count();
                let first_line = text.lines().next().unwrap_or("divergence").to_string();
                eprintln!("[dev] {first_line}");
                eprintln!(
                    "[dev] edit changed recorded history — re-recording live from seq {seq} \
                     ({dropped} recorded call(s) discarded, everything before replays free)"
                );
                records.retain(|r| r.seq < seq);
            }
        }
    }
    anyhow::bail!("dev loop: divergence truncation did not converge (this is a bug)")
}

/// The recorded input travels with the run (like `resume`): once a journal
/// exists, its `input.json` wins over the command line so replay keys match.
/// Before any journal exists the CLI input is authoritative.
fn dev_run_input(run_base: &Path, run_id: &str, cli_input: &Value) -> Value {
    let input_path = run_base.join(run_id).join("input.json");
    std::fs::read_to_string(&input_path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| cli_input.clone())
}

/// Run an engine with the same stderr prompt-progress notes as plain
/// `chidori run`.
fn run_engine(
    engine: &Engine,
    file: &Path,
    input_value: &Value,
) -> Result<crate::runtime::engine::RunResult> {
    match spawn_prompt_progress_listener() {
        Some((event_tx, drain)) => {
            let result = engine.run_streaming_announced(file, input_value, event_tx);
            drain.join().ok();
            result
        }
        None => engine.run_announced(file, input_value),
    }
}

/// Print one dev iteration's outcome: pause state, replay/live split, and the
/// output (or "output unchanged" when it is byte-identical to the previous
/// iteration, which is the common case while editing past the frontier).
fn report_dev_iteration(result: &crate::runtime::engine::RunResult, last_output: Option<&Value>) {
    if let Some(signal) = &result.paused_signal {
        let names = signal.listen_names();
        eprintln!(
            "[dev] run {} paused, awaiting signal{} '{}' (deliver via `chidori serve`)",
            result.run_id,
            if names.len() > 1 { " (any of)" } else { "" },
            names.join("', '")
        );
        return;
    }
    let total = result.call_log.records().len() as u64;
    let live = total.saturating_sub(result.replayed_calls);
    if last_output == Some(&result.output) {
        eprintln!(
            "[dev] run {} ok — {} call(s) replayed, {} live; output unchanged",
            result.run_id, result.replayed_calls, live
        );
    } else {
        eprintln!(
            "[dev] run {} ok — {} call(s) replayed, {} live",
            result.run_id, result.replayed_calls, live
        );
        match serde_json::to_string_pretty(&result.output) {
            Ok(text) => println!("{text}"),
            Err(_) => println!("{}", result.output),
        }
    }
}

/// The files a dev session watches: the entry file plus every module the
/// run's snapshot manifest fingerprints (the exact set the resume check
/// verifies). Falls back to just the entry when no manifest exists yet.
fn watch_set(run_base: &Path, run_id: Option<&str>, file: &Path) -> Vec<PathBuf> {
    let mut paths = vec![file.to_path_buf()];
    if let Some(run_id) = run_id {
        if let Ok(manifest) =
            crate::runtime::snapshot::SnapshotStore::new(run_base.join(run_id)).load_manifest()
        {
            paths.push(manifest.entry.path.clone());
            for module in &manifest.modules {
                paths.push(module.path.clone());
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

/// Cheap change signature per watched file: (mtime, len). Content hashing is
/// unnecessary — a same-second same-length edit re-runs at worst a free
/// replay.
fn watch_signatures(
    paths: &[PathBuf],
) -> std::collections::HashMap<PathBuf, Option<(std::time::SystemTime, u64)>> {
    paths
        .iter()
        .map(|p| {
            let sig = std::fs::metadata(p)
                .ok()
                .and_then(|m| Some((m.modified().ok()?, m.len())));
            (p.clone(), sig)
        })
        .collect()
}

/// Extract the seq from a `Replay divergence at seq N: …` error rendering.
fn parse_divergence_seq(text: &str) -> Option<u64> {
    const MARKER: &str = "Replay divergence at seq ";
    let rest = &text[text.find(MARKER)? + MARKER.len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// After a failed run (whose `RunResult` — and run id — never came back),
/// find the run directory the engine created after `since`, so the dev loop
/// can adopt its journal.
fn newest_run_dir_since(run_base: &Path, since: std::time::SystemTime) -> Option<String> {
    let entries = std::fs::read_dir(run_base).ok()?;
    let mut newest: Option<(std::time::SystemTime, String)> = None;
    for entry in entries.flatten() {
        let meta = entry.metadata().ok()?;
        if !meta.is_dir() {
            continue;
        }
        let created = meta.modified().ok()?;
        if created < since {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if newest.as_ref().is_none_or(|(t, _)| created > *t) {
            newest = Some((created, name));
        }
    }
    newest.map(|(_, name)| name)
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
                // As in `resume`: a dead holder's lease lapses on its own, and
                // with a mirror configured it isn't a local file to delete.
                Ok(Err(holder)) => anyhow::bail!(
                    "chat session {session_id} is already being driven by another process \
                     (lease holder `{}`, expires {}). Two concurrent drivers would corrupt \
                     the journal — close the other chat, or retry after {} if the holder \
                     is dead.",
                    holder.owner,
                    holder.expires_at,
                    holder.expires_at
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
        // A dead holder stops renewing, so the lease lapses at `expires` and
        // the next attempt takes it over — hence "wait", not "delete a file":
        // with a durable mirror configured the lease is fleet state living in
        // the mirror, not `<run_dir>/lease.json`.
        Ok(Err(holder)) => anyhow::bail!(
            "run {run_id} is already being driven by another process (lease holder \
             `{}`, expires {}). Two concurrent drivers would corrupt the journal — \
             wait for it to finish, or retry after {} if the holder is dead.",
            holder.owner,
            holder.expires_at,
            holder.expires_at
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

/// `chidori holdings` — aggregate what a run is holding right now (pending
/// operation, signal inbox, open actors, detached agents, branches, armed
/// compensations) into one JSON view. See `runtime::holdings`.
fn cmd_holdings(run_id: &str, dir: Option<&std::path::Path>) -> Result<()> {
    let base_dir = dir
        .map(|d| d.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let run_base = base_dir.join(".chidori").join("runs");
    let run_dir = run_base.join(run_id);

    let factory = crate::runtime::store::RunStoreFactory::shared(&run_base);
    let _ = factory.hydrate(run_id);
    // A holdings report for a run that doesn't exist would read as "holds
    // nothing" — a typo'd id must error, like `trace` and `rollback` do.
    if !run_dir.is_dir() {
        anyhow::bail!(
            "no run {run_id} under {} — check the id with `chidori stats` (or pass --dir)",
            run_base.display()
        );
    }
    let store = factory.store_for(run_id);
    let registry_factory = factory.clone();
    let lookup = move |name: &str| registry_factory.registry_get(name).ok().flatten();

    let holdings =
        crate::runtime::holdings::compute_holdings(run_id, store.as_ref(), &run_dir, &lookup)
            .with_context(|| format!("computing holdings for run {run_id}"))?;
    println!("{}", serde_json::to_string_pretty(&holdings)?);
    Ok(())
}

/// `chidori rollback` — execute a run's registered compensations in reverse
/// (`docs/host-api.md`, `runtime::compensation`). Each compensation is an
/// agent module + its recorded input, run as its own ordinary journaled run;
/// a failed compensation is reported and rollback continues past it.
fn cmd_rollback(
    run_id: &str,
    dir: Option<&std::path::Path>,
    untrusted: bool,
    trusted: bool,
) -> Result<()> {
    let base_dir = dir
        .map(|d| d.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let run_base = base_dir.join(".chidori").join("runs");

    let factory = crate::runtime::store::RunStoreFactory::shared(&run_base);
    let _ = factory.hydrate(run_id);
    let store = factory.store_for(run_id);

    let providers = Arc::new(ProviderRegistry::from_env());
    let template_engine = Arc::new(TemplateEngine::new(&base_dir));
    let tokio_rt =
        Arc::new(scheduler::new_tokio_runtime().context("Failed to create tokio runtime")?);
    let engine = Engine::new(providers, template_engine, tokio_rt)
        .with_tools(Arc::new(ToolRegistry::new()))
        .with_policy(cli_policy(untrusted, trusted))
        .with_persist_base(run_base.clone())
        .with_workspace_root(abs_dir(&base_dir));

    let mut run_agent = |path: &Path, input: &Value| {
        engine
            .run_announced(path, input)
            .map(|result| result.run_id)
            .map_err(|err| format!("{err:#}"))
    };
    let outcomes =
        crate::runtime::compensation::rollback_run(store.as_ref(), &base_dir, &mut run_agent)
            .with_context(|| format!("rolling back run {run_id}"))?;

    if outcomes.is_empty() {
        eprintln!("Run {run_id} has no registered compensations — nothing to roll back.");
        return Ok(());
    }
    let mut failed = 0usize;
    for outcome in &outcomes {
        match (&outcome.run_id, &outcome.error) {
            (Some(comp_run), None) => {
                eprintln!("  ✓ {} ({}) — run {comp_run}", outcome.name, outcome.agent)
            }
            (_, Some(error)) => {
                failed += 1;
                eprintln!("  ✗ {} ({}) — {error}", outcome.name, outcome.agent);
            }
            _ => {}
        }
    }
    eprintln!(
        "Rolled back {} compensation(s), newest first ({} failed). Report written to \
         rollback.json in the run directory.",
        outcomes.len(),
        failed
    );
    if failed > 0 {
        anyhow::bail!(
            "{failed} compensation(s) failed — inspect their runs above and re-run them \
             individually with `chidori run`"
        );
    }
    Ok(())
}

/// `chidori verify` — checkpoint-as-test as a first-class command. Replays a
/// recorded run with no provider configured, a deny-all policy, and no
/// persistence (the run directory is never written), then asserts the run
/// completed with byte-identical output. Every drift mode fails loudly:
/// changed source refuses via the manifest check, a diverging recorded call
/// errors positionally, a run that reaches for anything live has no provider
/// (and no allowed gated effects) to reach.
fn cmd_verify(
    file: &Path,
    run_id: &str,
    dir: Option<&std::path::Path>,
    runs_dir: Option<&std::path::Path>,
) -> Result<()> {
    let base_dir = dir
        .map(|d| d.to_path_buf())
        .or_else(|| file.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    // `--runs-dir` points straight at a runs base (e.g. a committed
    // `chidori export --fixture` directory); otherwise the run lives under
    // the project's `.chidori/runs/` as usual.
    let run_base = runs_dir
        .map(|d| d.to_path_buf())
        .unwrap_or_else(|| base_dir.join(".chidori").join("runs"));
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

    // Drift gate 0: a fixture without a snapshot manifest cannot be
    // fingerprint-checked at all — the resume path skips with a warning for
    // pre-manifest runs, but a verification that silently skips its source
    // gate is not a verification. `chidori export` always includes the
    // manifest, so only hand-assembled run dirs trip this.
    if crate::runtime::snapshot::SnapshotStore::new(run_dir.clone())
        .load_manifest()
        .is_err()
    {
        anyhow::bail!(
            "verify refused: no runtime.snapshot.json under {} — without the manifest the \
             source fingerprints cannot be checked. Export fixtures from complete runs with \
             `chidori export`.",
            run_dir.display()
        );
    }

    // Drift gate 1: the agent source must match the recorded fingerprints.
    // No `--allow-source-change` escape here — a verify against edited code
    // is not a verification.
    crate::runtime::snapshot::validate_manifest_for_resume(&run_base, Some(run_id), file, false)
        .context("verify refused: the agent source no longer matches this run's checkpoint")?;

    // Verification posture for the replay itself: a journal miss is an error
    // rather than a live fallthrough, and argument-drift tolerance is off —
    // both would let a divergent journal verify clean. Process-local: verify
    // is a one-shot CLI run, so the env vars scope to this process.
    std::env::set_var("CHIDORI_REPLAY_STRICT", "1");
    std::env::set_var("CHIDORI_REPLAY_LAX", "0");

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
    // Structural identity: a clean verify replays the exact recorded path,
    // so the replayed run journals exactly as many records as the recorded
    // journal holds. Fewer means the run took a shorter path and left
    // journal records unconsumed — a real divergence the live-count check
    // below cannot see (unconsumed records don't execute anything).
    if total != journal_len {
        anyhow::bail!(
            "verify FAILED: the replayed run journaled {total} record(s) but the recorded \
             journal has {journal_len} — the run no longer takes the recorded path \
             ({} served from the journal, the rest never consumed)",
            result.replayed_calls
        );
    }
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

/// `chidori resume --ci`: replay a checkpoint non-interactively and report
/// whether the run still replays byte-identically. Prints one JSON object to
/// stdout and returns a stable exit code:
///
///   0 — exact replay: every call was served from the checkpoint, none went
///       live, and the recorded shape (seq/function/args/result/error) matches.
///   3 — divergence: the agent's behavior no longer matches the checkpoint
///       (code drift); the report carries the first mismatch.
///   1 — the run errored or the checkpoint couldn't be loaded.
///
/// This is the regression-test mode `tael eval run --cmd` consumes: a golden
/// case whose fixture is a checkpoint replays at $0 in milliseconds, and any
/// nonzero exit marks the case failed.
fn cmd_resume_ci(file: &Path, run_id: &str, dir: Option<&std::path::Path>) -> i32 {
    let report = |value: Value| {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
    };

    let base_dir = dir
        .map(|d| d.to_path_buf())
        .or_else(|| file.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    let run_base = base_dir.join(".chidori").join("runs");
    let run_dir = run_base.join(run_id);

    use crate::runtime::store::RunStore as _;
    let store = crate::runtime::store::FsRunStore::new(run_dir.clone());
    let load = || -> Result<(Vec<crate::runtime::call_log::CallRecord>, Value)> {
        let records = store
            .load_call_log()?
            .ok_or_else(|| anyhow::anyhow!("No checkpoint found under {}", run_dir.display()))?;
        let input_path = run_dir.join("input.json");
        let input = if input_path.exists() {
            serde_json::from_str(&std::fs::read_to_string(&input_path)?)
                .unwrap_or(Value::Object(Default::default()))
        } else {
            Value::Object(Default::default())
        };
        Ok((records, input))
    };
    let (records, input_value) = match load() {
        Ok(v) => v,
        Err(e) => {
            report(serde_json::json!({
                "status": "error",
                "run_id": run_id,
                "error": format!("{e:#}"),
            }));
            return 1;
        }
    };

    // Source drift IS divergence for a regression gate: a fixture replayed
    // against edited agent code is testing something else.
    if let Err(e) =
        crate::runtime::snapshot::validate_manifest_for_resume(&run_base, Some(run_id), file, false)
    {
        report(serde_json::json!({
            "status": "diverged",
            "run_id": run_id,
            "checkpoint_path": run_dir.display().to_string(),
            "live_cost_usd": 0.0,
            "divergence": { "kind": "source_changed", "detail": format!("{e:#}") },
        }));
        return 3;
    }

    // Same posture as `chidori verify`: no providers, deny-all policy, no
    // persistence — the replay must answer every effect from the journal (a
    // record whose args drifted aborts as divergence), so live spend is $0
    // by construction.
    let run = || -> Result<crate::runtime::engine::RunResult> {
        let providers = Arc::new(ProviderRegistry::new());
        let template_engine = Arc::new(TemplateEngine::new(&base_dir));
        let tokio_rt =
            Arc::new(scheduler::new_tokio_runtime().context("Failed to create tokio runtime")?);
        let manifest_model = crate::runtime::snapshot::SnapshotStore::new(run_dir.clone())
            .load_manifest()
            .ok()
            .and_then(|manifest| manifest.default_model);
        let engine = Engine::new(providers, template_engine, tokio_rt)
            .with_tools(Arc::new(ToolRegistry::new()))
            .with_policy(Arc::new(
                policy::builtin_profile("untrusted").expect("built-in untrusted profile exists"),
            ))
            .with_default_model(manifest_model)
            .with_workspace_root(abs_dir(&base_dir));
        engine.resume_run(file, &input_value, records.clone(), run_id)
    };
    let result = match run() {
        Ok(r) => r,
        Err(e) => {
            // A replay-divergence abort IS the regression signal: the agent no
            // longer makes the recorded call (changed function or args).
            let msg = format!("{e:#}");
            if msg.contains("Replay divergence") {
                report(serde_json::json!({
                    "status": "diverged",
                    "run_id": run_id,
                    "checkpoint_path": run_dir.display().to_string(),
                    "live_cost_usd": 0.0,
                    "divergence": { "kind": "changed_call", "detail": msg },
                }));
                return 3;
            }
            report(serde_json::json!({
                "status": "error",
                "run_id": run_id,
                "error": msg,
            }));
            return 1;
        }
    };

    // Compare the replayed log against the checkpoint, keyed by seq. A
    // byte-identical replay reproduces every record (try_replay copies records
    // verbatim), but the *order* records land in the new log legitimately
    // differs — a container call's absorbed subtree (branch sub-records,
    // nested tool calls) is re-appended when the container replays, not at its
    // original interleaved position. Divergence is therefore: a checkpoint seq
    // the replay never produced, a seq whose recorded shape changed, or a seq
    // the checkpoint doesn't have (the agent made a new live call). Timestamps
    // and durations are excluded (copied verbatim on replay hits anyway);
    // `parent_seq` is excluded because a replay fills in parentage that
    // pre-nesting checkpoints serialized as None.
    let fingerprint = |r: &crate::runtime::call_log::CallRecord| {
        serde_json::json!({
            "function": r.function,
            "args": r.args,
            "result": r.result,
            "error": r.error,
        })
    };
    let by_seq = |rs: &[crate::runtime::call_log::CallRecord]| {
        rs.iter()
            .map(|r| (r.seq, fingerprint(r)))
            .collect::<std::collections::BTreeMap<u64, Value>>()
    };
    let expected_by_seq = by_seq(&records);
    let replayed_by_seq = by_seq(result.call_log.records());
    let mut divergence: Option<Value> = None;
    for (seq, expected) in &expected_by_seq {
        match replayed_by_seq.get(seq) {
            None => {
                divergence = Some(serde_json::json!({
                    "at_seq": seq,
                    "kind": "missing_call",
                    "expected": expected,
                }));
                break;
            }
            Some(got) if got != expected => {
                divergence = Some(serde_json::json!({
                    "at_seq": seq,
                    "kind": "changed_call",
                    "expected": expected,
                    "got": got,
                }));
                break;
            }
            Some(_) => {}
        }
    }
    if divergence.is_none() {
        if let Some((seq, got)) = replayed_by_seq
            .iter()
            .find(|(seq, _)| !expected_by_seq.contains_key(seq))
        {
            divergence = Some(serde_json::json!({
                "at_seq": seq,
                "kind": "extra_call",
                "got": got,
            }));
        }
    }

    let (input_tokens, output_tokens) = result.call_log.total_tokens();
    let base = serde_json::json!({
        "run_id": run_id,
        "checkpoint_path": run_dir.display().to_string(),
        "calls_expected": expected_by_seq.len(),
        "calls_replayed": replayed_by_seq.len(),
        // Replay never re-executes providers: the live spend of this
        // invocation is $0 regardless of the recorded token totals.
        "live_cost_usd": 0.0,
        "recorded_input_tokens": input_tokens,
        "recorded_output_tokens": output_tokens,
        "output": result.output,
    });
    match divergence {
        Some(d) => {
            let mut v = base;
            v["status"] = Value::String("diverged".to_string());
            v["divergence"] = d;
            report(v);
            3
        }
        None => {
            let mut v = base;
            v["status"] = Value::String("match".to_string());
            report(v);
            0
        }
    }
}

/// Archive `.chidori/runs/<run_id>/` as a gzip tarball whose entries are
/// rooted at `<run_id>/`, so `checkpoint import` (or plain `tar -xzf` inside
/// `.chidori/runs/`) restores the run under its original id.
fn cmd_checkpoint_export(
    run_id: &str,
    output: Option<&std::path::Path>,
    dir: Option<&std::path::Path>,
) -> Result<()> {
    let base_dir = dir
        .map(|d| d.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let run_dir = base_dir.join(".chidori").join("runs").join(run_id);
    if !run_dir.is_dir() {
        anyhow::bail!("No persisted run at {}", run_dir.display());
    }

    let out_path = output
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from(format!("{run_id}.chidori-run.tar.gz")));
    let file = std::fs::File::create(&out_path)
        .with_context(|| format!("Failed to create {}", out_path.display()))?;
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    builder
        .append_dir_all(run_id, &run_dir)
        .with_context(|| format!("Failed to archive {}", run_dir.display()))?;
    builder
        .into_inner()
        .and_then(|gz| gz.finish())
        .context("Failed to finalize archive")?;

    eprintln!("Exported {} -> {}", run_dir.display(), out_path.display());
    println!(
        "{}",
        serde_json::json!({
            "run_id": run_id,
            "archive": out_path.display().to_string(),
        })
    );
    Ok(())
}

/// Unpack a `checkpoint export` archive under `<base>/.chidori/runs/`. The
/// archive's entries are rooted at the run id, so extraction recreates
/// `.chidori/runs/<run_id>/` ready for `chidori resume`.
fn cmd_checkpoint_import(archive: &std::path::Path, dir: Option<&std::path::Path>) -> Result<()> {
    let base_dir = dir
        .map(|d| d.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let runs_dir = base_dir.join(".chidori").join("runs");
    std::fs::create_dir_all(&runs_dir)
        .with_context(|| format!("Failed to create {}", runs_dir.display()))?;

    let file = std::fs::File::open(archive)
        .with_context(|| format!("Failed to open {}", archive.display()))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut ar = tar::Archive::new(decoder);
    // Collect the top-level run id(s) while unpacking, to report what landed.
    let mut run_ids = std::collections::BTreeSet::new();
    for entry in ar.entries().context("Failed to read archive")? {
        let mut entry = entry.context("Failed to read archive entry")?;
        let path = entry.path().context("Bad entry path")?.into_owned();
        if let Some(first) = path.components().next() {
            run_ids.insert(first.as_os_str().to_string_lossy().to_string());
        }
        entry
            .unpack_in(&runs_dir)
            .with_context(|| format!("Failed to unpack {}", path.display()))?;
    }

    for id in &run_ids {
        eprintln!("Imported run {} -> {}", id, runs_dir.join(id).display());
    }
    println!(
        "{}",
        serde_json::json!({
            "runs": run_ids.iter().collect::<Vec<_>>(),
            "runs_dir": runs_dir.display().to_string(),
        })
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

/// One history store contributing to `chidori history`: the run's own trunk,
/// or one branch sub-run's chain.
struct HistoryScope {
    /// `None` for the run trunk; the branch id for a branch store.
    branch_id: Option<String>,
    /// Branch label + status from `branch.json`, when readable.
    branch_label: Option<String>,
    branch_status: Option<String>,
    dir: PathBuf,
    commits: Vec<crate::runtime::source_history::SourceCommit>,
}

/// `chidori history` — the implementation side of a run's history. The
/// execution journal (`chidori trace`) says what the run did; this command
/// says what the run *was* at each point: the git-like chain of source
/// versions recorded at run start, on every accepted edit-and-resume, and in
/// each branch store (fork / resume / edit-and-rerun), each anchored to the
/// journal frontier where that code took over.
fn cmd_history(
    run_id: &str,
    dir: Option<&std::path::Path>,
    show: Option<&str>,
    diff: Option<&str>,
    path_filter: Option<&std::path::Path>,
    json: bool,
) -> Result<()> {
    use crate::runtime::source_history::{self as sh, short_id, SourceCommit, TreeChange};
    use crate::runtime::store::{FsRunStore, RunStore as _};

    let base_dir = dir
        .map(|d| d.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let run_dir = base_dir.join(".chidori").join("runs").join(run_id);
    if !run_dir.is_dir() {
        anyhow::bail!("no run directory at {}", run_dir.display());
    }

    // Collect every history store under the run: the trunk plus one scope per
    // branch sub-run (out-of-band branch reads stay filesystem-local, like
    // `chidori branches`).
    let mut scopes: Vec<HistoryScope> = vec![HistoryScope {
        branch_id: None,
        branch_label: None,
        branch_status: None,
        dir: run_dir.clone(),
        commits: sh::load_commits(&FsRunStore::new(&run_dir))?,
    }];
    let branches_root = run_dir.join("branches");
    if branches_root.is_dir() {
        let mut op_dirs: Vec<PathBuf> = std::fs::read_dir(&branches_root)?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_dir())
            .collect();
        op_dirs.sort();
        for op_dir in op_dirs {
            let mut branch_dirs: Vec<PathBuf> = std::fs::read_dir(&op_dir)?
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| path.is_dir())
                .collect();
            branch_dirs.sort();
            for branch_dir in branch_dirs {
                let commits = sh::load_commits(&FsRunStore::new(&branch_dir))?;
                if commits.is_empty() {
                    continue;
                }
                let meta: Option<Value> = std::fs::read(branch_dir.join("branch.json"))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice(&bytes).ok());
                let meta_str = |key: &str| -> Option<String> {
                    meta.as_ref()
                        .and_then(|meta| meta.get(key))
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                };
                let branch_id = meta_str("branch_id")
                    .or_else(|| commits.first().and_then(|c| c.branch_id.clone()))
                    .unwrap_or_else(|| branch_dir.display().to_string());
                scopes.push(HistoryScope {
                    branch_id: Some(branch_id),
                    branch_label: meta_str("label"),
                    branch_status: meta_str("status"),
                    dir: branch_dir,
                    commits,
                });
            }
        }
    }

    if scopes.iter().all(|scope| scope.commits.is_empty()) {
        anyhow::bail!(
            "no implementation history recorded for run {run_id} — runs persisted before \
             source history existed have none (a new resume or branch operation will start one)"
        );
    }

    // Commit id → (scope index, commit), for parent resolution and prefix
    // lookups across the whole DAG.
    let mut by_id: Vec<(usize, &SourceCommit)> = Vec::new();
    for (scope_index, scope) in scopes.iter().enumerate() {
        for commit in &scope.commits {
            by_id.push((scope_index, commit));
        }
    }
    let resolve = |reference: &str| -> Result<(usize, &SourceCommit)> {
        let matches: Vec<&(usize, &SourceCommit)> = by_id
            .iter()
            .filter(|(_, commit)| sh::id_matches(&commit.id, reference))
            .collect();
        match matches.as_slice() {
            [] => anyhow::bail!(
                "no commit matching `{reference}` in run {run_id}'s history (prefixes need \
                 at least 4 hex chars; list ids with `chidori history {run_id}`)"
            ),
            [one] => Ok(**one),
            many => anyhow::bail!(
                "commit reference `{reference}` is ambiguous: matches {}",
                many.iter()
                    .map(|(_, commit)| short_id(&commit.id).to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    };
    // Objects live in the store whose chain recorded them; a fork commit's
    // parent lives in the run trunk — so lookups try the commit's own scope
    // first, then every other store.
    let load_object = |scope_index: usize, object: &str| -> Result<String> {
        let mut order: Vec<usize> = (0..scopes.len()).collect();
        order.retain(|index| *index != scope_index);
        order.insert(0, scope_index);
        for index in order {
            if let Some(text) = sh::load_object(&FsRunStore::new(&scopes[index].dir), object)? {
                return Ok(text);
            }
        }
        anyhow::bail!("source object {object} not found in any of the run's history stores")
    };
    let parent_of = |commit: &SourceCommit| -> Option<(usize, &SourceCommit)> {
        commit.parents.first().and_then(|parent| {
            by_id
                .iter()
                .find(|(_, candidate)| candidate.id == *parent)
                .copied()
        })
    };

    if let Some(reference) = show {
        let (scope_index, commit) = resolve(reference)?;
        let entries: Vec<_> = commit
            .tree
            .iter()
            .filter(|entry| path_filter.is_none_or(|path| entry.path == path))
            .collect();
        if entries.is_empty() {
            anyhow::bail!(
                "commit {} has no file matching --path (tree: {})",
                short_id(&commit.id),
                commit
                    .tree
                    .iter()
                    .map(|entry| entry.path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if json {
            let mut files = serde_json::Map::new();
            for entry in entries {
                files.insert(
                    entry.path.display().to_string(),
                    Value::String(load_object(scope_index, &entry.object)?),
                );
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "commit": commit,
                    "files": files,
                }))?
            );
        } else {
            for entry in entries {
                println!(
                    "// {} @ {} ({})",
                    entry.path.display(),
                    short_id(&commit.id),
                    commit.event
                );
                println!("{}", load_object(scope_index, &entry.object)?);
            }
        }
        return Ok(());
    }

    if let Some(spec) = diff {
        let (old, new) = match spec.split_once("..") {
            Some((a, b)) => (resolve(a.trim())?, resolve(b.trim())?),
            None => {
                let new = resolve(spec.trim())?;
                let old = parent_of(new.1).ok_or_else(|| {
                    anyhow::anyhow!(
                        "commit {} has no recorded parent to diff against; use \
                         `--diff <a>..<b>`",
                        short_id(&new.1.id)
                    )
                })?;
                (old, new)
            }
        };
        let mut paths: Vec<&std::path::Path> = old
            .1
            .tree
            .iter()
            .chain(new.1.tree.iter())
            .map(|entry| entry.path.as_path())
            .filter(|path| path_filter.is_none_or(|filter| *path == filter))
            .collect();
        paths.sort();
        paths.dedup();
        if paths.is_empty() {
            anyhow::bail!("--path matches no file in either commit's tree");
        }
        let mut printed = false;
        for path in paths {
            let old_text = match old.1.tree_object(path) {
                Some(object) => load_object(old.0, object)?,
                None => String::new(),
            };
            let new_text = match new.1.tree_object(path) {
                Some(object) => load_object(new.0, object)?,
                None => String::new(),
            };
            let rendered = sh::unified_diff(
                &old_text,
                &new_text,
                &format!("a/{} @{}", path.display(), short_id(&old.1.id)),
                &format!("b/{} @{}", path.display(), short_id(&new.1.id)),
            );
            if !rendered.is_empty() {
                print!("{rendered}");
                printed = true;
            }
        }
        if !printed {
            eprintln!(
                "no differences between {} and {}",
                short_id(&old.1.id),
                short_id(&new.1.id)
            );
        }
        return Ok(());
    }

    if json {
        let branches: Vec<Value> = scopes
            .iter()
            .skip(1)
            .map(|scope| {
                serde_json::json!({
                    "branchId": scope.branch_id,
                    "label": scope.branch_label,
                    "status": scope.branch_status,
                    "commits": scope.commits,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "runId": run_id,
                "run": scopes[0].commits,
                "branches": branches,
            }))?
        );
        return Ok(());
    }

    // Human listing: the trunk with execution spans, then each branch chain.
    let total_records = FsRunStore::new(&run_dir)
        .load_call_log()
        .ok()
        .flatten()
        .map(|records| records.len() as u64);
    let describe_changes = |commit: &SourceCommit| -> String {
        // A fork's parent is the run trunk's head — a different module set,
        // not an edit of it — so diffing against it would misread ("- agent.ts
        // + strategy.ts"). List the fork's own files instead.
        let parent =
            if commit.event == crate::runtime::source_history::SourceCommitEvent::BranchFork {
                None
            } else {
                parent_of(commit).map(|(_, parent)| parent)
            };
        if parent.is_none() {
            return format!(
                "{} file(s): {}",
                commit.tree.len(),
                commit
                    .tree
                    .iter()
                    .map(|entry| entry.path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let changes = sh::tree_changes(parent, commit);
        if changes.is_empty() {
            return "no file changes".to_string();
        }
        changes
            .iter()
            .map(|(path, change)| {
                let marker = match change {
                    TreeChange::Added => "+",
                    TreeChange::Modified => "~",
                    TreeChange::Removed => "-",
                };
                format!("{marker} {}", path.display())
            })
            .collect::<Vec<_>>()
            .join(", ")
    };

    println!("Implementation history for run {run_id}");
    println!(
        "(the code side of the run's history; `chidori trace {run_id}` shows the execution side)\n"
    );
    println!("run:");
    if scopes[0].commits.is_empty() {
        println!("  (no trunk history — recorded before source history existed)");
    }
    for (index, commit) in scopes[0].commits.iter().enumerate() {
        println!(
            "  * {} {:<21} {}  {}",
            short_id(&commit.id),
            commit.event.to_string(),
            commit.created_at.format("%Y-%m-%d %H:%M:%S UTC"),
            describe_changes(commit),
        );
        // The execution records this version was live for: from its frontier
        // to the next commit's (or the journal's end).
        let start = commit.journal_frontier;
        let end = scopes[0]
            .commits
            .get(index + 1)
            .map(|next| next.journal_frontier)
            .or(total_records);
        match end {
            Some(end) if end > start => {
                println!(
                    "  |     journal records {}..{} executed under this version",
                    start + 1,
                    end
                );
            }
            _ => {
                println!("  |     active from journal frontier {start}");
            }
        }
    }
    for scope in scopes.iter().skip(1) {
        let branch_id = scope.branch_id.as_deref().unwrap_or("?");
        let mut headline = format!("\nbranch {branch_id}");
        if let Some(label) = &scope.branch_label {
            headline.push_str(&format!(" [label \"{label}\"]"));
        }
        if let Some(status) = &scope.branch_status {
            headline.push_str(&format!(" ({status})"));
        }
        let fork = scope.commits.first();
        if let Some(fork) = fork {
            headline.push_str(&format!(", forked at parent seq {}", fork.journal_frontier));
            if let Some(parent) = fork.parents.first() {
                headline.push_str(&format!(" from {}", short_id(parent)));
            }
        }
        println!("{headline}:");
        for commit in &scope.commits {
            println!(
                "  * {} {:<21} {}  {}",
                short_id(&commit.id),
                commit.event.to_string(),
                commit.created_at.format("%Y-%m-%d %H:%M:%S UTC"),
                describe_changes(commit),
            );
        }
    }
    println!(
        "\nInspect a version with `chidori history {run_id} --show <commit>`, compare with \
         `--diff <a>..<b>` (or `--diff <commit>` against its parent)."
    );
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
    app: Option<&Path>,
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

    // Application manifest: an explicit `--app` (or CHIDORI_APP_MANIFEST) must
    // load or the server refuses to start; the probed default is optional.
    let app_manifest = match app.map(Path::to_path_buf).or_else(|| {
        std::env::var("CHIDORI_APP_MANIFEST")
            .ok()
            .map(PathBuf::from)
    }) {
        Some(path) => Some(crate::app_manifest::AppManifest::load(&path)?),
        None => crate::app_manifest::AppManifest::find_in(&base_dir)
            .map(|path| crate::app_manifest::AppManifest::load(&path))
            .transpose()?,
    };
    if let Some(manifest) = &app_manifest {
        eprintln!(
            "App manifest: {} — {} agent(s) ({} kept alive, {} scheduled), {} route(s)",
            manifest.name.as_deref().unwrap_or("(unnamed)"),
            manifest.agents.len(),
            manifest.fleet().count(),
            manifest
                .agents
                .iter()
                .filter(|a| a.schedule.is_some())
                .count(),
            manifest.routes.len(),
        );
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
        app_manifest,
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
    fn parse_divergence_seq_extracts_the_seq() {
        // The exact rendering `runtime::context::try_replay` produces, framed
        // by the JS-exception wrapper the engine adds — the dev loop parses
        // the full rendered chain.
        let text = "JavaScript exception: Error: Replay divergence at seq 7: `mark` was \
                    recorded with arguments {\"label\":\"a\"} but the agent now calls it \
                    with {\"label\":\"b\"}";
        assert_eq!(parse_divergence_seq(text), Some(7));
        assert_eq!(
            parse_divergence_seq("Replay divergence at seq 123: step"),
            Some(123)
        );
        assert_eq!(parse_divergence_seq("some unrelated error"), None);
        assert_eq!(parse_divergence_seq("Replay divergence at seq x"), None);
    }

    #[test]
    fn watch_set_falls_back_to_the_entry_file() {
        let dir = tempfile::tempdir().unwrap();
        let entry = dir.path().join("agent.ts");
        // No run yet (no manifest): the watch set is just the entry.
        assert_eq!(watch_set(dir.path(), None, &entry), vec![entry.clone()]);
        assert_eq!(
            watch_set(dir.path(), Some("missing-run"), &entry),
            vec![entry]
        );
    }

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
