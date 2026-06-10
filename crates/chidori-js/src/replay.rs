//! The replay durable runtime (plan P3 + P4).
//!
//! Durability = deterministic replay of an effect journal, **not** a VM-image
//! snapshot. A durable agent calls *host effects* (registered JS functions for
//! time, randomness, fs, network, prompts, tools, …). Each call is addressed by a
//! deterministic key (effect name + per-name invocation index). In record mode
//! the result is produced live and appended to the journal; in replay mode the
//! recorded result is fed back without re-performing the effect.
//!
//! Restore re-evaluates the (possibly edited) code bundle and re-runs from the
//! top, feeding journaled results at each host call until it reaches the *pending
//! frontier* — the first call with no journal entry — where it blocks exactly as
//! the original run did. Because we re-execute source rather than restoring a
//! frozen program counter, **editing code after the frontier resumes cleanly**
//! (modify-and-resume, P4). Editing code before the frontier is detected via the
//! journal keys and handled by the edit-conflict policy below (P4: fail-loud).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use serde_json::Value as Json;

use crate::journal::{EffectOutcome, Journal};
use crate::value::Value;
use crate::vm::{ErrorKind, RunOutcome, Vm};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Record,
    Replay,
}

#[derive(Clone)]
struct PendingOp {
    name: String,
    args: Json,
    site: String,
    seq: u64,
}

struct JournalState {
    journal: Journal,
    mode: Mode,
    counters: HashMap<String, u64>,
    pending: HashMap<u64, PendingOp>,
    /// Cursor into the recorded journal: how many entries replay has matched so
    /// far. Entries at/after the cursor are the live frontier.
    cursor: usize,
    /// Divergence detected during replay (edit touched already-journaled code).
    divergence: Option<String>,
}

/// Outcome of driving the runtime.
#[derive(Debug)]
pub enum DriveOutcome {
    Completed,
    /// Blocked on a host effect the driver's handler declined to resolve inline
    /// (the process should persist the journal and suspend here).
    Suspended { op_id: u64, name: String, args: Json },
}

/// A durable JS runtime: a VM plus an effect journal.
pub struct ReplayRuntime {
    pub vm: Vm,
    bundle: String,
    bundle_hash: String,
    state: Rc<RefCell<JournalState>>,
    started: bool,
}

impl ReplayRuntime {
    /// Create a runtime in record mode for a fresh durable execution.
    pub fn record(bundle: &str, effects: &[&str]) -> ReplayRuntime {
        let bundle_hash = Journal::hash_bundle(bundle);
        let state = Rc::new(RefCell::new(JournalState {
            journal: Journal::new(bundle_hash.clone()),
            mode: Mode::Record,
            counters: HashMap::new(),
            pending: HashMap::new(),
            cursor: 0,
            divergence: None,
        }));
        let mut rt = ReplayRuntime {
            vm: Vm::new(),
            bundle: bundle.to_string(),
            bundle_hash,
            state,
            started: false,
        };
        rt.install_effects(effects);
        rt.install_memo();
        rt
    }

    /// Restore a runtime from a persisted journal, re-evaluating `bundle` (which
    /// may differ from the recorded one — modify-and-resume). Replays recorded
    /// effects until the pending frontier.
    pub fn restore(bundle: &str, journal_bytes: &[u8], effects: &[&str]) -> Result<ReplayRuntime, String> {
        let journal = Journal::from_bytes(journal_bytes)?;
        let bundle_hash = Journal::hash_bundle(bundle);
        let state = Rc::new(RefCell::new(JournalState {
            journal,
            mode: Mode::Replay,
            counters: HashMap::new(),
            pending: HashMap::new(),
            cursor: 0,
            divergence: None,
        }));
        let mut rt = ReplayRuntime {
            vm: Vm::new(),
            bundle: bundle.to_string(),
            bundle_hash,
            state,
            started: false,
        };
        rt.install_effects(effects);
        rt.install_memo();
        Ok(rt)
    }

    pub fn journal_bytes(&self) -> Vec<u8> {
        self.state.borrow().journal.to_bytes()
    }

    pub fn bundle_hash(&self) -> &str {
        &self.bundle_hash
    }

    /// Whether replay detected an edit that diverged from the journal.
    pub fn divergence(&self) -> Option<String> {
        self.state.borrow().divergence.clone()
    }

    /// Install each named effect as a global async function backed by the
    /// journal. Calling `name(...args)` returns a promise that resolves from the
    /// journal (replay) or pends awaiting a live result (record/frontier).
    fn install_effects(&mut self, effects: &[&str]) {
        let global = self.vm.realm.global.clone();
        for name in effects {
            let nm = name.to_string();
            let state = self.state.clone();
            self.vm.define_method(&global, name, 1, move |vm, _this, args| {
                let args_json = Json::Array(args.iter().map(|a| vm.value_to_json(a)).collect());
                // Allocate the deterministic key.
                let (site, seq) = {
                    let mut s = state.borrow_mut();
                    let seq = *s.counters.get(&nm).unwrap_or(&0);
                    s.counters.insert(nm.clone(), seq + 1);
                    (nm.clone(), seq)
                };
                // Ordered journal consumption: the next recorded entry must match
                // this call's key, else an edit changed already-executed effects
                // (fail-loud divergence, the P4 default policy).
                enum Decision {
                    Resolve(Json),
                    Reject(String),
                    Frontier,
                    Diverged(String),
                }
                let decision = {
                    let mut s = state.borrow_mut();
                    let cursor = s.cursor;
                    if cursor < s.journal.entries.len() {
                        let entry = s.journal.entries[cursor].clone();
                        if entry.site == site && entry.seq == seq {
                            s.cursor += 1;
                            match entry.outcome {
                                EffectOutcome::Resolved(j) => Decision::Resolve(j),
                                EffectOutcome::Rejected(m) => Decision::Reject(m),
                            }
                        } else {
                            let msg = format!(
                                "expected effect '{}'#{} from journal but program called '{}'#{} \
                                 (an edit changed already-executed code before the resume point)",
                                entry.site, entry.seq, site, seq
                            );
                            s.divergence = Some(msg.clone());
                            Decision::Diverged(msg)
                        }
                    } else {
                        Decision::Frontier
                    }
                };
                let (id, promise) = vm.register_host_op();
                state.borrow_mut().pending.insert(
                    id,
                    PendingOp {
                        name: nm.clone(),
                        args: args_json,
                        site,
                        seq,
                    },
                );
                match decision {
                    Decision::Resolve(j) => {
                        let v = vm.json_to_value(&j);
                        vm.resolve_host_op(id, v);
                    }
                    Decision::Reject(msg) => {
                        let e = vm.make_error(ErrorKind::Error, &msg);
                        vm.reject_host_op(id, e);
                    }
                    Decision::Diverged(msg) => {
                        let e = vm.make_error(ErrorKind::Error, &msg);
                        vm.reject_host_op(id, e);
                    }
                    Decision::Frontier => { /* stays pending; resolved live */ }
                }
                Ok(Value::Object(promise))
            });
        }
    }

    /// Install `durableStep(fn)` — value checkpointing (plan P6). Runs `fn` once
    /// (record), journals its plain-value result, and on replay returns the
    /// journaled value **without re-running `fn`**. This bounds replay cost on
    /// long histories: expensive deterministic computation between effects is
    /// memoized rather than re-executed. The result must be JSON-serializable
    /// (a plain value, not a continuation).
    fn install_memo(&mut self) {
        let global = self.vm.realm.global.clone();
        let state = self.state.clone();
        self.vm.define_method(&global, "durableStep", 1, move |vm, _this, args| {
            let f = args.get(0).cloned().unwrap_or(Value::Undefined);
            let site = "durableStep".to_string();
            let seq = {
                let mut s = state.borrow_mut();
                let seq = *s.counters.get(&site).unwrap_or(&0);
                s.counters.insert(site.clone(), seq + 1);
                seq
            };
            enum Decision {
                Cached(Json),
                CachedErr(String),
                Run,
                Diverged(String),
            }
            let decision = {
                let mut s = state.borrow_mut();
                let cursor = s.cursor;
                if cursor < s.journal.entries.len() {
                    let entry = s.journal.entries[cursor].clone();
                    if entry.site == site && entry.seq == seq {
                        s.cursor += 1;
                        match entry.outcome {
                            EffectOutcome::Resolved(j) => Decision::Cached(j),
                            EffectOutcome::Rejected(m) => Decision::CachedErr(m),
                        }
                    } else {
                        let msg = format!(
                            "expected '{}'#{} from journal but program reached durableStep#{} \
                             (edit changed already-executed code)",
                            entry.site, entry.seq, seq
                        );
                        s.divergence = Some(msg.clone());
                        Decision::Diverged(msg)
                    }
                } else {
                    Decision::Run
                }
            };
            let (id, promise) = vm.register_host_op();
            let key = crate::host::HostKey { site, seq };
            match decision {
                Decision::Cached(j) => {
                    let v = vm.json_to_value(&j);
                    vm.resolve_host_op(id, v);
                }
                Decision::CachedErr(m) => {
                    let e = vm.make_error(ErrorKind::Error, &m);
                    vm.reject_host_op(id, e);
                }
                Decision::Diverged(m) => {
                    let e = vm.make_error(ErrorKind::Error, &m);
                    vm.reject_host_op(id, e);
                }
                Decision::Run => match vm.call(f, Value::Undefined, &[]) {
                    Ok(v) => {
                        let j = vm.value_to_json(&v);
                        {
                            let mut s = state.borrow_mut();
                            s.journal.append(&key, EffectOutcome::Resolved(j.clone()));
                            s.cursor = s.journal.entries.len();
                        }
                        let rv = vm.json_to_value(&j);
                        vm.resolve_host_op(id, rv);
                    }
                    Err(e) => {
                        let msg = vm.error_to_string(&e);
                        {
                            let mut s = state.borrow_mut();
                            s.journal.append(&key, EffectOutcome::Rejected(msg.clone()));
                            s.cursor = s.journal.entries.len();
                        }
                        let err = vm.make_error(ErrorKind::Error, &msg);
                        vm.reject_host_op(id, err);
                    }
                },
            }
            Ok(Value::Object(promise))
        });
    }

    fn start(&mut self) -> Result<(), String> {
        if self.started {
            return Ok(());
        }
        self.started = true;
        let proto = crate::compiler::compile_script(&self.bundle)?;
        let func = self.vm.make_closure(Rc::new(proto), Vec::new());
        match self.vm.call(Value::Object(func), Value::Undefined, &[]) {
            Ok(_) => Ok(()),
            Err(e) => Err(self.vm.error_to_string(&e)),
        }
    }

    /// Drive execution. For each host effect at the frontier, the `handler` is
    /// asked to produce a result; returning `None` suspends the process there
    /// (persist the journal and resume later, possibly in another process or with
    /// edited code). Returns when the program completes or suspends.
    pub fn drive(
        &mut self,
        handler: &mut dyn FnMut(&str, &Json) -> Option<Result<Json, String>>,
    ) -> Result<DriveOutcome, String> {
        self.start()?;
        loop {
            if let Some(d) = self.state.borrow().divergence.clone() {
                return Err(format!("replay divergence: {d}"));
            }
            match self.vm.run_jobs_until_blocked() {
                RunOutcome::Completed => return Ok(DriveOutcome::Completed),
                RunOutcome::BlockedOnHost(id) => {
                    let op = self
                        .state
                        .borrow()
                        .pending
                        .get(&id)
                        .cloned()
                        .ok_or_else(|| format!("unknown host op {id}"))?;
                    match handler(&op.name, &op.args) {
                        None => {
                            return Ok(DriveOutcome::Suspended {
                                op_id: id,
                                name: op.name,
                                args: op.args,
                            })
                        }
                        Some(result) => {
                            let key = crate::host::HostKey {
                                site: op.site.clone(),
                                seq: op.seq,
                            };
                            match result {
                                Ok(json) => {
                                    {
                                        let mut s = self.state.borrow_mut();
                                        s.journal.append(&key, EffectOutcome::Resolved(json.clone()));
                                        s.cursor = s.journal.entries.len();
                                    }
                                    let v = self.vm.json_to_value(&json);
                                    self.vm.resolve_host_op(id, v);
                                }
                                Err(msg) => {
                                    {
                                        let mut s = self.state.borrow_mut();
                                        s.journal.append(&key, EffectOutcome::Rejected(msg.clone()));
                                        s.cursor = s.journal.entries.len();
                                    }
                                    let e = self.vm.make_error(ErrorKind::Error, &msg);
                                    self.vm.reject_host_op(id, e);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Resolve a specific suspended host op from outside (out-of-process resume),
    /// appending to the journal, then continue driving with `handler`.
    pub fn provide_and_drive(
        &mut self,
        op_id: u64,
        result: Result<Json, String>,
        handler: &mut dyn FnMut(&str, &Json) -> Option<Result<Json, String>>,
    ) -> Result<DriveOutcome, String> {
        let op = self
            .state
            .borrow()
            .pending
            .get(&op_id)
            .cloned()
            .ok_or_else(|| format!("unknown host op {op_id}"))?;
        let key = crate::host::HostKey {
            site: op.site,
            seq: op.seq,
        };
        match result {
            Ok(json) => {
                {
                                        let mut s = self.state.borrow_mut();
                                        s.journal.append(&key, EffectOutcome::Resolved(json.clone()));
                                        s.cursor = s.journal.entries.len();
                                    }
                let v = self.vm.json_to_value(&json);
                self.vm.resolve_host_op(op_id, v);
            }
            Err(msg) => {
                {
                                        let mut s = self.state.borrow_mut();
                                        s.journal.append(&key, EffectOutcome::Rejected(msg.clone()));
                                        s.cursor = s.journal.entries.len();
                                    }
                let e = self.vm.make_error(ErrorKind::Error, &msg);
                self.vm.reject_host_op(op_id, e);
            }
        }
        self.drive(handler)
    }

    pub fn console(&self) -> &[String] {
        &self.vm.console_log
    }

    // ---- lower-level primitives (used by the SnapshotCapableJsEngine adapter) ----

    /// Compile + start the bundle if not already started.
    pub fn ensure_started(&mut self) -> Result<(), String> {
        self.start()
    }

    /// Drain microtasks to the next host block (or completion), without invoking
    /// any inline handler. The caller resolves blocked ops via `resolve_op`.
    pub fn run_until_blocked(&mut self) -> Result<RunOutcome, String> {
        self.start()?;
        if let Some(d) = self.state.borrow().divergence.clone() {
            return Err(format!("replay divergence: {d}"));
        }
        Ok(self.vm.run_jobs_until_blocked())
    }

    /// The effect name + JSON args of a pending host op (for the driver to fulfill).
    pub fn pending_op(&self, op_id: u64) -> Option<(String, Json)> {
        self.state
            .borrow()
            .pending
            .get(&op_id)
            .map(|p| (p.name.clone(), p.args.clone()))
    }

    /// Resolve/reject a pending host op, journaling the outcome (live frontier).
    pub fn resolve_op(&mut self, op_id: u64, result: Result<Json, String>) -> Result<(), String> {
        let op = self
            .state
            .borrow()
            .pending
            .get(&op_id)
            .cloned()
            .ok_or_else(|| format!("unknown host op {op_id}"))?;
        let key = crate::host::HostKey {
            site: op.site,
            seq: op.seq,
        };
        match result {
            Ok(json) => {
                {
                    let mut s = self.state.borrow_mut();
                    s.journal.append(&key, EffectOutcome::Resolved(json.clone()));
                    s.cursor = s.journal.entries.len();
                }
                let v = self.vm.json_to_value(&json);
                self.vm.resolve_host_op(op_id, v);
            }
            Err(msg) => {
                {
                    let mut s = self.state.borrow_mut();
                    s.journal.append(&key, EffectOutcome::Rejected(msg.clone()));
                    s.cursor = s.journal.entries.len();
                }
                let e = self.vm.make_error(ErrorKind::Error, &msg);
                self.vm.reject_host_op(op_id, e);
            }
        }
        Ok(())
    }
}

/// A self-describing durable artifact: the code bundle plus its effect journal.
/// `restore` needs the bundle (the journal references it by content hash), so we
/// bundle them together rather than threading the bundle through the trait.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct DurableBlob {
    pub bundle: String,
    pub effects: Vec<String>,
    pub journal: Vec<u8>,
}

impl ReplayRuntime {
    /// Serialize the full durable artifact (bundle + journal).
    pub fn to_blob(&self, effects: &[&str]) -> Vec<u8> {
        let blob = DurableBlob {
            bundle: self.bundle.clone(),
            effects: effects.iter().map(|s| s.to_string()).collect(),
            journal: self.journal_bytes(),
        };
        serde_json::to_vec(&blob).unwrap_or_default()
    }

    /// Reconstruct a runtime from a `to_blob` artifact, replaying to the frontier.
    pub fn from_blob(bytes: &[u8]) -> Result<ReplayRuntime, String> {
        let blob: DurableBlob = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        let effects: Vec<&str> = blob.effects.iter().map(|s| s.as_str()).collect();
        ReplayRuntime::restore(&blob.bundle, &blob.journal, &effects)
    }
}
