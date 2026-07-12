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
}

/// Link/evaluation status of a module record (a subset of the spec's states).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ModuleStatus {
    Unlinked,
    Linked,
    Evaluating,
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
}

impl ModuleRecord {
    pub fn new(compiled: CompiledModule) -> ModuleRecord {
        ModuleRecord {
            compiled,
            resolved: HashMap::new(),
            cells: Vec::new(),
            status: ModuleStatus::Unlinked,
            namespace: None,
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
        // Phase 3: evaluate in dependency post-order.
        self.eval_modules(registry, entry_key, &mut HashSet::new())?;
        Ok(Value::Undefined)
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
                    let ns = self.module_namespace(registry, &dep)?;
                    Rc::new(RefCell::new(ns))
                }
            };
            rec.borrow_mut().cells[local_idx] = cell;
        }
        Ok(())
    }

    /// Phase 3: evaluate `key`'s dependencies, then its body, exactly once.
    fn eval_modules(
        &mut self,
        registry: &ModuleRegistry,
        key: &str,
        done: &mut HashSet<String>,
    ) -> Result<(), Value> {
        if !done.insert(key.to_string()) {
            return Ok(());
        }
        let rec = self.get_module(registry, key)?;
        // Already evaluated by an earlier graph run (e.g. a repeated dynamic
        // `import()` of the same specifier): evaluate-once, like the spec's
        // Evaluated-state short-circuit. Its dependencies are Evaluated too.
        if rec.borrow().status == ModuleStatus::Evaluated {
            return Ok(());
        }
        let (requested, resolved) = {
            let b = rec.borrow();
            (b.compiled.requested.clone(), b.resolved.clone())
        };
        for req in &requested {
            if let Some(dep_key) = resolved.get(req) {
                self.eval_modules(registry, dep_key, done)?;
            }
        }
        // Run the body with the module's pre-allocated (import-wired) cells. Its
        // stable top-level cells mutate in place, so the wired bindings stay live.
        let (proto, cells, has_tla) = {
            let b = rec.borrow();
            (
                b.compiled.proto.clone(),
                b.cells.clone(),
                b.compiled.has_tla,
            )
        };
        let bf = Rc::new(BytecodeFunction {
            proto: proto.clone(),
            upvalues: Vec::new(),
            home_object: None,
            is_class_ctor: false,
            captured_with: Vec::new(),
            captured_priv_env: None,
        });
        let mut frame = self.make_frame(bf, Value::Undefined, &[], Value::Undefined);
        frame.cells = cells;
        rec.borrow_mut().status = ModuleStatus::Evaluated;
        if has_tla {
            // Top-level await: evaluate the body as an async function and drive its
            // evaluation promise to settlement (the test harness model is run-to-
            // quiescence). A rejection is the module's top-level-await failure.
            let promise = self.start_async(frame);
            let _ = self.run_jobs_until_blocked();
            if let Value::Object(p) = &promise {
                if let PromiseState::Rejected(e) = self.promise_state(p) {
                    return Err(e);
                }
            }
            Ok(())
        } else {
            match self.run_frame(frame) {
                Flow::Return(_) => Ok(()),
                Flow::Throw(e) => Err(e),
                Flow::Suspend(_) => {
                    // A non-TLA body should never suspend; surface defensively.
                    Err(self.throw_type("module body suspended unexpectedly"))
                }
            }
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
                            let through = match &imp.import_name {
                                ImportName::Named(n) => Some(n.clone()),
                                ImportName::Default => Some("default".to_string()),
                                // `import * as ns` then `export {ns}`: the
                                // namespace itself is the binding (below).
                                ImportName::Namespace => None,
                            };
                            if let Some(n) = through {
                                let dep_key = resolved
                                    .get(&imp.module_request)
                                    .cloned()
                                    .ok_or_else(|| {
                                        self.throw_syntax(&format!(
                                            "Cannot resolve '{}'",
                                            imp.module_request
                                        ))
                                    })?;
                                let dep = self.get_module(registry, &dep_key)?;
                                return self.resolve_export_cell(registry, &dep, &n, seen);
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
