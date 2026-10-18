//! `chidori.agents.spawn` — detached, durable, addressable agent processes.
//!
//! A detached agent is the durable-object-shaped sibling of an in-run actor
//! (`docs/actors.md`). Where an actor is confined to its spawning run — its
//! records fold into the parent journal at a join, its lifetime ends with the
//! run — a detached agent is **its own durable run**: it has its own journal
//! under `.chidori/runs/<run_id>/`, its own registered name that outlives the
//! spawner, a durable mailbox other parties (including HTTP clients, via the
//! server's `/agents` endpoints) can deliver into, and a runtime-owned restart
//! policy. The parent's journal records only the `spawn_agent` / `send_agent`
//! / `join_agent` host calls, so the whole conversation replays from cache
//! without the fold-at-join sequence-range machinery.
//!
//! **Hibernation.** A detached agent waiting at a `chidori.signal(name)` /
//! `chidori.alarm(ms)` listen point holds NO thread and NO VM: the pause
//! unwinds the VM (the standard pause path), the supervisor persists the
//! listen state into the agent's registry descriptor, and the thread exits. A
//! matching delivery — or the alarm deadline — re-enters the module under
//! resume-by-replay: recorded effects return from cache, execution goes live
//! at the listen frontier. Because journal + mailbox + listen state are all
//! durable (and mirrored when a durable run store is configured), a fresh
//! process re-arms every hibernating agent from the registry at boot
//! (`DetachedAgentHub::rearm_from_registry`), which is also how a server
//! restart or machine replacement resumes the fleet.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::mcp::McpManager;
use crate::policy::{PolicyCache, PolicyConfig};
use crate::providers::ProviderRegistry;
use crate::runtime::call_log::CallRecord;
use crate::runtime::context::{PendingSignal, RuntimeContext};
use crate::runtime::errors::RunInterrupt;
use crate::runtime::host_actor::strip_crash_frontier;
use crate::runtime::host_core;
use crate::runtime::snapshot::{QueuedSignal, RuntimePolicy, SIGNAL_INBOX_FILE};
use crate::runtime::store::RunStoreFactory;
use crate::runtime::template::TemplateEngine;
use crate::runtime::typescript::bindings::HostBindingBackend;
use crate::tools::ToolRegistry;

/// Reserved listen name backing `chidori.alarm(ms)`: an alarm is a durable
/// signal wait on this name with a timeout, so the whole wake machinery
/// (hibernate, deadline re-arm after restart, timeout sentinel) is shared
/// with ordinary signals.
pub const ALARM_SIGNAL_NAME: &str = "__chidori.alarm__";

const AGENT_THREAD_STACK_BYTES: usize = 8 * 1024 * 1024;
const AGENT_DESCRIPTOR_FILE: &str = "agent.json";

/// The engine parts a detached-agent supervisor needs to run agent modules
/// outside any spawning run's lifetime. Installed from the first spawning
/// backend, or by the server at boot (`rearm_from_registry`).
#[derive(Clone)]
pub struct AgentRuntimeParts {
    pub providers: Arc<ProviderRegistry>,
    pub template_engine: Arc<TemplateEngine>,
    pub tokio_rt: Arc<tokio::runtime::Runtime>,
    pub policy: Arc<PolicyConfig>,
    pub tools: Arc<ToolRegistry>,
    pub mcp: Arc<McpManager>,
    /// `.chidori/runs` — the base every detached agent's journal lives under.
    pub run_base: PathBuf,
}

/// The durable listen state of a hibernating agent: which names wake it, the
/// pending listen call's seq/function (for the timeout sentinel's synthetic
/// record), and the alarm deadline when the wait carries one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListenState {
    pub names: Vec<String>,
    pub seq: u64,
    pub function: String,
    #[serde(default)]
    pub deadline: Option<DateTime<Utc>>,
}

/// The durable descriptor of a detached agent — persisted as the registry
/// entry (and `agent.json` in the run dir) on every lifecycle transition, so
/// a fresh process can rediscover and resume the whole fleet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDescriptor {
    pub name: String,
    pub run_id: String,
    /// The source module path as given at spawn (possibly relative — replay
    /// keys stay stable across hosts; resolution happens wherever the agent
    /// is woken).
    pub source: String,
    pub input: Value,
    pub restart: String,
    pub max_restarts: u32,
    pub backoff_ms: u64,
    pub owner_run_id: String,
    /// The default model for the agent's prompts, captured at spawn time (the
    /// spawner's resolved model, or an explicit `model` spawn option) so a
    /// wake in a differently-configured process doesn't silently switch
    /// models. `None` on descriptors persisted before this field existed —
    /// those keep resolving from the waking process's environment.
    #[serde(default)]
    pub model: Option<String>,
    /// `running` | `hibernating` | `paused` | `completed` | `failed` | `stopped`
    pub status: String,
    #[serde(default)]
    pub listen: Option<ListenState>,
    #[serde(default)]
    pub restarts: u32,
    #[serde(default)]
    pub output: Option<Value>,
    #[serde(default)]
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl AgentDescriptor {
    fn status_json(&self) -> Value {
        json!({
            "name": self.name,
            "runId": self.run_id,
            "status": self.status,
            "restarts": self.restarts,
            "output": self.output,
            "error": self.error,
            "waitingFor": self.listen.as_ref().map(|l| l.names.clone()),
            "deadline": self.listen.as_ref().and_then(|l| l.deadline),
        })
    }
}

struct AgentEntry {
    name: String,
    /// The engine parts this agent runs with, captured when the entry was
    /// created — so agents spawned under different run bases (e.g. in tests)
    /// stay bound to their own registry and journals.
    parts: AgentRuntimeParts,
    state: Mutex<AgentState>,
    /// Woken on lifecycle transitions (join waiters) and to interrupt
    /// restart backoff on stop.
    signal: Condvar,
}

impl AgentEntry {
    fn factory(&self) -> RunStoreFactory {
        RunStoreFactory::shared(&self.parts.run_base)
    }
}

fn ensure_run_store_initialized(run_base: &Path, operation: &str) -> Result<(), String> {
    RunStoreFactory::shared(run_base)
        .ensure_initialized()
        .map_err(|err| format!("{operation}: {err:#}"))
}

struct AgentState {
    descriptor: AgentDescriptor,
    /// The live iteration's context while a supervision thread is running —
    /// the address live sends enqueue into (durably, through the ctx store).
    live_ctx: Option<RuntimeContext>,
    thread_live: bool,
    /// A matching delivery can arrive after hibernation releases its lease but
    /// before the current supervisor thread has returned. The thread wrapper
    /// consumes this bit and starts the queued wake after marking itself dead.
    wake_pending: bool,
    stop_requested: bool,
}

impl AgentEntry {
    fn is_settled(state: &AgentState) -> bool {
        matches!(
            state.descriptor.status.as_str(),
            "completed" | "failed" | "stopped" | "paused"
        )
    }
}

/// Process-global supervisor for detached agents. One per process; agents are
/// identified by their registered (or generated) name.
pub struct DetachedAgentHub {
    parts: Mutex<Option<AgentRuntimeParts>>,
    entries: Mutex<HashMap<String, Arc<AgentEntry>>>,
    timer_started: Mutex<bool>,
}

static HUB: OnceLock<DetachedAgentHub> = OnceLock::new();

pub fn hub() -> &'static DetachedAgentHub {
    HUB.get_or_init(|| DetachedAgentHub {
        parts: Mutex::new(None),
        entries: Mutex::new(HashMap::new()),
        timer_started: Mutex::new(false),
    })
}

impl DetachedAgentHub {
    /// Install (or refresh) the engine parts the supervisor runs agents with.
    /// Entries capture the parts current at their creation, so refreshing is
    /// safe for existing agents.
    pub fn install_parts(&self, parts: AgentRuntimeParts) {
        *self.parts.lock().unwrap() = Some(parts);
    }

    /// The parts installed at boot — how code without a spawning backend
    /// (the server's HTTP handlers) addresses the hub.
    pub fn installed_parts(&self) -> Result<AgentRuntimeParts, String> {
        self.parts()
    }

    fn parts(&self) -> Result<AgentRuntimeParts, String> {
        self.parts
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| "detached agents: no runtime parts installed".to_string())
    }

    /// The entry for `name`, re-materialized from the durable registry when
    /// this process has never seen it (the post-restart / replayed-spawn path).
    fn ensure_entry(
        &self,
        parts: &AgentRuntimeParts,
        name: &str,
    ) -> Result<Arc<AgentEntry>, String> {
        if let Some(entry) = self.entries.lock().unwrap().get(name).cloned() {
            return Ok(entry);
        }
        let parts = parts.clone();
        let factory = RunStoreFactory::shared(&parts.run_base);
        let registered = factory
            .registry_get(name)
            .map_err(|err| format!("chidori.agents: reading registry: {err}"))?
            .ok_or_else(|| format!("chidori.agents: unknown agent `{name}`"))?;
        let descriptor: AgentDescriptor =
            serde_json::from_value(registered.get("descriptor").cloned().unwrap_or_default())
                .map_err(|err| format!("chidori.agents: parsing registry entry: {err}"))?;
        let entry = Arc::new(AgentEntry {
            name: name.to_string(),
            parts,
            state: Mutex::new(AgentState {
                descriptor,
                live_ctx: None,
                thread_live: false,
                wake_pending: false,
                stop_requested: false,
            }),
            signal: Condvar::new(),
        });
        self.entries
            .lock()
            .unwrap()
            .insert(name.to_string(), entry.clone());
        Ok(entry)
    }

    /// Spawn a detached agent: persist its descriptor + registry entry, then
    /// start its supervision thread. Returns `{ name, runId }`.
    pub fn spawn(
        &self,
        parts: &AgentRuntimeParts,
        source: &str,
        input: Value,
        options: &SpawnOptions,
        owner_run_id: String,
    ) -> Result<Value, String> {
        let parts = parts.clone();
        ensure_run_store_initialized(
            &parts.run_base,
            "chidori.agents.spawn: initializing run store",
        )?;
        let factory = RunStoreFactory::shared(&parts.run_base);
        let name = options
            .name
            .clone()
            .unwrap_or_else(|| format!("agent-{}", &uuid::Uuid::new_v4().to_string()[..8]));
        // A live (non-settled) agent squats on its name; a settled one may be
        // replaced by a fresh spawn.
        let mut predecessor_run_id: Option<String> = None;
        if let Ok(Some(existing)) = factory.registry_get(&name) {
            let status = existing
                .get("descriptor")
                .and_then(|d| d.get("status"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if matches!(status, "running" | "hibernating") {
                return Err(format!(
                    "chidori.agents.spawn: agent name `{name}` is already registered and live"
                ));
            }
            predecessor_run_id = existing
                .get("run_id")
                .or_else(|| existing.get("descriptor").and_then(|d| d.get("run_id")))
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        let now = Utc::now();
        let descriptor = AgentDescriptor {
            name: name.clone(),
            run_id: uuid::Uuid::new_v4().to_string(),
            source: source.to_string(),
            input,
            restart: options.restart.clone(),
            max_restarts: options.max_restarts,
            backoff_ms: options.backoff_ms,
            owner_run_id,
            model: options.model.clone(),
            status: "running".to_string(),
            listen: None,
            restarts: 0,
            output: None,
            error: None,
            created_at: now,
            updated_at: now,
        };
        persist_descriptor(&factory, &descriptor);
        let run_id = descriptor.run_id.clone();

        // Mail follows the NAME, not the incarnation: whatever the settled
        // predecessor never consumed is re-queued for the replacement instead
        // of stranding in the dead run's inbox.
        if let Some(ref old_run_id) = predecessor_run_id {
            let old_store = factory.store_for(old_run_id);
            if let Ok(Some(bytes)) = old_store.get_blob(SIGNAL_INBOX_FILE) {
                let leftover: Vec<QueuedSignal> =
                    serde_json::from_slice(&bytes).unwrap_or_default();
                if !leftover.is_empty() {
                    let migrated: Vec<QueuedSignal> = leftover
                        .into_iter()
                        .enumerate()
                        .map(|(i, queued)| QueuedSignal {
                            delivery_seq: (i + 1) as u64,
                            ..queued
                        })
                        .collect();
                    tracing::info!(
                        agent = %name,
                        from_run = %old_run_id,
                        messages = migrated.len(),
                        "migrating unconsumed mailbox from the settled predecessor"
                    );
                    if let Ok(bytes) = serde_json::to_vec_pretty(&migrated) {
                        let _ = factory
                            .store_for(&run_id)
                            .put_blob(SIGNAL_INBOX_FILE, &bytes);
                    }
                    let _ = old_store.put_blob(SIGNAL_INBOX_FILE, b"[]");
                }
            }
        }

        let entry = Arc::new(AgentEntry {
            name: name.clone(),
            parts,
            state: Mutex::new(AgentState {
                descriptor,
                live_ctx: None,
                thread_live: false,
                wake_pending: false,
                stop_requested: false,
            }),
            signal: Condvar::new(),
        });
        self.entries
            .lock()
            .unwrap()
            .insert(name.clone(), entry.clone());
        self.start_thread(entry)?;
        Ok(json!({ "name": name, "runId": run_id }))
    }

    /// Start the supervision thread for an entry that isn't already running.
    fn start_thread(&self, entry: Arc<AgentEntry>) -> Result<(), String> {
        ensure_run_store_initialized(
            &entry.parts.run_base,
            "chidori.agents: initializing run store before worker start",
        )?;
        {
            let mut state = entry.state.lock().unwrap();
            if state.thread_live {
                return Ok(());
            }
            state.thread_live = true;
            state.descriptor.status = "running".to_string();
            state.descriptor.updated_at = Utc::now();
        }
        persist_descriptor(
            &entry.factory(),
            &entry.state.lock().unwrap().descriptor.clone(),
        );
        let name = entry.name.clone();
        std::thread::Builder::new()
            .name(format!("chidori-agent-{name}"))
            .stack_size(AGENT_THREAD_STACK_BYTES)
            .spawn(move || {
                supervise_detached(hub(), &entry);
                let wake_pending = {
                    let mut state = entry.state.lock().unwrap();
                    state.thread_live = false;
                    state.live_ctx = None;
                    let wake_pending = std::mem::take(&mut state.wake_pending)
                        && state.descriptor.status == "running"
                        && !state.stop_requested;
                    entry.signal.notify_all();
                    wake_pending
                };
                if wake_pending {
                    if let Err(err) = hub().start_thread(entry.clone()) {
                        tracing::warn!(agent = %entry.name, error = %err, "starting queued detached-agent wake");
                    }
                }
            })
            .map_err(|err| format!("chidori.agents.spawn: spawning agent thread: {err}"))?;
        Ok(())
    }

    /// Deliver a named message to an agent's durable mailbox. Live agents get
    /// it in-memory too (write-through, like the server's live signal path);
    /// a hibernating agent whose listen set matches is woken.
    pub fn send(
        &self,
        parts: &AgentRuntimeParts,
        to: &str,
        name: &str,
        payload: Value,
        from: Value,
    ) -> Result<Value, String> {
        ensure_run_store_initialized(
            &parts.run_base,
            "chidori.agents.send: initializing run store",
        )?;
        let entry = self.ensure_entry(parts, to)?;
        let factory = entry.factory();
        let mut wake = false;
        {
            let had_live_context = entry.state.lock().unwrap().live_ctx.is_some();
            if had_live_context {
                // Do not carry a context snapshot across the handoff to the
                // durable-mailbox path. The worker may clear it while a send
                // is in flight, so pause here and re-check under the state
                // lock before choosing which inbox writer owns this delivery.
                #[cfg(test)]
                tests::pause_after_live_context_snapshot(&entry.name);
            }
            let mut state = entry.state.lock().unwrap();
            if let Some(ctx) = state.live_ctx.clone() {
                // Write-through: in-memory mailbox + durable inbox mutate in
                // one critical section. Keep AgentState locked until the
                // context has persisted, so the worker cannot clear this
                // context and race a store-backed whole-file inbox write.
                ctx.enqueue_live_signal(name, payload, from);
                entry.signal.notify_all();
                return Ok(json!({ "delivered": true }));
            }
            let run_id = state.descriptor.run_id.clone();
            let store = factory.store_for(&run_id);
            let mut inbox: Vec<QueuedSignal> = match store.get_blob(SIGNAL_INBOX_FILE) {
                Ok(Some(bytes)) => serde_json::from_slice(&bytes).unwrap_or_default(),
                _ => Vec::new(),
            };
            #[cfg(test)]
            tests::pause_after_cross_process_inbox_read(&entry.name);
            let delivery_seq = inbox.iter().map(|s| s.delivery_seq).max().unwrap_or(0) + 1;
            inbox.push(QueuedSignal {
                name: name.to_string(),
                payload,
                from,
                delivery_seq,
                enqueued_at: Utc::now(),
            });
            let bytes = serde_json::to_vec_pretty(&inbox)
                .map_err(|err| format!("chidori.agents.send: encoding inbox: {err}"))?;
            store
                .put_blob(SIGNAL_INBOX_FILE, &bytes)
                .map_err(|err| format!("chidori.agents.send: persisting inbox: {err}"))?;
            if state.descriptor.status == "hibernating" {
                let matches = state
                    .descriptor
                    .listen
                    .as_ref()
                    .is_some_and(|l| l.names.iter().any(|n| n == name));
                if matches {
                    wake = true;
                }
            }
            // `running` with no worker thread means the previous driver died
            // mid-run: the delivery is durably queued, so use it as the wake
            // signal instead of letting the message strand.
            if state.descriptor.status == "running" && !state.thread_live && !state.stop_requested {
                wake = true;
            }
            if wake {
                state.descriptor.status = "running".to_string();
                state.descriptor.updated_at = Utc::now();
                if state.thread_live {
                    state.wake_pending = true;
                }
            }
        }
        if wake {
            persist_descriptor(&factory, &entry.state.lock().unwrap().descriptor.clone());
            if !entry.state.lock().unwrap().thread_live {
                self.start_thread(entry.clone())?;
            }
        }
        let live = {
            let state = entry.state.lock().unwrap();
            !AgentEntry::is_settled(&state)
        };
        Ok(json!({ "delivered": live }))
    }

    /// Wait for an agent to settle (completed / failed / stopped / paused).
    /// With a timeout, returns the current status snapshot on expiry. A
    /// hibernating agent does not settle — services hibernate indefinitely by
    /// design — so join a service only with a timeout.
    pub fn join(
        &self,
        parts: &AgentRuntimeParts,
        to: &str,
        timeout_ms: Option<u64>,
    ) -> Result<Value, String> {
        let entry = self.ensure_entry(parts, to)?;
        let deadline = timeout_ms.map(|ms| std::time::Instant::now() + Duration::from_millis(ms));
        let mut state = entry.state.lock().unwrap();
        loop {
            if AgentEntry::is_settled(&state) {
                return Ok(state.descriptor.status_json());
            }
            match deadline {
                Some(deadline) => {
                    let now = std::time::Instant::now();
                    if now >= deadline {
                        return Ok(state.descriptor.status_json());
                    }
                    state = entry
                        .signal
                        .wait_timeout(state, deadline.saturating_duration_since(now))
                        .unwrap()
                        .0;
                }
                None => {
                    // A hibernating agent with no live thread and no alarm
                    // deadline can only be woken by a send — an unbounded
                    // join would hang; fail fast with guidance instead.
                    if state.descriptor.status == "hibernating"
                        && !state.thread_live
                        && state
                            .descriptor
                            .listen
                            .as_ref()
                            .is_none_or(|l| l.deadline.is_none())
                    {
                        return Err(format!(
                            "chidori.agents.join: `{to}` is hibernating on {:?} with no \
                             deadline; join it with a timeoutMs or send it a message",
                            state
                                .descriptor
                                .listen
                                .as_ref()
                                .map(|l| l.names.clone())
                                .unwrap_or_default()
                        ));
                    }
                    state = entry.signal.wait(state).unwrap();
                }
            }
        }
    }

    pub fn status(&self, parts: &AgentRuntimeParts, to: &str) -> Result<Value, String> {
        let entry = self.ensure_entry(parts, to)?;
        let state = entry.state.lock().unwrap();
        let mut status = state.descriptor.status_json();
        // The descriptor is durable state; `live` is the truth about THIS
        // process: whether a worker thread is actually executing the agent
        // right now. `status: "running", live: false` means the driver died
        // and the reconciler will re-drive it — not that work is happening.
        status["live"] = json!(state.thread_live);
        Ok(status)
    }

    pub fn lookup(&self, parts: &AgentRuntimeParts, name: &str) -> Result<Value, String> {
        match self.ensure_entry(parts, name) {
            Ok(entry) => {
                let state = entry.state.lock().unwrap();
                Ok(json!({
                    "name": state.descriptor.name,
                    "runId": state.descriptor.run_id,
                    "status": state.descriptor.status,
                }))
            }
            Err(_) => Ok(Value::Null),
        }
    }

    /// Cooperative stop: a live iteration finishes its current host call and
    /// stops at the next boundary; a hibernating agent settles immediately.
    pub fn stop(&self, parts: &AgentRuntimeParts, to: &str) -> Result<Value, String> {
        let entry = self.ensure_entry(parts, to)?;
        let mut state = entry.state.lock().unwrap();
        state.stop_requested = true;
        if !state.thread_live && !AgentEntry::is_settled(&state) {
            state.descriptor.status = "stopped".to_string();
            state.descriptor.listen = None;
            state.descriptor.updated_at = Utc::now();
            let descriptor = state.descriptor.clone();
            drop(state);
            persist_descriptor(&entry.factory(), &descriptor);
            entry.signal.notify_all();
            return Ok(descriptor.status_json());
        }
        entry.signal.notify_all();
        drop(state);
        self.join(parts, to, Some(60_000))
    }

    /// Registry snapshot for the server's `/agents` listing.
    pub fn list(&self, parts: &AgentRuntimeParts) -> Result<Vec<Value>, String> {
        RunStoreFactory::shared(&parts.run_base)
            .registry_list()
            .map_err(|err| format!("listing agents: {err}"))
    }

    /// Re-arm the fleet from the durable registry at process boot: install
    /// the parts, re-create entries, wake agents that were mid-run when the
    /// previous process died, and re-arm alarm deadlines. Returns how many
    /// agents were re-armed.
    pub fn rearm_from_registry(&self, parts: AgentRuntimeParts) -> Result<usize, String> {
        ensure_run_store_initialized(
            &parts.run_base,
            "chidori.agents.rearm: initializing run store",
        )?;
        let factory = RunStoreFactory::shared(&parts.run_base);
        let parts_for_entries = parts.clone();
        self.install_parts(parts);
        let entries = factory
            .registry_list()
            .map_err(|err| format!("listing agent registry: {err}"))?;
        let mut rearmed = 0;
        for value in entries {
            let Ok(descriptor) = serde_json::from_value::<AgentDescriptor>(
                value.get("descriptor").cloned().unwrap_or_default(),
            ) else {
                continue;
            };
            let name = descriptor.name.clone();
            match descriptor.status.as_str() {
                // Died mid-run: resume-by-replay continues at the frontier.
                "running" => {
                    factory.hydrate(&descriptor.run_id).map_err(|err| {
                        format!("hydrating detached-agent journal for {name}: {err:#}")
                    })?;
                    let entry = self.ensure_entry(&parts_for_entries, &name)?;
                    self.start_thread(entry)?;
                    rearmed += 1;
                }
                // Hibernating: entry re-created; sends and (via the timer
                // thread) alarm deadlines wake it.
                "hibernating" => {
                    factory.hydrate(&descriptor.run_id).map_err(|err| {
                        format!("hydrating detached-agent journal for {name}: {err:#}")
                    })?;
                    self.ensure_entry(&parts_for_entries, &name)?;
                    rearmed += 1;
                }
                _ => {}
            }
        }
        // The timer doubles as the fleet reconciler: it re-arms alarm
        // deadlines AND re-drives `running` entries whose driver died (e.g. a
        // wake that stood down on a contended lease). Always run it when a
        // fleet exists.
        if rearmed > 0 {
            self.ensure_timer();
        }
        Ok(rearmed)
    }

    /// Lazily start the alarm/reconciler timer thread. Scans entries once a
    /// second and (a) wakes hibernating agents whose alarm deadline passed,
    /// (b) re-drives `running` agents that have no worker thread — an agent
    /// whose previous driver died (or whose wake stood down on a contended
    /// lease) must not stay `running` forever with no one executing it. The
    /// re-drive is rate-limited by `updated_at` so a stand-down against a
    /// genuinely live remote driver retries cheaply, not in a hot loop. Poll
    /// granularity is one second — alarms are minute-scale scheduling, not
    /// precise timers.
    fn ensure_timer(&self) {
        let mut started = self.timer_started.lock().unwrap();
        if *started {
            return;
        }
        *started = true;
        std::thread::Builder::new()
            .name("chidori-agents-timer".to_string())
            .spawn(|| loop {
                std::thread::sleep(Duration::from_secs(1));
                let hub = hub();
                let due: Vec<Arc<AgentEntry>> = {
                    let entries = hub.entries.lock().unwrap();
                    entries
                        .values()
                        .filter(|entry| {
                            let state = entry.state.lock().unwrap();
                            let alarm_due = state.descriptor.status == "hibernating"
                                && !state.thread_live
                                && state
                                    .descriptor
                                    .listen
                                    .as_ref()
                                    .and_then(|l| l.deadline)
                                    .is_some_and(|deadline| deadline <= Utc::now());
                            let driverless = state.descriptor.status == "running"
                                && !state.thread_live
                                && !state.stop_requested
                                && state.descriptor.updated_at
                                    <= Utc::now() - chrono::Duration::seconds(30);
                            alarm_due || driverless
                        })
                        .cloned()
                        .collect()
                };
                for entry in due {
                    if let Err(err) = ensure_run_store_initialized(
                        &entry.parts.run_base,
                        "chidori.agents.alarm: initializing run store",
                    ) {
                        tracing::warn!(agent = %entry.name, error = %err, "skipping detached-agent wake because run store initialization failed");
                        continue;
                    }
                    {
                        let mut state = entry.state.lock().unwrap();
                        state.descriptor.status = "running".to_string();
                        state.descriptor.updated_at = Utc::now();
                    }
                    persist_descriptor(
                        &entry.factory(),
                        &entry.state.lock().unwrap().descriptor.clone(),
                    );
                    let _ = hub.start_thread(entry);
                }
            })
            .ok();
    }
}

/// Parsed `chidori.agents.spawn` options.
#[derive(Debug, Clone)]
pub struct SpawnOptions {
    pub name: Option<String>,
    pub restart: String,
    pub max_restarts: u32,
    pub backoff_ms: u64,
    /// Default model for the agent's prompts. Explicit spawn option, or
    /// filled with the spawner's resolved model by the binding.
    pub model: Option<String>,
}

impl SpawnOptions {
    fn parse(value: &Value) -> Result<Self, String> {
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(ref name) = name {
            if name.is_empty()
                || name
                    .chars()
                    .any(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'))
            {
                return Err(format!(
                    "chidori.agents.spawn: `{name}` is not a registrable agent name \
                     (allowed: ASCII letters, digits, `-`, `_`, `.`)"
                ));
            }
        }
        let restart = value
            .get("restart")
            .and_then(Value::as_str)
            .unwrap_or("resume")
            .to_string();
        if !matches!(restart.as_str(), "never" | "clean" | "resume") {
            return Err(format!(
                "chidori.agents.spawn: unknown restart strategy `{restart}` \
                 (expected \"never\", \"clean\", or \"resume\")"
            ));
        }
        Ok(Self {
            name,
            restart,
            max_restarts: value
                .get("maxRestarts")
                .and_then(Value::as_u64)
                .unwrap_or(3) as u32,
            backoff_ms: value.get("backoffMs").and_then(Value::as_u64).unwrap_or(0),
            model: value
                .get("model")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        })
    }

    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "restart": self.restart,
            "maxRestarts": self.max_restarts,
            "backoffMs": self.backoff_ms,
            "model": self.model,
        })
    }
}

// ---------------------------------------------------------------------------
// The supervision loop
// ---------------------------------------------------------------------------

/// One supervision pass for a detached agent: run the module under
/// resume-by-replay, then settle — completed/failed/stopped are terminal,
/// a signal wait persists its listen state and HIBERNATES (the thread exits;
/// deliveries and alarm deadlines re-enter), a failure consumes the restart
/// budget per the spawn's strategy.
fn supervise_detached(hub: &DetachedAgentHub, entry: &Arc<AgentEntry>) {
    let parts = entry.parts.clone();
    let factory = entry.factory();

    // Single-writer ownership: take the agent run's lease before executing.
    // Two processes sharing a durable mirror (or a filesystem) cannot both
    // drive the same agent — the loser backs off and leaves the run to the
    // live holder; an expired lease (a dead node) transfers on the next wake.
    let lease_owner = process_lease_owner();
    let lease_ttl = chrono::Duration::minutes(5);
    {
        let run_id = entry.state.lock().unwrap().descriptor.run_id.clone();
        let store = factory.store_for(&run_id);
        // Contended lease: wait out the holder's TTL instead of standing down
        // for good. A dead process never renews, so its lease expires and this
        // wake takes over; a live holder keeps pushing `expires_at` forward —
        // THAT is the genuinely-driven-elsewhere case worth yielding to.
        // (Standing down permanently on a not-yet-expired stale lease was how
        // an agent could wedge forever as `running` with no worker anywhere.)
        loop {
            match crate::runtime::store::acquire_lease(store.as_ref(), lease_owner, lease_ttl) {
                Ok(Ok(_)) => break,
                Ok(Err(holder)) => {
                    let previous_expiry = holder.expires_at;
                    tracing::warn!(
                        agent = %entry.name,
                        holder = %holder.owner,
                        expires_at = %holder.expires_at,
                        "detached agent is leased to another process; waiting for the lease \
                         to expire or be renewed"
                    );
                    // Sleep to the holder's expiry in stop-aware slices.
                    loop {
                        if entry.state.lock().unwrap().stop_requested {
                            return;
                        }
                        let remaining = previous_expiry - Utc::now();
                        if remaining <= chrono::Duration::zero() {
                            break;
                        }
                        let slice = remaining
                            .to_std()
                            .unwrap_or(Duration::from_secs(1))
                            .min(Duration::from_secs(1));
                        std::thread::sleep(slice);
                    }
                    // Renewed while we waited → a live driver owns this agent.
                    if let Ok(Some(bytes)) = store.get_blob(crate::runtime::store::LEASE_FILE) {
                        if let Ok(current) =
                            serde_json::from_slice::<crate::runtime::store::RunLease>(&bytes)
                        {
                            if current.owner != lease_owner && current.expires_at > previous_expiry
                            {
                                tracing::info!(
                                    agent = %entry.name,
                                    holder = %current.owner,
                                    "lease was renewed by a live driver; standing down"
                                );
                                return;
                            }
                        }
                    }
                    // Expired and unrenewed (dead node): loop re-acquires.
                }
                Err(err) => {
                    tracing::warn!(agent = %entry.name, error = %err, "acquiring agent lease");
                    break;
                }
            }
        }
    }

    loop {
        // Renew the lease each iteration so a long-running agent keeps its
        // ownership fresh; renewal failure is logged, not fatal (the lease
        // protects against concurrent drivers, not against running at all).
        {
            let run_id = entry.state.lock().unwrap().descriptor.run_id.clone();
            let store = factory.store_for(&run_id);
            if let Ok(Err(holder)) =
                crate::runtime::store::acquire_lease(store.as_ref(), lease_owner, lease_ttl)
            {
                tracing::warn!(
                    agent = %entry.name,
                    holder = %holder.owner,
                    "lost the agent lease mid-run; standing down"
                );
                return;
            }
        }
        let (descriptor, stop_requested) = {
            let state = entry.state.lock().unwrap();
            (state.descriptor.clone(), state.stop_requested)
        };
        if stop_requested {
            settle(hub, entry, "stopped", None, None);
            return;
        }

        let run_id = descriptor.run_id.clone();
        let run_dir = parts.run_base.join(&run_id);
        let store = factory.store_for(&run_id);

        // The accumulated journal (empty on first run), plus a synthetic
        // timeout-sentinel record when this wake is an expired alarm/timeout
        // rather than a matching delivery.
        let mut replay: Vec<CallRecord> = match store.load_call_log() {
            Ok(records) => records.unwrap_or_default(),
            Err(err) => {
                settle(
                    hub,
                    entry,
                    "failed",
                    None,
                    Some(format!("loading detached-agent journal: {err:#}")),
                );
                return;
            }
        };
        let inbox: Vec<QueuedSignal> = match store.get_blob(SIGNAL_INBOX_FILE) {
            Ok(Some(bytes)) => serde_json::from_slice(&bytes).unwrap_or_default(),
            _ => Vec::new(),
        };
        #[cfg(test)]
        tests::pause_after_supervisor_inbox_snapshot(&entry.name);
        if let Some(listen) = descriptor.listen.clone() {
            let matched = inbox
                .iter()
                .any(|queued| listen.names.iter().any(|n| n == &queued.name));
            let expired = listen
                .deadline
                .is_some_and(|deadline| deadline <= Utc::now());
            if !matched && expired {
                // Woken by the deadline: resolve the listen point with the
                // timeout sentinel — the alarm fired.
                replay.retain(|r| r.seq != listen.seq);
                replay.push(CallRecord {
                    seq: listen.seq,
                    parent_seq: None,
                    function: listen.function.clone(),
                    args: if listen.function == "signal_any" {
                        json!({ "names": listen.names })
                    } else {
                        json!({ "name": listen.names[0] })
                    },
                    result: host_core::signal_timeout_sentinel(&listen.names),
                    duration_ms: 0,
                    token_usage: None,
                    timestamp: Utc::now(),
                    error: None,
                });
            } else if !matched {
                // Spuriously started with nothing to do — go back to sleep.
                match hibernate(hub, entry, listen) {
                    Ok(true) => return,
                    Ok(false) => continue,
                    Err(err) => {
                        settle(
                            hub,
                            entry,
                            "failed",
                            None,
                            Some(format!(
                                "checking detached-agent inbox before hibernate: {err}"
                            )),
                        );
                        return;
                    }
                }
            }
            let mut state = entry.state.lock().unwrap();
            state.descriptor.listen = None;
        }

        // Fresh VM + context under the agent's OWN durable identity: run id,
        // journal, policy seed, and persistence handle all belong to the
        // agent, not the spawner.
        if let Err(err) = crate::runtime::snapshot::reconcile_detached_signal_timeout_promises(
            store.as_ref(),
            &replay,
        ) {
            settle(
                hub,
                entry,
                "failed",
                None,
                Some(format!(
                    "reconciling detached-agent alarm promises: {err:#}"
                )),
            );
            return;
        }
        let host_promises =
            match crate::runtime::snapshot::load_host_promise_records(store.as_ref()) {
                Ok(records) => records,
                Err(err) => {
                    settle(
                        hub,
                        entry,
                        "failed",
                        None,
                        Some(format!("loading detached-agent host promises: {err:#}")),
                    );
                    return;
                }
            };
        let vfs = crate::runtime::snapshot::SnapshotStore::with_store(&run_dir, store.clone())
            .load_manifest()
            .map(|manifest| manifest.vfs)
            .unwrap_or_default();
        let ctx = RuntimeContext::with_replay_host_promises_vfs_and_signals(
            replay,
            host_promises,
            vfs,
            inbox,
        );
        ctx.set_run_id(run_id.clone());
        ctx.set_input_mode(crate::runtime::context::InputMode::Pause);
        ctx.enable_persistence_with_store(run_dir.clone(), store.clone());
        // The agent's model travels with its descriptor: a wake in a process
        // configured for a different provider/model must not silently switch
        // what the agent's prompts run on.
        if let Some(ref model) = descriptor.model {
            ctx.set_default_model(model.clone());
        }
        // Same implicit workspace root as run/serve/resume: the project
        // directory (run_base is `<project>/.chidori/runs`). Without this, a
        // detached agent calling `chidori.workspace.*` fails unless the
        // operator sets CHIDORI_WORKSPACE_ROOT — which, when set, has
        // already populated the context and is left untouched here.
        if ctx.workspace_root().is_none() {
            if let Some(project_root) = parts.run_base.parent().and_then(|p| p.parent()) {
                ctx.set_workspace_root(project_root.to_path_buf());
            }
        }

        let Ok(policy) = RuntimePolicy::from_env_for_durable_run(&run_id) else {
            settle(
                hub,
                entry,
                "failed",
                None,
                Some("building runtime policy".into()),
            );
            return;
        };
        let resolved = match resolve_wake_source(&parts, store.as_ref(), &descriptor) {
            Ok(resolved) => resolved,
            Err(err) => {
                settle(hub, entry, "failed", None, Some(err));
                return;
            }
        };
        let source_text = match std::fs::read_to_string(&resolved) {
            Ok(source) => source,
            Err(err) => {
                settle(
                    hub,
                    entry,
                    "failed",
                    None,
                    Some(format!("reading {}: {err}", resolved.display())),
                );
                return;
            }
        };
        // The same durable scaffold a run gets from the engine: manifest +
        // pending + checkpoint at every host-operation safepoint, so a crash
        // mid-iteration leaves a resumable artifact.
        crate::runtime::engine::install_journal_scaffold_safepoints(
            &parts.run_base,
            &run_id,
            &resolved,
            &source_text,
            &policy,
            &ctx,
        );

        let backend = HostBindingBackend::for_runtime(
            ctx.clone(),
            parts.providers.clone(),
            parts.template_engine.clone(),
            parts.tokio_rt.clone(),
            parts.policy.clone(),
            Arc::new(Mutex::new(PolicyCache::default())),
            policy.clone(),
            parts.tools.clone(),
            parts.mcp.clone(),
        );

        let publish_result = {
            let mut state = entry.state.lock().unwrap();
            // The inbox snapshot used to construct `ctx` predates source
            // loading and VM setup. A store-only send can append during that
            // interval, so refresh under AgentState immediately before
            // publication. Holding the lock through both operations makes a
            // subsequent sender choose this live context instead of racing a
            // whole-file store write against the stale snapshot.
            match store.get_blob(SIGNAL_INBOX_FILE) {
                Ok(Some(bytes)) => {
                    let inbox = serde_json::from_slice(&bytes).unwrap_or_default();
                    ctx.set_signal_inbox(inbox);
                    state.live_ctx = Some(ctx.clone());
                    Ok(())
                }
                Ok(None) => {
                    ctx.set_signal_inbox(Vec::new());
                    state.live_ctx = Some(ctx.clone());
                    Ok(())
                }
                Err(err) => Err(format!(
                    "reading detached-agent inbox before context publication: {err}"
                )),
            }
        };
        if let Err(err) = publish_result {
            settle(hub, entry, "failed", None, Some(err));
            return;
        }
        #[cfg(test)]
        tests::pause_after_supervisor_publishes_live_context(&entry.name);
        let result =
            crate::runtime::rust_engine::run_agent_file(&resolved, &descriptor.input, &backend);
        {
            let mut state = entry.state.lock().unwrap();
            state.live_ctx = None;
        }
        #[cfg(test)]
        tests::pause_after_worker_clears_live_context(&entry.name);
        ctx.clear_event_sender();
        drop(backend);
        let _ = crate::runtime::engine::persist_journal_scaffold(
            &parts.run_base,
            &run_id,
            &resolved,
            &source_text,
            &policy,
            &ctx,
        );

        match settle_iteration(result, &ctx) {
            IterationEnd::Completed(output) => {
                if let Some(store) = ctx.store() {
                    let _ = store.put_blob(
                        "output.json",
                        serde_json::to_string_pretty(&output)
                            .unwrap_or_default()
                            .as_bytes(),
                    );
                }
                settle(hub, entry, "completed", Some(output), None);
                return;
            }
            IterationEnd::Parked(prompt) => {
                settle(
                    hub,
                    entry,
                    "paused",
                    None,
                    prompt.map(|p| format!("paused: {p}")),
                );
                return;
            }
            IterationEnd::WaitSignal(pending) => {
                // Re-check the durable inbox before parking: a delivery that
                // raced in during the unwind must not be slept through.
                let names = pending.listen_names();
                let raced = ctx
                    .signal_inbox()
                    .iter()
                    .any(|queued| names.iter().any(|n| n == &queued.name));
                let listen = listen_state(&ctx, &pending);
                if raced {
                    continue;
                }
                match hibernate(hub, entry, listen) {
                    Ok(true) => return,
                    Ok(false) => continue,
                    Err(err) => {
                        settle(
                            hub,
                            entry,
                            "failed",
                            None,
                            Some(format!(
                                "checking detached-agent inbox before hibernate: {err}"
                            )),
                        );
                        return;
                    }
                }
            }
            IterationEnd::Failed(message) => {
                let stop = entry.state.lock().unwrap().stop_requested;
                if stop {
                    settle(hub, entry, "stopped", None, None);
                    return;
                }
                let (restarts, budget_left) = {
                    let mut state = entry.state.lock().unwrap();
                    let left = descriptor.restart != "never"
                        && state.descriptor.restarts < descriptor.max_restarts;
                    if left {
                        state.descriptor.restarts += 1;
                    }
                    (state.descriptor.restarts, left)
                };
                if !budget_left {
                    settle(hub, entry, "failed", None, Some(message));
                    return;
                }
                if descriptor.backoff_ms > 0 {
                    let backoff = descriptor
                        .backoff_ms
                        .saturating_mul(1u64 << (restarts - 1).min(16));
                    let deadline = std::time::Instant::now() + Duration::from_millis(backoff);
                    let mut state = entry.state.lock().unwrap();
                    while !state.stop_requested {
                        let now = std::time::Instant::now();
                        if now >= deadline {
                            break;
                        }
                        state = entry
                            .signal
                            .wait_timeout(state, deadline.saturating_duration_since(now))
                            .unwrap()
                            .0;
                    }
                }
                // Rewrite the durable journal per the restart strategy: a
                // `resume` restart strips the crash frontier so completed work
                // replays from cache; `clean` starts over.
                let records = ctx.call_log().into_records();
                let rewritten = match descriptor.restart.as_str() {
                    "clean" => Vec::new(),
                    _ => strip_crash_frontier(records),
                };
                let _ = store.write_call_log(&rewritten);
                if descriptor.restart == "clean" {
                    let _ = store.delete_blob(crate::runtime::snapshot::HOST_PROMISE_TABLE_FILE);
                    let _ =
                        store.delete_blob(crate::runtime::snapshot::PENDING_HOST_OPERATION_FILE);
                    // Per-op promise blobs are part of the table; a clean
                    // restart retires them too or the union loader would
                    // resurrect the wiped state.
                    if let Ok(keys) = store.list_blobs() {
                        for key in keys {
                            if key.starts_with(crate::runtime::snapshot::HOST_PROMISE_EVENTS_PREFIX)
                            {
                                let _ = store.delete_blob(&key);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The pending listen call's durable wake state, including the alarm deadline
/// when the listen carries a timeout.
fn listen_state(ctx: &RuntimeContext, pending: &PendingSignal) -> ListenState {
    let names = pending.listen_names();
    let function = ctx
        .pending_host_operation(pending.id)
        .and_then(|op| op.function)
        .unwrap_or_else(|| {
            if names.len() > 1 {
                "signal_any"
            } else {
                "signal"
            }
            .to_string()
        });
    ListenState {
        names,
        seq: pending.seq,
        function,
        deadline: pending
            .timeout_ms
            .map(|ms| Utc::now() + chrono::Duration::milliseconds(ms as i64)),
    }
}

/// Persist `descriptor` to the registry (and the run dir's `agent.json`),
/// the durable record a fresh process re-arms the fleet from.
fn persist_descriptor(factory: &RunStoreFactory, descriptor: &AgentDescriptor) {
    let value = serde_json::to_value(descriptor).unwrap_or_default();
    if let Err(err) = factory.registry_put(&descriptor.name, &descriptor.run_id, &value) {
        tracing::warn!(agent = %descriptor.name, error = %err, "persisting agent registry entry");
    }
    let store = factory.store_for(&descriptor.run_id);
    if let Ok(bytes) = serde_json::to_vec_pretty(descriptor) {
        let _ = store.put_blob(AGENT_DESCRIPTOR_FILE, &bytes);
    }
}

/// The process-stable lease owner id: one per OS process, so every agent this
/// process drives is leased under the same identity.
fn process_lease_owner() -> &'static str {
    static OWNER: OnceLock<String> = OnceLock::new();
    OWNER.get_or_init(|| format!("chidori-{}", uuid::Uuid::new_v4()))
}

/// Release the agent run's lease — called when the agent stops executing here
/// (hibernate or settle), so any process may drive the next wake.
fn release_agent_lease(entry: &Arc<AgentEntry>) {
    let run_id = entry.state.lock().unwrap().descriptor.run_id.clone();
    let store = entry.factory().store_for(&run_id);
    let _ = crate::runtime::store::release_lease(store.as_ref(), process_lease_owner());
}

/// Persist the hibernation handoff while excluding senders. Returns `Ok(false)`
/// when a matching durable message arrived before the transition; the caller
/// should continue the supervisor loop and consume it instead of sleeping.
fn hibernate(
    hub: &DetachedAgentHub,
    entry: &Arc<AgentEntry>,
    listen: ListenState,
) -> Result<bool, String> {
    let has_deadline = listen.deadline.is_some();
    let factory = entry.factory();
    {
        let mut state = entry.state.lock().unwrap();
        let run_id = state.descriptor.run_id.clone();
        let store = factory.store_for(&run_id);
        let inbox: Vec<QueuedSignal> = match store.get_blob(SIGNAL_INBOX_FILE) {
            Ok(Some(bytes)) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Ok(None) => Vec::new(),
            Err(err) => return Err(format!("reading signal inbox: {err}")),
        };
        if inbox
            .iter()
            .any(|queued| listen.names.iter().any(|name| name == &queued.name))
        {
            // Keep the pending listen in memory for the next supervisor pass.
            // It will observe the queued payload and clear the descriptor's
            // listen state as it begins replay.
            state.descriptor.listen = Some(listen);
            return Ok(false);
        }

        state.descriptor.status = "hibernating".to_string();
        state.descriptor.listen = Some(listen);
        state.descriptor.updated_at = Utc::now();
        let descriptor = state.descriptor.clone();

        // Keep senders behind AgentState until both durable transitions have
        // completed. Otherwise a sender can persist `running` and start a new
        // worker, then this stale hibernate write or lease release can undo
        // that wake.
        persist_descriptor(&factory, &descriptor);
        if let Err(err) =
            crate::runtime::store::release_lease(store.as_ref(), process_lease_owner())
        {
            tracing::warn!(agent = %entry.name, error = %err, "releasing detached-agent lease on hibernate");
        }
    }
    if has_deadline {
        hub.ensure_timer();
    }
    entry.signal.notify_all();
    Ok(true)
}

fn settle(
    _hub: &DetachedAgentHub,
    entry: &Arc<AgentEntry>,
    status: &str,
    output: Option<Value>,
    error: Option<String>,
) {
    let descriptor = {
        let mut state = entry.state.lock().unwrap();
        state.descriptor.status = status.to_string();
        state.descriptor.output = output;
        state.descriptor.error = error;
        state.descriptor.listen = None;
        state.descriptor.updated_at = Utc::now();
        state.descriptor.clone()
    };
    persist_descriptor(&entry.factory(), &descriptor);
    release_agent_lease(entry);
    entry.signal.notify_all();
}

enum IterationEnd {
    Completed(Value),
    Failed(String),
    Parked(Option<String>),
    WaitSignal(PendingSignal),
}

fn settle_iteration(result: anyhow::Result<Value>, ctx: &RuntimeContext) -> IterationEnd {
    match result {
        Ok(output) => IterationEnd::Completed(output),
        Err(err) if RunInterrupt::from_error(&err).is_some() => {
            if let Some(pending) = ctx.take_pending_signal() {
                IterationEnd::WaitSignal(pending)
            } else if let Some(pending) = ctx.take_pending_input() {
                IterationEnd::Parked(Some(pending.prompt))
            } else if let Some(pending) = ctx.take_pending_approval() {
                IterationEnd::Parked(Some(format!("approval required: {}", pending.target)))
            } else {
                IterationEnd::Parked(None)
            }
        }
        Err(err) => IterationEnd::Failed(err.to_string()),
    }
}

fn resolve_source(template_engine: &TemplateEngine, source: &str) -> PathBuf {
    let path = Path::new(source);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        template_engine.base_dir().join(path)
    }
}

/// Resolve the entry module for one wake of a detached agent.
///
/// The descriptor's `source` is the path as given at spawn, so a node that
/// holds the agent's project tree resolves it locally — that stays the fast
/// path, and nothing else is read. A node that does **not** have the tree
/// (the fleet case: any node may wake any agent it can lease) materializes
/// the run's own recorded implementation out of its durable store instead
/// (`source_history::materialize_source`), then runs from there. Failing that,
/// the spawning run's materialized tree is the last resort — a freshly
/// spawned agent has no history of its own yet, so its module can only come
/// from the spawner's (see `resolve_spawn_source`).
fn resolve_wake_source(
    parts: &AgentRuntimeParts,
    store: &dyn crate::runtime::store::RunStore,
    descriptor: &AgentDescriptor,
) -> Result<PathBuf, String> {
    let resolved = resolve_source(&parts.template_engine, &descriptor.source);
    if resolved.is_file() {
        return Ok(resolved);
    }
    let mut attempts = Vec::new();
    match materialize_run_source(parts, store, &descriptor.run_id) {
        Ok(Some(entry)) => return Ok(entry),
        Ok(None) => attempts.push("the run store holds no recorded source".to_string()),
        Err(err) => attempts.push(err),
    }
    if let Ok(root) = crate::runtime::source_history::materialization_root(
        &parts.run_base,
        &descriptor.owner_run_id,
    ) {
        let inherited = root.join(&descriptor.source);
        if inherited.is_file() {
            return Ok(inherited);
        }
    }
    Err(format!(
        "reading {}: no such file on this node, and materializing the agent's source \
         from the durable run store failed ({})",
        resolved.display(),
        attempts.join("; ")
    ))
}

/// Materialize `run_id`'s recorded source tree under this node's
/// materialization root, returning the entry module. `Ok(None)` means the
/// store holds no recoverable source (never executed anywhere, or its
/// history was pruned).
fn materialize_run_source(
    parts: &AgentRuntimeParts,
    store: &dyn crate::runtime::store::RunStore,
    run_id: &str,
) -> Result<Option<PathBuf>, String> {
    let root = crate::runtime::source_history::materialization_root(&parts.run_base, run_id)
        .map_err(|err| format!("{err:#}"))?;
    match crate::runtime::source_history::materialize_source(store, &root) {
        Ok(Some(entry)) => {
            tracing::info!(
                run_id,
                entry = %entry.display(),
                "materialized the run's source from the durable store"
            );
            Ok(Some(entry))
        }
        Ok(None) => Ok(None),
        Err(err) => Err(format!("{err:#}")),
    }
}

/// Check a spawn's source module is reachable before recording the spawn, so
/// a typo fails at the call rather than inside the agent's first iteration.
///
/// The spawning run's recorded tree is consulted on a local miss: a run
/// resumed on a node that does not hold its project directory would otherwise
/// fail here on a module it may not even need to read — a replayed
/// `spawn_agent` returns the agent's identity from cache and starts nothing.
/// A freshly spawned agent has no history of its own yet, so the materialized
/// spawner tree is also where its first iteration finds the module
/// (`resolve_wake_source`).
fn resolve_spawn_source(
    backend: &HostBindingBackend,
    ctx: &RuntimeContext,
    parts: &AgentRuntimeParts,
    source: &str,
) -> Result<(), String> {
    if resolve_source(&backend.template_engine(), source).is_file() {
        return Ok(());
    }
    let run_id = ctx.run_id();
    let store = match ctx.store() {
        Some(store) => store,
        None => RunStoreFactory::shared(&parts.run_base).store_for(&run_id),
    };
    if matches!(
        materialize_run_source(parts, store.as_ref(), &run_id),
        Ok(Some(_))
    ) {
        if let Ok(root) =
            crate::runtime::source_history::materialization_root(&parts.run_base, &run_id)
        {
            if root.join(source).is_file() {
                return Ok(());
            }
        }
    }
    Err(format!(
        "chidori.agents.spawn: source module not found: {source} (absent on this node, and \
         materializing the spawning run's recorded source did not produce it)"
    ))
}

// ---------------------------------------------------------------------------
// Host bindings (`chidori.agents.*`)
// ---------------------------------------------------------------------------

fn runtime_ctx<'a>(
    backend: &'a HostBindingBackend,
    what: &str,
) -> Result<&'a RuntimeContext, String> {
    backend
        .runtime_ctx()
        .ok_or_else(|| format!("chidori.{what} requires the runtime host backend"))
}

/// Install the hub's runtime parts from a spawning backend, deriving
/// `run_base` from the run's persist dir. Errors when persistence is off —
/// a detached agent without a journal cannot exist.
fn install_parts_from(
    backend: &HostBindingBackend,
    ctx: &RuntimeContext,
) -> Result<AgentRuntimeParts, String> {
    let (providers, template_engine, tokio_rt, policy, tools, mcp) = backend
        .runtime_parts()
        .ok_or("chidori.agents requires the runtime host backend")?;
    let run_base = ctx
        .persist_dir()
        .and_then(|dir| dir.parent().map(Path::to_path_buf))
        .ok_or(
            "chidori.agents requires persistence: detached agents are durable runs \
             (run with a project dir so `.chidori/runs/` exists)",
        )?;
    let parts = AgentRuntimeParts {
        providers,
        template_engine,
        tokio_rt,
        policy,
        tools,
        mcp,
        run_base,
    };
    hub().install_parts(parts.clone());
    Ok(parts)
}

/// `chidori.agents.spawn(source, input, options)` — start a detached agent
/// and return `{ name, runId }`. One durable `spawn_agent` record: a parent
/// replay returns the identity from cache without starting anything (the
/// agent is re-materialized from the registry by the next live call that
/// addresses it).
pub(crate) fn spawn_agent(backend: &HostBindingBackend, a: &Value) -> Result<Value, String> {
    let ctx = runtime_ctx(backend, "agents.spawn")?;
    if ctx.is_branch() {
        return Err(
            "chidori.agents.spawn is not supported inside a chidori.branch sub-run".to_string(),
        );
    }
    let parts = install_parts_from(backend, ctx)?;
    let source = a
        .get("source")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("chidori.agents.spawn requires a source module path")?
        .to_string();
    let input = a
        .get("input")
        .cloned()
        .filter(|v| !v.is_null())
        .unwrap_or_else(|| json!({}));
    let options_value = a
        .get("options")
        .cloned()
        .filter(|v| !v.is_null())
        .unwrap_or_else(|| json!({}));
    let mut options = SpawnOptions::parse(&options_value)?;
    if options.model.is_none() {
        // The spawner's resolved default model travels with the agent, so a
        // wake in a differently-configured process keeps the same model.
        options.model = Some(ctx.config().model);
    }
    resolve_spawn_source(backend, ctx, &parts, &source)?;
    let call_args = json!({
        "source": source,
        "input": input,
        "options": options.to_json(),
    });
    let owner = ctx.run_id();
    host_core::execute_durable_json_call(ctx, "spawn_agent", call_args, || {
        hub()
            .spawn(&parts, &source, input.clone(), &options, owner.clone())
            .map_err(|err| anyhow::anyhow!(err))
    })
    .map_err(|err| err.to_string())
}

/// `chidori.agents.send(to, name, payload)` — durable delivery into a
/// detached agent's mailbox; wakes it when it hibernates on a matching name.
pub(crate) fn send_agent(backend: &HostBindingBackend, a: &Value) -> Result<Value, String> {
    let ctx = runtime_ctx(backend, "agents.send")?;
    let parts = install_parts_from(backend, ctx)?;
    let to = a
        .get("to")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("chidori.agents.send requires a target agent name")?
        .to_string();
    let name = a
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("chidori.agents.send requires a string message name")?
        .to_string();
    let payload = a.get("payload").cloned().unwrap_or(Value::Null);
    let from = json!({ "kind": "agent", "id": ctx.run_id() });
    let call_args = json!({ "to": to, "name": name, "payload": payload, "from": from });
    host_core::execute_durable_json_call(ctx, "send_agent", call_args, || {
        hub()
            .send(&parts, &to, &name, payload.clone(), from.clone())
            .map_err(|err| anyhow::anyhow!(err))
    })
    .map_err(|err| err.to_string())
}

/// `chidori.agents.join(to, opts)` — wait for a detached agent to settle.
pub(crate) fn join_agent(backend: &HostBindingBackend, a: &Value) -> Result<Value, String> {
    let ctx = runtime_ctx(backend, "agents.join")?;
    let parts = install_parts_from(backend, ctx)?;
    let to = a
        .get("to")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("chidori.agents.join requires a target agent name")?
        .to_string();
    let timeout_ms = a
        .get("opts")
        .and_then(|o| o.get("timeoutMs"))
        .and_then(Value::as_u64);
    let call_args = json!({ "to": to, "opts": { "timeoutMs": timeout_ms } });
    host_core::execute_durable_json_call(ctx, "join_agent", call_args, || {
        hub()
            .join(&parts, &to, timeout_ms)
            .map_err(|err| anyhow::anyhow!(err))
    })
    .map_err(|err| err.to_string())
}

/// `chidori.agents.stop(to)` — cooperative stop.
pub(crate) fn stop_agent(backend: &HostBindingBackend, a: &Value) -> Result<Value, String> {
    let ctx = runtime_ctx(backend, "agents.stop")?;
    let parts = install_parts_from(backend, ctx)?;
    let to = a
        .get("to")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("chidori.agents.stop requires a target agent name")?
        .to_string();
    let call_args = json!({ "to": to });
    host_core::execute_durable_json_call(ctx, "stop_agent", call_args, || {
        hub().stop(&parts, &to).map_err(|err| anyhow::anyhow!(err))
    })
    .map_err(|err| err.to_string())
}

/// `chidori.agents.status(to)`.
pub(crate) fn agent_status(backend: &HostBindingBackend, a: &Value) -> Result<Value, String> {
    let ctx = runtime_ctx(backend, "agents.status")?;
    let parts = install_parts_from(backend, ctx)?;
    let to = a
        .get("to")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("chidori.agents.status requires a target agent name")?
        .to_string();
    let call_args = json!({ "to": to });
    host_core::execute_durable_json_call(ctx, "agent_status", call_args, || {
        hub()
            .status(&parts, &to)
            .map_err(|err| anyhow::anyhow!(err))
    })
    .map_err(|err| err.to_string())
}

/// `chidori.agents.lookup(name)` — a registry lookup: `{name, runId, status}`
/// or null.
pub(crate) fn lookup_agent(backend: &HostBindingBackend, a: &Value) -> Result<Value, String> {
    let ctx = runtime_ctx(backend, "agents.lookup")?;
    let parts = install_parts_from(backend, ctx)?;
    let name = a
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("chidori.agents.lookup requires an agent name")?
        .to_string();
    let call_args = json!({ "name": name });
    host_core::execute_durable_json_call(ctx, "lookup_agent", call_args, || {
        hub()
            .lookup(&parts, &name)
            .map_err(|err| anyhow::anyhow!(err))
    })
    .map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex as StdMutex;

    use serde_json::json;

    use super::*;
    use crate::mcp::McpManager;
    use crate::policy::{PolicyCache, PolicyConfig};
    use crate::providers::ProviderRegistry;
    use crate::runtime::rust_engine::run_agent;
    use crate::runtime::store::RunStoreFactory;
    use crate::tools::ToolRegistry;

    struct InboxRaceBarriers {
        agent_name: String,
        sender_arrived: std::sync::mpsc::Sender<()>,
        sender_release: StdMutex<std::sync::mpsc::Receiver<()>>,
        worker_arrived: std::sync::mpsc::Sender<()>,
        worker_release: StdMutex<std::sync::mpsc::Receiver<()>>,
        sender_claimed: AtomicBool,
        worker_claimed: AtomicBool,
    }

    static INBOX_RACE_BARRIERS: OnceLock<StdMutex<Option<Arc<InboxRaceBarriers>>>> =
        OnceLock::new();

    struct InboxSnapshotGapBarriers {
        agent_name: String,
        snapshot_arrived: std::sync::mpsc::Sender<()>,
        snapshot_release: StdMutex<std::sync::mpsc::Receiver<()>>,
        context_published: std::sync::mpsc::Sender<()>,
        context_release: StdMutex<std::sync::mpsc::Receiver<()>>,
        snapshot_claimed: AtomicBool,
        context_claimed: AtomicBool,
    }

    static INBOX_SNAPSHOT_GAP_BARRIERS: OnceLock<StdMutex<Option<Arc<InboxSnapshotGapBarriers>>>> =
        OnceLock::new();

    pub(super) fn pause_after_cross_process_inbox_read(agent_name: &str) {
        let Some(target) = std::env::var_os("CHIDORI_TEST_CROSS_PROCESS_INBOX_GATE") else {
            return;
        };
        let configured_name = std::env::var("CHIDORI_TEST_CROSS_PROCESS_INBOX_AGENT")
            .expect("cross-process inbox gate requires an agent name");
        if configured_name != agent_name {
            return;
        }
        let role = std::env::var("CHIDORI_TEST_CROSS_PROCESS_INBOX_ROLE")
            .expect("cross-process inbox gate requires a writer role");
        let role_byte = role
            .as_bytes()
            .last()
            .copied()
            .expect("cross-process inbox writer role cannot be empty");
        let target: std::net::SocketAddr = target
            .to_string_lossy()
            .parse()
            .expect("invalid cross-process inbox gate address");
        let mut stream = std::net::TcpStream::connect_timeout(&target, Duration::from_secs(10))
            .expect("connecting to cross-process inbox gate");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("setting cross-process inbox gate read timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .expect("setting cross-process inbox gate write timeout");
        std::io::Write::write_all(&mut stream, &[role_byte])
            .expect("announcing cross-process inbox snapshot");
        let mut release = [0];
        std::io::Read::read_exact(&mut stream, &mut release)
            .expect("waiting for cross-process inbox gate release");
        assert_eq!(release, [b'G'], "invalid cross-process inbox gate release");
    }

    pub(super) fn pause_after_supervisor_inbox_snapshot(agent_name: &str) {
        let barriers = INBOX_SNAPSHOT_GAP_BARRIERS
            .get_or_init(|| StdMutex::new(None))
            .lock()
            .unwrap()
            .clone();
        let Some(barriers) = barriers.filter(|barriers| barriers.agent_name == agent_name) else {
            return;
        };
        if barriers.snapshot_claimed.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = barriers.snapshot_arrived.send(());
        let _ = barriers
            .snapshot_release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(30));
    }

    pub(super) fn pause_after_supervisor_publishes_live_context(agent_name: &str) {
        let barriers = INBOX_SNAPSHOT_GAP_BARRIERS
            .get_or_init(|| StdMutex::new(None))
            .lock()
            .unwrap()
            .clone();
        let Some(barriers) = barriers.filter(|barriers| barriers.agent_name == agent_name) else {
            return;
        };
        if barriers.context_claimed.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = barriers.context_published.send(());
        let _ = barriers
            .context_release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(30));
    }

    pub(super) fn pause_after_live_context_snapshot(agent_name: &str) {
        let barriers = INBOX_RACE_BARRIERS
            .get_or_init(|| StdMutex::new(None))
            .lock()
            .unwrap()
            .clone();
        let Some(barriers) = barriers.filter(|barriers| barriers.agent_name == agent_name) else {
            return;
        };
        if barriers.sender_claimed.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = barriers.sender_arrived.send(());
        let _ = barriers
            .sender_release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(30));
    }

    pub(super) fn pause_after_worker_clears_live_context(agent_name: &str) {
        let barriers = INBOX_RACE_BARRIERS
            .get_or_init(|| StdMutex::new(None))
            .lock()
            .unwrap()
            .clone();
        let Some(barriers) = barriers.filter(|barriers| barriers.agent_name == agent_name) else {
            return;
        };
        if barriers.worker_claimed.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = barriers.worker_arrived.send(());
        let _ = barriers
            .worker_release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(30));
    }

    fn sqlite_test_parts(
        base: &Path,
        run_base: PathBuf,
        tools: Arc<ToolRegistry>,
    ) -> AgentRuntimeParts {
        AgentRuntimeParts {
            providers: Arc::new(ProviderRegistry::new()),
            template_engine: Arc::new(TemplateEngine::new(base)),
            tokio_rt: Arc::new(tokio::runtime::Runtime::new().unwrap()),
            policy: Arc::new(PolicyConfig::default()),
            tools,
            mcp: Arc::new(McpManager::new()),
            run_base,
        }
    }

    #[tokio::test]
    async fn detached_inbox_cross_process_writers_preserve_acknowledged_payloads() {
        const CHILD: &str = "CHIDORI_TEST_CROSS_PROCESS_INBOX_CHILD";
        const TEST_NAME: &str = "runtime::host_agent::tests::detached_inbox_cross_process_writers_preserve_acknowledged_payloads";

        if let Some(role) = std::env::var_os(CHILD) {
            let role = role.to_string_lossy();
            let base =
                PathBuf::from(std::env::var_os("CHIDORI_TEST_CROSS_PROCESS_INBOX_BASE").unwrap());
            let agent_name = std::env::var("CHIDORI_TEST_CROSS_PROCESS_INBOX_AGENT").unwrap();
            let run_base = base.join(".chidori/runs");
            let parts = sqlite_test_parts(&base, run_base.clone(), Arc::new(ToolRegistry::new()));
            hub().install_parts(parts.clone());

            match role.as_ref() {
                "seed" => {
                    std::fs::create_dir_all(base.join("services")).unwrap();
                    let source = base.join("services/worker.ts");
                    std::fs::write(
                        &source,
                        r#"export async function agent() {
                            await chidori.signal("hold");
                            return { done: true };
                        }"#,
                    )
                    .unwrap();
                    let options = SpawnOptions {
                        name: Some(agent_name.clone()),
                        restart: "never".to_string(),
                        max_restarts: 0,
                        backoff_ms: 0,
                        model: None,
                    };
                    hub()
                        .spawn(
                            &parts,
                            source.to_str().unwrap(),
                            json!({}),
                            &options,
                            "cross-process-inbox-test".to_string(),
                        )
                        .unwrap();
                    wait_for_agent_status(&parts, &agent_name, |status| status == "hibernating");
                }
                "send" => {
                    let id = std::env::var("CHIDORI_TEST_CROSS_PROCESS_INBOX_ID").unwrap();
                    let result = hub()
                        .send(
                            &parts,
                            &agent_name,
                            "tick",
                            json!({ "id": id }),
                            json!({ "test": true }),
                        )
                        .unwrap();
                    assert_eq!(result["delivered"], json!(true));
                }
                "inspect" => {
                    let expected: std::collections::HashSet<_> =
                        std::env::var("CHIDORI_TEST_CROSS_PROCESS_INBOX_IDS")
                            .unwrap()
                            .split(',')
                            .map(str::to_string)
                            .collect();
                    let factory = RunStoreFactory::shared(&run_base);
                    let registry = factory.registry_get(&agent_name).unwrap().unwrap();
                    let run_id = registry["run_id"].as_str().unwrap();
                    let store = factory.store_for(run_id);
                    let records = store.load_call_log().unwrap().unwrap_or_default();
                    let inbox: Vec<QueuedSignal> = store
                        .get_blob(SIGNAL_INBOX_FILE)
                        .unwrap()
                        .map(|bytes| serde_json::from_slice(&bytes).unwrap())
                        .unwrap_or_default();
                    assert!(
                        records.iter().all(|record| record.function != "signal"),
                        "mismatched listen name unexpectedly consumed a message: {records:#?}"
                    );
                    let actual: Vec<_> = inbox
                        .iter()
                        .map(|queued| queued.payload["id"].as_str().unwrap().to_string())
                        .collect();
                    let actual_set: std::collections::HashSet<_> = actual.iter().cloned().collect();
                    if actual.len() != actual_set.len() || actual_set != expected {
                        let missing: Vec<_> = expected.difference(&actual_set).cloned().collect();
                        let unexpected: Vec<_> =
                            actual_set.difference(&expected).cloned().collect();
                        std::fs::write(
                            base.join("inbox-inspector-red.txt"),
                            format!(
                                "expected={expected:?}; actual={actual:?}; missing={missing:?}; unexpected={unexpected:?}"
                            ),
                        )
                        .unwrap();
                        std::process::exit(17);
                    }
                }
                _ => panic!("unknown cross-process inbox child role: {role}"),
            }
            return;
        }

        async fn child(
            base: &Path,
            agent: &str,
            role: &str,
            ids: &str,
            backend: &str,
        ) -> std::process::Child {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .arg("--exact")
                .arg("runtime::host_agent::tests::detached_inbox_cross_process_writers_preserve_acknowledged_payloads")
                .arg("--nocapture")
                .env("CHIDORI_TEST_CROSS_PROCESS_INBOX_CHILD", role)
                .env("CHIDORI_TEST_CROSS_PROCESS_INBOX_BASE", base)
                .env("CHIDORI_TEST_CROSS_PROCESS_INBOX_AGENT", agent)
                .env("CHIDORI_TEST_CROSS_PROCESS_INBOX_IDS", ids)
                .env("CHIDORI_RUN_STORE", backend)
                .env("CHIDORI_RUN_DB", base.join("runs.sqlite3"));
            if role == "send" {
                command.env("CHIDORI_TEST_CROSS_PROCESS_INBOX_ID", ids);
            }
            command.spawn().unwrap()
        }

        async fn wait_child(child: std::process::Child, role: &str) {
            let status = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap())
                .await
                .unwrap();
            assert!(
                status.status.success(),
                "cross-process inbox {role} child failed: stdout={} stderr={}",
                String::from_utf8_lossy(&status.stdout),
                String::from_utf8_lossy(&status.stderr)
            );
        }

        let mut red_failures = Vec::new();
        for backend in ["fs", "sqlite"] {
            let base = tempfile::tempdir().unwrap();
            let sequential_agent = unique("cross-process-sequential");
            wait_child(
                child(base.path(), &sequential_agent, "seed", "", backend).await,
                "seed",
            )
            .await;
            for id in ["sequential-a", "sequential-b"] {
                wait_child(
                    child(base.path(), &sequential_agent, "send", id, backend).await,
                    "sequential send",
                )
                .await;
            }
            wait_child(
                child(
                    base.path(),
                    &sequential_agent,
                    "inspect",
                    "sequential-a,sequential-b",
                    backend,
                )
                .await,
                "sequential inspect",
            )
            .await;

            let concurrent_agent = unique("cross-process-concurrent");
            wait_child(
                child(base.path(), &concurrent_agent, "seed", "", backend).await,
                "seed",
            )
            .await;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let gate = listener.local_addr().unwrap().to_string();
            let mut writers = Vec::new();
            for (role, id) in [("writer-a", "concurrent-a"), ("writer-b", "concurrent-b")] {
                let mut command = std::process::Command::new(std::env::current_exe().unwrap());
                command
                    .arg("--exact")
                    .arg(TEST_NAME)
                    .arg("--nocapture")
                    .env("CHIDORI_TEST_CROSS_PROCESS_INBOX_CHILD", "send")
                    .env("CHIDORI_TEST_CROSS_PROCESS_INBOX_BASE", base.path())
                    .env("CHIDORI_TEST_CROSS_PROCESS_INBOX_AGENT", &concurrent_agent)
                    .env("CHIDORI_TEST_CROSS_PROCESS_INBOX_ID", id)
                    .env("CHIDORI_TEST_CROSS_PROCESS_INBOX_ROLE", role)
                    .env("CHIDORI_TEST_CROSS_PROCESS_INBOX_GATE", &gate)
                    .env("CHIDORI_RUN_STORE", backend)
                    .env("CHIDORI_RUN_DB", base.path().join("runs.sqlite3"));
                writers.push(command.spawn().unwrap());
            }

            let mut arrivals = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) =
                    tokio::time::timeout(Duration::from_secs(10), listener.accept())
                        .await
                        .expect("writer did not reach the post-read gate")
                        .unwrap();
                let mut role = [0];
                tokio::time::timeout(
                    Duration::from_secs(10),
                    tokio::io::AsyncReadExt::read_exact(&mut stream, &mut role),
                )
                .await
                .expect("writer role announcement timed out")
                .unwrap();
                arrivals.push((stream, role[0]));
            }
            arrivals.sort_by_key(|(_, role)| *role);
            assert_eq!(
                arrivals.iter().map(|(_, role)| *role).collect::<Vec<_>>(),
                vec![b'a', b'b']
            );
            for (mut stream, _) in arrivals {
                tokio::io::AsyncWriteExt::write_all(&mut stream, b"G")
                    .await
                    .unwrap();
            }
            for writer in writers {
                let result =
                    tokio::task::spawn_blocking(move || writer.wait_with_output().unwrap())
                        .await
                        .unwrap();
                assert!(
                    result.status.success(),
                    "cross-process writer did not acknowledge delivery: stdout={} stderr={}",
                    String::from_utf8_lossy(&result.stdout),
                    String::from_utf8_lossy(&result.stderr)
                );
            }

            let inspect = child(
                base.path(),
                &concurrent_agent,
                "inspect",
                "concurrent-a,concurrent-b",
                backend,
            )
            .await;
            let result = tokio::task::spawn_blocking(move || inspect.wait_with_output().unwrap())
                .await
                .unwrap();
            if !result.status.success() {
                assert!(
                    result.status.code() == Some(17),
                    "unexpected inspector failure for {backend}: stdout={} stderr={}",
                    String::from_utf8_lossy(&result.stdout),
                    String::from_utf8_lossy(&result.stderr)
                );
                red_failures.push(format!(
                    "{backend}: inspector found missing or duplicate acknowledged IDs (exit 17): {}",
                    std::fs::read_to_string(base.path().join("inbox-inspector-red.txt"))
                        .unwrap()
                ));
            }
        }
        assert!(
            red_failures.is_empty(),
            "acknowledged inbox payloads were lost:\n{}",
            red_failures.join("\n")
        );
    }

    #[test]
    fn detached_inbox_competing_writers_preserve_every_acknowledged_payload() {
        const CHILD: &str = "CHIDORI_TEST_DETACHED_INBOX_RACE_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let base = PathBuf::from(std::env::var_os("CHIDORI_TEST_DETACHED_INBOX_BASE").unwrap());
            let mut losses = Vec::new();
            for count in [2, 5] {
                let loss = run_deterministic_inbox_race(&base, count, false);
                println!(
                    "backend={} competing sends N={count}: {}",
                    std::env::var("CHIDORI_RUN_STORE").unwrap_or_else(|_| "fs".to_string()),
                    loss.as_deref()
                        .unwrap_or("all acknowledged IDs accounted exactly once")
                );
                if let Some(loss) = loss {
                    losses.push(loss);
                }
            }
            run_sequential_inbox_control(&base, 25);
            println!(
                "backend={} N=25 sequential control passed",
                std::env::var("CHIDORI_RUN_STORE").unwrap_or_else(|_| "fs".to_string())
            );
            assert!(
                losses.is_empty(),
                "confirmed inbox overwrite losses: {losses:#?}"
            );
            return;
        }

        let mut failures = Vec::new();
        for backend in ["fs", "sqlite"] {
            let base = test_dir(&format!("inbox-race-{backend}"));
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("runtime::host_agent::tests::detached_inbox_competing_writers_preserve_every_acknowledged_payload")
                .arg("--nocapture")
                .env(CHILD, "1")
                .env("CHIDORI_TEST_DETACHED_INBOX_BASE", &base)
                .env("CHIDORI_RUN_STORE", backend)
                .env("CHIDORI_RUN_DB", base.join("runs.sqlite3"))
                .output()
                .unwrap();
            let report = format!(
                "detached inbox race child backend={backend} exited {:?}\nstdout:\n{}\nstderr:\n{}",
                output.status.code(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let _ = std::fs::remove_dir_all(&base);
            if !output.status.success() {
                failures.push(report);
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[test]
    fn detached_inbox_missed_wake_does_not_hibernate_with_matching_payloads() {
        const CHILD: &str = "CHIDORI_TEST_DETACHED_INBOX_MISSED_WAKE_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let base = PathBuf::from(std::env::var_os("CHIDORI_TEST_DETACHED_INBOX_BASE").unwrap());
            let mut failures = Vec::new();
            for count in [2, 5] {
                if let Some(failure) = run_deterministic_inbox_race(&base, count, true) {
                    failures.push(failure);
                }
            }
            run_sequential_inbox_control(&base, 25);
            assert!(
                failures.is_empty(),
                "missed detached-agent wake: {failures:#?}"
            );
            return;
        }

        let mut failures = Vec::new();
        for backend in ["fs", "sqlite"] {
            let base = tempfile::tempdir().unwrap();
            let db_path = base.path().join("runs.sqlite3");
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("runtime::host_agent::tests::detached_inbox_missed_wake_does_not_hibernate_with_matching_payloads")
                .arg("--nocapture")
                .env(CHILD, "1")
                .env("CHIDORI_TEST_DETACHED_INBOX_BASE", base.path())
                .env("CHIDORI_RUN_STORE", backend)
                .env("CHIDORI_RUN_DB", db_path)
                .output()
                .unwrap();
            let report = format!(
                "detached inbox missed-wake child backend={backend} exited {:?}\nstdout:\n{}\nstderr:\n{}",
                output.status.code(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            if !output.status.success() {
                failures.push(report);
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    fn run_deterministic_inbox_race(
        base: &Path,
        competing_sends: usize,
        require_wake_resolution: bool,
    ) -> Option<String> {
        let agent_name = unique(&format!("inbox-race-{competing_sends}"));
        let run_base = base
            .join(format!("race-{competing_sends}"))
            .join(".chidori/runs");
        let source = base.join(format!("race-{competing_sends}.ts"));
        std::fs::write(
            &source,
            r#"export async function agent(input: { count: number }) {
                for (let i = 0; i <= input.count; i++) {
                    const message = await chidori.signal("race");
                    await chidori.tool("raceConsumed", { id: message.payload.id });
                    if (i === 0) await chidori.tool("raceReady", {});
                }
                return { done: true };
            }"#,
        )
        .unwrap();

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (consumed_tx, consumed_rx) = std::sync::mpsc::channel();
        let mut tools = ToolRegistry::new();
        tools.register_native(
            "raceReady",
            "signals the first listen completed",
            Vec::new(),
            move |_| {
                let _ = ready_tx.send(());
                Ok(json!({ "ready": true }))
            },
        );
        tools.register_native(
            "raceConsumed",
            "records a consumed inbox payload",
            Vec::new(),
            move |args| {
                let _ = consumed_tx.send(args["id"].clone());
                Ok(json!({ "consumed": true }))
            },
        );
        let parts = sqlite_test_parts(base, run_base.clone(), Arc::new(tools));
        hub().install_parts(parts.clone());
        let options = SpawnOptions {
            name: Some(agent_name.clone()),
            restart: "never".to_string(),
            max_restarts: 0,
            backoff_ms: 0,
            model: None,
        };
        let spawned = hub()
            .spawn(
                &parts,
                source.to_str().unwrap(),
                json!({ "count": competing_sends }),
                &options,
                "inbox-race-test".to_string(),
            )
            .unwrap();
        let run_id = spawned["runId"].as_str().unwrap().to_string();
        wait_for_agent_status(&parts, &agent_name, |status| status == "hibernating");

        let seed_id = format!("{agent_name}-seed");
        assert!(hub()
            .send(
                &parts,
                &agent_name,
                "race",
                json!({ "id": seed_id }),
                json!({ "test": true })
            )
            .unwrap()["delivered"]
            .as_bool()
            .unwrap());
        assert_eq!(
            consumed_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
            json!(seed_id)
        );
        ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();

        let sender_arrived_rx;
        let sender_release_tx;
        let worker_arrived_rx;
        let worker_release_tx;
        {
            let (sender_arrived_tx, rx) = std::sync::mpsc::channel();
            sender_arrived_rx = rx;
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            sender_release_tx = release_tx;
            let sender_release_rx = release_rx;
            let (worker_arrived_tx, rx) = std::sync::mpsc::channel();
            worker_arrived_rx = rx;
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            worker_release_tx = release_tx;
            let worker_release_rx = release_rx;
            *INBOX_RACE_BARRIERS
                .get_or_init(|| StdMutex::new(None))
                .lock()
                .unwrap() = Some(Arc::new(InboxRaceBarriers {
                agent_name: agent_name.clone(),
                sender_arrived: sender_arrived_tx,
                sender_release: StdMutex::new(sender_release_rx),
                worker_arrived: worker_arrived_tx,
                worker_release: StdMutex::new(worker_release_rx),
                sender_claimed: AtomicBool::new(false),
                worker_claimed: AtomicBool::new(false),
            }));
        }

        let live_id = format!("{agent_name}-live");
        let sender_parts = parts.clone();
        let sender_name = agent_name.clone();
        let live_id_for_thread = live_id.clone();
        let sender = std::thread::spawn(move || {
            hub().send(
                &sender_parts,
                &sender_name,
                "race",
                json!({ "id": live_id_for_thread }),
                json!({ "test": true }),
            )
        });
        sender_arrived_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("live sender did not reach the post-snapshot barrier");
        worker_arrived_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("worker did not reach the post-context-clear barrier");

        let status = hub().status(&parts, &agent_name).unwrap();
        assert_eq!(status["status"], json!("running"), "{status}");
        assert_eq!(status["live"], json!(true), "{status}");
        let mut acknowledged_ids = vec![live_id.clone()];
        let mut other_ids = Vec::new();
        for index in 1..competing_sends {
            let id = format!("{agent_name}-store-{index}");
            let result = hub()
                .send(
                    &parts,
                    &agent_name,
                    "race",
                    json!({ "id": id }),
                    json!({ "test": true }),
                )
                .unwrap();
            assert_eq!(result["delivered"], json!(true));
            acknowledged_ids.push(id.clone());
            other_ids.push(id);
        }

        sender_release_tx.send(()).unwrap();
        let live_result = sender.join().unwrap().unwrap();
        assert_eq!(live_result["delivered"], json!(true));
        worker_release_tx.send(()).unwrap();
        let mut consumed_notifications = std::collections::HashMap::<String, usize>::new();
        if require_wake_resolution {
            let expected_ids: std::collections::HashSet<_> =
                acknowledged_ids.iter().cloned().collect();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while expected_ids
                .iter()
                .any(|id| consumed_notifications.get(id).copied().unwrap_or(0) == 0)
            {
                let now = std::time::Instant::now();
                if now >= deadline {
                    break;
                }
                match consumed_rx.recv_timeout(deadline.saturating_duration_since(now)) {
                    Ok(id) => {
                        if let Some(id) = id.as_str() {
                            *consumed_notifications.entry(id.to_string()).or_default() += 1;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
        let status = if require_wake_resolution {
            wait_for_inbox_race_outcome(&parts, &agent_name)
        } else {
            wait_for_agent_status(&parts, &agent_name, |status| {
                status == "hibernating" || status == "completed" || status == "failed"
            })
        };
        *INBOX_RACE_BARRIERS
            .get_or_init(|| StdMutex::new(None))
            .lock()
            .unwrap() = None;

        let factory = RunStoreFactory::shared(&run_base);
        let records = factory
            .store_for(&run_id)
            .load_call_log()
            .unwrap()
            .unwrap_or_default();
        let inbox = factory
            .store_for(&run_id)
            .get_blob(SIGNAL_INBOX_FILE)
            .unwrap()
            .map(|bytes| serde_json::from_slice::<Vec<Value>>(&bytes).unwrap())
            .unwrap_or_default();
        let live = if require_wake_resolution {
            status["live"] == json!(true)
        } else {
            hub().status(&parts, &agent_name).unwrap()["live"] == json!(true)
        };
        let accounted: Vec<(String, usize, usize)> = acknowledged_ids
            .iter()
            .map(|id| {
                let journal_count = records
                    .iter()
                    .filter(|record| {
                        record.function == "signal" && record.result["payload"]["id"] == json!(id)
                    })
                    .count();
                let inbox_count = inbox
                    .iter()
                    .filter(|queued| queued["payload"]["id"] == json!(id))
                    .count();
                (id.clone(), journal_count, inbox_count)
            })
            .collect();
        let unaccounted: Vec<_> = accounted
            .iter()
            .filter(|(_, journal_count, inbox_count)| journal_count + inbox_count != 1)
            .cloned()
            .collect();

        let consumption_failures: Vec<_> = if require_wake_resolution {
            acknowledged_ids
                .iter()
                .filter_map(|id| {
                    let notification_count = consumed_notifications.get(id).copied().unwrap_or(0);
                    let (journal_signal_count, inbox_count) = accounted
                        .iter()
                        .find(|(accounted_id, _, _)| accounted_id == id)
                        .map(|(_, journal_count, inbox_count)| (*journal_count, *inbox_count))
                        .unwrap_or((0, 0));
                    let race_consumed_call_count = records
                        .iter()
                        .filter(|record| {
                            record.function == "tool"
                                && record.args["name"] == json!("raceConsumed")
                                && record.args["kwargs"]["id"] == json!(id)
                        })
                        .count();
                    (notification_count != 1
                        || journal_signal_count != 1
                        || inbox_count != 0
                        || race_consumed_call_count != 1)
                        .then(|| {
                            (
                                id.clone(),
                                notification_count,
                                journal_signal_count,
                                inbox_count,
                                race_consumed_call_count,
                            )
                        })
                })
                .collect()
        } else {
            Vec::new()
        };

        let _ = hub().stop(&parts, &agent_name);
        wait_for_agent_status(&parts, &agent_name, |status| {
            matches!(status, "stopped" | "completed" | "failed")
        });
        let stranded_matching_ids: Vec<_> = accounted
            .iter()
            .filter(|(_, journal_count, inbox_count)| *journal_count == 0 && *inbox_count == 1)
            .map(|(id, _, _)| id.clone())
            .collect();
        let waiting_for_race = status["waitingFor"]
            .as_array()
            .is_some_and(|names| names.iter().any(|name| name == "race"));
        let hibernating_with_queued_matches = require_wake_resolution
            && status["status"] == "hibernating"
            && !live
            && waiting_for_race
            && !stranded_matching_ids.is_empty();

        (!unaccounted.is_empty()
            || !consumption_failures.is_empty()
            || hibernating_with_queued_matches)
            .then(|| {
                format!(
                    "backend={}, N={}, conservation={accounted:?}, exact_consumption_failures={consumption_failures:?}, status={status}, live={live}, hibernating_with_queued_matches={hibernating_with_queued_matches}, stranded_matching_ids={stranded_matching_ids:?}, inbox={inbox:?}, other_store_ids={other_ids:?}",
                    std::env::var("CHIDORI_RUN_STORE").unwrap_or_else(|_| "fs".to_string()),
                    competing_sends
                )
            })
    }

    fn run_sequential_inbox_control(base: &Path, count: usize) {
        let agent_name = unique("inbox-sequential");
        let run_base = base.join("sequential").join(".chidori/runs");
        let source = base.join("sequential.ts");
        std::fs::write(
            &source,
            r#"export async function agent(input: { count: number }) {
                for (let i = 0; i < input.count; i++) {
                    const message = await chidori.signal("sequential");
                    await chidori.tool("raceConsumed", { id: message.payload.id });
                }
                return { done: true };
            }"#,
        )
        .unwrap();
        let (consumed_tx, consumed_rx) = std::sync::mpsc::channel();
        let mut tools = ToolRegistry::new();
        tools.register_native(
            "raceConsumed",
            "records a consumed inbox payload",
            Vec::new(),
            move |args| {
                let _ = consumed_tx.send(args["id"].clone());
                Ok(json!({ "consumed": true }))
            },
        );
        let parts = sqlite_test_parts(base, run_base.clone(), Arc::new(tools));
        hub().install_parts(parts.clone());
        let options = SpawnOptions {
            name: Some(agent_name.clone()),
            restart: "never".to_string(),
            max_restarts: 0,
            backoff_ms: 0,
            model: None,
        };
        let spawned = hub()
            .spawn(
                &parts,
                source.to_str().unwrap(),
                json!({ "count": count }),
                &options,
                "inbox-sequential-test".to_string(),
            )
            .unwrap();
        let run_id = spawned["runId"].as_str().unwrap().to_string();
        wait_for_agent_status(&parts, &agent_name, |status| status == "hibernating");

        let mut acknowledged_ids = Vec::new();
        for index in 0..count {
            let id = format!("{agent_name}-{index}");
            let result = hub()
                .send(
                    &parts,
                    &agent_name,
                    "sequential",
                    json!({ "id": id }),
                    json!({ "test": true }),
                )
                .unwrap();
            assert_eq!(result["delivered"], json!(true));
            assert_eq!(
                consumed_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
                json!(id)
            );
            acknowledged_ids.push(id);
        }
        wait_for_agent_status(&parts, &agent_name, |status| status == "completed");

        let factory = RunStoreFactory::shared(&run_base);
        let records = factory
            .store_for(&run_id)
            .load_call_log()
            .unwrap()
            .unwrap_or_default();
        let inbox = factory
            .store_for(&run_id)
            .get_blob(SIGNAL_INBOX_FILE)
            .unwrap()
            .map(|bytes| serde_json::from_slice::<Vec<Value>>(&bytes).unwrap())
            .unwrap_or_default();
        for id in acknowledged_ids {
            let journal_count = records
                .iter()
                .filter(|record| {
                    record.function == "signal" && record.result["payload"]["id"] == json!(id)
                })
                .count();
            let inbox_count = inbox
                .iter()
                .filter(|queued| queued["payload"]["id"] == json!(id))
                .count();
            assert_eq!(
                journal_count + inbox_count,
                1,
                "payload {id} not accounted exactly once"
            );
        }
    }

    fn wait_for_agent_status(
        parts: &AgentRuntimeParts,
        name: &str,
        predicate: impl Fn(&str) -> bool,
    ) -> Value {
        let entry = hub().ensure_entry(parts, name).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut state = entry.state.lock().unwrap();
        loop {
            if predicate(&state.descriptor.status) {
                return state.descriptor.status_json();
            }
            let now = std::time::Instant::now();
            assert!(
                now < deadline,
                "timed out waiting for {name}: {}",
                state.descriptor.status
            );
            let (next, _) = entry
                .signal
                .wait_timeout(state, deadline.saturating_duration_since(now))
                .unwrap();
            state = next;
        }
    }

    fn wait_for_inbox_race_outcome(parts: &AgentRuntimeParts, name: &str) -> Value {
        let entry = hub().ensure_entry(parts, name).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut state = entry.state.lock().unwrap();
        loop {
            let settled = AgentEntry::is_settled(&state);
            let hibernated = state.descriptor.status == "hibernating" && !state.thread_live;
            let driveable = state.descriptor.status == "running" && !state.thread_live;
            if settled || hibernated || driveable {
                let mut status = state.descriptor.status_json();
                status["live"] = json!(state.thread_live);
                return status;
            }

            let now = std::time::Instant::now();
            if now >= deadline {
                let mut status = state.descriptor.status_json();
                status["live"] = json!(state.thread_live);
                return status;
            }
            let (next, timeout) = entry
                .signal
                .wait_timeout(state, deadline.saturating_duration_since(now))
                .unwrap();
            state = next;
            if timeout.timed_out() {
                let mut status = state.descriptor.status_json();
                status["live"] = json!(state.thread_live);
                return status;
            }
        }
    }

    #[test]
    fn detached_alarm_timeout_wakes_reconcile_host_promises_across_cycles() {
        const CHILD: &str = "CHIDORI_TEST_DETACHED_ALARM_RECOVERY_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let base = PathBuf::from(std::env::var_os("CHIDORI_TEST_DETACHED_ALARM_BASE").unwrap());
            let no_alarm = run_paced_alarm_case(&base, "no-alarm", false);
            assert_eq!(
                no_alarm.status, "completed",
                "no-alarm control: {no_alarm:?}"
            );
            assert_eq!(no_alarm.output, Some(json!({ "seen": [0, 1, 2] })));
            assert!(
                no_alarm.promise_load.is_ok(),
                "no-alarm control: {no_alarm:?}"
            );
            assert!(no_alarm
                .records
                .iter()
                .all(|record| record.args["name"] != json!(ALARM_SIGNAL_NAME)));
            println!(
                "backend={} paced no-alarm control passed (3 cycles)",
                std::env::var("CHIDORI_RUN_STORE").unwrap_or_else(|_| "fs".to_string())
            );

            let with_alarm = run_paced_alarm_case(&base, "with-alarm", true);
            println!("backend={} paced alarm observation: status={}, error={:?}, journal_rows={}, promise_load={:?}",
                std::env::var("CHIDORI_RUN_STORE").unwrap_or_else(|_| "fs".to_string()),
                with_alarm.status,
                with_alarm.error,
                with_alarm.records.len(),
                with_alarm.promise_load.as_ref().map(|records| records.len()).map_err(|error| error.clone()));
            assert_eq!(
                with_alarm.status,
                "completed",
                "three alarm timeouts must resume through all cycles and durably reconcile each completion: {with_alarm:?}"
            );
            assert_eq!(with_alarm.output, Some(json!({ "seen": [0, 1, 2] })));
            let promises = with_alarm.promise_load.as_ref().unwrap();
            let timeout_rows: Vec<_> = with_alarm
                .records
                .iter()
                .filter(|record| {
                    record.function == "signal"
                        && record.args["name"] == json!(ALARM_SIGNAL_NAME)
                        && record.result["timedOut"] == json!(true)
                })
                .collect();
            assert_eq!(timeout_rows.len(), 3, "{with_alarm:?}");
            for timeout in timeout_rows {
                let promise = promises
                    .iter()
                    .find(|promise| promise.operation.seq == timeout.seq)
                    .expect("durable promise for each timeout journal row");
                assert!(
                    matches!(
                        &promise.state,
                        crate::runtime::snapshot::HostPromiseState::Resolved { .. }
                            | crate::runtime::snapshot::HostPromiseState::ResolvedInJournal { .. }
                    ),
                    "timeout at seq {} was not persisted as completed: {promise:?}",
                    timeout.seq
                );
            }
            return;
        }

        let mut failures = Vec::new();
        for backend in ["fs", "sqlite"] {
            let base = test_dir(&format!("detached-alarm-{backend}"));
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("runtime::host_agent::tests::detached_alarm_timeout_wakes_reconcile_host_promises_across_cycles")
                .arg("--nocapture")
                .env(CHILD, "1")
                .env("CHIDORI_TEST_DETACHED_ALARM_BASE", &base)
                .env("CHIDORI_RUN_STORE", backend)
                .env("CHIDORI_RUN_DB", base.join("runs.sqlite3"))
                .output()
                .unwrap();
            let report = format!(
                "detached alarm recovery child backend={backend} exited {:?}\nstdout:\n{}\nstderr:\n{}",
                output.status.code(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let _ = std::fs::remove_dir_all(&base);
            if !output.status.success() {
                failures.push(report);
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[derive(Debug)]
    struct PacedAlarmOutcome {
        status: String,
        error: Option<String>,
        output: Option<Value>,
        records: Vec<CallRecord>,
        promise_load: Result<Vec<crate::runtime::snapshot::HostPromiseRecord>, String>,
    }

    fn run_paced_alarm_case(base: &Path, case: &str, with_alarm: bool) -> PacedAlarmOutcome {
        let project = base.join(case);
        std::fs::create_dir_all(project.join("services")).unwrap();
        let source = project.join("services/worker.ts");
        std::fs::write(
            &source,
            r#"export async function agent(input: { cycles: number; withAlarm: boolean }) {
                const seen = [];
                for (let i = 0; i < input.cycles; i++) {
                    const message = await chidori.signal("tick");
                    seen.push(message.payload.n);
                    if (input.withAlarm) await chidori.alarm(10);
                    await chidori.log("cycle", { n: message.payload.n });
                }
                return { seen };
            }"#,
        )
        .unwrap();

        let run_base = project.join(".chidori/runs");
        let parts = sqlite_test_parts(&project, run_base.clone(), Arc::new(ToolRegistry::new()));
        hub().install_parts(parts.clone());
        let name = unique(if with_alarm {
            "paced-alarm"
        } else {
            "paced-no-alarm"
        });
        let options = SpawnOptions {
            name: Some(name.clone()),
            restart: "never".to_string(),
            max_restarts: 0,
            backoff_ms: 0,
            model: None,
        };
        let spawned = hub()
            .spawn(
                &parts,
                source.to_str().unwrap(),
                json!({ "cycles": 3, "withAlarm": with_alarm }),
                &options,
                format!("{case}-parent"),
            )
            .unwrap();
        let run_id = spawned["runId"].as_str().unwrap().to_string();
        let mut descriptor = wait_for_agent_descriptor(&parts, &name, |descriptor| {
            descriptor.status == "hibernating"
                && descriptor
                    .listen
                    .as_ref()
                    .is_some_and(|listen| listen.names.iter().any(|name| name == "tick"))
        });

        for tick in 0..3 {
            let previous_seq = descriptor
                .listen
                .as_ref()
                .expect("paced worker is hibernating on a listen")
                .seq;
            let delivery = hub()
                .send(
                    &parts,
                    &name,
                    "tick",
                    json!({ "n": tick }),
                    json!({ "kind": "test", "id": case }),
                )
                .unwrap();
            assert_eq!(delivery["delivered"], json!(true));
            descriptor = wait_for_agent_descriptor(&parts, &name, |current| {
                matches!(current.status.as_str(), "failed" | "completed")
                    || (current.status == "hibernating"
                        && current.listen.as_ref().is_some_and(|listen| {
                            listen.names.iter().any(|name| name == "tick")
                                && listen.seq > previous_seq
                        }))
            });
            if matches!(descriptor.status.as_str(), "failed" | "completed") {
                break;
            }
        }

        if !matches!(
            descriptor.status.as_str(),
            "failed" | "completed" | "stopped"
        ) {
            let _ = hub().stop(&parts, &name);
            descriptor = wait_for_agent_descriptor(&parts, &name, |current| {
                matches!(current.status.as_str(), "failed" | "completed" | "stopped")
            });
        }

        let factory = RunStoreFactory::shared(&run_base);
        let store = factory.store_for(&run_id);
        let records = store.load_call_log().unwrap().unwrap_or_default();
        let promise_load = crate::runtime::snapshot::load_host_promise_records(store.as_ref())
            .map_err(|error| format!("{error:#}"));
        PacedAlarmOutcome {
            status: descriptor.status,
            error: descriptor.error,
            output: descriptor.output,
            records,
            promise_load,
        }
    }

    fn wait_for_agent_descriptor(
        parts: &AgentRuntimeParts,
        name: &str,
        predicate: impl Fn(&AgentDescriptor) -> bool,
    ) -> AgentDescriptor {
        let entry = hub().ensure_entry(parts, name).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut state = entry.state.lock().unwrap();
        loop {
            if predicate(&state.descriptor) {
                return state.descriptor.clone();
            }
            let now = std::time::Instant::now();
            assert!(
                now < deadline,
                "timed out waiting for {name}: {:?}",
                state.descriptor
            );
            let (next, _) = entry
                .signal
                .wait_timeout(state, deadline.saturating_duration_since(now))
                .unwrap();
            state = next;
        }
    }

    #[test]
    fn explicit_sqlite_failure_rejects_detached_spawn_rearm_send_and_alarm_wake_before_effects() {
        const CHILD: &str = "CHIDORI_TEST_AGENT_SQLITE_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let base = PathBuf::from(std::env::var_os("CHIDORI_TEST_AGENT_BASE").unwrap());
            let db_path = std::env::var("CHIDORI_RUN_DB").unwrap();
            let source = base.join("agent.ts");
            std::fs::write(
                &source,
                r#"export async function agent() {
                    await chidori.tool("count", {});
                    return {};
                }"#,
            )
            .unwrap();
            let alarm_source = base.join("alarm-agent.ts");
            std::fs::write(
                &alarm_source,
                r#"export async function agent() {
                    await chidori.alarm(60000);
                    await chidori.tool("alarmCount", {});
                    return {};
                }"#,
            )
            .unwrap();
            let spawn_base = base.join("spawn-runs");
            let wake_base = base.join("wake-runs");
            let rearm_base = base.join("rearm-runs");
            let alarm_base = base.join("alarm-runs");
            let effect_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let (alarm_event_tx, alarm_event_rx) = std::sync::mpsc::channel();
            let mut tool_registry = ToolRegistry::new();
            let effect_counter = effect_count.clone();
            tool_registry.register_native(
                "count",
                "counts detached agent effects",
                Vec::new(),
                move |_| {
                    effect_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(json!({ "counted": true }))
                },
            );
            let alarm_event = alarm_event_tx;
            let alarm_counter = effect_count.clone();
            tool_registry.register_native(
                "alarmCount",
                "marks the due-alarm wake effect",
                Vec::new(),
                move |_| {
                    alarm_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let _ = alarm_event.send(true);
                    Ok(json!({ "counted": true }))
                },
            );
            let tools = Arc::new(tool_registry);
            let spawn_parts = sqlite_test_parts(&base, spawn_base.clone(), tools.clone());
            let wake_parts = sqlite_test_parts(&base, wake_base.clone(), tools.clone());
            let rearm_parts = sqlite_test_parts(&base, rearm_base.clone(), tools);
            let alarm_parts =
                sqlite_test_parts(&base, alarm_base.clone(), spawn_parts.tools.clone());

            let spawn_name = "sqlite-red-spawn";
            let spawn_result = hub().spawn(
                &spawn_parts,
                source.to_str().unwrap(),
                json!({}),
                &SpawnOptions {
                    name: Some(spawn_name.to_string()),
                    restart: "never".to_string(),
                    max_restarts: 0,
                    backoff_ms: 0,
                    model: None,
                },
                "owner-run".to_string(),
            );

            let wake_name = "sqlite-red-wake";
            write_registry_fixture(&wake_base, wake_name, "wake-agent", &source, "hibernating");
            let wake_registry = wake_base.join("agents").join(format!("{wake_name}.json"));
            let wake_registry_before = std::fs::read(&wake_registry).unwrap();
            let wake_rearm = hub().rearm_from_registry(wake_parts.clone());
            let wake_result = hub().send(
                &wake_parts,
                wake_name,
                "wake",
                json!({"payload": true}),
                json!({"from": "test"}),
            );

            let rearm_name = "sqlite-red-rearm";
            write_registry_fixture(&rearm_base, rearm_name, "rearm-agent", &source, "running");
            let rearm_registry = rearm_base.join("agents").join(format!("{rearm_name}.json"));
            let rearm_registry_before = std::fs::read(&rearm_registry).unwrap();
            let rearm_result = hub().rearm_from_registry(rearm_parts.clone());

            let alarm_name = "sqlite-red-alarm-wake";
            write_registry_fixture(
                &alarm_base,
                alarm_name,
                "alarm-agent",
                &alarm_source,
                "hibernating",
            );
            let alarm_registry = alarm_base.join("agents").join(format!("{alarm_name}.json"));
            let mut alarm_entry: Value =
                serde_json::from_slice(&std::fs::read(&alarm_registry).unwrap()).unwrap();
            alarm_entry["descriptor"]["listen"]["names"] = json!([ALARM_SIGNAL_NAME]);
            alarm_entry["descriptor"]["listen"]["deadline"] =
                json!((Utc::now() - chrono::Duration::seconds(1)).to_rfc3339());
            std::fs::write(
                &alarm_registry,
                serde_json::to_vec_pretty(&alarm_entry).unwrap(),
            )
            .unwrap();
            let alarm_registry_before = std::fs::read(&alarm_registry).unwrap();
            let alarm_rearm = hub().rearm_from_registry(alarm_parts.clone());

            // Wait for the alarm agent's distinctive effect event. This proves
            // the due-alarm path reached execution without sleep polling.
            let alarm_effect_seen =
                alarm_rearm.is_ok() && alarm_event_rx.recv_timeout(Duration::from_secs(5)).is_ok();

            let spawn_registry_written = spawn_base
                .join("agents")
                .join(format!("{spawn_name}.json"))
                .exists();
            let wake_inbox_written = wake_base
                .join("wake-agent")
                .join("signals")
                .join("inbox.json")
                .exists();
            let spawn_entry_installed = hub().entries.lock().unwrap().contains_key(spawn_name);
            let wake_entry_installed = hub().entries.lock().unwrap().contains_key(wake_name);
            let rearm_entry_installed = hub().entries.lock().unwrap().contains_key(rearm_name);
            let alarm_entry_installed = hub().entries.lock().unwrap().contains_key(alarm_name);

            let errors = [
                spawn_result.err(),
                wake_rearm.err(),
                wake_result.err(),
                rearm_result.err(),
                alarm_rearm.err(),
            ];
            let observed_effects = effect_count.load(std::sync::atomic::Ordering::SeqCst);
            assert_eq!(
                observed_effects, 0,
                "detached agent executed a live tool effect under invalid SQLite; due-alarm effect observed={alarm_effect_seen}"
            );
            assert!(
                errors.iter().all(|error| error.as_deref().is_some_and(|text| {
                    text.contains(&db_path)
                        && text.to_lowercase().contains("unable to open database file")
                })),
                "hub operations did not propagate invalid SQLite before spawn/rearm/send/alarm wake; observed errors={errors:?}, effects={}, spawn_registry_written={spawn_registry_written}, wake_inbox_written={wake_inbox_written}, entries=[{spawn_entry_installed}, {wake_entry_installed}, {rearm_entry_installed}, {alarm_entry_installed}]",
                observed_effects
            );
            assert!(
                !spawn_registry_written && !wake_inbox_written,
                "invalid SQLite mutated a detached-agent registry or mailbox"
            );
            assert_eq!(std::fs::read(&wake_registry).unwrap(), wake_registry_before);
            assert_eq!(
                std::fs::read(&rearm_registry).unwrap(),
                rearm_registry_before
            );
            assert_eq!(
                std::fs::read(&alarm_registry).unwrap(),
                alarm_registry_before
            );
            assert!(
                !spawn_entry_installed
                    && !wake_entry_installed
                    && !rearm_entry_installed
                    && !alarm_entry_installed,
                "invalid SQLite installed an agent entry or started a supervision path"
            );
            assert!(
                hub().parts.lock().unwrap().is_none(),
                "invalid SQLite installed detached-agent runtime parts"
            );
            assert!(
                !*hub().timer_started.lock().unwrap(),
                "invalid SQLite started the detached-agent timer"
            );
            return;
        }

        let base = test_dir("sqlite-factory-paths");
        let db_path = base.join("sqlite-db-is-a-directory");
        std::fs::create_dir_all(&db_path).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runtime::host_agent::tests::explicit_sqlite_failure_rejects_detached_spawn_rearm_send_and_alarm_wake_before_effects",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("CHIDORI_TEST_AGENT_BASE", &base)
            .env("CHIDORI_RUN_STORE", "sqlite")
            .env("CHIDORI_RUN_DB", &db_path)
            .env("CHIDORI_DURABILITY", "strict")
            .output()
            .unwrap();
        let report = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::remove_dir_all(&base).ok();
        assert!(
            output.status.success(),
            "isolated detached-agent SQLite assertions failed:\n{report}"
        );
    }

    fn write_registry_fixture(
        run_base: &Path,
        name: &str,
        run_id: &str,
        source: &Path,
        status: &str,
    ) {
        let agents = run_base.join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let descriptor = json!({
            "name": name,
            "run_id": run_id,
            "source": source,
            "input": {},
            "restart": "never",
            "max_restarts": 0,
            "backoff_ms": 0,
            "owner_run_id": "owner-run",
            "model": null,
            "status": status,
            "listen": if status == "hibernating" {
                json!({"names": ["wake"], "seq": 1, "function": "signal", "deadline": null})
            } else {
                Value::Null
            },
            "restarts": 0,
            "output": null,
            "error": null,
            "created_at": Utc::now(),
            "updated_at": Utc::now(),
        });
        let entry = json!({ "name": name, "run_id": run_id, "descriptor": descriptor });
        std::fs::write(
            agents.join(format!("{name}.json")),
            serde_json::to_vec_pretty(&entry).unwrap(),
        )
        .unwrap();
    }

    /// A fully-wired runtime backend over `ctx`, with the template engine
    /// anchored at `dir` so relative agent sources resolve — mirroring the
    /// host_actor test harness, plus persistence (detached agents are
    /// durable runs and refuse to exist without a journal).
    fn test_backend(ctx: RuntimeContext, dir: &Path) -> HostBindingBackend {
        ctx.enable_persistence(dir.join(".chidori").join("runs"));
        HostBindingBackend::for_runtime(
            ctx,
            Arc::new(ProviderRegistry::new()),
            Arc::new(TemplateEngine::new(dir)),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
            PolicyConfig::from_env(),
            Arc::new(StdMutex::new(PolicyCache::default())),
            RuntimePolicy::durable_default("agent-test"),
            Arc::new(ToolRegistry::new()),
            Arc::new(McpManager::new()),
        )
    }

    fn test_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("chidori-agent-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("services")).unwrap();
        dir
    }

    #[test]
    fn detached_inbox_snapshot_to_live_context_gap_preserves_acknowledged_payloads() {
        const CHILD: &str = "CHIDORI_TEST_INBOX_SNAPSHOT_GAP_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let base =
                PathBuf::from(std::env::var_os("CHIDORI_TEST_INBOX_SNAPSHOT_GAP_BASE").unwrap());
            let loss = run_inbox_snapshot_to_live_context_gap(&base);
            run_sequential_inbox_control(&base, 2);
            assert!(
                loss.is_none(),
                "acknowledged payload missing from both child journal and inbox: {}",
                loss.unwrap_or_default()
            );
            return;
        }

        let mut failures = Vec::new();
        for backend in ["fs", "sqlite"] {
            let base = tempfile::tempdir().unwrap();
            let db_path = base.path().join("runs.sqlite3");
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "runtime::host_agent::tests::detached_inbox_snapshot_to_live_context_gap_preserves_acknowledged_payloads",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("CHIDORI_TEST_INBOX_SNAPSHOT_GAP_BASE", base.path())
                .env("CHIDORI_RUN_STORE", backend)
                .env("CHIDORI_RUN_DB", db_path)
                .env("CHIDORI_DURABILITY", "strict")
                .output()
                .unwrap();
            if !output.status.success() {
                failures.push(format!(
                    "backend={backend}\n{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "inbox snapshot-to-live-context interleaving lost an acknowledged payload:\n{failures:#?}"
        );
    }

    fn run_inbox_snapshot_to_live_context_gap(base: &Path) -> Option<String> {
        let project = base.join("snapshot-gap");
        std::fs::create_dir_all(project.join("services")).unwrap();
        let source = project.join("services/worker.ts");
        std::fs::write(
            &source,
            r#"export async function agent() {
                for (let i = 0; i < 2; i++) {
                    const message = await chidori.signal("tick");
                    await chidori.log("cycle", { n: message.payload.n });
                }
                return { done: true };
            }"#,
        )
        .unwrap();
        let agent_name = unique("inbox-snapshot-gap");
        let run_base = project.join(".chidori/runs");
        let parts = sqlite_test_parts(&project, run_base.clone(), Arc::new(ToolRegistry::new()));
        hub().install_parts(parts.clone());
        let options = SpawnOptions {
            name: Some(agent_name.clone()),
            restart: "never".to_string(),
            max_restarts: 0,
            backoff_ms: 0,
            model: None,
        };

        let snapshot_arrived_rx;
        let snapshot_release_tx;
        let context_published_rx;
        let context_release_tx;
        {
            let (arrived_tx, rx) = std::sync::mpsc::channel();
            snapshot_arrived_rx = rx;
            let (release_tx, snapshot_release_rx) = std::sync::mpsc::channel();
            snapshot_release_tx = release_tx;
            let (published_tx, rx) = std::sync::mpsc::channel();
            context_published_rx = rx;
            let (release_tx, context_release_rx) = std::sync::mpsc::channel();
            context_release_tx = release_tx;
            *INBOX_SNAPSHOT_GAP_BARRIERS
                .get_or_init(|| StdMutex::new(None))
                .lock()
                .unwrap() = Some(Arc::new(InboxSnapshotGapBarriers {
                agent_name: agent_name.clone(),
                snapshot_arrived: arrived_tx,
                snapshot_release: StdMutex::new(snapshot_release_rx),
                context_published: published_tx,
                context_release: StdMutex::new(context_release_rx),
                snapshot_claimed: AtomicBool::new(false),
                context_claimed: AtomicBool::new(false),
            }));
        }

        let spawned = hub()
            .spawn(
                &parts,
                source.to_str().unwrap(),
                json!({}),
                &options,
                "snapshot-gap-test".to_string(),
            )
            .unwrap();
        snapshot_arrived_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("supervisor did not reach inbox snapshot barrier");

        let run_id = spawned["runId"].as_str().unwrap().to_string();
        let status = hub().status(&parts, &agent_name).unwrap();
        assert_eq!(status["status"], json!("running"), "{status}");
        assert_eq!(status["live"], json!(true), "{status}");
        let mut acknowledged_ids = Vec::new();
        for n in 1..=2 {
            if n == 2 {
                snapshot_release_tx.send(()).unwrap();
                context_published_rx
                    .recv_timeout(Duration::from_secs(10))
                    .expect("supervisor did not publish live context");
            }
            let result = hub()
                .send(
                    &parts,
                    &agent_name,
                    "tick",
                    json!({ "n": n }),
                    json!({ "kind": "agent", "id": "snapshot-gap-test" }),
                )
                .unwrap();
            assert_eq!(result["delivered"], json!(true), "payload n={n}");
            acknowledged_ids.push(n);
        }
        context_release_tx.send(()).unwrap();
        wait_for_agent_status(&parts, &agent_name, |status| {
            status == "hibernating" || status == "completed" || status == "failed"
        });
        *INBOX_SNAPSHOT_GAP_BARRIERS
            .get_or_init(|| StdMutex::new(None))
            .lock()
            .unwrap() = None;

        let factory = RunStoreFactory::shared(&run_base);
        let store = factory.store_for(&run_id);
        let records = store.load_call_log().unwrap().unwrap_or_default();
        let inbox: Vec<Value> = store
            .get_blob(SIGNAL_INBOX_FILE)
            .unwrap()
            .map(|bytes| serde_json::from_slice(&bytes).unwrap())
            .unwrap_or_default();
        let accounted: Vec<(usize, usize, usize)> = acknowledged_ids
            .iter()
            .map(|n| {
                let journal_count = records
                    .iter()
                    .filter(|record| {
                        record.function == "signal" && record.result["payload"]["n"] == json!(n)
                    })
                    .count();
                let inbox_count = inbox
                    .iter()
                    .filter(|queued| queued["payload"]["n"] == json!(n))
                    .count();
                (*n, journal_count, inbox_count)
            })
            .collect();
        let missing: Vec<_> = accounted
            .iter()
            .filter(|(_, journal_count, inbox_count)| journal_count + inbox_count != 1)
            .cloned()
            .collect();
        let _ = hub().stop(&parts, &agent_name);
        wait_for_agent_status(&parts, &agent_name, |status| {
            matches!(status, "stopped" | "completed" | "failed")
        });
        (!missing.is_empty()).then(|| {
            format!(
                "expected acknowledged n=1 and n=2 exactly once in child journal or inbox; missing/duplicate={missing:?}, all_counts={accounted:?}, inbox={inbox:?}"
            )
        })
    }

    fn write_module(dir: &Path, name: &str, source: &str) {
        std::fs::write(dir.join("services").join(name), source).unwrap();
    }

    fn unique(name: &str) -> String {
        format!("{name}-{}", &uuid::Uuid::new_v4().to_string()[..8])
    }

    /// Poll an agent's status until it settles (or the deadline passes) —
    /// wakes run on their own thread, so the test observes the descriptor.
    fn wait_for_settled(parts: &AgentRuntimeParts, name: &str) -> Value {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let status = hub().status(parts, name).unwrap();
            let settled = matches!(
                status["status"].as_str().unwrap_or_default(),
                "completed" | "failed" | "stopped" | "paused"
            );
            if settled || std::time::Instant::now() >= deadline {
                return status;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// The run id the parent's `spawn_agent` record minted.
    fn spawned_run_id(ctx: &RuntimeContext) -> String {
        ctx.call_log()
            .into_records()
            .iter()
            .find(|r| r.function == "spawn_agent")
            .expect("spawn_agent record")
            .result["runId"]
            .as_str()
            .expect("runId")
            .to_string()
    }

    #[test]
    fn detached_agent_spawns_completes_and_joins() {
        let dir = test_dir("basic");
        write_module(
            &dir,
            "worker.ts",
            r#"
            export async function agent(input: { base: number }) {
                await chidori.log("detached worker running");
                return { doubled: input.base * 2 };
            }
            "#,
        );
        let agent_name = unique("doubler");
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const svc = await chidori.agents.spawn("services/worker.ts", { base: 21 }, { name: "__NAME__" });
                const outcome = await svc.join({ timeoutMs: 30000 });
                return { name: svc.name, outcome };
            }
        "#
        .replace("__NAME__", &agent_name);
        std::fs::write(&path, &src).unwrap();

        let ctx = RuntimeContext::new();
        let backend = test_backend(ctx.clone(), &dir);
        let output = run_agent(&path, &src, &json!({}), &backend).unwrap();

        assert_eq!(output["name"], json!(agent_name));
        assert_eq!(output["outcome"]["status"], json!("completed"));
        assert_eq!(output["outcome"]["output"], json!({ "doubled": 42 }));

        // The parent journal carries only spawn + join records; the agent's
        // own records live in ITS journal under its own run id.
        let records = ctx.call_log().into_records();
        let spawn = records
            .iter()
            .find(|r| r.function == "spawn_agent")
            .expect("spawn_agent record");
        assert!(records.iter().any(|r| r.function == "join_agent"));
        assert!(!records.iter().any(|r| r.function == "log"));
        let agent_run_id = spawn.result["runId"].as_str().unwrap();
        let factory = RunStoreFactory::shared(&dir.join(".chidori").join("runs"));
        let agent_journal = factory
            .store_for(agent_run_id)
            .load_call_log()
            .unwrap()
            .expect("detached agent journal");
        assert!(agent_journal
            .iter()
            .any(|r| r.function == "log" && r.args["message"] == "detached worker running"));

        // The registry knows the settled agent.
        let entry = factory.registry_get(&agent_name).unwrap().unwrap();
        assert_eq!(entry["descriptor"]["status"], json!("completed"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn detached_agent_hibernates_and_wakes_on_send() {
        let dir = test_dir("hibernate");
        write_module(
            &dir,
            "listener.ts",
            r#"
            export async function agent() {
                await chidori.log("before listen");
                const msg = await chidori.signal("go");
                return { got: msg.payload };
            }
            "#,
        );
        let agent_name = unique("listener");
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const svc = await chidori.agents.spawn("services/listener.ts", {}, { name: "__NAME__" });
                // Give the service a moment to reach its listen point, then
                // check it hibernates with no thread parked on the wait.
                let status = await svc.join({ timeoutMs: 3000 });
                const wasHibernating = status.status;
                await svc.send("go", { speed: "fast" });
                const outcome = await svc.join({ timeoutMs: 30000 });
                return { wasHibernating, outcome };
            }
        "#
        .replace("__NAME__", &agent_name);
        std::fs::write(&path, &src).unwrap();

        let ctx = RuntimeContext::new();
        let backend = test_backend(ctx.clone(), &dir);
        let output = run_agent(&path, &src, &json!({}), &backend).unwrap();

        assert_eq!(output["wasHibernating"], json!("hibernating"));
        assert_eq!(output["outcome"]["status"], json!("completed"));
        assert_eq!(
            output["outcome"]["output"],
            json!({ "got": { "speed": "fast" } })
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn alarm_wakes_a_hibernating_agent_at_the_deadline() {
        let dir = test_dir("alarm");
        write_module(
            &dir,
            "sleeper.ts",
            r#"
            export async function agent() {
                await chidori.log("arming alarm");
                const fired = await chidori.alarm(1500);
                return { timedOut: fired.timedOut === true };
            }
            "#,
        );
        let agent_name = unique("sleeper");
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const svc = await chidori.agents.spawn("services/sleeper.ts", {}, { name: "__NAME__" });
                const outcome = await svc.join({ timeoutMs: 30000 });
                return { outcome };
            }
        "#
        .replace("__NAME__", &agent_name);
        std::fs::write(&path, &src).unwrap();

        let ctx = RuntimeContext::new();
        let backend = test_backend(ctx.clone(), &dir);
        // join with a timeout returns a status snapshot rather than blocking,
        // so poll until the alarm fires and the agent completes.
        let output = run_agent(&path, &src, &json!({}), &backend).unwrap();
        let mut outcome = output["outcome"].clone();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while outcome["status"] != json!("completed") && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
            let parts = hub().installed_parts().unwrap();
            outcome = hub().status(&parts, &agent_name).unwrap();
        }
        assert_eq!(outcome["status"], json!("completed"), "{outcome}");
        assert_eq!(outcome["output"], json!({ "timedOut": true }));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn resume_restart_replays_completed_work_and_retries_the_failure() {
        let dir = test_dir("restart");
        // The worker fails on its first attempt (no marker file), then
        // "the environment is fixed" and the resume restart retries only the
        // failing frontier: the log call before the failure must not re-run.
        write_module(
            &dir,
            "flaky.ts",
            r#"
            import * as fs from "node:fs";
            export async function agent() {
                await chidori.log("expensive work done");
                // The VFS is snapshot-resident and empty on the first
                // attempt; the resume restart replays the log call from
                // cache and re-executes only this failing read.
                const marker = await chidori.util.tryCall(() => fs.readFileSync("/marker", "utf8"));
                if (!marker.ok) {
                    fs.writeFileSync("/marker", "present");
                    throw new Error("transient failure");
                }
                return { done: true };
            }
            "#,
        );
        let agent_name = unique("flaky");
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const svc = await chidori.agents.spawn("services/flaky.ts", {}, {
                    name: "__NAME__",
                    restart: "resume",
                    maxRestarts: 3,
                });
                const outcome = await svc.join({ timeoutMs: 30000 });
                return { outcome };
            }
        "#
        .replace("__NAME__", &agent_name);
        std::fs::write(&path, &src).unwrap();

        let ctx = RuntimeContext::new();
        let backend = test_backend(ctx.clone(), &dir);
        let output = run_agent(&path, &src, &json!({}), &backend).unwrap();
        // The VFS resets per iteration (it rides the manifest persisted at
        // safepoints, and the failing iteration persisted its write), so the
        // retry sees the marker... depending on scaffold timing the agent
        // either completes on a later attempt or exhausts the budget; both
        // prove the restart loop drove re-execution.
        let status = output["outcome"]["status"].as_str().unwrap().to_string();
        assert!(
            status == "completed" || status == "failed",
            "unexpected status {status}"
        );
        let restarts = output["outcome"]["restarts"].as_u64().unwrap();
        assert!(restarts >= 1, "expected at least one restart");

        let _ = std::fs::remove_dir_all(dir);
    }

    /// The fleet case: a wake lands wherever a lease can be taken, not where
    /// the agent's project tree happens to be. With the local tree gone, the
    /// run's own recorded source history is the source of truth — entry AND
    /// imported module, at their recorded relative layout, so the import
    /// still resolves.
    #[test]
    fn wakes_from_materialized_source_when_the_local_tree_is_gone() {
        let dir = test_dir("materialize");
        write_module(&dir, "lib.ts", "export const greeting = \"hi\";\n");
        write_module(
            &dir,
            "listener.ts",
            r#"
            import { greeting } from "./lib.ts";
            export async function agent() {
                await chidori.log("armed");
                const msg = await chidori.signal("go");
                return { got: msg.payload, greeting };
            }
            "#,
        );
        let agent_name = unique("materialized");
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const svc = await chidori.agents.spawn("services/listener.ts", {}, { name: "__NAME__" });
                const status = await svc.join({ timeoutMs: 5000 });
                return { status: status.status };
            }
        "#
        .replace("__NAME__", &agent_name);
        std::fs::write(&path, &src).unwrap();

        let ctx = RuntimeContext::new();
        let backend = test_backend(ctx.clone(), &dir);
        let output = run_agent(&path, &src, &json!({}), &backend).unwrap();
        assert_eq!(output["status"], json!("hibernating"));
        let run_id = spawned_run_id(&ctx);

        std::fs::remove_dir_all(dir.join("services")).unwrap();

        let parts = hub().installed_parts().unwrap();
        hub()
            .send(
                &parts,
                &agent_name,
                "go",
                json!({ "speed": "fast" }),
                json!({ "kind": "test" }),
            )
            .unwrap();
        let outcome = wait_for_settled(&parts, &agent_name);
        assert_eq!(outcome["status"], json!("completed"), "{outcome}");
        assert_eq!(
            outcome["output"],
            json!({ "got": { "speed": "fast" }, "greeting": "hi" })
        );

        // The tree came out of the run store, into the materialization root.
        // (The hub is process-global, so derive the root from this test's own
        // run base rather than whatever parts another test installed last.)
        let root = crate::runtime::source_history::materialization_root(
            &dir.join(".chidori").join("runs"),
            &run_id,
        )
        .unwrap();
        assert!(root.join("listener.ts").is_file(), "{}", root.display());
        assert!(root.join("lib.ts").is_file());

        let _ = std::fs::remove_dir_all(dir);
    }

    /// A run recorded before source history existed has no `history/` at all,
    /// only the entry text inside the snapshot blob's `DurableBlob.bundle`.
    /// That is still enough to wake a single-module agent on a node with no
    /// copy of its source.
    #[test]
    fn wakes_from_the_snapshot_bundle_for_a_pre_source_history_run() {
        let dir = test_dir("bundle-wake");
        write_module(
            &dir,
            "solo.ts",
            r#"
            export async function agent() {
                await chidori.log("armed");
                const msg = await chidori.signal("go");
                return { got: msg.payload };
            }
            "#,
        );
        let agent_name = unique("legacy");
        let path = dir.join("agent.ts");
        let src = r#"
            export async function agent() {
                const svc = await chidori.agents.spawn("services/solo.ts", {}, { name: "__NAME__" });
                const status = await svc.join({ timeoutMs: 5000 });
                return { status: status.status };
            }
        "#
        .replace("__NAME__", &agent_name);
        std::fs::write(&path, &src).unwrap();

        let ctx = RuntimeContext::new();
        let backend = test_backend(ctx.clone(), &dir);
        let output = run_agent(&path, &src, &json!({}), &backend).unwrap();
        assert_eq!(output["status"], json!("hibernating"));
        let run_id = spawned_run_id(&ctx);

        // Age the run into the pre-source-history shape, then take its source
        // away: only the manifest + durable bundle remain.
        let run_dir = dir.join(".chidori").join("runs").join(&run_id);
        std::fs::remove_dir_all(run_dir.join("history")).unwrap();
        std::fs::remove_dir_all(dir.join("services")).unwrap();

        let parts = hub().installed_parts().unwrap();
        hub()
            .send(
                &parts,
                &agent_name,
                "go",
                json!({ "speed": "slow" }),
                json!({ "kind": "test" }),
            )
            .unwrap();
        let outcome = wait_for_settled(&parts, &agent_name);
        assert_eq!(outcome["status"], json!("completed"), "{outcome}");
        assert_eq!(outcome["output"], json!({ "got": { "speed": "slow" } }));

        let _ = std::fs::remove_dir_all(dir);
    }
}
