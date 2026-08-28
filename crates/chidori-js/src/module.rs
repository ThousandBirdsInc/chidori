//! ECMAScript module records, linking, and evaluation.
//!
//! The engine is otherwise script-only; this layer adds Source Text Module
//! Records on top of the existing bytecode/VM. The key idea that makes live
//! bindings cheap: a binding's storage is an `Rc<RefCell<Value>>` cell, and
//! closures already share cells by cloning the `Rc`. So `import {x} from './m'`
//! is implemented by placing module `m`'s *export cell* (the same `Rc`) into the
//! importing module's cell slot for the local name — reads then see `m`'s live
//! value with no extra machinery. See [`link_module`].

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::bytecode::FuncProto;
use crate::value::Value;

/// What an import binds to in the target module's exports.
#[derive(Clone, Debug, PartialEq)]
pub enum ImportName {
    /// `import {a as b}` / `import {a}` — a named export.
    Named(String),
    /// `import d from '…'` — the `default` export.
    Default,
    /// `import * as ns from '…'` — a namespace object.
    Namespace,
}

/// `import local from request` / `import {imported as local} from request`.
#[derive(Clone, Debug)]
pub struct ImportEntry {
    pub module_request: String,
    pub import_name: ImportName,
    pub local_name: String,
}

/// An export's resolution kind.
#[derive(Clone, Debug)]
pub enum ExportKind {
    /// `export <decl>` / `export {local}` / `export default …` — a binding in
    /// THIS module, named by `local_name` (its cell carries the live value).
    Local { local_name: String },
    /// `export {imported as exported} from request` — re-export of another
    /// module's named binding without importing it locally.
    Indirect {
        module_request: String,
        import_name: String,
    },
    /// `export * from request` — star re-export (all of another module's names).
    Star { module_request: String },
}

#[derive(Clone, Debug)]
pub struct ExportEntry {
    /// The name seen by importers (`None` only for `export *` star entries).
    pub export_name: Option<String>,
    pub kind: ExportKind,
}

/// The compiled artifact of a single module's source text.
///
/// `Clone` is shallow (the proto is a shared `Rc`) and exists for the
/// compile cache: a cached artifact is handed out as a clone per run, while
/// all mutable linkage state lives on [`ModuleRecord`], never here.
#[derive(Clone)]
pub struct CompiledModule {
    /// `Rc` so the body proto has a stable identity — the evaluator matches it by
    /// pointer to capture the module's final cells (see `Vm::module_capture`).
    pub proto: Rc<FuncProto>,
    pub imports: Vec<ImportEntry>,
    pub exports: Vec<ExportEntry>,
    /// Distinct requested specifiers, in source order.
    pub requested: Vec<String>,
    /// Cell index (into the module body's cells) for each top-level binding name
    /// — both local declarations and imported locals. Linking pre-allocates the
    /// cell vector and overwrites the import slots with the exporter's cells.
    pub cell_of_name: HashMap<String, u32>,
    /// Number of cells the module body's frame uses.
    pub num_cells: u32,
    /// Whether the body has top-level `await` (so it must be evaluated as an
    /// async function whose evaluation promise the linker drives to settle).
    /// Detected by the presence of `Op::Await` in the module proto's own code —
    /// nested async functions compile to separate protos, so this is exact.
    pub has_tla: bool,
    /// Hoisted top-level function declarations (`export default function`
    /// included) as `(cell index, Const::Func index)`. The linker initializes
    /// these at LINK time (spec InitializeEnvironment), so a cyclic importer
    /// that evaluates first can already call them.
    pub hoisted_funcs: Vec<(u32, u32)>,
}

/// Link/evaluation status of a module record (a subset of the spec's states).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ModuleStatus {
    Unlinked,
    Linked,
    Evaluating,
    /// Started (or waiting to start) an async (top-level-await) evaluation
    /// that has not settled yet — spec ~evaluating-async~.
    EvaluatingAsync,
    Evaluated,
}

/// A Source Text Module Record plus its runtime linkage state.
pub struct ModuleRecord {
    pub compiled: CompiledModule,
    /// Each `module_request` specifier resolved to its registry key, filled by the
    /// host loader before evaluation.
    pub resolved: HashMap<String, String>,
    /// This module's final top-level binding cells, captured after its body runs;
    /// importers read its export cells from here for live bindings.
    pub cells: Vec<Rc<RefCell<Value>>>,
    pub status: ModuleStatus,
    /// The module namespace exotic object (built lazily for `import * as ns`).
    pub namespace: Option<Value>,
    /// Spec `[[EvaluationError]]`: the error a (sync or async) evaluation threw,
    /// cached so re-importing an errored module rejects with the same error.
    pub eval_error: Option<Value>,
    /// Spec `[[PendingAsyncDependencies]]`: dependencies still evaluating-async.
    pub pending_async_deps: usize,
    /// Spec `[[AsyncEvaluationOrder]]`: assigned when the module transitions to
    /// evaluating-async; orders the ancestor exec list on fulfillment.
    pub async_order: Option<u64>,
    /// Spec `[[AsyncParentModules]]`: importers (registry keys) whose execution
    /// waits on this module. A parent appears once per async dependency edge.
    pub waiters: Vec<String>,
    /// Promises to settle when this module leaves evaluating-async (the entry's
    /// evaluation promise and any dynamic `import()` of an in-flight module),
    /// each tagged with the registry key whose NAMESPACE resolves it — an
    /// `import()` of a non-root cycle member parks its promise on the cycle
    /// root but must still resolve with the requested module's namespace.
    pub completion_promises: Vec<(crate::value::JsObject, String)>,
    /// Spec `[[DFSIndex]]` / `[[DFSAncestorIndex]]` (Tarjan SCC bookkeeping
    /// during evaluation) and `[[CycleRoot]]`: the root of this module's
    /// strongly-connected component — importers of any cycle member wait on
    /// the root, and the whole component finishes together.
    pub dfs_ancestor: usize,
    pub cycle_root: Option<String>,
}

impl ModuleRecord {
    pub fn new(compiled: CompiledModule) -> ModuleRecord {
        ModuleRecord {
            compiled,
            resolved: HashMap::new(),
            cells: Vec::new(),
            status: ModuleStatus::Unlinked,
            namespace: None,
            eval_error: None,
            pending_async_deps: 0,
            async_order: None,
            waiters: Vec::new(),
            completion_promises: Vec::new(),
            dfs_ancestor: 0,
            cycle_root: None,
        }
    }
}

/// A module graph keyed by resolved specifier (canonical path string).
/// `Clone` is shallow (the records are shared `Rc`s), so a host can snapshot
/// the registry to evaluate a graph without holding a `RefCell` borrow open —
/// a dynamic `import()` job firing mid-evaluation (top-level await) needs to
/// re-borrow the live registry to load new modules.
#[derive(Default, Clone)]
pub struct ModuleRegistry {
    pub modules: HashMap<String, Rc<RefCell<ModuleRecord>>>,
}

use crate::value::{BytecodeFunction, Property, PropertyKey};
use crate::vm::{Flow, PromiseState, Vm};
use std::collections::HashSet;

impl Vm {
    /// Link and evaluate a fully-loaded module graph rooted at `entry_key`. The
    /// registry must already contain every transitively-requested module with its
    /// `resolved` map filled by the host loader. Three phases (per spec): allocate
    /// every module's stable top-level cells, wire imports to the exporter's cell
    /// (so self/circular live bindings resolve), then evaluate depth-first in
    /// post-order. Returns the entry's thrown error, if any.
    pub fn run_module_graph(
        &mut self,
        registry: &ModuleRegistry,
        entry_key: &str,
    ) -> Result<Value, Value> {
        self.link_module_graph(registry, entry_key)?;
        // Phase 3: evaluate in dependency post-order (async-aware). The
        // synchronous contract here (embedder entrypoints, the test harness)
        // drives jobs to quiescence when the entry is still evaluating-async
        // and reports the entry's rejection as the thrown error.
        self.eval_modules(registry, entry_key)?;
        let rec = self.get_module(registry, entry_key)?;
        if rec.borrow().status == ModuleStatus::EvaluatingAsync {
            let p = self.new_promise();
            rec.borrow_mut()
                .completion_promises
                .push((p.clone(), entry_key.to_string()));
            let _ = self.run_jobs_until_blocked();
            if let PromiseState::Rejected(e) = self.promise_state(&p) {
                return Err(e);
            }
        } else if let Some(e) = rec.borrow().eval_error.clone() {
            return Err(e);
        }
        Ok(Value::Undefined)
    }

    /// Phases 1–2.6 of `run_module_graph`: allocate stable cells, wire imports,
    /// validate indirect exports, create hoisted functions and `import.meta`.
    /// Idempotent for already-linked (or evaluating/evaluated) subgraphs.
    fn link_module_graph(
        &mut self,
        registry: &ModuleRegistry,
        entry_key: &str,
    ) -> Result<(), Value> {
        // Phase 1: allocate cells for every reachable module.
        let mut order: Vec<String> = Vec::new();
        let mut seen = HashSet::new();
        self.alloc_module_cells(registry, entry_key, &mut seen, &mut order)?;
        // Phase 2: wire imports (every module's cells now exist).
        for key in &order {
            self.wire_module_imports(registry, key)?;
        }
        // Spec InitializeEnvironment step 9: every reachable module's INDIRECT
        // export entries must resolve — not missing, not circular, not
        // ambiguous — even when nothing imports them. A failure is a
        // link-time SyntaxError.
        for key in &order {
            let rec = self.get_module(registry, key)?;
            let exports = rec.borrow().compiled.exports.clone();
            for e in &exports {
                if matches!(e.kind, ExportKind::Indirect { .. }) {
                    if let Some(name) = e.export_name.as_deref() {
                        self.resolve_export_cell(registry, &rec, name, &mut HashSet::new())?;
                    }
                }
            }
        }
        // Phase 2.6 (spec InitializeEnvironment step 35): hoisted function
        // declarations are created at LINK time, so a cyclic importer whose
        // body evaluates first can already call them. The body's own hoist
        // pass re-initializes the same (stable) cells at evaluation.
        for key in &order {
            let rec = self.get_module(registry, key)?;
            // Only freshly-linked modules: re-creating hoisted closures for a
            // module that already started evaluating would reset live bindings.
            if rec.borrow().status != ModuleStatus::Linked {
                continue;
            }
            let (hoisted, consts) = {
                let b = rec.borrow();
                (b.compiled.hoisted_funcs.clone(), b.compiled.proto.clone())
            };
            for (cell_idx, const_idx) in &hoisted {
                let proto = match consts.consts.get(*const_idx as usize) {
                    Some(crate::bytecode::Const::Func(p)) => p.clone(),
                    _ => continue,
                };
                let upvalues: Vec<Rc<RefCell<Value>>> = {
                    let b = rec.borrow();
                    proto
                        .upvalues
                        .iter()
                        .map(|src| match src {
                            crate::bytecode::UpvalueSource::ParentCell(i) => {
                                b.cells[*i as usize].clone()
                            }
                            // A module top-level function has no grandparent
                            // frame; unreachable by construction.
                            crate::bytecode::UpvalueSource::ParentUpvalue(_) => {
                                Rc::new(RefCell::new(Value::Undefined))
                            }
                        })
                        .collect()
                };
                let f = self.make_closure(proto, upvalues);
                let cell = rec.borrow().cells[*cell_idx as usize].clone();
                *cell.borrow_mut() = Value::Object(f);
            }
            // `import.meta`: one ordinary null-prototype object per module
            // (spec 13.3.12.1 + HostGetImportMetaProperties default), created
            // once and shared by every `import.meta` in the module.
            let meta_cell = {
                let b = rec.borrow();
                b.compiled
                    .cell_of_name
                    .get("%importmeta")
                    .map(|i| b.cells[*i as usize].clone())
            };
            if let Some(cell) = meta_cell {
                if matches!(*cell.borrow(), Value::Uninitialized) {
                    let meta = self.new_object();
                    meta.borrow_mut().proto = None;
                    *cell.borrow_mut() = Value::Object(meta);
                }
            }
        }
        Ok(())
    }

    /// Evaluate for a dynamic `import()`: link, then return a promise that
    /// resolves with the module's namespace once evaluation completes
    /// (immediately for an already-evaluated module, later for a module that
    /// is — or transitively waits on — an in-flight top-level-await body) and
    /// rejects with the module's (possibly cached) evaluation error.
    pub fn module_evaluate_promise(
        &mut self,
        registry: &ModuleRegistry,
        key: &str,
    ) -> Result<Value, Value> {
        self.link_module_graph(registry, key)?;
        let p = self.new_promise();
        let rec = self.get_module(registry, key)?;
        let mut status = rec.borrow().status;
        if matches!(status, ModuleStatus::Unlinked | ModuleStatus::Linked) {
            if let Err(e) = self.eval_modules(registry, key) {
                self.reject_promise(&p, e);
                return Ok(Value::Object(p));
            }
            status = rec.borrow().status;
        }
        match status {
            ModuleStatus::EvaluatingAsync => {
                // Spec Evaluate() on an evaluating-async module returns its
                // CYCLE ROOT's capability; the promise still resolves with
                // the REQUESTED module's namespace (the tag).
                let root_key = rec.borrow().cycle_root.clone();
                let park = match root_key {
                    Some(rk) if rk != key => self.get_module(registry, &rk)?,
                    _ => rec.clone(),
                };
                park.borrow_mut()
                    .completion_promises
                    .push((p.clone(), key.to_string()));
            }
            _ => {
                let err = rec.borrow().eval_error.clone();
                match err {
                    Some(e) => self.reject_promise(&p, e),
                    None => {
                        let ns = self.module_namespace(registry, &rec)?;
                        self.resolve_promise(&p, ns);
                    }
                }
            }
        }
        Ok(Value::Object(p))
    }

    fn get_module(
        &mut self,
        registry: &ModuleRegistry,
        key: &str,
    ) -> Result<Rc<RefCell<ModuleRecord>>, Value> {
        registry
            .modules
            .get(key)
            .cloned()
            .ok_or_else(|| self.throw_type(&format!("module not found: {key}")))
    }

    /// Phase 1: DFS-allocate each module's stable top-level cells (all in TDZ).
    /// `order` collects every reachable module key (for the wire pass).
    fn alloc_module_cells(
        &mut self,
        registry: &ModuleRegistry,
        key: &str,
        seen: &mut HashSet<String>,
        order: &mut Vec<String>,
    ) -> Result<(), Value> {
        if !seen.insert(key.to_string()) {
            return Ok(());
        }
        let rec = self.get_module(registry, key)?;
        let (num_cells, requested, resolved) = {
            let b = rec.borrow();
            (
                b.compiled.num_cells,
                b.compiled.requested.clone(),
                b.resolved.clone(),
            )
        };
        {
            let mut b = rec.borrow_mut();
            if b.cells.is_empty() {
                // All cells start in TDZ; the body's hoist initializes `var` and
                // function cells (and TDZ-marks lexicals) in place.
                b.cells = (0..num_cells)
                    .map(|_| Rc::new(RefCell::new(Value::Uninitialized)))
                    .collect();
            }
            // Don't downgrade a module already evaluated by an earlier graph run
            // (repeated dynamic import); evaluate-once relies on the status.
            if b.status == ModuleStatus::Unlinked {
                b.status = ModuleStatus::Linked;
            }
        }
        order.push(key.to_string());
        for req in &requested {
            let dep_key = resolved.get(req).cloned().ok_or_else(|| {
                self.throw_syntax(&format!("Cannot resolve module specifier '{req}'"))
            })?;
            self.alloc_module_cells(registry, &dep_key, seen, order)?;
        }
        Ok(())
    }

    /// Phase 2: bind each import's local cell to the exporter's live export cell
    /// (or a namespace object). A missing/ambiguous export is a SyntaxError.
    fn wire_module_imports(&mut self, registry: &ModuleRegistry, key: &str) -> Result<(), Value> {
        let rec = self.get_module(registry, key)?;
        let (imports, resolved, cell_of_name) = {
            let b = rec.borrow();
            (
                b.compiled.imports.clone(),
                b.resolved.clone(),
                b.compiled.cell_of_name.clone(),
            )
        };
        for imp in &imports {
            let dep_key = resolved.get(&imp.module_request).cloned().ok_or_else(|| {
                self.throw_syntax(&format!(
                    "Cannot resolve module specifier '{}'",
                    imp.module_request
                ))
            })?;
            let dep = self.get_module(registry, &dep_key)?;
            let local_idx = *cell_of_name.get(&imp.local_name).ok_or_else(|| {
                self.throw_type(&format!("import local '{}' has no cell", imp.local_name))
            })? as usize;
            let cell = match &imp.import_name {
                ImportName::Named(name) => {
                    self.resolve_export_cell(registry, &dep, name, &mut HashSet::new())?
                }
                ImportName::Default => {
                    self.resolve_export_cell(registry, &dep, "default", &mut HashSet::new())?
                }
                ImportName::Namespace => {
                    // Fill the PRE-ALLOCATED cell instead of replacing the slot:
                    // a re-exported namespace import (`import * as z; export {z}`)
                    // resolves to this slot's cell, and an importer wired earlier
                    // in the graph order may already hold it. Swapping in a fresh
                    // Rc would strand that importer on the original cell —
                    // permanently Uninitialized (this was how `import { z } from
                    // "zod"` produced "Cannot access binding before
                    // initialization" forever).
                    let ns = self.module_namespace(registry, &dep)?;
                    let cell = rec.borrow().cells[local_idx].clone();
                    *cell.borrow_mut() = ns;
                    continue;
                }
            };
            rec.borrow_mut().cells[local_idx] = cell;
        }
        Ok(())
    }

    /// Phase 3: evaluate `key`'s dependencies, then its body, exactly once.
    /// Spec Evaluate() steps 5–9 around InnerModuleEvaluation: on an abrupt
    /// completion, every module still on the DFS stack records the error
    /// (status evaluated + `[[EvaluationError]]`).
    fn eval_modules(&mut self, registry: &ModuleRegistry, key: &str) -> Result<(), Value> {
        let mut stack: Vec<String> = Vec::new();
        match self.inner_module_evaluation(registry, key, &mut stack, 0) {
            Ok(_) => Ok(()),
            Err(e) => {
                for k in stack {
                    if let Ok(rec) = self.get_module(registry, &k) {
                        let _ = self.module_eval_failed(&rec, e.clone());
                    }
                }
                Err(e)
            }
        }
    }

    /// Spec InnerModuleEvaluation. Bodies execute depth-first post-order; a
    /// module with top-level await starts its async body WITHOUT blocking
    /// siblings, and a module with an async dependency (its cycle root, for
    /// an already-popped component) defers its own execution, registering as
    /// a waiter (spec `[[AsyncParentModules]]`). Strongly-connected
    /// components are detected with the spec's DFS-index bookkeeping so every
    /// member learns its `[[CycleRoot]]`.
    fn inner_module_evaluation(
        &mut self,
        registry: &ModuleRegistry,
        key: &str,
        stack: &mut Vec<String>,
        mut index: usize,
    ) -> Result<usize, Value> {
        let rec = self.get_module(registry, key)?;
        match rec.borrow().status {
            // Evaluate-once: a repeated import re-throws the cached error.
            ModuleStatus::Evaluated => {
                return match rec.borrow().eval_error.clone() {
                    Some(e) => Err(e),
                    None => Ok(index),
                };
            }
            // In-flight async component (the importer counts its cycle root as
            // pending below) or a member already on the current DFS stack.
            ModuleStatus::EvaluatingAsync | ModuleStatus::Evaluating => return Ok(index),
            _ => {}
        }
        let my_index = index;
        {
            let mut b = rec.borrow_mut();
            b.status = ModuleStatus::Evaluating;
            b.dfs_ancestor = my_index;
        }
        index += 1;
        stack.push(key.to_string());
        let (requested, resolved, has_tla) = {
            let b = rec.borrow();
            (
                b.compiled.requested.clone(),
                b.resolved.clone(),
                b.compiled.has_tla,
            )
        };
        let mut pending = 0usize;
        for req in &requested {
            let dep_key = match resolved.get(req) {
                Some(k) => k.clone(),
                None => continue,
            };
            index = self.inner_module_evaluation(registry, &dep_key, stack, index)?;
            let dep = self.get_module(registry, &dep_key)?;
            let dep_status = dep.borrow().status;
            // For a member of an already-finished component, the module to
            // wait on (and whose error to inherit) is its CYCLE ROOT.
            let required = if dep_status == ModuleStatus::Evaluating {
                let da = dep.borrow().dfs_ancestor;
                let mut b = rec.borrow_mut();
                b.dfs_ancestor = b.dfs_ancestor.min(da);
                dep.clone()
            } else {
                let root_key = dep.borrow().cycle_root.clone();
                let root = match root_key {
                    Some(rk) if rk != dep_key => self.get_module(registry, &rk)?,
                    _ => dep.clone(),
                };
                let err = root.borrow().eval_error.clone();
                if let Some(e) = err {
                    return Err(e);
                }
                root
            };
            // Spec 11.c.v: an async-evaluating requirement (a same-cycle
            // member that already started its TLA body included) makes this
            // module pending on it.
            let is_async = {
                let b = required.borrow();
                b.async_order.is_some() && b.status != ModuleStatus::Evaluated
            };
            if is_async {
                pending += 1;
                required.borrow_mut().waiters.push(key.to_string());
            }
        }
        rec.borrow_mut().pending_async_deps = pending;
        if pending > 0 || has_tla {
            // Stamp the global async-evaluation order (spec
            // IncrementModuleAsyncEvaluationCount).
            self.module_async_order += 1;
            rec.borrow_mut().async_order = Some(self.module_async_order);
        }
        if pending == 0 {
            if has_tla {
                self.execute_async_module(registry, key);
            } else {
                self.run_module_body(&rec)?;
            }
        }
        // SCC pop: this module is its component's root — every member popped
        // here learns the root and its settled-or-async status.
        if rec.borrow().dfs_ancestor == my_index {
            while let Some(ckey) = stack.pop() {
                if let Ok(c) = self.get_module(registry, &ckey) {
                    let mut b = c.borrow_mut();
                    b.cycle_root = Some(key.to_string());
                    b.status = if b.async_order.is_some() {
                        ModuleStatus::EvaluatingAsync
                    } else {
                        ModuleStatus::Evaluated
                    };
                }
                if ckey == key {
                    break;
                }
            }
        }
        Ok(index)
    }

    /// Run a module body synchronously with its pre-allocated (import-wired)
    /// cells. The stable top-level cells mutate in place, keeping the wired
    /// bindings live.
    fn run_module_body(&mut self, rec: &Rc<RefCell<ModuleRecord>>) -> Result<(), Value> {
        let frame = self.module_frame(rec);
        match self.run_frame(frame) {
            Flow::Return(_) => Ok(()),
            Flow::Throw(e) => Err(e),
            Flow::Suspend(_) => {
                // A non-TLA body should never suspend; surface defensively.
                Err(self.throw_type("module body suspended unexpectedly"))
            }
        }
    }

    fn module_frame(&mut self, rec: &Rc<RefCell<ModuleRecord>>) -> Box<crate::vm::Frame> {
        let (proto, cells) = {
            let b = rec.borrow();
            (b.compiled.proto.clone(), b.cells.clone())
        };
        let bf = Rc::new(BytecodeFunction {
            proto,
            upvalues: Vec::new(),
            home_object: None,
            is_class_ctor: false,
            captured_with: Vec::new(),
            captured_priv_env: None,
        });
        let mut frame = self.make_frame(bf, Value::Undefined, &[], Value::Undefined);
        frame.cells = cells;
        frame
    }

    /// Record a module's evaluation error (spec `[[EvaluationError]]`), mark it
    /// evaluated, reject its completion promises, and hand the error back.
    fn module_eval_failed(&mut self, rec: &Rc<RefCell<ModuleRecord>>, e: Value) -> Value {
        let promises = {
            let mut b = rec.borrow_mut();
            if b.eval_error.is_none() {
                b.eval_error = Some(e.clone());
            }
            b.status = ModuleStatus::Evaluated;
            std::mem::take(&mut b.completion_promises)
        };
        for (p, _) in promises {
            self.reject_promise(&p, e.clone());
        }
        e
    }

    /// Mark a module evaluated and resolve its completion promises with its
    /// namespace object.
    fn module_eval_done(&mut self, registry: &ModuleRegistry, rec: &Rc<RefCell<ModuleRecord>>) {
        let promises = {
            let mut b = rec.borrow_mut();
            b.status = ModuleStatus::Evaluated;
            std::mem::take(&mut b.completion_promises)
        };
        for (p, ns_key) in promises {
            let ns = self
                .module_namespace_by_key(registry, &ns_key)
                .unwrap_or(Value::Undefined);
            self.resolve_promise(&p, ns);
        }
    }

    /// Spec ExecuteAsyncModule: start the TLA body as an async function and
    /// attach callbacks that resume the module graph on settlement. The
    /// callbacks prefer the host's LIVE registry (`Vm::module_registry`) so
    /// waiters linked by later dynamic imports resolve; the captured snapshot
    /// is the fallback (its records are shared `Rc`s either way).
    fn execute_async_module(&mut self, registry: &ModuleRegistry, key: &str) {
        let rec = match self.get_module(registry, key) {
            Ok(r) => r,
            Err(_) => return,
        };
        let frame = self.module_frame(&rec);
        let promise = self.start_async(frame);
        let p = match promise {
            Value::Object(p) => p,
            _ => return,
        };
        let snap = registry.clone();
        let k = key.to_string();
        let on_f = self.new_native("", 0, move |vm: &mut Vm, _t, _a: &[Value]| {
            let reg = match vm.module_registry.as_ref() {
                Some(r) => r.borrow().clone(),
                None => snap.clone(),
            };
            vm.async_module_fulfilled(&reg, &k);
            Ok(Value::Undefined)
        });
        let snap2 = registry.clone();
        let k2 = key.to_string();
        let on_r = self.new_native("", 1, move |vm: &mut Vm, _t, args: &[Value]| {
            let e = args.first().cloned().unwrap_or(Value::Undefined);
            let reg = match vm.module_registry.as_ref() {
                Some(r) => r.borrow().clone(),
                None => snap2.clone(),
            };
            vm.async_module_rejected(&reg, &k2, e);
            Ok(Value::Undefined)
        });
        self.promise_then(&p, Value::Object(on_f), Value::Object(on_r));
    }

    /// Spec AsyncModuleExecutionFulfilled: mark this module done, settle its
    /// completion promises, then execute every ancestor whose pending count
    /// reached zero — in async-evaluation order (leaf to root).
    fn async_module_fulfilled(&mut self, registry: &ModuleRegistry, key: &str) {
        let rec = match self.get_module(registry, key) {
            Ok(r) => r,
            Err(_) => return,
        };
        if rec.borrow().eval_error.is_some() {
            return; // already rejected through a dependency's failure
        }
        self.module_eval_done(registry, &rec);
        let mut exec_list: Vec<String> = Vec::new();
        self.gather_available_ancestors(registry, key, &mut exec_list);
        let mut sorted: Vec<(u64, String)> = exec_list
            .into_iter()
            .filter_map(|k| {
                let order = self
                    .get_module(registry, &k)
                    .ok()
                    .and_then(|r| r.borrow().async_order);
                order.map(|o| (o, k))
            })
            .collect();
        sorted.sort_by_key(|(o, _)| *o);
        for (_, wkey) in sorted {
            let w = match self.get_module(registry, &wkey) {
                Ok(r) => r,
                Err(_) => continue,
            };
            // Rejected while this batch ran (a dependency's rejection
            // propagated): spec skips it here.
            if w.borrow().eval_error.is_some() {
                continue;
            }
            if w.borrow().compiled.has_tla {
                self.execute_async_module(registry, &wkey);
            } else {
                match self.run_module_body(&w) {
                    Ok(()) => self.module_eval_done(registry, &w),
                    Err(e) => self.async_module_rejected(registry, &wkey, e),
                }
            }
        }
    }

    /// Spec GatherAvailableAncestors: decrement each waiter's pending count;
    /// a waiter reaching zero joins the exec list, and — when it has no TLA of
    /// its own (it will run synchronously in this batch) — its own waiters are
    /// gathered transitively.
    fn gather_available_ancestors(
        &mut self,
        registry: &ModuleRegistry,
        key: &str,
        exec_list: &mut Vec<String>,
    ) {
        let rec = match self.get_module(registry, key) {
            Ok(r) => r,
            Err(_) => return,
        };
        let waiters = rec.borrow().waiters.clone();
        for wkey in waiters {
            let w = match self.get_module(registry, &wkey) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let (ready, has_tla) = {
                let mut b = w.borrow_mut();
                if b.eval_error.is_some()
                    || b.status == ModuleStatus::Evaluated
                    || exec_list.contains(&wkey)
                {
                    continue;
                }
                b.pending_async_deps = b.pending_async_deps.saturating_sub(1);
                (b.pending_async_deps == 0, b.compiled.has_tla)
            };
            if !ready {
                continue;
            }
            exec_list.push(wkey.clone());
            if !has_tla {
                self.gather_available_ancestors(registry, &wkey, exec_list);
            }
        }
    }

    /// Spec AsyncModuleExecutionRejected: cache the error, reject completion
    /// promises, and propagate to every waiting importer.
    fn async_module_rejected(&mut self, registry: &ModuleRegistry, key: &str, e: Value) {
        let rec = match self.get_module(registry, key) {
            Ok(r) => r,
            Err(_) => return,
        };
        {
            let b = rec.borrow();
            if b.eval_error.is_some() || b.status == ModuleStatus::Evaluated {
                return; // already settled
            }
        }
        let e = self.module_eval_failed(&rec, e);
        let waiters = rec.borrow().waiters.clone();
        for wkey in waiters {
            self.async_module_rejected(registry, &wkey, e.clone());
        }
    }

    /// `ResolveExport(module, name)` → the live cell backing that export. Follows
    /// indirect re-exports and `export *`; a missing or ambiguous name is a
    /// SyntaxError (the resolution-phase negative tests).
    fn resolve_export_cell(
        &mut self,
        registry: &ModuleRegistry,
        module: &Rc<RefCell<ModuleRecord>>,
        name: &str,
        seen: &mut HashSet<String>,
    ) -> Result<Rc<RefCell<Value>>, Value> {
        // Circularity guard (spec: ResolveExport's resolveSet): revisiting the
        // same (module, export name) pair means a circular `export {x} from`
        // chain that never reaches a concrete binding — a SyntaxError, matching
        // the resolution-phase negative tests.
        let guard = format!("{:p}\u{0}{name}", Rc::as_ptr(module));
        if !seen.insert(guard) {
            return Err(self.throw_syntax(&format!("circular import of '{name}'")));
        }
        let exports = module.borrow().compiled.exports.clone();
        let resolved = module.borrow().resolved.clone();
        // Direct local / indirect exports.
        for e in &exports {
            if e.export_name.as_deref() == Some(name) {
                match &e.kind {
                    ExportKind::Local { local_name } => {
                        // A re-export of an IMPORTED binding resolves through
                        // to the source module's export (spec ResolveExport
                        // follows import entries), which also makes the
                        // star-ambiguity identity check order-independent.
                        let imp = module
                            .borrow()
                            .compiled
                            .imports
                            .iter()
                            .find(|i| &i.local_name == local_name)
                            .cloned();
                        if let Some(imp) = imp {
                            let dep_key =
                                resolved.get(&imp.module_request).cloned().ok_or_else(|| {
                                    self.throw_syntax(&format!(
                                        "Cannot resolve '{}'",
                                        imp.module_request
                                    ))
                                })?;
                            let dep = self.get_module(registry, &dep_key)?;
                            match &imp.import_name {
                                ImportName::Named(n) => {
                                    let n = n.clone();
                                    return self.resolve_export_cell(registry, &dep, &n, seen);
                                }
                                ImportName::Default => {
                                    return self
                                        .resolve_export_cell(registry, &dep, "default", seen);
                                }
                                // `import * as ns` then `export {ns}` is spec
                                // ParseModule's namespace re-export: the binding
                                // IS the dependency's namespace object, so two
                                // stars re-exporting namespaces of the same
                                // module resolve unambiguously (identity is the
                                // namespace object, not this module's cell).
                                ImportName::Namespace => {
                                    let ns = self.module_namespace(registry, &dep)?;
                                    return Ok(Rc::new(RefCell::new(ns)));
                                }
                            }
                        }
                        let idx = *module
                            .borrow()
                            .compiled
                            .cell_of_name
                            .get(local_name)
                            .ok_or_else(|| {
                                self.throw_syntax(&format!("export '{name}' has no binding"))
                            })?;
                        let cell = module.borrow().cells.get(idx as usize).cloned();
                        return cell.ok_or_else(|| {
                            self.throw_syntax(&format!(
                                "export '{name}' referenced before module evaluated"
                            ))
                        });
                    }
                    ExportKind::Indirect {
                        module_request,
                        import_name,
                    } => {
                        let dep_key = resolved.get(module_request).cloned().ok_or_else(|| {
                            self.throw_syntax(&format!("Cannot resolve '{module_request}'"))
                        })?;
                        let dep = self.get_module(registry, &dep_key)?;
                        return self.resolve_export_cell(registry, &dep, import_name, seen);
                    }
                    ExportKind::Star { module_request } => {
                        // `export * as name from "mod"`: the export IS the
                        // dependency's namespace object.
                        let dep_key = resolved.get(module_request).cloned().ok_or_else(|| {
                            self.throw_syntax(&format!("Cannot resolve '{module_request}'"))
                        })?;
                        let dep = self.get_module(registry, &dep_key)?;
                        let ns = self.module_namespace(registry, &dep)?;
                        return Ok(Rc::new(RefCell::new(ns)));
                    }
                }
            }
        }
        // `export *` star re-exports. Two stars resolving the name to
        // DIFFERENT bindings make it ambiguous — a SyntaxError per spec
        // ResolveExport (the "ambiguous" resolution). "default" is NEVER
        // provided by a star export.
        if name == "default" {
            return Err(self.throw_syntax("Module does not provide export 'default'"));
        }
        let mut star_resolution: Option<Rc<RefCell<Value>>> = None;
        for e in &exports {
            if let ExportKind::Star { module_request } = &e.kind {
                if e.export_name.is_none() {
                    let dep_key = match resolved.get(module_request) {
                        Some(k) => k.clone(),
                        None => continue,
                    };
                    let guard = format!("{dep_key}\u{0}{name}");
                    if !seen.insert(guard) {
                        continue;
                    }
                    let dep = self.get_module(registry, &dep_key)?;
                    if let Ok(cell) = self.resolve_export_cell(registry, &dep, name, seen) {
                        // Two cells are the SAME binding when they are the
                        // same Rc, or both hold the same object (namespace
                        // re-exports mint fresh cells around one namespace).
                        let same_binding = |a: &Rc<RefCell<Value>>, b: &Rc<RefCell<Value>>| {
                            Rc::ptr_eq(a, b)
                                || matches!(
                                    (&*a.borrow(), &*b.borrow()),
                                    (Value::Object(x), Value::Object(y)) if x.same(y)
                                )
                        };
                        match &star_resolution {
                            Some(prev) if !same_binding(prev, &cell) => {
                                return Err(self
                                    .throw_syntax(&format!("ambiguous star export of '{name}'")));
                            }
                            _ => star_resolution = Some(cell),
                        }
                    }
                }
            }
        }
        if let Some(cell) = star_resolution {
            return Ok(cell);
        }
        Err(self.throw_syntax(&format!("Module does not provide export '{name}'")))
    }

    /// Spec `GetExportedNames`: this module's own export names plus — minus
    /// "default" — the exported names of its `export *` dependencies,
    /// cycle-safe. Ambiguous names stay listed here; namespace construction
    /// drops the ones whose resolution fails.
    fn exported_names(
        &mut self,
        registry: &ModuleRegistry,
        module: &Rc<RefCell<ModuleRecord>>,
        seen: &mut HashSet<usize>,
    ) -> Result<Vec<String>, Value> {
        if !seen.insert(Rc::as_ptr(module) as usize) {
            return Ok(Vec::new());
        }
        let exports = module.borrow().compiled.exports.clone();
        let resolved = module.borrow().resolved.clone();
        let mut names: Vec<String> = Vec::new();
        for e in &exports {
            if let Some(n) = &e.export_name {
                if !names.contains(n) {
                    names.push(n.clone());
                }
            } else if let ExportKind::Star { module_request } = &e.kind {
                if let Some(dep_key) = resolved.get(module_request) {
                    let dep = self.get_module(registry, dep_key)?;
                    for n in self.exported_names(registry, &dep, seen)? {
                        if n != "default" && !names.contains(&n) {
                            names.push(n);
                        }
                    }
                }
            }
        }
        Ok(names)
    }

    /// The namespace object for a registry module by key — the value a dynamic
    /// `import(specifier)` resolves with (the module must already be evaluated).
    pub fn module_namespace_by_key(
        &mut self,
        registry: &ModuleRegistry,
        key: &str,
    ) -> Result<Value, Value> {
        let rec = self.get_module(registry, key)?;
        self.module_namespace(registry, &rec)
    }

    /// Build (and cache) the Module Namespace object for `import * as ns`: an
    /// object with a live accessor per export name and `@@toStringTag = "Module"`.
    fn module_namespace(
        &mut self,
        registry: &ModuleRegistry,
        module: &Rc<RefCell<ModuleRecord>>,
    ) -> Result<Value, Value> {
        if let Some(ns) = &module.borrow().namespace {
            return Ok(ns.clone());
        }
        // Publish an EMPTY namespace object before resolving exports: a cyclic
        // `export * as ns` graph re-enters here and must get this same object
        // (filled below by the outermost call) instead of recursing forever.
        let obj = self.alloc(crate::value::ObjectData::new(
            None,
            crate::value::Internal::ModuleNamespace(Box::new(crate::value::NamespaceData {
                exports: indexmap::IndexMap::new(),
            })),
        ));
        module.borrow_mut().namespace = Some(Value::Object(obj.clone()));
        let mut names = self.exported_names(registry, module, &mut HashSet::new())?;
        names.sort();
        // Module Namespace exotic object: null prototype, non-extensible,
        // exports backed by the live binding cells (see `Internal::ModuleNamespace`
        // dispatch in the VM's property paths).
        let mut export_cells: indexmap::IndexMap<crate::value::JsString, Rc<RefCell<Value>>> =
            indexmap::IndexMap::new();
        for n in &names {
            if let Ok(cell) = self.resolve_export_cell(registry, module, n, &mut HashSet::new()) {
                export_cells.insert(crate::value::JsString::new(n), cell);
            }
        }
        if let crate::value::Internal::ModuleNamespace(ns) = &mut obj.borrow_mut().internal {
            ns.exports = export_cells;
        }
        {
            let mut b = obj.borrow_mut();
            b.extensible = false;
            // @@toStringTag = "Module" — non-writable, non-enumerable,
            // non-configurable (spec 28.3.1).
            let tag = self.realm.symbol_to_string_tag.clone();
            b.own_insert(
                PropertyKey::Sym(tag),
                Property {
                    kind: crate::value::PropertyKind::Data {
                        value: Value::str("Module"),
                        writable: false,
                    },
                    enumerable: false,
                    configurable: false,
                },
            );
        }
        Ok(Value::Object(obj))
    }
}
