//! `chidori deploy` — sync a local directory with a Chidori Deploy server.
//!
//! Modeled on Val Town's `vt`: a deployment is a local directory kept in sync
//! with the cloud. `push` ships the current directory as a new immutable, live
//! version; `status` / `versions` inspect it; `rollback` / `promote` re-point
//! which version is live; `pull` brings a version's tree back down.
//!
//! Transport is the deploy server's REST shim (`/v1/deploy…`), authed by an API
//! key (`Authorization: Bearer ak_…`). Config precedence: CLI flag → env
//! (`CHIDORI_DEPLOY_URL` / `CHIDORI_API_KEY`) → `~/.chidori/credentials.json`
//! (`deploy_url` / `deploy_api_key`).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const DEFAULT_URL: &str = "http://localhost:8090";

/// Directory / file names never uploaded, regardless of `.chidoriignore`.
const ALWAYS_IGNORE: &[&str] = &[
    ".git",
    ".chidori",
    ".chidoriignore",
    "node_modules",
    "target",
    ".DS_Store",
    ".env",
];

#[derive(Args)]
pub struct DeployArgs {
    #[command(subcommand)]
    pub action: Option<DeployAction>,

    /// Deploy server base URL (or `CHIDORI_DEPLOY_URL`; default
    /// `http://localhost:8090`).
    #[arg(long, global = true)]
    pub url: Option<String>,

    /// API key `ak_…` (or `CHIDORI_API_KEY`).
    #[arg(long, global = true)]
    pub token: Option<String>,
}

#[derive(Subcommand)]
pub enum DeployAction {
    /// Sign in through the browser and save an API key (like `vt`/`gh login`).
    /// Opens the console, you sign in (or sign up), and a key is handed back to
    /// the CLI and stored in `~/.chidori/credentials.json`.
    Login(LoginArgs),
    /// Push the current directory as a new live version (the default when no
    /// subcommand is given).
    Push(PushArgs),
    /// Show the live version and version count.
    Status(TargetArgs),
    /// List the version history (newest first).
    Versions(TargetArgs),
    /// Re-point live at the previous version (or `--to N`).
    Rollback {
        #[command(flatten)]
        target: TargetArgs,
        /// Version to make live; omit for the version before the current live.
        #[arg(long)]
        to: Option<u64>,
    },
    /// Re-point live at a specific version.
    Promote {
        #[command(flatten)]
        target: TargetArgs,
        /// Version number to make live.
        version: u64,
    },
    /// Download a version's tree into a local directory.
    Pull {
        #[command(flatten)]
        target: TargetArgs,
        /// Version to fetch; omit for the current live version.
        #[arg(long)]
        version: Option<u64>,
        /// Directory to write into (default: `./<name>`).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Stream recent run activity for the deployment.
    Logs {
        #[command(flatten)]
        target: TargetArgs,
        /// Number of recent runs to show.
        #[arg(long, default_value_t = 20)]
        tail: u32,
    },
    /// Watch the directory and auto-push on every change (like `vt watch`).
    Watch(WatchArgs),
    /// List your deployed agents with their live version and recent run health.
    #[command(visible_alias = "ls")]
    List {
        /// Refresh continuously (clear the screen and re-fetch).
        #[arg(long)]
        watch: bool,
        /// Refresh interval in seconds (with `--watch`).
        #[arg(long, default_value_t = 5)]
        interval: u64,
    },
    /// Create and manage cron schedules that run your deployed agents.
    Schedule {
        #[command(subcommand)]
        cmd: ScheduleCmd,
    },
    /// Operational health of your fleet — success rate, latency, schedules
    /// (a richer monitoring view than `list`), over a rolling window.
    Fleet {
        /// Rollup window in hours (default 168 = 7 days).
        #[arg(long, default_value_t = 168)]
        window: u32,
    },
}

#[derive(Subcommand)]
pub enum ScheduleCmd {
    /// Create a schedule that runs an agent on a cron cadence.
    Create {
        /// Deployment (agent) name to run.
        name: String,
        /// Cron expression, e.g. "0 9 * * 1" (Mondays 09:00). 5- or 6-field.
        #[arg(long)]
        cron: String,
        /// JSON input passed to the agent on each run (e.g. '{"team":"eng"}').
        #[arg(long)]
        input: Option<String>,
        /// Create the schedule paused (enable later with `schedule resume`).
        #[arg(long)]
        disabled: bool,
    },
    /// List all your schedules.
    #[command(visible_alias = "ls")]
    List,
    /// Delete a schedule by id.
    #[command(visible_alias = "rm")]
    Delete {
        /// Schedule id (from `schedule list`).
        id: String,
    },
    /// Pause (disable) a schedule.
    Pause {
        /// Schedule id (from `schedule list`).
        id: String,
    },
    /// Resume (enable) a schedule.
    Resume {
        /// Schedule id (from `schedule list`).
        id: String,
    },
    /// Associate an agent with an existing schedule (schedules can fan out to
    /// several agents).
    Add {
        /// Schedule id (from `schedule list`).
        id: String,
        /// Deployment (agent) name to add.
        agent: String,
    },
    /// Remove an agent from a schedule (the schedule itself stays).
    Remove {
        /// Schedule id (from `schedule list`).
        id: String,
        /// Deployment (agent) name to remove.
        agent: String,
    },
}

#[derive(Args)]
pub struct WatchArgs {
    /// Directory to watch (default: current directory).
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// Deployment target name (default: the directory's name).
    #[arg(long)]
    pub name: Option<String>,
    /// Entrypoint file within the tree.
    #[arg(long, default_value = "agent.ts")]
    pub entrypoint: String,
    /// Poll interval in milliseconds.
    #[arg(long, default_value_t = 800)]
    pub interval_ms: u64,
}

#[derive(Args)]
pub struct PushArgs {
    /// Directory to deploy (default: current directory).
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// Deployment target name (default: the directory's name).
    #[arg(long)]
    pub name: Option<String>,
    /// Entrypoint file within the tree.
    #[arg(long, default_value = "agent.ts")]
    pub entrypoint: String,
    /// Optional note recorded on the version (e.g. a git sha).
    #[arg(long, default_value = "")]
    pub note: String,
}

#[derive(Args)]
pub struct TargetArgs {
    /// Deployment target name (default: the current directory's name).
    #[arg(long)]
    pub name: Option<String>,
}

#[derive(Args)]
pub struct LoginArgs {
    /// Console URL to sign in through (or `CHIDORI_CONSOLE_URL`; default
    /// `http://localhost:3020`).
    #[arg(long)]
    pub console: Option<String>,
    /// Label for the minted key (default: this machine's hostname).
    #[arg(long)]
    pub name: Option<String>,
}

/// Synchronous entry point from `main` — builds a runtime and dispatches.
pub fn run(args: DeployArgs) -> Result<()> {
    let rt = tokio::runtime::Runtime::new().context("failed to start async runtime")?;
    rt.block_on(run_async(args))
}

async fn run_async(args: DeployArgs) -> Result<()> {
    // Login mints the credentials, so it must run before we try to resolve one.
    if let Some(DeployAction::Login(l)) = &args.action {
        return login(args.url.clone(), l.console.clone(), l.name.clone()).await;
    }
    let cfg = Config::resolve(args.url.clone(), args.token.clone())?;
    match args.action {
        None => push(&cfg, None, None, "agent.ts".to_string(), String::new()).await,
        Some(DeployAction::Login(_)) => unreachable!("handled above"),
        Some(DeployAction::Push(p)) => push(&cfg, p.dir, p.name, p.entrypoint, p.note).await,
        Some(DeployAction::Status(t)) => status(&cfg, t.name).await,
        Some(DeployAction::Versions(t)) => versions(&cfg, t.name).await,
        Some(DeployAction::Rollback { target, to }) => rollback(&cfg, target.name, to).await,
        Some(DeployAction::Promote { target, version }) => {
            promote(&cfg, target.name, version).await
        }
        Some(DeployAction::Pull {
            target,
            version,
            out,
        }) => pull(&cfg, target.name, version, out).await,
        Some(DeployAction::Logs { target, tail }) => logs(&cfg, target.name, tail).await,
        Some(DeployAction::Watch(w)) => {
            watch(&cfg, w.dir, w.name, w.entrypoint, w.interval_ms).await
        }
        Some(DeployAction::List { watch, interval }) => list_agents(&cfg, watch, interval).await,
        Some(DeployAction::Schedule { cmd }) => schedule(&cfg, cmd).await,
        Some(DeployAction::Fleet { window }) => fleet(&cfg, window).await,
    }
}

// ── config ──────────────────────────────────────────────────────────────────

struct Config {
    base_url: String,
    token: String,
}

impl Config {
    fn resolve(url: Option<String>, token: Option<String>) -> Result<Self> {
        let creds = read_credentials();
        let base_url = url
            .or_else(|| non_empty_env("CHIDORI_DEPLOY_URL"))
            .or_else(|| creds_str(&creds, "deploy_url"))
            .unwrap_or_else(|| DEFAULT_URL.to_string());
        let token = token
            .or_else(|| non_empty_env("CHIDORI_API_KEY"))
            .or_else(|| creds_str(&creds, "deploy_api_key"))
            .context(
                "no API key found — set CHIDORI_API_KEY, pass --token ak_…, or add \
                 \"deploy_api_key\" to ~/.chidori/credentials.json",
            )?;
        Ok(Config {
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
        })
    }
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn creds_str(creds: &Option<Value>, key: &str) -> Option<String> {
    creds
        .as_ref()?
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn read_credentials() -> Option<Value> {
    let home = std::env::var("HOME").ok()?;
    let path = Path::new(&home).join(".chidori").join("credentials.json");
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

// ── HTTP ────────────────────────────────────────────────────────────────────

async fn send(cfg: &Config, req: reqwest::RequestBuilder) -> Result<Value> {
    let resp = req
        .bearer_auth(&cfg.token)
        .send()
        .await
        .context("request to the deploy server failed")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let msg = text.trim();
        bail!(
            "deploy server returned {}{}",
            status,
            if msg.is_empty() {
                String::new()
            } else {
                format!(": {msg}")
            }
        );
    }
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).context("invalid JSON from the deploy server")
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

// ── commands ────────────────────────────────────────────────────────────────

/// The outcome of a single push, for both `push` and `watch` output.
struct PushOutcome {
    version: u64,
    deduped: bool,
    hash: String,
}

/// POST a packaged tree to the deploy server. Shared by `push` and `watch`.
async fn do_push(
    cfg: &Config,
    name: &str,
    entrypoint: &str,
    note: &str,
    files: &[DeployFile],
) -> Result<PushOutcome> {
    let body = serde_json::json!({
        "program_name": name,
        "entrypoint": entrypoint,
        "note": note,
        "files": files,
    });
    let resp = send(
        cfg,
        client()
            .post(format!("{}/v1/deploy", cfg.base_url))
            .json(&body),
    )
    .await?;
    Ok(PushOutcome {
        version: resp.get("version").and_then(Value::as_u64).unwrap_or(0),
        deduped: resp
            .get("deduped")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        hash: resp
            .get("content_hash")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

async fn push(
    cfg: &Config,
    dir: Option<PathBuf>,
    name: Option<String>,
    entrypoint: String,
    note: String,
) -> Result<()> {
    let dir = dir.unwrap_or_else(|| PathBuf::from("."));
    let name = name.map(Ok).unwrap_or_else(|| dir_name(&dir))?;
    let files = collect_files(&dir)?;
    let total_bytes: usize = files.iter().map(|f| f.content.len()).sum();
    eprintln!(
        "Deploying {} ({} file{}, {}) as \"{}\"…",
        dir.display(),
        files.len(),
        if files.len() == 1 { "" } else { "s" },
        human_bytes(total_bytes),
        name
    );
    let out = do_push(cfg, &name, &entrypoint, &note, &files).await?;
    if out.deduped {
        println!(
            "Up to date — {name}@v{} is already live (identical tree) [{}]",
            out.version,
            short_hash(&out.hash)
        );
    } else {
        println!(
            "Deployed {name}@v{} — live [{}]",
            out.version,
            short_hash(&out.hash)
        );
    }
    Ok(())
}

async fn watch(
    cfg: &Config,
    dir: Option<PathBuf>,
    name: Option<String>,
    entrypoint: String,
    interval_ms: u64,
) -> Result<()> {
    let dir = dir.unwrap_or_else(|| PathBuf::from("."));
    let name = name.map(Ok).unwrap_or_else(|| dir_name(&dir))?;
    eprintln!(
        "Watching {} → \"{}\" (auto-push on change; Ctrl-C to stop)…",
        dir.display(),
        name
    );
    let interval = std::time::Duration::from_millis(interval_ms.max(100));
    let mut last_fp: Option<u64> = None;
    loop {
        match collect_files(&dir) {
            Ok(files) => {
                let fp = fingerprint(&files);
                if last_fp != Some(fp) {
                    match do_push(cfg, &name, &entrypoint, "watch", &files).await {
                        Ok(out) if out.deduped => {
                            // First scan matched what's already live — record it
                            // silently so we only announce real changes.
                            if last_fp.is_some() {
                                println!("· no change ({name}@v{})", out.version);
                            }
                        }
                        Ok(out) => println!(
                            "↑ pushed {name}@v{} [{}]",
                            out.version,
                            short_hash(&out.hash)
                        ),
                        Err(e) => eprintln!("  push failed: {e:#}"),
                    }
                    last_fp = Some(fp);
                }
            }
            Err(e) => eprintln!("  scan failed: {e:#}"),
        }
        tokio::time::sleep(interval).await;
    }
}

/// A cheap change-detection fingerprint over the sorted tree (not a security
/// hash — the server content-addresses authoritatively). `collect_files`
/// already returns sorted, so this is stable.
fn fingerprint(files: &[DeployFile]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for f in files {
        f.path.hash(&mut h);
        f.content.hash(&mut h);
    }
    h.finish()
}

async fn logs(cfg: &Config, name: Option<String>, tail: u32) -> Result<()> {
    let name = resolve_name(name)?;
    let resp = send(
        cfg,
        client().get(format!(
            "{}/v1/deploy/{}/logs?tail={}",
            cfg.base_url, name, tail
        )),
    )
    .await?;
    let empty = vec![];
    let rows = resp.get("logs").and_then(Value::as_array).unwrap_or(&empty);
    if rows.is_empty() {
        println!("{name}: no runs yet");
        return Ok(());
    }
    for r in rows {
        let at = r.get("at").and_then(Value::as_str).unwrap_or("");
        let status = r.get("status").and_then(Value::as_str).unwrap_or("");
        let run_id = r.get("run_id").and_then(Value::as_str).unwrap_or("");
        let line = r.get("line").and_then(Value::as_str).unwrap_or("");
        println!(
            "{at}  {:<9} {}  {}",
            status,
            &run_id.chars().take(8).collect::<String>(),
            line
        );
    }
    Ok(())
}

async fn status(cfg: &Config, name: Option<String>) -> Result<()> {
    let name = resolve_name(name)?;
    let resp = send(
        cfg,
        client().get(format!("{}/v1/deploy/{}/status", cfg.base_url, name)),
    )
    .await?;
    let count = resp
        .get("version_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    match resp.get("live_version") {
        Some(v) if !v.is_null() => {
            let ver = v.get("version").and_then(Value::as_u64).unwrap_or(0);
            let hash = v.get("content_hash").and_then(Value::as_str).unwrap_or("");
            let entry = v.get("entrypoint").and_then(Value::as_str).unwrap_or("");
            let created = v.get("created_at").and_then(Value::as_str).unwrap_or("");
            println!("{name}: live {name}@v{ver} [{}]", short_hash(hash));
            println!("  entrypoint: {entry}");
            println!("  deployed:   {created}");
            println!("  versions:   {count}");
        }
        _ => println!("{name}: no live version yet ({count} version(s))"),
    }
    Ok(())
}

async fn versions(cfg: &Config, name: Option<String>) -> Result<()> {
    let name = resolve_name(name)?;
    let resp = send(
        cfg,
        client().get(format!("{}/v1/deploy/{}/versions", cfg.base_url, name)),
    )
    .await?;
    let empty = vec![];
    let list = resp
        .get("versions")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    if list.is_empty() {
        println!("{name}: no versions yet");
        return Ok(());
    }
    println!("{name} — {} version(s):", list.len());
    for v in list {
        let ver = v.get("version").and_then(Value::as_u64).unwrap_or(0);
        let live = v.get("live").and_then(Value::as_bool).unwrap_or(false);
        let hash = v.get("content_hash").and_then(Value::as_str).unwrap_or("");
        let files = v.get("file_count").and_then(Value::as_u64).unwrap_or(0);
        let created = v.get("created_at").and_then(Value::as_str).unwrap_or("");
        let note = v.get("note").and_then(Value::as_str).unwrap_or("");
        println!(
            "  {} v{:<4} [{}]  {} file{}  {}{}",
            if live { "*" } else { " " },
            ver,
            short_hash(hash),
            files,
            if files == 1 { "" } else { "s" },
            created,
            if note.is_empty() {
                String::new()
            } else {
                format!("  — {note}")
            },
        );
    }
    Ok(())
}

async fn rollback(cfg: &Config, name: Option<String>, to: Option<u64>) -> Result<()> {
    let name = resolve_name(name)?;
    let body = serde_json::json!({ "to_version": to.unwrap_or(0) });
    let resp = send(
        cfg,
        client()
            .post(format!("{}/v1/deploy/{}/rollback", cfg.base_url, name))
            .json(&body),
    )
    .await?;
    let ver = resp.get("version").and_then(Value::as_u64).unwrap_or(0);
    println!("Rolled back {name} — live is now {name}@v{ver}");
    Ok(())
}

async fn promote(cfg: &Config, name: Option<String>, version: u64) -> Result<()> {
    let name = resolve_name(name)?;
    let body = serde_json::json!({ "version": version });
    let resp = send(
        cfg,
        client()
            .post(format!("{}/v1/deploy/{}/promote", cfg.base_url, name))
            .json(&body),
    )
    .await?;
    let ver = resp.get("version").and_then(Value::as_u64).unwrap_or(0);
    println!("Promoted {name}@v{ver} — now live");
    Ok(())
}

async fn pull(
    cfg: &Config,
    name: Option<String>,
    version: Option<u64>,
    out: Option<PathBuf>,
) -> Result<()> {
    let name = resolve_name(name)?;
    let mut url = format!("{}/v1/deploy/{}/pull", cfg.base_url, name);
    if let Some(v) = version {
        url.push_str(&format!("?version={v}"));
    }
    let resp = send(cfg, client().get(url)).await?;
    let ver = resp.get("version").and_then(Value::as_u64).unwrap_or(0);
    let empty = vec![];
    let files = resp
        .get("files")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let out_dir = out.unwrap_or_else(|| PathBuf::from(&name));
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("failed to create {}", out_dir.display()))?;
    let mut written = 0;
    for f in files {
        let rel = f.get("path").and_then(Value::as_str).unwrap_or_default();
        let content = f.get("content").and_then(Value::as_str).unwrap_or_default();
        let safe = sanitize_rel(rel)
            .with_context(|| format!("refusing unsafe path from server: {rel}"))?;
        let dest = out_dir.join(&safe);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, content)?;
        written += 1;
    }
    println!(
        "Pulled {name}@v{ver} → {} ({written} file{})",
        out_dir.display(),
        if written == 1 { "" } else { "s" }
    );
    Ok(())
}

// ── directory packaging ─────────────────────────────────────────────────────

#[derive(serde::Serialize)]
struct DeployFile {
    path: String,
    content: String,
}

fn collect_files(dir: &Path) -> Result<Vec<DeployFile>> {
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    let ignore = load_ignore(dir);
    let mut out = Vec::new();
    walk(dir, dir, &ignore, &mut out)?;
    if out.is_empty() {
        bail!("no deployable files found in {}", dir.display());
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn walk(root: &Path, dir: &Path, ignore: &[String], out: &mut Vec<DeployFile>) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(Result::ok)
        .collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if ALWAYS_IGNORE.contains(&name.as_str()) {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if is_ignored(&rel, &name, ignore) {
            continue;
        }
        if path.is_dir() {
            walk(root, &path, ignore, out)?;
        } else if path.is_file() {
            match std::fs::read(&path) {
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(content) => out.push(DeployFile { path: rel, content }),
                    Err(_) => eprintln!("  skipping non-UTF-8 file: {rel}"),
                },
                Err(e) => bail!("reading {rel}: {e}"),
            }
        }
    }
    Ok(())
}

/// A `.chidoriignore` entry matches a file/dir by exact relative path, by
/// basename, or as a directory prefix. (Simple prefix/name matching — not full
/// gitignore globbing.)
fn is_ignored(rel: &str, name: &str, ignore: &[String]) -> bool {
    ignore
        .iter()
        .any(|pat| rel == pat || name == pat || rel.starts_with(&format!("{pat}/")))
}

fn load_ignore(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join(".chidoriignore"))
        .ok()
        .map(|s| {
            s.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(|l| l.trim_matches('/').to_string())
                .collect()
        })
        .unwrap_or_default()
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn dir_name(dir: &Path) -> Result<String> {
    let abs = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    abs.file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .context("could not derive a deployment name from the directory; pass --name")
}

fn resolve_name(name: Option<String>) -> Result<String> {
    match name {
        Some(n) => Ok(n),
        None => dir_name(&PathBuf::from(".")),
    }
}

/// Reject absolute paths and parent traversal in a server-provided path before
/// writing it to disk (pull/clone).
fn sanitize_rel(rel: &str) -> Result<PathBuf> {
    let rel = rel.trim_start_matches("./");
    if rel.is_empty() || rel.starts_with('/') {
        bail!("absolute or empty path");
    }
    let mut out = PathBuf::new();
    for seg in rel.split('/') {
        if seg == ".." || seg == "." || seg.is_empty() {
            bail!("path traversal");
        }
        out.push(seg);
    }
    Ok(out)
}

fn short_hash(h: &str) -> String {
    h.chars().take(12).collect()
}

fn human_bytes(n: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = KB * 1024;
    if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}

// ── login (browser handoff) ─────────────────────────────────────────────────

const DEFAULT_CONSOLE_URL: &str = "http://localhost:3020";

/// `chidori deploy login` — open the console, sign in, and store the API key it
/// hands back to a local loopback listener (the `vt`/`gh login` pattern).
async fn login(
    server_url: Option<String>,
    console: Option<String>,
    label: Option<String>,
) -> Result<()> {
    let server = server_url
        .or_else(|| non_empty_env("CHIDORI_DEPLOY_URL"))
        .unwrap_or_else(|| DEFAULT_URL.to_string())
        .trim_end_matches('/')
        .to_string();
    let console = console
        .or_else(|| non_empty_env("CHIDORI_CONSOLE_URL"))
        .unwrap_or_else(|| DEFAULT_CONSOLE_URL.to_string())
        .trim_end_matches('/')
        .to_string();
    let label = label.unwrap_or_else(hostname);

    // Loopback listener the console redirects the minted key back to.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("failed to start the local callback listener")?;
    let port = listener.local_addr()?.port();
    let state = random_state();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let auth_url = format!(
        "{console}/cli-auth?redirect_uri={}&state={}&label={}",
        urlencode(&redirect_uri),
        urlencode(&state),
        urlencode(&label),
    );

    eprintln!("Opening your browser to sign in to Chidori Deploy…");
    eprintln!("  If it doesn't open, visit:\n  {auth_url}\n");
    let _ = open_browser(&auth_url);

    let key = tokio::time::timeout(
        std::time::Duration::from_secs(300),
        accept_callback(listener, &state),
    )
    .await
    .context("timed out after 5 minutes waiting for the browser")??;

    save_credentials(&server, &key)?;
    println!("Logged in \u{2713}  API key saved to ~/.chidori/credentials.json");
    println!("  server: {server}");
    Ok(())
}

/// Accept loopback connections until one delivers a valid `/callback?key&state`.
async fn accept_callback(
    listener: tokio::net::TcpListener,
    expected_state: &str,
) -> Result<String> {
    loop {
        let (mut sock, _) = listener.accept().await?;
        let mut buf = vec![0u8; 8192];
        let n = sock.read(&mut buf).await.unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]);
        let path = req
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("");
        if !path.starts_with("/callback") {
            let _ = sock
                .write_all(http_page("Waiting…", false).as_bytes())
                .await;
            continue;
        }
        let query = path.splitn(2, '?').nth(1).unwrap_or("");
        let (mut key, mut got_state) = (None, None);
        for kv in query.split('&') {
            let mut it = kv.splitn(2, '=');
            match (it.next(), it.next()) {
                (Some("key"), Some(v)) => key = Some(urldecode(v)),
                (Some("state"), Some(v)) => got_state = Some(urldecode(v)),
                _ => {}
            }
        }
        if got_state.as_deref() != Some(expected_state) {
            let _ = sock
                .write_all(http_page("State mismatch — please run login again.", false).as_bytes())
                .await;
            bail!("callback state mismatch (possible CSRF); aborting");
        }
        match key {
            Some(k) if k.starts_with("ak_") => {
                let _ = sock
                    .write_all(
                        http_page("Chidori CLI connected — you can close this tab.", true)
                            .as_bytes(),
                    )
                    .await;
                return Ok(k);
            }
            _ => {
                let _ = sock
                    .write_all(http_page("No API key was returned.", false).as_bytes())
                    .await;
                bail!("no API key in the callback");
            }
        }
    }
}

fn http_page(msg: &str, ok: bool) -> String {
    let heading = if ok {
        "\u{2713} Connected"
    } else {
        "Chidori CLI"
    };
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Chidori CLI</title></head>\
         <body style=\"font-family:system-ui,sans-serif;padding:3rem;text-align:center\">\
         <h2>{heading}</h2><p>{msg}</p></body></html>"
    );
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

/// Persist `deploy_url` + `deploy_api_key` into `~/.chidori/credentials.json`,
/// preserving any other keys (e.g. the OpenRouter model-login token).
fn save_credentials(server: &str, key: &str) -> Result<()> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    let dir = Path::new(&home).join(".chidori");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("credentials.json");
    let mut obj = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Map<String, Value>>(&s).ok())
        .unwrap_or_default();
    obj.insert("deploy_url".into(), Value::String(server.to_string()));
    obj.insert("deploy_api_key".into(), Value::String(key.to_string()));
    std::fs::write(&path, serde_json::to_string_pretty(&Value::Object(obj))?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn open_browser(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let (bin, args): (&str, Vec<&str>) = ("open", vec![url]);
    #[cfg(target_os = "linux")]
    let (bin, args): (&str, Vec<&str>) = ("xdg-open", vec![url]);
    #[cfg(target_os = "windows")]
    let (bin, args): (&str, Vec<&str>) = ("cmd", vec!["/C", "start", "", url]);
    std::process::Command::new(bin)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "cli".to_string())
}

/// A high-entropy state token for CSRF on the loopback callback. `RandomState`
/// is seeded by the OS, so two fresh hashers give 128 unpredictable bits.
fn random_state() -> String {
    use std::hash::{BuildHasher, Hasher};
    let a = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    let b = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    format!("{a:016x}{b:016x}")
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

// ── list / monitor deployed agents ──────────────────────────────────────────

async fn list_agents(cfg: &Config, watch: bool, interval: u64) -> Result<()> {
    loop {
        let resp = send(cfg, client().get(format!("{}/v1/deploy", cfg.base_url))).await?;
        let empty = vec![];
        let rows = resp
            .get("deployments")
            .and_then(Value::as_array)
            .unwrap_or(&empty);
        if watch {
            // Clear screen + move cursor home, then stamp the refresh.
            print!("\x1b[2J\x1b[H");
            println!(
                "Chidori Deploy — {} agent(s)   (refreshing every {interval}s, Ctrl-C to stop)\n",
                rows.len()
            );
        }
        render_deployments(rows);
        if !watch {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(interval.max(1))).await;
    }
    Ok(())
}

fn render_deployments(rows: &[Value]) {
    if rows.is_empty() {
        println!("No deployed agents yet. Run `chidori deploy` in an agent directory.");
        return;
    }
    println!(
        "{:<26} {:>6} {:>5}  {:<24} {:>5} {:>5}",
        "AGENT", "LIVE", "VERS", "LAST RUN", "24H", "FAIL"
    );
    for d in rows {
        let name = d.get("program_name").and_then(Value::as_str).unwrap_or("");
        let live = d.get("live_version").and_then(Value::as_u64);
        let vers = d.get("version_count").and_then(Value::as_u64).unwrap_or(0);
        let status = d.get("last_run_status").and_then(Value::as_str);
        let age = d.get("last_run_age_secs").and_then(Value::as_i64);
        let runs = d.get("runs_24h").and_then(Value::as_u64).unwrap_or(0);
        let fail = d.get("failed_24h").and_then(Value::as_u64).unwrap_or(0);

        let live_s = live
            .map(|v| format!("@v{v}"))
            .unwrap_or_else(|| "—".to_string());
        let last = match (status, age) {
            (Some(s), Some(a)) => format!("{s} · {}", rel_time(a)),
            _ => "(never run)".to_string(),
        };
        let fail_s = if fail > 0 {
            format!("{fail}")
        } else {
            "·".to_string()
        };
        println!(
            "{:<26} {:>6} {:>5}  {:<24} {:>5} {:>5}",
            truncate(name, 26),
            live_s,
            vers,
            truncate(&last, 24),
            runs,
            fail_s
        );
    }
}

/// A compact "N{s,m,h,d} ago" from a server-computed age in seconds.
fn rel_time(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s ago")
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else if s < 86_400 {
        format!("{}h ago", s / 3600)
    } else {
        format!("{}d ago", s / 86_400)
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

// ── fleet health ─────────────────────────────────────────────────────────────

async fn fleet(cfg: &Config, window: u32) -> Result<()> {
    let resp = send(
        cfg,
        client().get(format!("{}/v1/fleet?window_hours={}", cfg.base_url, window)),
    )
    .await?;
    let empty = vec![];
    let rows = resp
        .get("agents")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    if rows.is_empty() {
        println!("No agents in your fleet yet. Deploy one with `chidori deploy`.");
        return Ok(());
    }
    println!("Fleet health over the last {window}h (schedule * = enabled):\n");
    println!(
        "{:<24} {:<9} {:>5} {:>5} {:>7} {:>7} {:>6} {}",
        "AGENT", "HEALTH", "OK%", "RUNS", "P50ms", "P95ms", "SCHED", "NEXT RUN"
    );
    for a in rows {
        let name = a.get("name").and_then(Value::as_str).unwrap_or("");
        let health = a.get("health").and_then(Value::as_str).unwrap_or("");
        let ok = a.get("success_rate").and_then(Value::as_f64).unwrap_or(0.0) * 100.0;
        let runs = a.get("runs_total").and_then(Value::as_u64).unwrap_or(0);
        let p50 = a.get("p50_ms").and_then(Value::as_i64).unwrap_or(0);
        let p95 = a.get("p95_ms").and_then(Value::as_i64).unwrap_or(0);
        let sched = a.get("schedule_count").and_then(Value::as_u64).unwrap_or(0);
        let enabled = a
            .get("has_enabled_schedule")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let next = a.get("next_run_at").and_then(Value::as_str);

        let ok_disp = if runs == 0 {
            "—".to_string()
        } else {
            format!("{ok:.0}%")
        };
        let sched_disp = if sched == 0 {
            "—".to_string()
        } else {
            format!("{sched}{}", if enabled { "*" } else { "" })
        };
        let next_disp = next.map(short_time).unwrap_or_else(|| "—".to_string());
        println!(
            "{:<24} {:<9} {:>5} {:>5} {:>7} {:>7} {:>6} {}",
            truncate(name, 24),
            health,
            ok_disp,
            runs,
            p50,
            p95,
            sched_disp,
            next_disp
        );
    }
    Ok(())
}

// ── schedules ────────────────────────────────────────────────────────────────

async fn schedule(cfg: &Config, cmd: ScheduleCmd) -> Result<()> {
    match cmd {
        ScheduleCmd::Create {
            name,
            cron,
            input,
            disabled,
        } => schedule_create(cfg, name, cron, input, !disabled).await,
        ScheduleCmd::List => schedule_list(cfg).await,
        ScheduleCmd::Delete { id } => schedule_delete(cfg, id).await,
        ScheduleCmd::Pause { id } => schedule_set_enabled(cfg, id, false).await,
        ScheduleCmd::Resume { id } => schedule_set_enabled(cfg, id, true).await,
        ScheduleCmd::Add { id, agent } => schedule_agent(cfg, id, agent, true).await,
        ScheduleCmd::Remove { id, agent } => schedule_agent(cfg, id, agent, false).await,
    }
}

async fn schedule_create(
    cfg: &Config,
    name: String,
    cron: String,
    input: Option<String>,
    enabled: bool,
) -> Result<()> {
    let mut body = serde_json::json!({ "cron": cron, "enabled": enabled });
    if let Some(raw) = input {
        let v: Value = serde_json::from_str(&raw).context("--input must be valid JSON")?;
        body["input"] = v;
    }
    let resp = send(
        cfg,
        client()
            .post(format!("{}/v1/deploy/{}/schedules", cfg.base_url, name))
            .json(&body),
    )
    .await?;
    let id = resp.get("id").and_then(Value::as_str).unwrap_or("");
    let next = resp
        .get("next_run_at")
        .and_then(Value::as_str)
        .unwrap_or("—");
    let state = if resp.get("enabled").and_then(Value::as_bool).unwrap_or(true) {
        "enabled"
    } else {
        "paused"
    };
    println!("Scheduled {name} — {cron} ({state})");
    println!("  id:       {id}");
    println!("  next run: {}", short_time(next));
    Ok(())
}

async fn schedule_list(cfg: &Config) -> Result<()> {
    let resp = send(
        cfg,
        client().get(format!("{}/v1/deploy-schedules", cfg.base_url)),
    )
    .await?;
    let empty = vec![];
    let rows = resp
        .get("schedules")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    if rows.is_empty() {
        println!(
            "No schedules yet. Create one with:\n  chidori deploy schedule create <agent> --cron \"0 9 * * 1\""
        );
        return Ok(());
    }
    println!(
        "{:<38} {:<15} {:<7} {:<18} {}",
        "ID", "CRON", "STATE", "NEXT RUN", "AGENTS"
    );
    for s in rows {
        let id = s.get("id").and_then(Value::as_str).unwrap_or("");
        let cron = s.get("cron").and_then(Value::as_str).unwrap_or("");
        let enabled = s.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        let next = s.get("next_run_at").and_then(Value::as_str).unwrap_or("—");
        let agents = s
            .get("agents")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        println!(
            "{:<38} {:<15} {:<7} {:<18} {}",
            id,
            cron,
            if enabled { "on" } else { "paused" },
            short_time(next),
            agents
        );
    }
    Ok(())
}

async fn schedule_delete(cfg: &Config, id: String) -> Result<()> {
    send(
        cfg,
        client().delete(format!("{}/v1/deploy-schedules/{}", cfg.base_url, id)),
    )
    .await?;
    println!("Deleted schedule {id}");
    Ok(())
}

async fn schedule_set_enabled(cfg: &Config, id: String, enabled: bool) -> Result<()> {
    send(
        cfg,
        client()
            .patch(format!("{}/v1/deploy-schedules/{}", cfg.base_url, id))
            .json(&serde_json::json!({ "enabled": enabled })),
    )
    .await?;
    println!(
        "{} schedule {id}",
        if enabled { "Resumed" } else { "Paused" }
    );
    Ok(())
}

async fn schedule_agent(cfg: &Config, id: String, agent: String, add: bool) -> Result<()> {
    let resp = if add {
        send(
            cfg,
            client()
                .post(format!(
                    "{}/v1/deploy-schedules/{}/agents",
                    cfg.base_url, id
                ))
                .json(&serde_json::json!({ "agent": agent })),
        )
        .await?
    } else {
        send(
            cfg,
            client().delete(format!(
                "{}/v1/deploy-schedules/{}/agents/{}",
                cfg.base_url,
                id,
                urlencode(&agent)
            )),
        )
        .await?
    };
    let agents = resp
        .get("agents")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    println!(
        "{} {agent} {} schedule {id}",
        if add { "Added" } else { "Removed" },
        if add { "to" } else { "from" }
    );
    println!(
        "  agents now: {}",
        if agents.is_empty() {
            "(none)".to_string()
        } else {
            agents
        }
    );
    Ok(())
}

/// Trim an RFC3339 timestamp to "YYYY-MM-DD HH:MM" for compact display.
fn short_time(ts: &str) -> String {
    if ts.is_ascii() && ts.len() >= 16 {
        ts[..16].replace('T', " ")
    } else {
        ts.to_string()
    }
}
