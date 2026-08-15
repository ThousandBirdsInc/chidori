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
    #[allow(dead_code)] // Recorded at construction; not yet read back.
    mode: Mode,
    counters: HashMap<String, u64>,
    pending: HashMap<u64, PendingOp>,
    /// Cursor into the recorded journal: how many entries replay has matched so
    /// far. Entries at/after the cursor are the live frontier.
    cursor: usize,
    /// Divergence detected during replay (edit touched already-journaled code).
    divergence: Option<String>,
    /// True when `restore` was given a bundle whose content hash differs from
    /// the one the journal was recorded against (modify-and-resume). Legal,
    /// but recorded so divergence errors can say *why* replay drifted and
    /// callers can surface it.
    bundle_changed: bool,
}

impl JournalState {
    /// The state a not-yet-used restore target carries: the host natives close
    /// over it at construction, and the real journal is written into it when
    /// the VM is actually consumed by a restore.
    fn empty() -> JournalState {
        JournalState {
            journal: Journal::default(),
            mode: Mode::Record,
            counters: HashMap::new(),
            pending: HashMap::new(),
            cursor: 0,
            divergence: None,
            bundle_changed: false,
        }
    }
}

/// A restore target built ahead of time: a VM sitting at the image baseline,
/// paired with the journal state its host natives close over.
///
/// The pairing is the whole point. `install_effects` captures the runtime's
/// `Rc<RefCell<JournalState>>` inside each effect native, so a VM cannot be
/// separated from the state cell it was built against — reusing one means
/// overwriting that cell's *contents*, not swapping the `Rc`.
struct BaselinedVm {
    vm: Vm,
    state: Rc<RefCell<JournalState>>,
}

thread_local! {
    /// At most one spare baselined VM per effect-name list, filled only by an
    /// explicit [`ReplayRuntime::prime_image_restore`] call.
    ///
    /// Deliberately not self-refilling. Building a baselined VM costs the same
    /// whenever it happens, so refilling inside `from_image` would move the
    /// work around without removing any of it — a throughput wash that only
    /// looks like a win if you stop measuring at the `return`. Priming is
    /// opt-in so the caller decides when to spend it: a worker node primes at
    /// startup and again after a restore, while it is idle, and the restore
    /// itself then skips the whole prologue.
    static RESTORE_POOL: RefCell<HashMap<Vec<String>, BaselinedVm>> =
        RefCell::new(HashMap::new());
}

/// A pending host op, as written into a runtime image.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PendingOpImg {
    name: String,
    args: Json,
    site: String,
    seq: u64,
}

/// A [`ReplayRuntime`] image: the VM image plus the journal bookkeeping that
/// lives outside the VM. Both halves are needed — the heap says what the
/// program *is*, the cursor says where the journal picks up.
///
/// The journal itself is deliberately NOT in here. It already travels with the
/// artifact ([`DurableBlob::journal`]) and is what the fallback path needs;
/// carrying a second copy would make the image grow with history, which is the
/// one thing it exists not to do.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RuntimeImage {
    version: u32,
    bundle_hash: String,
    counters: HashMap<String, u64>,
    cursor: usize,
    pending: Vec<(u64, PendingOpImg)>,
    started: bool,
    vm: crate::image::VmImage,
}

/// Outcome of driving the runtime.
#[derive(Debug)]
pub enum DriveOutcome {
    Completed,
    /// Blocked on a host effect the driver's handler declined to resolve inline
    /// (the process should persist the journal and suspend here).
    Suspended {
        op_id: u64,
        name: String,
        args: Json,
    },
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
            bundle_changed: false,
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
    pub fn restore(
        bundle: &str,
        journal_bytes: &[u8],
        effects: &[&str],
    ) -> Result<ReplayRuntime, String> {
        let journal = Journal::from_bytes(journal_bytes)?;
        let bundle_hash = Journal::hash_bundle(bundle);
        // The journal pins the bundle it was recorded against; actually
        // compare the pin. A changed bundle is legal (modify-and-resume is
        // the feature) but must be *detectable*, not silently absorbed.
        let bundle_changed = !journal.bundle_hash.is_empty() && journal.bundle_hash != bundle_hash;
        let state = Rc::new(RefCell::new(JournalState {
            journal,
            mode: Mode::Replay,
            counters: HashMap::new(),
            pending: HashMap::new(),
            cursor: 0,
            divergence: None,
            bundle_changed,
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

    /// Turn on VM imaging: freeze everything built so far — the realm, the
    /// effect functions, the memo helper — as the image baseline, so an image
    /// only has to carry what the *program* creates.
    ///
    /// Call before the bundle starts. The restoring side reaches the same
    /// baseline by construction: same engine, same effect names, same order.
    /// That is also what re-binds host effects on the way back — the restored
    /// natives close over the *new* process's journal state, and the image
    /// refers to them by baseline id rather than trying to serialize a Rust
    /// closure.
    pub fn enable_imaging(&mut self) {
        self.vm.mark_image_baseline();
    }

    /// Capture the live runtime — heap, closures, promises, suspended frames,
    /// plus the journal cursor — as a restorable image.
    ///
    /// `Err` means this state has no image form (see
    /// [`crate::image::ImageError::Unsupported`]); the caller keeps the
    /// journal and resumes by replay instead. It is never wrong to fall back.
    pub fn to_image(&self) -> Result<RuntimeImage, String> {
        if !self.vm.has_image_baseline() {
            return Err("imaging was not enabled on this runtime".to_string());
        }
        let vm = self.vm.snapshot_image().map_err(|e| e.to_string())?;
        let s = self.state.borrow();
        let img = RuntimeImage {
            version: crate::image::IMAGE_VERSION,
            bundle_hash: self.bundle_hash.clone(),
            counters: s.counters.clone(),
            cursor: s.cursor,
            // Only ops that are STILL pending. `JournalState::pending` keeps
            // an entry per host call ever made — handy for divergence
            // messages, but it grows with history, and copying it wholesale
            // would make the image O(history) through the back door. The VM's
            // own pending-host map is the authoritative live set (entries are
            // removed as ops settle), and a settled op can never be resolved
            // again after a restore.
            pending: {
                let mut live: Vec<(u64, PendingOpImg)> = s
                    .pending
                    .iter()
                    .filter(|(id, _)| self.vm.pending_host.contains_key(*id))
                    .map(|(id, op)| {
                        (
                            *id,
                            PendingOpImg {
                                name: op.name.clone(),
                                args: op.args.clone(),
                                site: op.site.clone(),
                                seq: op.seq,
                            },
                        )
                    })
                    .collect();
                // HashMap order is not stable; sort so the same suspension
                // always produces the same bytes.
                live.sort_by_key(|(id, _)| *id);
                live
            },
            started: self.started,
            vm,
        };
        Ok(img)
    }

    /// Rebuild a runtime from an image: the heap comes back as it was, and the
    /// bundle is **not** re-executed. This is the O(live state) counterpart of
    /// [`Self::restore`]'s O(history) replay.
    ///
    /// The bundle is still required — it is recompiled so closures can point at
    /// real bytecode — and it must be the same source the image was taken
    /// against, which is checked. An *edited* bundle has no meaning here (there
    /// is no re-execution to absorb the edit); resume that through
    /// [`Self::restore`], which is what modify-and-resume is for.
    pub fn from_image(
        img: &RuntimeImage,
        bundle: &str,
        journal_bytes: &[u8],
        effects: &[&str],
    ) -> Result<ReplayRuntime, String> {
        if img.version != crate::image::IMAGE_VERSION {
            return Err(format!(
                "runtime image version {} but this engine writes {}",
                img.version,
                crate::image::IMAGE_VERSION
            ));
        }
        let bundle = bundle.to_string();
        let bundle_hash = Journal::hash_bundle(&bundle);
        if bundle_hash != img.bundle_hash {
            return Err(
                "image was taken against a different bundle; resume by journal replay \
                 (`restore`) if the source was edited"
                    .to_string(),
            );
        }
        let journal = Journal::from_bytes(journal_bytes)?;
        let restored = JournalState {
            journal,
            mode: Mode::Record,
            counters: img.counters.clone(),
            cursor: img.cursor,
            pending: img
                .pending
                .iter()
                .map(|(id, op)| {
                    (
                        *id,
                        PendingOp {
                            name: op.name.clone(),
                            args: op.args.clone(),
                            site: op.site.clone(),
                            seq: op.seq,
                        },
                    )
                })
                .collect(),
            divergence: None,
            bundle_changed: false,
        };
        // A VM at the baseline the imaging side built, pooled or fresh. Taken
        // only after the version and bundle checks above, so a rejected image
        // never burns a primed target.
        let taken = Self::take_baselined(effects);
        // The effect natives already hold this cell; give them the restored
        // journal by overwriting its contents rather than swapping the `Rc`,
        // which they would not see.
        *taken.state.borrow_mut() = restored;
        let mut rt = ReplayRuntime {
            vm: taken.vm,
            bundle,
            bundle_hash,
            state: taken.state,
            started: img.started,
        };
        // Compile (not run) the bundle so imaged closures resolve to bytecode.
        let proto = crate::compiler::compile_script_cached(&rt.bundle)?;
        rt.vm.register_image_unit(rt.bundle_hash.clone(), proto);
        rt.vm.restore_image(&img.vm).map_err(|e| e.to_string())?;
        Ok(rt)
    }

    /// Build the VM an image restore for `effects` needs: fresh realm, effect
    /// natives and the memo helper installed, every lazy builtin section
    /// materialized, baseline marked. This is exactly the prologue
    /// [`Self::from_image`] would otherwise run inline, factored out so it can
    /// also be run ahead of time.
    ///
    /// The bundle plays no part — the baseline is the realm plus the host
    /// surface, and the bundle is only compiled (never run) afterwards — which
    /// is what makes a restore target reusable across different programs.
    fn build_baselined(effects: &[&str]) -> BaselinedVm {
        let state = Rc::new(RefCell::new(JournalState::empty()));
        let mut rt = ReplayRuntime {
            vm: Vm::new(),
            bundle: String::new(),
            bundle_hash: String::new(),
            state: state.clone(),
            started: false,
        };
        rt.install_effects(effects);
        rt.install_memo();
        rt.vm.mark_image_baseline();
        BaselinedVm { vm: rt.vm, state }
    }

    /// Pre-build the restore target for `effects` so the next
    /// [`Self::from_blob`]/[`Self::from_image`] with the same effect list skips
    /// the prologue (fresh realm, host surface, baseline walk — about 1.4 ms).
    ///
    /// This does not make a restore cheaper in total; it moves that cost to
    /// wherever this is called. It pays off for the pattern it exists for — a
    /// worker node that restores repeatedly and can prime while idle — and is
    /// a pure loss for a process that restores once. Hence opt-in.
    ///
    /// Holds one baselined VM (~0.7 MB) per distinct effect list until taken;
    /// [`Self::clear_image_restore_pool`] drops them.
    pub fn prime_image_restore(effects: &[&str]) {
        let key: Vec<String> = effects.iter().map(|s| s.to_string()).collect();
        if RESTORE_POOL.with(|p| p.borrow().contains_key(&key)) {
            return;
        }
        // Built outside the pool borrow: constructing a realm runs arbitrary
        // engine code, which must not re-enter a borrowed thread-local.
        let built = Self::build_baselined(effects);
        RESTORE_POOL.with(|p| p.borrow_mut().insert(key, built));
    }

    /// Drop every pooled restore target on this thread.
    pub fn clear_image_restore_pool() {
        RESTORE_POOL.with(|p| p.borrow_mut().clear());
    }

    /// Take the pooled restore target for `effects`, or build one now.
    ///
    /// Always *removes* what it takes: `restore_image` mutates the VM it lands
    /// in, so a target is single-use and must never be handed out twice.
    fn take_baselined(effects: &[&str]) -> BaselinedVm {
        let key: Vec<String> = effects.iter().map(|s| s.to_string()).collect();
        match RESTORE_POOL.with(|p| p.borrow_mut().remove(&key)) {
            Some(b) => b,
            None => Self::build_baselined(effects),
        }
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

    /// Whether this runtime was restored with a bundle whose content hash
    /// differs from the one the journal was recorded against
    /// (modify-and-resume). The journal has always stored the hash; this is
    /// where it is actually checked.
    pub fn bundle_changed(&self) -> bool {
        self.state.borrow().bundle_changed
    }

    /// Install each named effect as a global async function backed by the
    /// journal. Calling `name(...args)` returns a promise that resolves from the
    /// journal (replay) or pends awaiting a live result (record/frontier).
    fn install_effects(&mut self, effects: &[&str]) {
        let global = self.vm.realm.global.clone();
        for name in effects {
            let nm = name.to_string();
            let state = self.state.clone();
            self.vm
                .define_method(&global, name, 1, move |vm, _this, args| {
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
                            if entry.site != site || entry.seq != seq {
                                let msg = format!(
                                "expected effect '{}'#{} from journal but program called '{}'#{} \
                                 (an edit changed already-executed code before the resume point{})",
                                entry.site, entry.seq, site, seq,
                                if s.bundle_changed { "; the code bundle differs from the recorded one" } else { "" }
                            );
                                s.divergence = Some(msg.clone());
                                Decision::Diverged(msg)
                            } else if entry.args != Json::Null && entry.args != args_json {
                                // Same effect, same position, different request:
                                // replaying the recorded result would answer a
                                // question the program no longer asks.
                                let msg = format!(
                                    "effect '{}'#{} was recorded with args {} but the program now \
                                     calls it with {} (an edit changed an already-executed call's \
                                     arguments before the resume point{})",
                                    site, seq, entry.args, args_json,
                                    if s.bundle_changed { "; the code bundle differs from the recorded one" } else { "" }
                                );
                                s.divergence = Some(msg.clone());
                                Decision::Diverged(msg)
                            } else {
                                s.cursor += 1;
                                match entry.outcome {
                                    EffectOutcome::Resolved(j) => Decision::Resolve(j),
                                    EffectOutcome::Rejected(m) => Decision::Reject(m),
                                }
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
        self.vm
            .define_method(&global, "durableStep", 1, move |vm, _this, args| {
                let f = args.first().cloned().unwrap_or(Value::Undefined);
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
                                s.journal.append(
                                    &key,
                                    Json::Null,
                                    EffectOutcome::Resolved(j.clone()),
                                );
                                s.cursor = s.journal.entries.len();
                            }
                            let rv = vm.json_to_value(&j);
                            vm.resolve_host_op(id, rv);
                        }
                        Err(e) => {
                            let msg = vm.error_to_string(&e);
                            {
                                let mut s = state.borrow_mut();
                                s.journal.append(
                                    &key,
                                    Json::Null,
                                    EffectOutcome::Rejected(msg.clone()),
                                );
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
        // Compile through the thread-local source→proto cache: a restore/resume
        // recompiles the SAME bundle the journal was recorded against (the
        // common case — `bundle_hash` pins it), so replaying a run repeatedly
        // (crash recovery, branch delivers, `chidori trace`) pays the oxc
        // pipeline once per thread instead of once per restore. An *edited*
        // bundle is a different source string and simply misses the cache.
        let proto = crate::compiler::compile_script_cached(&self.bundle)?;
        if self.vm.has_image_baseline() {
            // Imaged closures address their bytecode as (unit, const path);
            // the restoring side recompiles the same source and registers the
            // same key, so the paths resolve there too.
            self.vm
                .register_image_unit(self.bundle_hash.clone(), proto.clone());
        }
        let func = self.vm.make_closure(proto, Vec::new());
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
                                        s.journal.append(
                                            &key,
                                            op.args.clone(),
                                            EffectOutcome::Resolved(json.clone()),
                                        );
                                        s.cursor = s.journal.entries.len();
                                    }
                                    let v = self.vm.json_to_value(&json);
                                    self.vm.resolve_host_op(id, v);
                                }
                                Err(msg) => {
                                    {
                                        let mut s = self.state.borrow_mut();
                                        s.journal.append(
                                            &key,
                                            op.args.clone(),
                                            EffectOutcome::Rejected(msg.clone()),
                                        );
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
                    s.journal
                        .append(&key, op.args.clone(), EffectOutcome::Resolved(json.clone()));
                    s.cursor = s.journal.entries.len();
                }
                let v = self.vm.json_to_value(&json);
                self.vm.resolve_host_op(op_id, v);
            }
            Err(msg) => {
                {
                    let mut s = self.state.borrow_mut();
                    s.journal
                        .append(&key, op.args.clone(), EffectOutcome::Rejected(msg.clone()));
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
                    s.journal
                        .append(&key, op.args.clone(), EffectOutcome::Resolved(json.clone()));
                    s.cursor = s.journal.entries.len();
                }
                let v = self.vm.json_to_value(&json);
                self.vm.resolve_host_op(op_id, v);
            }
            Err(msg) => {
                {
                    let mut s = self.state.borrow_mut();
                    s.journal
                        .append(&key, op.args.clone(), EffectOutcome::Rejected(msg.clone()));
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
    /// A VM image of the same suspension point, when one could be taken.
    ///
    /// Strictly a fast path: [`ReplayRuntime::from_blob`] uses it to skip
    /// replay and falls back to bundle+journal whenever it is absent or does
    /// not apply. The field is additive, so an artifact carrying an image
    /// still restores correctly in a reader that has never heard of one — it
    /// ignores the field and replays, which is always right.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<Box<RuntimeImage>>,
}

/// Which path a restore actually took. Worth surfacing: the image is a cache,
/// and a cache that silently stops hitting is a performance bug nobody sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestorePath {
    /// Rebuilt from the VM image — O(live state), no re-execution.
    Image,
    /// Re-executed against the journal. `reason` is `None` when the artifact
    /// carried no image at all, and `Some` when one was present but declined.
    Replay { reason: Option<String> },
}

impl ReplayRuntime {
    /// Serialize the full durable artifact (bundle + journal), including a VM
    /// image when imaging is enabled and this state can be imaged.
    pub fn to_blob(&self, effects: &[&str]) -> Vec<u8> {
        let image = if self.vm.has_image_baseline() {
            self.to_image().ok().map(Box::new)
        } else {
            None
        };
        let blob = DurableBlob {
            bundle: self.bundle.clone(),
            effects: effects.iter().map(|s| s.to_string()).collect(),
            journal: self.journal_bytes(),
            image,
        };
        serde_json::to_vec(&blob).unwrap_or_default()
    }

    /// Reconstruct a runtime from a `to_blob` artifact: from its VM image when
    /// there is a usable one, otherwise by replaying the journal.
    pub fn from_blob(bytes: &[u8]) -> Result<ReplayRuntime, String> {
        Ok(Self::from_blob_reporting(bytes)?.0)
    }

    /// As [`Self::from_blob`], also reporting which path was taken.
    pub fn from_blob_reporting(bytes: &[u8]) -> Result<(ReplayRuntime, RestorePath), String> {
        let blob: DurableBlob = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        let effect_names: Vec<&str> = blob.effects.iter().map(|s| s.as_str()).collect();
        let mut declined = None;
        if let Some(image) = &blob.image {
            match Self::from_image(image, &blob.bundle, &blob.journal, &effect_names) {
                Ok(rt) => return Ok((rt, RestorePath::Image)),
                // Never fatal: the journal can always rebuild this state, so a
                // stale or inapplicable image costs time, not correctness.
                Err(e) => declined = Some(e),
            }
        }
        let rt = Self::from_blob_by_replay(&blob)?;
        Ok((rt, RestorePath::Replay { reason: declined }))
    }

    fn from_blob_by_replay(blob: &DurableBlob) -> Result<ReplayRuntime, String> {
        let effects: Vec<&str> = blob.effects.iter().map(|s| s.as_str()).collect();
        ReplayRuntime::restore(&blob.bundle, &blob.journal, &effects)
    }
}
