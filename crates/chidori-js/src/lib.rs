//! chidori-js — a pure-Rust JavaScript engine with deterministic-replay durable
//! execution. No C, no `boa_engine` dependency. The front end is `oxc` (parser +
//! AST); everything below — bytecode compiler, VM, object model, GC, builtins,
//! microtask scheduler, and the replay runtime — is implemented here.
//!
//! See `docs/pure-rust-js-engine-plan.md` for the design and phasing.

pub mod builtins;
pub mod bytecode;
pub mod compiler;
pub mod convert;
pub mod exec;
pub mod gc;
pub mod generator;
pub mod host;
pub mod iter;
pub mod journal;
pub mod module;
pub mod promise;
pub mod proxy;
pub mod realm;
pub mod regexp;
pub mod replay;
pub mod trace;
pub mod typed_array;
mod unicode_tables;
pub mod value;
pub mod vm;

pub use trace::{TraceEnter, TraceObserver};
pub use value::Value;
pub use vm::{RunOutcome, Vm};

/// A handle bundling a VM and the top-level compiled script. This is the unit the
/// replay runtime and the `SnapshotCapableJsEngine` adapter build on.
pub struct Engine {
    pub vm: Vm,
    /// The global `chidori` host object, captured by `install_chidori_effects`.
    /// Passed as the entrypoint's second argument so agents written for the
    /// QuickJS convention — `agent(input, chidori)` — receive it as a parameter,
    /// in addition to it being reachable as a global.
    chidori: Option<Value>,
}

impl Default for Engine {
    fn default() -> Self {
        Engine::new()
    }
}

impl Engine {
    pub fn new() -> Engine {
        Engine {
            vm: Vm::new(),
            chidori: None,
        }
    }

    /// Compile and run a script to completion (draining microtasks), returning
    /// the completion value. Errors are returned as their string form.
    pub fn eval(&mut self, src: &str) -> Result<Value, String> {
        let proto = compiler::compile_script(src).map_err(|e| e)?;
        let func = self.vm.make_closure(std::rc::Rc::new(proto), Vec::new());
        let result = self
            .vm
            .call(Value::Object(func), Value::Undefined, &[])
            .map_err(|e| self.vm.error_to_string(&e))?;
        // Drain microtasks (promise reactions scheduled by the script).
        let _ = self.vm.run_jobs_until_blocked();
        Ok(result)
    }

    /// Console output collected so far.
    pub fn console(&self) -> &[String] {
        &self.vm.console_log
    }

    /// Install a global `chidori` host object whose methods forward to
    /// `dispatch` as `(effect_name, json_args) -> json_result`. The durable
    /// host (in the main crate) routes those through its own call log + OTEL, so
    /// host-effect durability and the host-call span tree are unchanged while
    /// this engine runs the JS and emits JS-level trace spans. Unknown effects
    /// should be surfaced as `Err` by the dispatcher; they become a thrown JS
    /// error here.
    pub fn install_chidori_effects(
        &mut self,
        dispatch: std::rc::Rc<
            dyn Fn(&str, &serde_json::Value) -> Result<serde_json::Value, String>,
        >,
    ) {
        let chidori = self.vm.new_object();
        let d = dispatch.clone();
        self.vm
            .define_method(&chidori, "log", 1, move |vm, _t, args| {
                let msg = args
                    .first()
                    .map(|v| vm.to_string_lossy(v))
                    .unwrap_or_default();
                forward_effect(vm, &d, "log", serde_json::json!({ "message": msg }))
            });
        let d = dispatch.clone();
        self.vm
            .define_method(&chidori, "tool", 2, move |vm, _t, args| {
                let name = args
                    .first()
                    .map(|v| vm.to_string_lossy(v))
                    .unwrap_or_default();
                let kwargs = args
                    .get(1)
                    .map(|v| vm.value_to_json(v))
                    .unwrap_or(serde_json::Value::Null);
                forward_effect(
                    vm,
                    &d,
                    "tool",
                    serde_json::json!({ "name": name, "kwargs": kwargs }),
                )
            });
        let d = dispatch.clone();
        self.vm
            .define_method(&chidori, "prompt", 2, move |vm, _t, args| {
                let text = args
                    .first()
                    .map(|v| vm.to_string_lossy(v))
                    .unwrap_or_default();
                let opts = args
                    .get(1)
                    .map(|v| vm.value_to_json(v))
                    .unwrap_or(serde_json::Value::Null);
                forward_effect(
                    vm,
                    &d,
                    "prompt",
                    serde_json::json!({ "text": text, "opts": opts }),
                )
            });
        let d = dispatch.clone();
        self.vm
            .define_method(&chidori, "input", 2, move |vm, _t, args| {
                let prompt = args
                    .first()
                    .map(|v| vm.to_string_lossy(v))
                    .unwrap_or_default();
                let opts = args
                    .get(1)
                    .map(|v| vm.value_to_json(v))
                    .unwrap_or(serde_json::Value::Null);
                forward_effect(
                    vm,
                    &d,
                    "input",
                    serde_json::json!({ "prompt": prompt, "opts": opts }),
                )
            });
        let d = dispatch.clone();
        self.vm
            .define_method(&chidori, "signal", 2, move |vm, _t, args| {
                let name = args
                    .first()
                    .map(|v| vm.value_to_json(v))
                    .unwrap_or(serde_json::Value::Null);
                let opts = args
                    .get(1)
                    .map(|v| vm.value_to_json(v))
                    .unwrap_or(serde_json::Value::Null);
                forward_effect(
                    vm,
                    &d,
                    "signal",
                    serde_json::json!({ "name": name, "opts": opts }),
                )
            });
        let d = dispatch.clone();
        self.vm
            .define_method(&chidori, "pollSignal", 1, move |vm, _t, args| {
                let name = args
                    .first()
                    .map(|v| vm.value_to_json(v))
                    .unwrap_or(serde_json::Value::Null);
                forward_effect(vm, &d, "poll_signal", serde_json::json!({ "name": name }))
            });
        let d = dispatch.clone();
        self.vm
            .define_method(&chidori, "checkpoint", 2, move |vm, _t, args| {
                let label = args
                    .first()
                    .map(|v| vm.value_to_json(v))
                    .unwrap_or(serde_json::Value::Null);
                let data = args
                    .get(1)
                    .map(|v| vm.value_to_json(v))
                    .unwrap_or(serde_json::Value::Null);
                forward_effect(
                    vm,
                    &d,
                    "checkpoint",
                    serde_json::json!({ "label": label, "data": data }),
                )
            });
        let d = dispatch.clone();
        self.vm.define_method(&chidori, "memory", 4, move |vm, _t, args| {
            let action = args.first().map(|v| vm.to_string_lossy(v)).unwrap_or_default();
            let key = args.get(1).map(|v| vm.value_to_json(v)).unwrap_or(serde_json::Value::Null);
            let value = args.get(2).map(|v| vm.value_to_json(v)).unwrap_or(serde_json::Value::Null);
            let opts = args.get(3).map(|v| vm.value_to_json(v)).unwrap_or(serde_json::Value::Null);
            forward_effect(vm, &d, "memory", serde_json::json!({ "action": action, "key": key, "value": value, "opts": opts }))
        });
        let d = dispatch.clone();
        self.vm
            .define_method(&chidori, "template", 3, move |vm, _t, args| {
                let template = args
                    .first()
                    .map(|v| vm.to_string_lossy(v))
                    .unwrap_or_default();
                let vars = args
                    .get(1)
                    .map(|v| vm.value_to_json(v))
                    .unwrap_or(serde_json::Value::Null);
                forward_effect(
                    vm,
                    &d,
                    "template",
                    serde_json::json!({ "template": template, "vars": vars }),
                )
            });
        let d = dispatch.clone();
        self.vm
            .define_method(&chidori, "http", 2, move |vm, _t, args| {
                let arg0 = args
                    .first()
                    .map(|v| vm.value_to_json(v))
                    .unwrap_or(serde_json::Value::Null);
                let arg1 = args
                    .get(1)
                    .map(|v| vm.value_to_json(v))
                    .unwrap_or(serde_json::Value::Null);
                forward_effect(
                    vm,
                    &d,
                    "http",
                    serde_json::json!({ "arg0": arg0, "arg1": arg1 }),
                )
            });
        let d = dispatch.clone();
        self.vm
            .define_method(&chidori, "callAgent", 2, move |vm, _t, args| {
                let path = args
                    .first()
                    .map(|v| vm.to_string_lossy(v))
                    .unwrap_or_default();
                let input = args
                    .get(1)
                    .map(|v| vm.value_to_json(v))
                    .unwrap_or(serde_json::Value::Null);
                forward_effect(
                    vm,
                    &d,
                    "callAgent",
                    serde_json::json!({ "path": path, "input": input }),
                )
            });
        let d = dispatch.clone();
        self.vm
            .define_method(&chidori, "execJs", 2, move |vm, _t, args| {
                let source = args
                    .first()
                    .map(|v| vm.to_string_lossy(v))
                    .unwrap_or_default();
                let opts = args
                    .get(1)
                    .map(|v| vm.value_to_json(v))
                    .unwrap_or(serde_json::Value::Null);
                forward_effect(
                    vm,
                    &d,
                    "execJs",
                    serde_json::json!({ "source": source, "opts": opts }),
                )
            });
        let d = dispatch.clone();
        self.vm
            .define_method(&chidori, "execPython", 2, move |vm, _t, args| {
                let source = args
                    .first()
                    .map(|v| vm.to_string_lossy(v))
                    .unwrap_or_default();
                let opts = args
                    .get(1)
                    .map(|v| vm.value_to_json(v))
                    .unwrap_or(serde_json::Value::Null);
                forward_effect(
                    vm,
                    &d,
                    "execPython",
                    serde_json::json!({ "source": source, "opts": opts }),
                )
            });
        let d = dispatch.clone();
        self.vm
            .define_method(&chidori, "execWasm", 2, move |vm, _t, args| {
                let source = args
                    .first()
                    .map(|v| vm.to_string_lossy(v))
                    .unwrap_or_default();
                let opts = args
                    .get(1)
                    .map(|v| vm.value_to_json(v))
                    .unwrap_or(serde_json::Value::Null);
                forward_effect(
                    vm,
                    &d,
                    "execWasm",
                    serde_json::json!({ "source": source, "opts": opts }),
                )
            });
        let d = dispatch.clone();
        // Synchronous, pure digest of a context segment chain — backs the JS
        // SDK's `Context.digest()`. Unlike the async-shaped effects above it
        // returns its value inline; the dispatcher computes a deterministic
        // hash and records nothing.
        self.vm
            .define_method(&chidori, "__contextDigest", 2, move |vm, _t, args| {
                let segments = args
                    .first()
                    .map(|v| vm.value_to_json(v))
                    .unwrap_or(serde_json::Value::Null);
                let opts = args
                    .get(1)
                    .map(|v| vm.value_to_json(v))
                    .unwrap_or(serde_json::Value::Null);
                forward_effect(
                    vm,
                    &d,
                    "contextDigest",
                    serde_json::json!({ "segments": segments, "opts": opts }),
                )
            });
        // chidori.workspace.<action>(...) — a nested object whose methods all
        // forward the "workspace" effect tagged with their action.
        let workspace = self.vm.new_object();
        let d = dispatch.clone();
        self.vm
            .define_method(&workspace, "list", 1, move |vm, _t, args| {
                let opts = args
                    .first()
                    .map(|v| vm.value_to_json(v))
                    .unwrap_or(serde_json::Value::Null);
                forward_effect(
                    vm,
                    &d,
                    "workspace",
                    serde_json::json!({ "action": "list", "args": opts }),
                )
            });
        let d = dispatch.clone();
        self.vm
            .define_method(&workspace, "read", 1, move |vm, _t, args| {
                let path = args
                    .first()
                    .map(|v| vm.to_string_lossy(v))
                    .unwrap_or_default();
                forward_effect(
                    vm,
                    &d,
                    "workspace",
                    serde_json::json!({ "action": "read", "args": { "path": path } }),
                )
            });
        let d = dispatch.clone();
        self.vm.define_method(&workspace, "write", 3, move |vm, _t, args| {
            let path = args.first().map(|v| vm.to_string_lossy(v)).unwrap_or_default();
            let content = args.get(1).map(|v| vm.to_string_lossy(v)).unwrap_or_default();
            let options = args.get(2).map(|v| vm.value_to_json(v)).unwrap_or(serde_json::Value::Null);
            forward_effect(vm, &d, "workspace", serde_json::json!({ "action": "write", "args": { "path": path, "content": content, "options": options } }))
        });
        let d = dispatch.clone();
        self.vm.define_method(&workspace, "delete", 2, move |vm, _t, args| {
            let path = args.first().map(|v| vm.to_string_lossy(v)).unwrap_or_default();
            let reason = args.get(1).map(|v| vm.to_string_lossy(v));
            forward_effect(vm, &d, "workspace", serde_json::json!({ "action": "delete", "args": { "path": path, "reason": reason } }))
        });
        let d = dispatch.clone();
        self.vm.define_method(&workspace, "remove", 2, move |vm, _t, args| {
            let path = args.first().map(|v| vm.to_string_lossy(v)).unwrap_or_default();
            let reason = args.get(1).map(|v| vm.to_string_lossy(v));
            forward_effect(vm, &d, "workspace", serde_json::json!({ "action": "remove", "args": { "path": path, "reason": reason } }))
        });
        let d = dispatch.clone();
        self.vm
            .define_method(&workspace, "manifest", 0, move |vm, _t, _args| {
                forward_effect(
                    vm,
                    &d,
                    "workspace",
                    serde_json::json!({ "action": "manifest", "args": {} }),
                )
            });
        self.vm
            .define_value(&chidori, "workspace", Value::Object(workspace));
        let global = self.vm.realm.global.clone();
        self.vm
            .define_value(&global, "chidori", Value::Object(chidori.clone()));
        // Keep a handle so the entrypoint can be invoked as `agent(input, chidori)`.
        self.chidori = Some(Value::Object(chidori));
    }

    /// Install synchronous `__chidori_*`-style native globals backed by
    /// `dispatch`. Unlike [`Engine::install_chidori_effects`] (whose host effects
    /// are async and return promises), each function installed here returns its
    /// result *synchronously* — the `node:` builtin shims (crypto hashing/HMAC,
    /// captured randomness, the VFS) call these inline and use the value
    /// immediately. Each call forwards `(name, [args...])` to `dispatch`; the
    /// JSON result becomes the return value, and an `Err` becomes a thrown JS
    /// `Error` (matching the QuickJS path). `names` pairs each global function
    /// name with its declared arity.
    pub fn install_sync_natives(
        &mut self,
        names: &[(&'static str, u32)],
        dispatch: std::rc::Rc<
            dyn Fn(&str, &serde_json::Value) -> Result<serde_json::Value, String>,
        >,
    ) {
        let global = self.vm.realm.global.clone();
        for &(name, arity) in names {
            let d = dispatch.clone();
            self.vm
                .define_method(&global, name, arity, move |vm, _t, args| {
                    let json_args = serde_json::Value::Array(
                        args.iter().map(|v| vm.value_to_json(v)).collect(),
                    );
                    match d(name, &json_args) {
                        Ok(j) => Ok(vm.json_to_value(&j)),
                        Err(e) => Err(vm.make_error(crate::vm::ErrorKind::Error, &e)),
                    }
                });
        }
    }

    /// Install the `run(handler)` entrypoint registrar as a global. Call before
    /// evaluating an agent module; whatever the module passes to `run(...)` lands
    /// in the returned slot.
    pub fn install_entrypoint(&mut self) -> std::rc::Rc<std::cell::RefCell<Option<Value>>> {
        let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        let captured = slot.clone();
        let global = self.vm.realm.global.clone();
        self.vm
            .define_method(&global, "run", 1, move |_vm, _t, args| {
                *captured.borrow_mut() = args.first().cloned();
                Ok(Value::Undefined)
            });
        slot
    }

    /// Compile `src` as an ES module and evaluate it — so a top-level
    /// `run(handler)` registers the entrypoint into `slot` — then call the
    /// entrypoint with `input` and return the settled result (awaiting a
    /// returned promise). Falls back to a named export (`fallback_export`, e.g.
    /// `agent` for agents or `run` for tools) when `run(...)` wasn't called.
    /// Single-file only — a module with (non-stripped) imports is rejected.
    pub fn run_entrypoint(
        &mut self,
        src: &str,
        input: &serde_json::Value,
        slot: &std::rc::Rc<std::cell::RefCell<Option<Value>>>,
        fallback_export: &str,
    ) -> Result<serde_json::Value, String> {
        let compiled = compiler::compile_module(src)?;
        if !compiled.requested.is_empty() {
            return Err("module imports are not supported in single-file entrypoints".to_string());
        }
        let cell_of_name = compiled.cell_of_name.clone();
        let rec = std::rc::Rc::new(std::cell::RefCell::new(module::ModuleRecord::new(compiled)));
        let entry = "<entry>";
        let mut registry = module::ModuleRegistry::default();
        registry.modules.insert(entry.to_string(), rec.clone());
        self.vm
            .run_module_graph(&registry, entry)
            .map_err(|e| self.vm.error_to_string(&e))?;
        // Entrypoint: whatever `run(...)` captured, else the named export.
        let handler = slot.borrow().clone().or_else(|| {
            cell_of_name
                .get(fallback_export)
                .map(|idx| rec.borrow().cells[*idx as usize].borrow().clone())
        });
        let handler = handler.ok_or_else(|| {
            format!("module did not call run(...) and has no `{fallback_export}` export")
        })?;
        let arg = self.vm.json_to_value(input);
        let chidori = self.chidori.clone().unwrap_or(Value::Undefined);
        let ret = self
            .vm
            .call(handler, Value::Undefined, &[arg, chidori])
            .map_err(|e| self.vm.error_to_string(&e))?;
        let settled = self
            .vm
            .settle(ret)
            .map_err(|e| self.vm.error_to_string(&e))?;
        Ok(self.vm.value_to_json(&settled))
    }

    /// Like [`run_entrypoint`], but resolves a multi-file module graph. `entry_key`
    /// is the entry module's registry key (e.g. its canonical path) and `entry_src`
    /// its already-transpiled source. Each transitively-requested specifier is
    /// resolved by the host `load` callback, which maps `(specifier, importer_key)`
    /// to `(resolved_key, transpiled_source)` — keeping filesystem resolution in the
    /// host while this engine handles ES module linking/evaluation.
    pub fn run_entrypoint_graph(
        &mut self,
        entry_key: &str,
        entry_src: &str,
        input: &serde_json::Value,
        slot: &std::rc::Rc<std::cell::RefCell<Option<Value>>>,
        fallback_export: &str,
        load: &mut dyn FnMut(&str, &str) -> Result<(String, String), String>,
    ) -> Result<serde_json::Value, String> {
        let mut registry = module::ModuleRegistry::default();
        // BFS the import graph, compiling each module once and recording how its
        // requested specifiers resolved (the linker reads `resolved` per record).
        let mut queue: Vec<(String, String)> = vec![(entry_key.to_string(), entry_src.to_string())];
        let mut entry_cell_of_name = None;
        let mut entry_rec = None;
        while let Some((key, src)) = queue.pop() {
            if registry.modules.contains_key(&key) {
                continue;
            }
            let compiled = compiler::compile_module(&src)
                .map_err(|e| format!("compiling module '{key}': {e}"))?;
            let cell_of_name = compiled.cell_of_name.clone();
            let requested = compiled.requested.clone();
            let mut rec = module::ModuleRecord::new(compiled);
            for spec in &requested {
                let (dep_key, dep_src) = load(spec, &key)?;
                rec.resolved.insert(spec.clone(), dep_key.clone());
                if !registry.modules.contains_key(&dep_key) {
                    queue.push((dep_key, dep_src));
                }
            }
            let rec = std::rc::Rc::new(std::cell::RefCell::new(rec));
            if key == entry_key {
                entry_cell_of_name = Some(cell_of_name);
                entry_rec = Some(rec.clone());
            }
            registry.modules.insert(key, rec);
        }

        self.vm
            .run_module_graph(&registry, entry_key)
            .map_err(|e| self.vm.error_to_string(&e))?;

        let entry_rec = entry_rec.ok_or_else(|| "entry module was not loaded".to_string())?;
        let cell_of_name =
            entry_cell_of_name.ok_or_else(|| "entry module was not compiled".to_string())?;
        // Entrypoint: whatever `run(...)` captured, else the named export.
        let handler = slot.borrow().clone().or_else(|| {
            cell_of_name
                .get(fallback_export)
                .map(|idx| entry_rec.borrow().cells[*idx as usize].borrow().clone())
        });
        let handler = handler.ok_or_else(|| {
            format!("module did not call run(...) and has no `{fallback_export}` export")
        })?;
        let arg = self.vm.json_to_value(input);
        let chidori = self.chidori.clone().unwrap_or(Value::Undefined);
        let ret = self
            .vm
            .call(handler, Value::Undefined, &[arg, chidori])
            .map_err(|e| self.vm.error_to_string(&e))?;
        let settled = self
            .vm
            .settle(ret)
            .map_err(|e| self.vm.error_to_string(&e))?;
        Ok(self.vm.value_to_json(&settled))
    }

    /// Compile, link, and evaluate the module graph rooted at `entry_src`, then
    /// return the JSON value of the entry module's named export WITHOUT invoking
    /// it. Used for tool-metadata discovery, where the exported `tool` value is a
    /// plain object, not a callable entrypoint.
    pub fn eval_module_export(
        &mut self,
        entry_key: &str,
        entry_src: &str,
        export_name: &str,
        load: &mut dyn FnMut(&str, &str) -> Result<(String, String), String>,
    ) -> Result<serde_json::Value, String> {
        let mut registry = module::ModuleRegistry::default();
        let mut queue: Vec<(String, String)> = vec![(entry_key.to_string(), entry_src.to_string())];
        let mut entry_cell_of_name = None;
        let mut entry_rec = None;
        while let Some((key, src)) = queue.pop() {
            if registry.modules.contains_key(&key) {
                continue;
            }
            let compiled = compiler::compile_module(&src)
                .map_err(|e| format!("compiling module '{key}': {e}"))?;
            let cell_of_name = compiled.cell_of_name.clone();
            let requested = compiled.requested.clone();
            let mut rec = module::ModuleRecord::new(compiled);
            for spec in &requested {
                let (dep_key, dep_src) = load(spec, &key)?;
                rec.resolved.insert(spec.clone(), dep_key.clone());
                if !registry.modules.contains_key(&dep_key) {
                    queue.push((dep_key, dep_src));
                }
            }
            let rec = std::rc::Rc::new(std::cell::RefCell::new(rec));
            if key == entry_key {
                entry_cell_of_name = Some(cell_of_name);
                entry_rec = Some(rec.clone());
            }
            registry.modules.insert(key, rec);
        }

        self.vm
            .run_module_graph(&registry, entry_key)
            .map_err(|e| self.vm.error_to_string(&e))?;

        let entry_rec = entry_rec.ok_or_else(|| "entry module was not loaded".to_string())?;
        let cell_of_name =
            entry_cell_of_name.ok_or_else(|| "entry module was not compiled".to_string())?;
        let idx = cell_of_name
            .get(export_name)
            .ok_or_else(|| format!("missing exported `{export_name}` value"))?;
        let val = entry_rec.borrow().cells[*idx as usize].borrow().clone();
        let settled = self
            .vm
            .settle(val)
            .map_err(|e| self.vm.error_to_string(&e))?;
        Ok(self.vm.value_to_json(&settled))
    }
}

/// Forward a host-effect call through `dispatch`, converting the JSON result to
/// a JS value (or a thrown JS error on failure).
fn forward_effect(
    vm: &mut Vm,
    dispatch: &std::rc::Rc<dyn Fn(&str, &serde_json::Value) -> Result<serde_json::Value, String>>,
    effect: &str,
    args: serde_json::Value,
) -> Result<Value, Value> {
    match dispatch(effect, &args) {
        Ok(j) => Ok(vm.json_to_value(&j)),
        // A host-effect failure surfaces to JS as a plain `Error` (matching the
        // QuickJS path), so `catch` blocks see `Error: ...`, not `TypeError: ...`.
        Err(e) => Err(vm.make_error(crate::vm::ErrorKind::Error, &e)),
    }
}

impl Vm {
    /// Drain microtasks, then settle `v`: if it is a promise, return its
    /// fulfilled value (or its rejection as `Err`); non-promises pass through.
    pub fn settle(&mut self, v: Value) -> Result<Value, Value> {
        let _ = self.run_jobs_until_blocked();
        if let Value::Object(o) = &v {
            let is_promise = matches!(o.borrow().internal, crate::value::Internal::Promise(_));
            if is_promise {
                return match self.promise_state(o) {
                    crate::vm::PromiseState::Fulfilled(val) => Ok(val),
                    crate::vm::PromiseState::Rejected(err) => Err(err),
                    crate::vm::PromiseState::Pending => Err(self.throw_type(
                        "agent promise did not settle (blocked on an unresolved host operation)",
                    )),
                };
            }
        }
        Ok(v)
    }
}

/// Convenience for tests / simple embedding: evaluate and return the completion
/// value's debug rendering.
pub fn eval_to_string(src: &str) -> Result<String, String> {
    let mut engine = Engine::new();
    let v = engine.eval(src)?;
    let s = engine.vm.to_string_lossy(&v);
    Ok(s)
}
