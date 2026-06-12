//! The bytecode virtual machine: execution loop, value conversions, property
//! access with prototype chains and accessors, call/construct, and the
//! promise/microtask/async/generator machinery.
//!
//! ## Suspension model
//!
//! `run_frame` executes one call frame. Synchronous calls recurse in Rust
//! (`call`), so a sync callee always returns before control comes back. The only
//! places execution suspends are `await` (async functions) and `yield`
//! (generators), and those opcodes are only ever reached when the suspending
//! frame is the top of the Rust call stack with no sync callee above it.
//! Therefore suspension is just "box the current frame and return `Flow::Suspend`
//! up one level" — no Rust-stack unwinding of sync callees is ever needed, and
//! **no VM state is ever serialized**. Durability lives one layer up in the
//! journal (see `replay.rs`).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use crate::realm::Realm;
use crate::value::*;

/// Result of running a frame to its next boundary.
pub enum Flow {
    Return(Value),
    Throw(Value),
    Suspend(Suspension),
}

pub struct Suspension {
    pub frame: Box<Frame>,
    pub kind: SuspendKind,
}

pub enum SuspendKind {
    Await(Value),
    Yield(Value),
    YieldStar(Value),
    /// Generator paused after its parameter prologue, before the body.
    GeneratorStart,
}

/// A pending non-local completion threaded through `finally` blocks. When a
/// `return`/`break`/`continue`/throw must cross one or more enclosing `finally`
/// regions, the completion is parked here while each finalizer runs; `EndFinally`
/// then resumes it (running the next outer finally, or performing the action when
/// none remain). This is the "completion register" the spec's
/// `UpdateEmpty`/abrupt-completion model needs.
#[derive(Clone)]
pub enum Completion {
    Return(Value),
    Throw(Value),
    /// `break`/`continue`: jump to `target` once finallys down to the target
    /// loop's handler depth (`boundary`) have run.
    Jump {
        target: u32,
        boundary: u32,
    },
}

/// A try/catch/finally region active in a frame.
#[derive(Clone)]
pub struct TryHandler {
    pub catch_ip: Option<u32>,
    pub finally_ip: Option<u32>,
    pub stack_depth: usize,
    /// Number of active `with` scopes when this handler was installed. On an
    /// unwind into the catch/finally, the with-scope stack is restored to this
    /// depth so a `with` body that throws does not leak its environment.
    pub with_depth: usize,
    /// A `yield*` delegation handler: it catches only EXTERNAL `.throw()`
    /// resumptions (to forward them to the inner iterator). The async-generator
    /// machinery's internal await-of-yielded-value rejection sets the frame's
    /// one-shot skip flag, which makes the unwind pass this handler by.
    pub delegation: bool,
    /// `yield*` return delegation: a `.return(v)` resumption unwinding across
    /// this handler jumps here (with `v` pushed) instead of completing the
    /// generator, so the loop can call the inner iterator's `return` method.
    pub delegation_return_ip: Option<u32>,
}

/// A single call frame. Self-contained (own operand stack + locals) so that a
/// suspended async/generator frame can be frozen in memory and resumed later.
pub struct Frame {
    pub func: BytecodeFunction,
    pub ip: usize,
    pub stack: Vec<Value>,
    pub locals: Vec<Value>,
    pub cells: Vec<Rc<RefCell<Value>>>,
    pub this: Value,
    pub new_target: Value,
    pub handlers: Vec<TryHandler>,
    /// A non-local completion parked while enclosing `finally` blocks run. See
    /// [`Completion`] and `Op::EndFinally`.
    pub pending_completion: Option<Completion>,
    /// Set when resuming a suspended frame with a rejection: raised at loop top.
    pub pending_throw: Option<Value>,
    /// Set when resuming a suspended generator via `.return(v)`: dispatched as a
    /// `Return` completion at loop top so enclosing `finally` blocks run.
    pub pending_return: Option<Value>,
    /// `arguments`-style raw args retained when the function uses `arguments`.
    pub args: Vec<Value>,
    /// The function OBJECT being executed (when known) — `arguments.callee`
    /// for mapped (sloppy, simple-params) arguments objects.
    pub func_obj: Option<JsObject>,
    /// Active `using`-declaration dispose capabilities, innermost last. Each
    /// entry is the (resource, disposeMethod) stack recorded by
    /// `TrackDisposable` and run (in reverse) by `DisposeScope`.
    pub dispose_scopes: Vec<Vec<(Value, Value)>>,
    /// Completion value for script-level evaluation (eval result).
    pub completion: Value,
    /// for-in enumerator stacks (key lists with cursor).
    pub enumerators: Vec<(Vec<JsString>, usize)>,
    /// Active `with` scope objects (innermost last). An unqualified identifier
    /// inside a `with` block resolves against these (honoring @@unscopables)
    /// before falling through to the lexical/global binding.
    pub with_scope: Vec<JsObject>,
    /// JS-level trace token for this activation (see [`crate::trace`]). Set at
    /// `on_enter`; rides the frame across suspend/resume so the matching exit is
    /// attributed correctly even when async resumption is non-LIFO. `None` when
    /// no trace sink is installed.
    pub trace_token: Option<u64>,
    /// One-shot: the next throw dispatched in this frame skips `delegation`
    /// try-handlers (see [`TryHandler::delegation`]). Set when an async
    /// generator's internal await of a yielded value rejects — that abrupt
    /// completion propagates out of a `yield*` rather than being delegated to
    /// the inner iterator's `throw`.
    pub skip_delegation_throw: bool,
    /// For functions containing direct `eval`: the object holding sloppy
    /// eval-introduced `var`s (also pushed as the outermost with-scope, so
    /// dynamic name ops and nested closures resolve them). `None` elsewhere.
    pub eval_vars: Option<JsObject>,
}

/// Promise internal state.
pub struct PromiseData {
    pub state: PromiseState,
    pub fulfill_reactions: Vec<Reaction>,
    pub reject_reactions: Vec<Reaction>,
    /// Whether this promise has been handled (to suppress unhandled-rejection).
    pub handled: bool,
    /// If this is a host-effect promise, its operation id.
    pub host_id: Option<u64>,
}

#[derive(Clone)]
pub enum PromiseState {
    Pending,
    Fulfilled(Value),
    Rejected(Value),
}

/// A reaction registered on a promise: either run JS callbacks (`.then`) or
/// resume a suspended async frame.
pub enum Reaction {
    /// `.then(onFulfilled, onRejected)` style: capability + handler.
    Then {
        handler: Option<Value>,      // the JS callback, or None for passthrough
        result_capability: JsObject, // the dependent promise
        is_reject: bool,
    },
    /// Resume an async function frame when its awaited promise settles. The
    /// frame is shared (in a take-once cell) between the fulfill and reject
    /// reactions so whichever settlement fires reclaims it.
    AsyncResume {
        frame: Rc<RefCell<Option<Box<Frame>>>>,
        own_promise: JsObject, // the async function's result promise
        is_reject: bool,
    },
}

/// One pending async-generator request (`next`/`return`/`throw`): the resume
/// kind, its argument, and the promise that this request settles. Async
/// generators serialize concurrent requests through the queue rather than
/// rejecting re-entrant calls (AsyncGeneratorQueue).
pub struct AsyncGenRequest {
    pub kind: crate::generator::ResumeKind,
    pub value: Value,
    pub result: JsObject,
}

/// Generator/async-generator state.
pub struct GeneratorData {
    pub state: GeneratorState,
    /// Whether this is an *async* generator (`async function*`). Sync and async
    /// generators share `Internal::Generator`; the prototype `next`/`return`/
    /// `throw` methods use this to reject/throw on a mismatched `this`.
    pub is_async: bool,
    /// Async-generator request queue. `next`/`return`/`throw` enqueue a request
    /// and a step is driven only when the generator is not already running; a
    /// completing step pops the front request and drains the next. Always empty
    /// for sync generators.
    pub queue: VecDeque<AsyncGenRequest>,
}

pub enum GeneratorState {
    SuspendedStart(Box<Frame>),
    SuspendedYield(Box<Frame>),
    Executing,
    Completed,
}

/// A queued microtask.
pub enum Microtask {
    /// Run a promise reaction with a settled value.
    Reaction { reaction: Reaction, argument: Value },
    /// A plain callback job (e.g. queueMicrotask).
    Job(Box<dyn FnOnce(&mut Vm) -> Result<(), Value>>),
}

/// Outcome of draining the microtask queue.
#[derive(Debug, Clone, PartialEq)]
pub enum RunOutcome {
    Completed,
    BlockedOnHost(u64),
}

pub struct Vm {
    pub realm: Realm,
    pub microtasks: VecDeque<Microtask>,
    pub symbol_counter: u64,
    pub call_depth: usize,
    pub max_call_depth: usize,
    /// Pending host operations (id -> promise object) awaiting resolution.
    pub pending_host: indexmap::IndexMap<u64, JsObject>,
    /// Monotonic host-op id allocator.
    pub next_host_id: u64,
    /// Host-effect dispatch + journal hook (installed by the replay runtime).
    pub host: Option<Box<dyn crate::host::HostDispatch>>,
    /// Collected console output (for tests / capture).
    pub console_log: Vec<String>,
    /// Unhandled rejections detected during the last drain.
    pub unhandled_rejections: Vec<Value>,
    /// PRNG state for `Math.random` when no host RNG is installed. Deterministic
    /// seed keeps pure-engine runs reproducible; the replay layer routes
    /// randomness through a host effect instead (determinism contract).
    pub rng_state: u64,
    /// Optional opcode budget. `None` = unlimited (default). When set, each
    /// executed opcode decrements it; exhaustion throws a RangeError. Used to
    /// bound runaway loops in conformance/untrusted execution.
    pub op_budget: Option<u64>,
    /// Optional cooperative-cancellation flag polled by the interpreter loop.
    /// When another thread sets it `true`, execution unwinds promptly with an
    /// uncatchable throw. Used by the conformance runner's per-test timeout so a
    /// slow test stops grinding instead of being abandoned to leak a CPU core.
    pub interrupt: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Wrapping counter so [`Vm::native_tick`] only polls the interrupt flag
    /// every 256 iterations (same cadence as the interpreter loop).
    pub(crate) native_poll: u32,
    /// Module evaluation: when set, `run_frame` snapshots the final cell vector of
    /// the frame whose proto pointer matches, into `module_capture`. This lets the
    /// module linker recover a module's top-level binding cells (its live exports)
    /// after its body runs, since `InitCell` replaces cell `Rc`s during execution.
    pub module_capture_proto: Option<std::rc::Rc<crate::bytecode::FuncProto>>,
    pub module_capture: Option<Vec<std::rc::Rc<std::cell::RefCell<Value>>>>,
    /// Optional JS-level trace observer. When installed, the VM notifies it on
    /// every function enter/exit/suspend/resume (see [`crate::trace`]). A pure
    /// write-only side channel — never read back, never affects execution or
    /// replay. `None` (default) makes tracing a single predictable-not-taken
    /// branch per call.
    pub trace_sink: Option<Box<dyn crate::trace::TraceObserver>>,
    /// Host hook for dynamic `import(specifier)`. Receives the coerced specifier
    /// string and must load/link/evaluate the module, returning its namespace
    /// object (`Err` is the thrown error value, which rejects the `import()`
    /// promise). Installed by hosts that can load modules (the test262 runner,
    /// the chidori module loader); when absent, `import()` rejects with a
    /// TypeError.
    pub dynamic_import: Option<std::rc::Rc<dyn Fn(&mut Vm, &str) -> Result<Value, Value>>>,
    /// Weak handle to every object this VM has allocated (see [`crate::gc`]).
    /// Reference counting cannot reclaim cycles (closure↔cell, promise↔
    /// reaction↔frame, ctor↔prototype); this registry lets `collect_cycles`
    /// find garbage cycles among live objects and lets `dispose` break EVERY
    /// allocation's outgoing edges, not just those still reachable from the
    /// realm roots. Entries are weak, so the registry never extends lifetimes.
    pub(crate) all_objects:
        std::cell::RefCell<Vec<std::rc::Weak<RefCell<crate::value::ObjectData>>>>,
    /// Registry compaction threshold (dead weak entries are pruned when the
    /// registry length crosses it; doubled after each compaction).
    pub(crate) gc_compact_at: std::cell::Cell<usize>,
    /// Value cells (`Rc<RefCell<Value>>`) held OUTSIDE the VM — e.g. a host's
    /// `ModuleRecord` cells — that also appear as closure upvalues inside it.
    /// `collect_cycles` treats their contents as roots; without registration,
    /// an object reachable only through such a shared cell could be collected
    /// while the host can still reach it.
    pub gc_cell_roots: Vec<std::rc::Rc<RefCell<Value>>>,
}

impl Vm {
    pub fn new() -> Vm {
        let mut vm = Vm {
            realm: Realm::placeholder(),
            microtasks: VecDeque::new(),
            symbol_counter: 1,
            call_depth: 0,
            max_call_depth: 2000,
            pending_host: indexmap::IndexMap::new(),
            next_host_id: 1,
            host: None,
            console_log: Vec::new(),
            unhandled_rejections: Vec::new(),
            rng_state: 0x2545F4914F6CDD1D,
            op_budget: None,
            interrupt: None,
            native_poll: 0,
            module_capture_proto: None,
            module_capture: None,
            trace_sink: None,
            dynamic_import: None,
            all_objects: std::cell::RefCell::new(Vec::new()),
            gc_compact_at: std::cell::Cell::new(1 << 12),
            gc_cell_roots: Vec::new(),
        };
        crate::realm::init_realm(&mut vm);
        // The placeholder realm's intrinsic objects were created before the VM
        // existed; register them so the cycle collector sees the full heap.
        for o in vm.realm.object_roots() {
            vm.track_object(&o);
        }
        vm
    }

    pub fn alloc_symbol(&mut self, description: Option<&str>) -> JsSymbol {
        let id = self.symbol_counter;
        self.symbol_counter += 1;
        JsSymbol(Rc::new(SymbolData {
            description: description.map(|d| Rc::from(d)),
            id,
        }))
    }

    // ---------------------------------------------------------------------
    // Object construction helpers
    // ---------------------------------------------------------------------

    pub fn new_object(&self) -> JsObject {
        self.alloc_ordinary(Some(self.realm.object_proto.clone()))
    }

    pub fn new_object_proto(&self, proto: Option<JsObject>) -> JsObject {
        self.alloc_ordinary(proto)
    }

    pub fn new_array(&self, elements: Vec<Value>) -> JsObject {
        self.alloc(ObjectData::new(
            Some(self.realm.array_proto.clone()),
            Internal::Array(elements),
        ))
    }

    pub fn new_string_object(&self, s: JsString) -> JsObject {
        let len = s.as_str().chars().count();
        let o = self.alloc(ObjectData::new(
            Some(self.realm.string_proto.clone()),
            Internal::StringObj(s),
        ));
        o.borrow_mut().props.insert(
            PropertyKey::str("length"),
            Property {
                kind: PropertyKind::Data {
                    value: Value::Number(len as f64),
                    writable: false,
                },
                enumerable: false,
                configurable: false,
            },
        );
        o
    }

    /// Build a native function object.
    pub fn new_native(
        &self,
        name: &str,
        length: u32,
        func: impl Fn(&mut Vm, Value, &[Value]) -> Result<Value, Value> + 'static,
    ) -> JsObject {
        let nf = NativeFunction {
            name: Rc::from(name),
            length,
            func: Rc::new(func),
            construct: None,
        };
        let obj = self.alloc(ObjectData::new(
            Some(self.realm.function_proto.clone()),
            Internal::Function(FunctionInner::Native(nf)),
        ));
        {
            let mut b = obj.borrow_mut();
            b.props.insert(
                PropertyKey::str("length"),
                Property {
                    kind: PropertyKind::Data {
                        value: Value::Number(length as f64),
                        writable: false,
                    },
                    enumerable: false,
                    configurable: true,
                },
            );
            b.props.insert(
                PropertyKey::str("name"),
                Property {
                    kind: PropertyKind::Data {
                        value: Value::str(name),
                        writable: false,
                    },
                    enumerable: false,
                    configurable: true,
                },
            );
        }
        obj
    }

    /// Build a native constructor (callable + `new`-able).
    pub fn new_native_ctor(
        &self,
        name: &str,
        length: u32,
        call: impl Fn(&mut Vm, Value, &[Value]) -> Result<Value, Value> + 'static,
        construct: impl Fn(&mut Vm, Value, &[Value]) -> Result<Value, Value> + 'static,
    ) -> JsObject {
        let nf = NativeFunction {
            name: Rc::from(name),
            length,
            func: Rc::new(call),
            construct: Some(Rc::new(construct)),
        };
        let obj = self.alloc(ObjectData::new(
            Some(self.realm.function_proto.clone()),
            Internal::Function(FunctionInner::Native(nf)),
        ));
        {
            let mut b = obj.borrow_mut();
            b.props.insert(
                PropertyKey::str("length"),
                Property {
                    kind: PropertyKind::Data {
                        value: Value::Number(length as f64),
                        writable: false,
                    },
                    enumerable: false,
                    configurable: true,
                },
            );
            b.props.insert(
                PropertyKey::str("name"),
                Property {
                    kind: PropertyKind::Data {
                        value: Value::str(name),
                        writable: false,
                    },
                    enumerable: false,
                    configurable: true,
                },
            );
        }
        obj
    }

    /// Wire `ctor.prototype = proto` (non-enumerable) and `proto.constructor =
    /// ctor` (non-enumerable), then bind `ctor` as a global of the given name.
    pub fn install_ctor(&self, name: &str, ctor: &JsObject, proto: &JsObject) {
        ctor.borrow_mut().props.insert(
            PropertyKey::str("prototype"),
            Property {
                kind: PropertyKind::Data {
                    value: Value::Object(proto.clone()),
                    writable: false,
                },
                enumerable: false,
                configurable: false,
            },
        );
        proto.borrow_mut().props.insert(
            PropertyKey::str("constructor"),
            Property::builtin(Value::Object(ctor.clone())),
        );
        self.define_value(&self.realm.global, name, Value::Object(ctor.clone()));
    }

    /// Define a non-enumerable method on `target`.
    pub fn define_method(
        &self,
        target: &JsObject,
        name: &str,
        length: u32,
        func: impl Fn(&mut Vm, Value, &[Value]) -> Result<Value, Value> + 'static,
    ) {
        let f = self.new_native(name, length, func);
        target
            .borrow_mut()
            .props
            .insert(PropertyKey::str(name), Property::builtin(Value::Object(f)));
    }

    pub fn define_value(&self, target: &JsObject, name: &str, value: Value) {
        target
            .borrow_mut()
            .props
            .insert(PropertyKey::str(name), Property::builtin(value));
    }

    /// Define a frozen constant (non-writable, non-enumerable, non-configurable).
    pub fn define_constant(&self, target: &JsObject, name: &str, value: Value) {
        target
            .borrow_mut()
            .props
            .insert(PropertyKey::str(name), Property::frozen(value));
    }

    pub fn define_value_sym(&self, target: &JsObject, key: JsSymbol, value: Value) {
        target
            .borrow_mut()
            .props
            .insert(PropertyKey::Sym(key), Property::builtin(value));
    }

    // ---------------------------------------------------------------------
    // Error helpers
    // ---------------------------------------------------------------------

    pub fn make_error(&self, kind: ErrorKind, message: &str) -> Value {
        let proto = match kind {
            ErrorKind::Error => &self.realm.error_proto,
            ErrorKind::Type => &self.realm.type_error_proto,
            ErrorKind::Range => &self.realm.range_error_proto,
            ErrorKind::Reference => &self.realm.reference_error_proto,
            ErrorKind::Syntax => &self.realm.syntax_error_proto,
            ErrorKind::Uri => &self.realm.uri_error_proto,
        };
        let obj = self.alloc(ObjectData::new(Some(proto.clone()), Internal::Error));
        obj.borrow_mut().props.insert(
            PropertyKey::str("message"),
            Property::builtin(Value::str(message)),
        );
        obj.borrow_mut().props.insert(
            PropertyKey::str("stack"),
            Property::builtin(Value::str(&format!("{}: {}", kind.name(), message))),
        );
        Value::Object(obj)
    }

    pub fn throw_type(&self, msg: &str) -> Value {
        self.make_error(ErrorKind::Type, msg)
    }
    pub fn throw_range(&self, msg: &str) -> Value {
        self.make_error(ErrorKind::Range, msg)
    }
    pub fn throw_reference(&self, msg: &str) -> Value {
        self.make_error(ErrorKind::Reference, msg)
    }
    pub fn throw_syntax(&self, msg: &str) -> Value {
        self.make_error(ErrorKind::Syntax, msg)
    }

    // ---------------------------------------------------------------------
    // Conversions (abstract operations)
    // ---------------------------------------------------------------------

    pub fn to_boolean(&self, v: &Value) -> bool {
        match v {
            Value::Undefined | Value::Uninitialized | Value::Hole | Value::Null => false,
            Value::Bool(b) => *b,
            Value::Number(n) => *n != 0.0 && !n.is_nan(),
            Value::String(s) => !s.as_str().is_empty(),
            Value::Symbol(_) => true,
            Value::BigInt(n) => !num_traits::Zero::is_zero(n.as_ref()),
            Value::Object(_) => true,
        }
    }

    /// ToPrimitive with a hint. May call user code (Symbol.toPrimitive / valueOf
    /// / toString).
    pub fn to_primitive(&mut self, v: &Value, hint: Hint) -> Result<Value, Value> {
        let obj = match v {
            Value::Object(o) => o.clone(),
            _ => return Ok(v.clone()),
        };
        // Symbol.toPrimitive
        let to_prim_sym = self.realm.symbol_to_primitive.clone();
        let exotic = self.get_prop(&Value::Object(obj.clone()), &PropertyKey::Sym(to_prim_sym))?;
        if !exotic.is_nullish() {
            let hint_str = match hint {
                Hint::Number => "number",
                Hint::String => "string",
                Hint::Default => "default",
            };
            let res = self.call(exotic, v.clone(), &[Value::str(hint_str)])?;
            if let Value::Object(_) = res {
                return Err(self.throw_type("Cannot convert object to primitive value"));
            }
            return Ok(res);
        }
        let order: [&str; 2] = match hint {
            Hint::String => ["toString", "valueOf"],
            _ => ["valueOf", "toString"],
        };
        for name in order {
            let method = self.get_prop(&Value::Object(obj.clone()), &PropertyKey::str(name))?;
            if self.is_callable(&method) {
                let res = self.call(method, Value::Object(obj.clone()), &[])?;
                if !matches!(res, Value::Object(_)) {
                    return Ok(res);
                }
            }
        }
        Err(self.throw_type("Cannot convert object to primitive value"))
    }

    pub fn to_number(&mut self, v: &Value) -> Result<f64, Value> {
        Ok(match v {
            Value::Undefined | Value::Uninitialized | Value::Hole => f64::NAN,
            Value::Null => 0.0,
            Value::Bool(b) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
            Value::Number(n) => *n,
            Value::String(s) => string_to_number(s.as_str()),
            Value::Symbol(_) => return Err(self.throw_type("Cannot convert a Symbol to a number")),
            Value::BigInt(_) => {
                return Err(self.throw_type("Cannot convert a BigInt value to a number"))
            }
            Value::Object(_) => {
                let prim = self.to_primitive(v, Hint::Number)?;
                self.to_number(&prim)?
            }
        })
    }

    pub fn to_int32(&mut self, v: &Value) -> Result<i32, Value> {
        let n = self.to_number(v)?;
        Ok(to_int32(n))
    }
    pub fn to_uint32(&mut self, v: &Value) -> Result<u32, Value> {
        let n = self.to_number(v)?;
        Ok(to_uint32(n))
    }

    /// ToString. May call user code via ToPrimitive(string).
    pub fn to_js_string(&mut self, v: &Value) -> Result<JsString, Value> {
        Ok(match v {
            Value::Undefined | Value::Uninitialized | Value::Hole => JsString::new("undefined"),
            Value::Null => JsString::new("null"),
            Value::Bool(b) => JsString::new(if *b { "true" } else { "false" }),
            Value::Number(n) => JsString::new(number_to_string(*n)),
            Value::String(s) => s.clone(),
            Value::Symbol(_) => return Err(self.throw_type("Cannot convert a Symbol to a string")),
            Value::BigInt(n) => JsString::new(n.to_string()),
            Value::Object(_) => {
                let prim = self.to_primitive(v, Hint::String)?;
                self.to_js_string(&prim)?
            }
        })
    }

    pub fn to_string_lossy(&mut self, v: &Value) -> String {
        self.to_js_string(v)
            .map(|s| s.as_str().to_string())
            .unwrap_or_else(|_| "<error>".to_string())
    }

    /// `RequireObjectCoercible(v)`: a nullish base throws TypeError before any
    /// further coercion (notably before ToPropertyKey of a computed key, whose
    /// `toString` side effects must not run). `action` reads like
    /// "read properties of" in the message.
    pub fn require_object_coercible(&mut self, v: &Value, action: &str) -> Result<(), Value> {
        match v {
            Value::Undefined => Err(self.throw_type(&format!("Cannot {action} undefined"))),
            Value::Null => Err(self.throw_type(&format!("Cannot {action} null"))),
            _ => Ok(()),
        }
    }

    pub fn to_property_key(&mut self, v: &Value) -> Result<PropertyKey, Value> {
        match v {
            Value::Symbol(s) => Ok(PropertyKey::Sym(s.clone())),
            Value::String(s) => Ok(PropertyKey::Str(s.clone())),
            _ => {
                let prim = self.to_primitive(v, Hint::String)?;
                if let Value::Symbol(s) = prim {
                    Ok(PropertyKey::Sym(s))
                } else {
                    Ok(PropertyKey::Str(self.to_js_string(&prim)?))
                }
            }
        }
    }

    /// ToObject — wraps primitives.
    pub fn to_object(&mut self, v: &Value) -> Result<JsObject, Value> {
        match v {
            Value::Object(o) => Ok(o.clone()),
            Value::String(s) => Ok(self.new_string_object(s.clone())),
            Value::Number(n) => Ok(self.alloc(ObjectData::new(
                Some(self.realm.number_proto.clone()),
                Internal::Number(*n),
            ))),
            Value::Bool(b) => Ok(self.alloc(ObjectData::new(
                Some(self.realm.boolean_proto.clone()),
                Internal::Boolean(*b),
            ))),
            Value::Symbol(s) => Ok(self.alloc(ObjectData::new(
                Some(self.realm.symbol_proto.clone()),
                Internal::Symbol(s.clone()),
            ))),
            Value::BigInt(n) => Ok(self.alloc(ObjectData::new(
                Some(self.realm.bigint_proto.clone()),
                Internal::BigIntObj(n.clone()),
            ))),
            Value::Undefined | Value::Uninitialized | Value::Hole | Value::Null => {
                Err(self.throw_type("Cannot convert undefined or null to object"))
            }
        }
    }

    /// Consume one unit of the opcode budget from a native builtin loop. The
    /// spec mandates O(len) walks for the generic Array methods, and `len` can
    /// be up to 2^53-1 on an array-like — without metering, a hostile
    /// `{length: 2**53}` receiver would hang the engine where a JS `while`
    /// loop could not. Same semantics as the interpreter loop: budget
    /// exhaustion and observed interrupts throw uncatchably (the budget is
    /// zeroed so `try/catch` cannot resume).
    pub fn native_tick(&mut self) -> Result<(), Value> {
        if let Some(budget) = self.op_budget.as_mut() {
            if *budget == 0 {
                return Err(self.throw_range("execution budget exceeded"));
            }
            *budget -= 1;
        }
        if self.interrupt.is_some() {
            self.native_poll = self.native_poll.wrapping_add(1);
            if self.native_poll & 0xFF == 0 {
                if let Some(flag) = &self.interrupt {
                    if flag.load(std::sync::atomic::Ordering::Relaxed) {
                        self.op_budget = Some(0);
                        return Err(self.throw_range("execution interrupted"));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn to_length(&mut self, v: &Value) -> Result<usize, Value> {
        let n = self.to_number(v)?;
        if n.is_nan() || n <= 0.0 {
            return Ok(0);
        }
        let n = n.floor();
        Ok(n.min(9007199254740991.0) as usize)
    }

    // ---------------------------------------------------------------------
    // Property access
    // ---------------------------------------------------------------------

    pub fn is_callable(&self, v: &Value) -> bool {
        let o = match v {
            Value::Object(o) => o,
            _ => return false,
        };
        // A Proxy is callable iff its (non-revoked) target is callable.
        let target = {
            let b = o.borrow();
            if b.is_callable() {
                return true;
            }
            match &b.internal {
                Internal::Proxy(p) if !p.revoked => p.target.clone(),
                _ => return false,
            }
        };
        self.is_callable(&Value::Object(target))
    }

    /// IsConstructor: the value is a function with a `[[Construct]]` — a native
    /// constructor, an ordinary (non-arrow/method/generator/async) bytecode
    /// function, or a bound function whose target is a constructor.
    pub fn is_constructor(&self, v: &Value) -> bool {
        let o = match v {
            Value::Object(o) => o,
            _ => return false,
        };
        // Resolve to a verdict (or, for bound functions, the target to recurse on)
        // without holding the borrow across the recursive call.
        enum Verdict {
            Is(bool),
            Bound(JsObject),
        }
        let verdict = {
            let b = o.borrow();
            match b.as_function() {
                Some(FunctionInner::Native(nf)) => Verdict::Is(nf.construct.is_some()),
                Some(FunctionInner::Bytecode(bf)) => {
                    let k = bf.proto.kind;
                    Verdict::Is(
                        !(k.is_async() || k.is_generator() || k.is_arrow() || k.is_method()),
                    )
                }
                Some(FunctionInner::Bound(bound)) => Verdict::Bound(bound.target.clone()),
                None => match &b.internal {
                    // A Proxy is a constructor iff its target is.
                    Internal::Proxy(p) if !p.revoked => Verdict::Bound(p.target.clone()),
                    _ => return false,
                },
            }
        };
        match verdict {
            Verdict::Is(b) => b,
            Verdict::Bound(target) => self.is_constructor(&Value::Object(target)),
        }
    }

    /// Ordinary [[Get]] following the prototype chain, honoring accessors and
    /// array exotic behavior.
    pub fn get_prop(&mut self, base: &Value, key: &PropertyKey) -> Result<Value, Value> {
        // Fast paths for primitives without boxing.
        match base {
            Value::Undefined | Value::Uninitialized | Value::Hole | Value::Null => {
                return Err(self.throw_type(&format!(
                    "Cannot read properties of {} (reading '{}')",
                    if base.is_null() { "null" } else { "undefined" },
                    key_display(key)
                )))
            }
            Value::String(s) => {
                if let Some(v) = self.string_own_prop(s, key) {
                    return Ok(v);
                }
                // fall through to String.prototype
                let proto = self.realm.string_proto.clone();
                return self.get_from_object(&proto, key, base.clone());
            }
            Value::Number(_) => {
                let proto = self.realm.number_proto.clone();
                return self.get_from_object(&proto, key, base.clone());
            }
            Value::Bool(_) => {
                let proto = self.realm.boolean_proto.clone();
                return self.get_from_object(&proto, key, base.clone());
            }
            Value::Symbol(_) => {
                let proto = self.realm.symbol_proto.clone();
                return self.get_from_object(&proto, key, base.clone());
            }
            Value::BigInt(_) => {
                let proto = self.realm.bigint_proto.clone();
                return self.get_from_object(&proto, key, base.clone());
            }
            Value::Object(o) => self.get_from_object(o, key, base.clone()),
        }
    }

    fn string_own_prop(&self, s: &JsString, key: &PropertyKey) -> Option<Value> {
        if let Some("length") = key.as_str() {
            return Some(Value::Number(s.as_str().chars().count() as f64));
        }
        if let Some(idx) = key.array_index() {
            let mut chars = s.as_str().chars();
            if let Some(c) = chars.nth(idx as usize) {
                return Some(Value::str(c.to_string()));
            }
        }
        None
    }

    pub(crate) fn get_from_object(
        &mut self,
        start: &JsObject,
        key: &PropertyKey,
        receiver: Value,
    ) -> Result<Value, Value> {
        // TypedArray exotic: integer indices read elements and never fall
        // through to the prototype (CanonicalNumericIndexString).
        {
            let is_ta = matches!(start.borrow().internal, Internal::TypedArray(_));
            if is_ta {
                if let Some(idx) = key.array_index() {
                    return Ok(self.ta_get(start, idx as usize));
                }
                if let Some(s) = key.as_str() {
                    if s != "length" && crate::vm::is_canonical_numeric(s) {
                        return Ok(Value::Undefined);
                    }
                }
            }
        }
        // Array exotic: index + length.
        let mut cur = start.clone();
        loop {
            // Proxy exotic [[Get]]: dispatch the trap (handles both a proxy base
            // and a proxy encountered while walking the prototype chain).
            if matches!(cur.borrow().internal, Internal::Proxy(_)) {
                return self.proxy_get(&cur, key, receiver);
            }
            // Module Namespace exotic [[Get]]: a string key reads the live
            // export binding (an uninitialized binding throws ReferenceError);
            // symbols (@@toStringTag) fall through to the ordinary props.
            {
                let cell = match &cur.borrow().internal {
                    Internal::ModuleNamespace(ns) => match key {
                        PropertyKey::Str(s) => ns.exports.get(s).cloned().map(Some).unwrap_or({
                            // Unknown string export: namespace proto is null.
                            None
                        }),
                        PropertyKey::Sym(_) => None,
                    },
                    _ => None,
                };
                if let Some(cell) = cell {
                    let v = cell.borrow().clone();
                    if matches!(v, Value::Uninitialized) {
                        return Err(
                            self.throw_reference("Cannot access binding before initialization")
                        );
                    }
                    return Ok(v);
                }
                let is_ns = matches!(cur.borrow().internal, Internal::ModuleNamespace(_));
                if is_ns {
                    if let PropertyKey::Str(_) = key {
                        return Ok(Value::Undefined);
                    }
                }
            }
            // Inspect own property without holding the borrow across calls.
            let found = {
                let b = cur.borrow();
                // array elements. A reified `props` entry for an index/length (a
                // non-dense descriptor defined via defineProperty) is
                // authoritative and shadows the dense-Vec slot, so only consult
                // the Vec when there is no own `props` entry for the key.
                if let Internal::Array(arr) = &b.internal {
                    if let Some("length") = key.as_str() {
                        if !b.props.contains_key(key) {
                            return Ok(Value::Number(arr.len() as f64));
                        }
                    } else if let Some(idx) = key.array_index() {
                        if !b.props.contains_key(key) {
                            // A hole is an absent index: skip it so the lookup
                            // continues up the prototype chain (reads undefined).
                            if let Some(v) = arr.get(idx as usize) {
                                if !matches!(v, Value::Hole) {
                                    return Ok(v.clone());
                                }
                            }
                        }
                    }
                }
                if let Internal::StringObj(s) = &b.internal {
                    if let Some(v) = self.string_own_prop(s, key) {
                        return Ok(v);
                    }
                }
                b.props.get(key).cloned()
            };
            match found {
                Some(prop) => match prop.kind {
                    PropertyKind::Data { value, .. } => return Ok(value),
                    PropertyKind::Accessor { get, .. } => {
                        return match get {
                            Some(getter) => self.call(getter, receiver, &[]),
                            None => Ok(Value::Undefined),
                        }
                    }
                },
                None => {
                    let proto = cur.borrow().proto.clone();
                    match proto {
                        Some(p) => cur = p,
                        None => return Ok(Value::Undefined),
                    }
                }
            }
        }
    }

    /// Ordinary [[Set]].
    /// `[[Set]]` with sloppy-mode semantics: a failed write (non-writable,
    /// setter-less accessor, primitive base, non-extensible add) silently no-ops.
    pub fn set_prop(&mut self, base: &Value, key: &PropertyKey, value: Value) -> Result<(), Value> {
        self.set_prop_mode(base, key, value, false)
    }

    /// `PutValue` with Throw=true (strict-mode assignment): the same failures
    /// throw a `TypeError` instead of silently no-op'ing.
    pub fn set_prop_strict(
        &mut self,
        base: &Value,
        key: &PropertyKey,
        value: Value,
    ) -> Result<(), Value> {
        self.set_prop_mode(base, key, value, true)
    }

    fn set_prop_mode(
        &mut self,
        base: &Value,
        key: &PropertyKey,
        value: Value,
        strict: bool,
    ) -> Result<(), Value> {
        let obj = match base {
            Value::Object(o) => o.clone(),
            Value::Undefined | Value::Null => {
                return Err(self.throw_type(&format!(
                    "Cannot set properties of {} (setting '{}')",
                    if base.is_null() { "null" } else { "undefined" },
                    key_display(key)
                )))
            }
            // Setting a property on a primitive: throws in strict mode, no-op
            // otherwise.
            _ => {
                if strict {
                    let ts = self.to_string_lossy(base);
                    return Err(self.throw_type(&format!(
                        "Cannot create property '{}' on {} '{}'",
                        key_display(key),
                        base.type_of(),
                        ts
                    )));
                }
                return Ok(());
            }
        };
        // TypedArray exotic: integer-index writes coerce per the element kind
        // (ToBigInt for BigInt arrays, ToNumber otherwise) and store the element
        // (out-of-range and non-index numeric keys are ignored after coercion).
        {
            let ta_kind = match &obj.borrow().internal {
                Internal::TypedArray(t) => Some(t.kind),
                _ => None,
            };
            if let Some(kind) = ta_kind {
                if let Some(idx) = key.array_index() {
                    self.ta_write(&obj, idx as usize, &value)?;
                    return Ok(());
                }
                if let Some(s) = key.as_str() {
                    if s != "length" && crate::vm::is_canonical_numeric(s) {
                        // numeric non-index key on a typed array: coerce (which may
                        // throw) then no-op.
                        if kind.is_bigint() {
                            let _ = self.to_bigint(&value)?;
                        } else {
                            let _ = self.to_number(&value)?;
                        }
                        return Ok(());
                    }
                }
            }
        }
        // Walk proto chain to find a setter / writable check.
        let mut cur = obj.clone();
        loop {
            // Proxy exotic [[Set]]: dispatch the trap (base or inherited proxy).
            if matches!(cur.borrow().internal, Internal::Proxy(_)) {
                self.proxy_set(&cur, key, value, base.clone())?;
                return Ok(());
            }
            // Module Namespace exotic [[Set]]: always returns false (a strict
            // write throws, sloppy is a silent no-op), for any key.
            if matches!(cur.borrow().internal, Internal::ModuleNamespace(_)) {
                if strict {
                    return Err(self.throw_type(&format!(
                        "Cannot assign to read only property '{}' of a module namespace object",
                        key_display(key)
                    )));
                }
                return Ok(());
            }
            // TypedArray exotic [[Set]] reached via the proto chain (receiver is
            // an ordinary object): a canonical numeric key that is NOT a valid
            // index is absorbed (returns true, no receiver property created); a
            // valid index behaves like an inherited writable data property
            // (shadows on the receiver).
            if !cur.same(&obj) && matches!(cur.borrow().internal, Internal::TypedArray(_)) {
                let n: Option<f64> = if let Some(i) = key.array_index() {
                    Some(i as f64)
                } else {
                    key.as_str().and_then(|s| {
                        if is_canonical_numeric(s) {
                            Some(s.parse::<f64>().unwrap_or(f64::NAN))
                        } else {
                            None
                        }
                    })
                };
                if let Some(n) = n {
                    if self.ta_valid_index(&cur, n) {
                        break;
                    }
                    return Ok(());
                }
            }
            let accessor = {
                let b = cur.borrow();
                match b.props.get(key) {
                    Some(Property {
                        kind: PropertyKind::Accessor { set, .. },
                        ..
                    }) => Some(set.clone()),
                    Some(Property {
                        kind: PropertyKind::Data { writable, .. },
                        ..
                    }) => {
                        if cur.same(&obj) {
                            // own data property: overwrite if writable
                            if *writable {
                                None // handled below by direct set
                            } else if strict {
                                return Err(self.throw_type(&format!(
                                    "Cannot assign to read only property '{}' of object",
                                    key_display(key)
                                )));
                            } else {
                                return Ok(()); // non-writable: ignore (non-strict)
                            }
                        } else {
                            // inherited data property: shadow on receiver if writable
                            if *writable {
                                break;
                            } else if strict {
                                return Err(self.throw_type(&format!(
                                    "Cannot assign to read only property '{}' of object",
                                    key_display(key)
                                )));
                            } else {
                                return Ok(());
                            }
                        }
                    }
                    None => {
                        // Array exotic own data slots live in `internal`, not
                        // `props`: the virtual `length` and any live (non-hole)
                        // dense element are own writable data properties, so
                        // the write must not consult the prototype chain
                        // (which could hold a proxy trap or a read-only index
                        // that would wrongly veto the assignment). A hole IS
                        // absent, so it still walks the chain like the spec's
                        // ordinary [[Set]].
                        let dense_own = match &b.internal {
                            Internal::Array(arr) => match key.as_str() {
                                Some("length") => true,
                                _ => key.array_index().map_or(false, |i| {
                                    (i as usize) < arr.len()
                                        && !matches!(arr[i as usize], Value::Hole)
                                }),
                            },
                            _ => false,
                        };
                        if dense_own {
                            break;
                        }
                        let proto = b.proto.clone();
                        drop(b);
                        match proto {
                            Some(p) => {
                                cur = p;
                                continue;
                            }
                            None => break,
                        }
                    }
                }
            };
            match accessor {
                Some(Some(setter)) => {
                    self.call(setter, base.clone(), &[value])?;
                    return Ok(());
                }
                Some(None) => {
                    // accessor with no setter: throws in strict, ignored otherwise
                    if strict {
                        return Err(self.throw_type(&format!(
                            "Cannot set property '{}' of object which has only a getter",
                            key_display(key)
                        )));
                    }
                    return Ok(());
                }
                None => break, // own writable data property
            }
        }
        // Define / update own data property on the receiver object.
        self.ordinary_define_own(&obj, key, value, strict)
    }

    fn ordinary_define_own(
        &mut self,
        obj: &JsObject,
        key: &PropertyKey,
        value: Value,
        strict: bool,
    ) -> Result<(), Value> {
        let mut b = obj.borrow_mut();
        // Array exotic write. A reified `props` entry for an index/length shadows
        // the dense store, so route the write through the ordinary props path
        // below (which honours its writable flag) when such an entry exists.
        let has_props_entry = b.props.contains_key(key);
        if !has_props_entry {
            if let Internal::Array(arr) = &mut b.internal {
                if let Some("length") = key.as_str() {
                    drop(b);
                    let n = self.to_number(&value)?;
                    let len = n as usize;
                    let mut b = obj.borrow_mut();
                    if let Internal::Array(arr) = &mut b.internal {
                        if (len as f64) != n || n < 0.0 {
                            return Err(self.throw_range("Invalid array length"));
                        }
                        if len > crate::value::MAX_DENSE_ARRAY {
                            return Err(self.throw_range("Array allocation exceeds engine limit"));
                        }
                        // Growing `length` creates holes, not undefined slots.
                        arr.resize(len, Value::Hole);
                    }
                    return Ok(());
                }
                if let Some(idx) = key.array_index() {
                    let idx = idx as usize;
                    if idx >= arr.len() {
                        if idx >= crate::value::MAX_DENSE_ARRAY {
                            return Err(self.throw_range("Array index exceeds engine limit"));
                        }
                        // Writing past the end leaves holes in the gap.
                        arr.resize(idx + 1, Value::Hole);
                    }
                    arr[idx] = value;
                    return Ok(());
                }
            }
        }
        match b.props.get_mut(key) {
            Some(p) => match &mut p.kind {
                PropertyKind::Data {
                    value: slot,
                    writable,
                } => {
                    if *writable {
                        *slot = value;
                    }
                }
                PropertyKind::Accessor { .. } => {
                    // shouldn't get here (handled above), ignore
                }
            },
            None => {
                if !b.extensible {
                    if strict {
                        drop(b);
                        return Err(self.throw_type(&format!(
                            "Cannot add property {}, object is not extensible",
                            key_display(key)
                        )));
                    }
                    return Ok(());
                }
                b.props.insert(key.clone(), Property::data(value));
            }
        }
        Ok(())
    }

    /// [[Delete]].
    pub fn delete_prop(&mut self, base: &Value, key: &PropertyKey) -> Result<bool, Value> {
        let obj = match base {
            Value::Object(o) => o.clone(),
            _ => return Ok(true),
        };
        // TypedArray integer-indexed exotic [[Delete]]: a valid index cannot be
        // deleted (false); any other canonical numeric index deletes vacuously.
        if self.ta_kind(&obj).is_some() {
            if let Some(n) = crate::builtins::fundamental::canonical_numeric_index(key) {
                return Ok(!self.ta_valid_index(&obj, n));
            }
        }
        if matches!(obj.borrow().internal, Internal::Proxy(_)) {
            return self.proxy_delete(&obj, key);
        }
        // Module Namespace exotic [[Delete]]: an export name refuses (false);
        // any other STRING key deletes vacuously (true). Symbol keys fall
        // through to the ordinary path (@@toStringTag is non-configurable).
        if let Internal::ModuleNamespace(ns) = &obj.borrow().internal {
            if let PropertyKey::Str(s) = key {
                return Ok(!ns.exports.contains_key(s));
            }
        }
        let mut b = obj.borrow_mut();
        // A reified `props` entry for an index shadows the dense slot; honour its
        // configurable flag and fall through to the ordinary delete below.
        let has_props_entry = b.props.contains_key(key);
        if !has_props_entry {
            if let Internal::Array(arr) = &mut b.internal {
                if let Some(idx) = key.array_index() {
                    let idx = idx as usize;
                    if idx < arr.len() {
                        arr[idx] = Value::Hole; // delete punches a hole
                    }
                    return Ok(true);
                }
            }
        }
        match b.props.get(key) {
            Some(p) if !p.configurable => Ok(false),
            Some(_) => {
                b.props.shift_remove(key);
                // Removing a reified array-index override exposes the stale dense
                // slot it shadowed; clear it so the deleted index reads as a hole.
                if let Internal::Array(arr) = &mut b.internal {
                    if let Some(idx) = key.array_index() {
                        let idx = idx as usize;
                        if idx < arr.len() {
                            arr[idx] = Value::Hole;
                        }
                    }
                }
                Ok(true)
            }
            None => Ok(true),
        }
    }

    pub fn has_prop(&mut self, base: &Value, key: &PropertyKey) -> Result<bool, Value> {
        let obj = match base {
            Value::Object(o) => o.clone(),
            _ => return Err(self.throw_type("Cannot use 'in' operator on non-object")),
        };
        // TypedArray integer-indexed exotic [[HasProperty]]: a canonical numeric
        // index is present iff it is a valid integer index (no prototype walk).
        if self.ta_kind(&obj).is_some() {
            if let Some(n) = crate::builtins::fundamental::canonical_numeric_index(key) {
                return Ok(self.ta_valid_index(&obj, n));
            }
        }
        let mut cur = obj;
        loop {
            // Proxy exotic [[HasProperty]]: dispatch the trap (base or inherited).
            if matches!(cur.borrow().internal, Internal::Proxy(_)) {
                return self.proxy_has(&cur, key);
            }
            let (has, proto) = {
                let b = cur.borrow();
                let mut has = b.props.contains_key(key);
                // Module Namespace exotic [[HasProperty]]: export names are
                // present (symbols consult the ordinary props above).
                if let Internal::ModuleNamespace(ns) = &b.internal {
                    if let PropertyKey::Str(s) = key {
                        if ns.exports.contains_key(s) {
                            has = true;
                        }
                    }
                }
                if let Internal::Array(arr) = &b.internal {
                    if let Some("length") = key.as_str() {
                        has = true;
                    }
                    if let Some(idx) = key.array_index() {
                        // A hole is absent for HasProperty / `in`.
                        if let Some(v) = arr.get(idx as usize) {
                            if !matches!(v, Value::Hole) {
                                has = true;
                            }
                        }
                    }
                }
                // String exotic: `length` and in-range character indices are own
                // properties (so generic array-likes over a String wrapper see
                // them — `get_from_object` already reads them via string_own_prop).
                if let Internal::StringObj(s) = &b.internal {
                    if let Some("length") = key.as_str() {
                        has = true;
                    } else if let Some(idx) = key.array_index() {
                        if (idx as usize) < s.as_str().chars().count() {
                            has = true;
                        }
                    }
                }
                (has, b.proto.clone())
            };
            if has {
                return Ok(true);
            }
            match proto {
                Some(p) => cur = p,
                None => return Ok(false),
            }
        }
    }

    // ---------------------------------------------------------------------
    // Own-key enumeration (spec OwnPropertyKeys ordering)
    // ---------------------------------------------------------------------

    /// Own keys in spec order: integer indices ascending, then string keys in
    /// insertion order, then symbols in insertion order.
    pub fn own_keys(&self, obj: &JsObject) -> Vec<PropertyKey> {
        let b = obj.borrow();
        let mut int_keys: Vec<u32> = Vec::new();
        let mut str_keys: Vec<PropertyKey> = Vec::new();
        let mut sym_keys: Vec<PropertyKey> = Vec::new();
        if let Internal::Array(arr) = &b.internal {
            for (i, v) in arr.iter().enumerate() {
                // Holes are absent: they contribute no own key.
                if !matches!(v, Value::Hole) {
                    int_keys.push(i as u32);
                }
            }
        }
        if let Internal::StringObj(s) = &b.internal {
            for i in 0..s.as_str().chars().count() {
                int_keys.push(i as u32);
            }
        }
        if let Internal::TypedArray(t) = &b.internal {
            // The LIVE element count: a length-tracking view follows its
            // resizable buffer, and an out-of-bounds view has no index keys.
            let len = crate::typed_array::ta_eff_length(t);
            for i in 0..len.min(crate::value::MAX_DENSE_ARRAY) {
                int_keys.push(i as u32);
            }
        }
        // Module Namespace exotic [[OwnPropertyKeys]]: the (pre-sorted) export
        // names come first, then the ordinary symbol keys (@@toStringTag).
        if let Internal::ModuleNamespace(ns) = &b.internal {
            for name in ns.exports.keys() {
                str_keys.push(PropertyKey::Str(name.clone()));
            }
        }
        for k in b.props.keys() {
            match k {
                PropertyKey::Str(s) => {
                    // Private names (`#x`) are modeled internally as `"#x"` string
                    // keys but must be invisible to all reflection (OwnPropertyKeys,
                    // getOwnPropertyNames, Reflect.ownKeys, for-in, JSON, ...).
                    if s.as_str().starts_with('#') {
                        continue;
                    }
                    if let Some(idx) = k.array_index() {
                        int_keys.push(idx);
                    } else {
                        str_keys.push(k.clone());
                    }
                }
                PropertyKey::Sym(_) => sym_keys.push(k.clone()),
            }
        }
        int_keys.sort_unstable();
        int_keys.dedup();
        let mut out: Vec<PropertyKey> =
            Vec::with_capacity(int_keys.len() + str_keys.len() + sym_keys.len());
        for i in int_keys {
            out.push(PropertyKey::from_index(i));
        }
        out.extend(str_keys);
        out.extend(sym_keys);
        out
    }

    /// `[[OwnPropertyKeys]]` that dispatches the Proxy `ownKeys` trap; otherwise
    /// the ordinary `own_keys`. Use this wherever the object may be a Proxy.
    pub fn own_property_keys(&mut self, obj: &JsObject) -> Result<Vec<PropertyKey>, Value> {
        if matches!(obj.borrow().internal, Internal::Proxy(_)) {
            self.proxy_own_keys(obj)
        } else {
            Ok(self.own_keys(obj))
        }
    }

    /// Enumerable own keys (string + symbol), proxy-aware: for a Proxy this uses
    /// the `ownKeys` + `getOwnPropertyDescriptor` traps; ordinary objects read
    /// their property map directly. Used by `Object.assign`, object spread, etc.
    pub fn enumerable_own_keys_dyn(&mut self, o: &JsObject) -> Result<Vec<PropertyKey>, Value> {
        let keys = self.own_property_keys(o)?;
        let is_proxy = matches!(o.borrow().internal, Internal::Proxy(_));
        let mut out = Vec::new();
        for k in keys {
            let enumerable = if is_proxy {
                let desc = self.proxy_get_own_descriptor(o, &k)?;
                match &desc {
                    Value::Object(_) => {
                        let e = self.get_prop(&desc, &PropertyKey::str("enumerable"))?;
                        self.to_boolean(&e)
                    }
                    _ => false,
                }
            } else {
                let b = o.borrow();
                match b.props.get(&k) {
                    Some(p) => p.enumerable,
                    None => match &b.internal {
                        Internal::Array(arr) => k
                            .array_index()
                            .and_then(|i| arr.get(i as usize))
                            .map(|v| !matches!(v, Value::Hole))
                            .unwrap_or(false),
                        Internal::StringObj(s) => k
                            .array_index()
                            .map(|i| (i as usize) < s.as_str().chars().count())
                            .unwrap_or(false),
                        _ => false,
                    },
                }
            };
            if enumerable {
                out.push(k);
            }
        }
        Ok(out)
    }

    /// Whether `key` is an enumerable own property of `obj` (proxy-aware). Used
    /// by `Object.values`/`entries` so the `[[GetOwnProperty]]` check and the
    /// subsequent `[[Get]]` interleave per key, matching the spec's observable
    /// EnumerableOwnProperties order.
    pub fn own_key_enumerable(&mut self, obj: &JsObject, key: &PropertyKey) -> Result<bool, Value> {
        if matches!(obj.borrow().internal, Internal::Proxy(_)) {
            let desc = self.proxy_get_own_descriptor(obj, key)?;
            return Ok(match &desc {
                Value::Object(_) => {
                    let e = self.get_prop(&desc, &PropertyKey::str("enumerable"))?;
                    self.to_boolean(&e)
                }
                _ => false,
            });
        }
        let b = obj.borrow();
        Ok(match b.props.get(key) {
            Some(p) => p.enumerable,
            None => match &b.internal {
                Internal::Array(arr) => key
                    .array_index()
                    .and_then(|i| arr.get(i as usize))
                    .map(|v| !matches!(v, Value::Hole))
                    .unwrap_or(false),
                Internal::StringObj(s) => key
                    .array_index()
                    .map(|i| (i as usize) < s.as_str().chars().count())
                    .unwrap_or(false),
                _ => false,
            },
        })
    }

    /// Enumerable own string keys (for `Object.keys`, `for-in` own portion).
    pub fn enumerable_own_string_keys(&self, obj: &JsObject) -> Vec<JsString> {
        let keys = self.own_keys(obj);
        let b = obj.borrow();
        let mut out = Vec::new();
        for k in keys {
            if let PropertyKey::Str(s) = &k {
                // A reified `props` entry shadows the exotic index slot, so its
                // enumerable flag wins over the implicit dense-element default.
                let enumerable = match b.props.get(&k) {
                    Some(p) => p.enumerable,
                    None => match &b.internal {
                        Internal::Array(_) if k.array_index().is_some() => true,
                        Internal::StringObj(_) if k.array_index().is_some() => true,
                        _ => false,
                    },
                };
                if enumerable {
                    out.push(s.clone());
                }
            }
        }
        out
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Hint {
    Number,
    String,
    Default,
}

impl Vm {
    /// Break the `Rc` reference cycles in the realm + runtime state so the whole
    /// object graph is reclaimed when this `Vm` is dropped. The GC is
    /// reference-counting and cannot free cycles (ctor↔prototype, global→builtins,
    /// closures→captured objects); without this every short-lived `Vm` (e.g. one
    /// per Test262 case) leaks its entire realm (~0.4 MB), OOMing long runs.
    /// The `Vm` must not be used after `dispose`.
    pub fn dispose(&mut self) {
        use std::collections::HashSet;
        self.microtasks.clear();
        self.pending_host.clear();
        self.unhandled_rejections.clear();
        self.console_log.clear();
        // The dynamic-import hook closes over host module state (registries whose
        // records hold realm values); drop it so those cells don't keep cycles.
        self.dynamic_import = None;
        self.gc_cell_roots.clear();
        self.module_capture = None;

        // Primary teardown: break the outgoing edges of EVERY object this VM
        // ever allocated (see `Vm::alloc` / `crate::gc`). Unlike the
        // realm-root walk below, this also reaches cycles that are no longer
        // connected to the realm (orphaned closure/promise/generator loops),
        // which used to leak per-VM and forced long conformance runs to be
        // chunked across processes.
        let tracked: Vec<JsObject> = {
            let mut reg = self.all_objects.borrow_mut();
            let live = reg
                .iter()
                .filter_map(|w| w.upgrade().map(JsObject))
                .collect();
            reg.clear();
            live
        };
        for o in &tracked {
            crate::gc::clear_object_edges(o);
        }
        drop(tracked);

        // Belt and braces: also walk from the realm roots so any object that
        // was created without registration still gets its edges broken.
        let mut seen: HashSet<usize> = HashSet::new();
        let mut stack: Vec<JsObject> = self.realm.object_roots();
        while let Some(o) = stack.pop() {
            if !seen.insert(o.ptr_id()) {
                continue;
            }
            let mut b = o.borrow_mut();
            if let Some(p) = b.proto.take() {
                stack.push(p);
            }
            for (_k, prop) in std::mem::take(&mut b.props) {
                match prop.kind {
                    PropertyKind::Data { value, .. } => push_dispose_obj(value, &mut stack),
                    PropertyKind::Accessor { get, set } => {
                        if let Some(g) = get {
                            push_dispose_obj(g, &mut stack);
                        }
                        if let Some(s) = set {
                            push_dispose_obj(s, &mut stack);
                        }
                    }
                }
            }
            break_internal_cycles(&mut b.internal, &mut stack);
        }
    }
}

fn push_dispose_obj(v: Value, stack: &mut Vec<JsObject>) {
    if let Value::Object(o) = v {
        stack.push(o);
    }
}

/// Drop (and collect for traversal) the object references held in exotic internal
/// slots, so cycles through arrays/maps/closures/typed-arrays/proxies break.
fn break_internal_cycles(internal: &mut Internal, stack: &mut Vec<JsObject>) {
    match internal {
        Internal::Array(v) => {
            for x in std::mem::take(v) {
                push_dispose_obj(x, stack);
            }
        }
        Internal::Map(m) | Internal::WeakMap(m) => {
            for (k, val) in std::mem::take(m) {
                push_dispose_obj(k.0, stack);
                push_dispose_obj(val, stack);
            }
        }
        Internal::Set(s) | Internal::WeakSet(s) => {
            for (k, _) in std::mem::take(s) {
                push_dispose_obj(k.0, stack);
            }
        }
        Internal::TypedArray(t) => stack.push(t.buffer.clone()),
        Internal::DataView(d) => stack.push(d.buffer.clone()),
        Internal::Proxy(p) => {
            stack.push(p.target.clone());
            stack.push(p.handler.clone());
        }
        Internal::Iterator(it) => {
            if let Some(t) = it.target.take() {
                stack.push(t);
            }
        }
        Internal::Function(f) => match f {
            FunctionInner::Bytecode(bf) => {
                if let Some(h) = bf.home_object.take() {
                    stack.push(h);
                }
                for cell in std::mem::take(&mut bf.upvalues) {
                    let v = cell.borrow().clone();
                    push_dispose_obj(v, stack);
                }
                for o in std::mem::take(&mut bf.captured_with) {
                    stack.push(o);
                }
            }
            FunctionInner::Bound(b) => {
                stack.push(b.target.clone());
                push_dispose_obj(
                    std::mem::replace(&mut b.bound_this, Value::Undefined),
                    stack,
                );
                for a in std::mem::take(&mut b.bound_args) {
                    push_dispose_obj(a, stack);
                }
            }
            FunctionInner::Native(_) => {}
        },
        _ => {}
    }
}

#[derive(Clone, Copy)]
pub enum ErrorKind {
    Error,
    Type,
    Range,
    Reference,
    Syntax,
    Uri,
}
impl ErrorKind {
    pub fn name(self) -> &'static str {
        match self {
            ErrorKind::Error => "Error",
            ErrorKind::Type => "TypeError",
            ErrorKind::Range => "RangeError",
            ErrorKind::Reference => "ReferenceError",
            ErrorKind::Syntax => "SyntaxError",
            ErrorKind::Uri => "URIError",
        }
    }
}

fn key_display(key: &PropertyKey) -> String {
    match key {
        PropertyKey::Str(s) => s.as_str().to_string(),
        PropertyKey::Sym(s) => format!("Symbol({})", s.description().unwrap_or("")),
    }
}

// =========================================================================
// Numeric helpers
// =========================================================================

/// Whether `s` is a CanonicalNumericIndexString (a string that round-trips
/// through ToNumber→ToString, e.g. "0", "1.5", "-0", "NaN", "Infinity"). Used by
/// typed-array exotic access to decide whether a non-index key is numeric.
pub fn is_canonical_numeric(s: &str) -> bool {
    if s == "-0" || s == "NaN" || s == "Infinity" || s == "-Infinity" {
        return true;
    }
    match s.parse::<f64>() {
        Ok(n) => number_to_string(n) == s,
        Err(_) => false,
    }
}

pub fn to_int32(n: f64) -> i32 {
    if !n.is_finite() || n == 0.0 {
        return 0;
    }
    let n = n.trunc();
    let m = n.rem_euclid(4294967296.0);
    let m = if m >= 2147483648.0 {
        m - 4294967296.0
    } else {
        m
    };
    m as i32
}

pub fn to_uint32(n: f64) -> u32 {
    if !n.is_finite() || n == 0.0 {
        return 0;
    }
    let n = n.trunc();
    n.rem_euclid(4294967296.0) as u32
}

pub fn string_to_number(s: &str) -> f64 {
    let t = s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return i64::from_str_radix(hex, 16)
            .map(|n| n as f64)
            .unwrap_or(f64::NAN);
    }
    if let Some(oct) = t.strip_prefix("0o").or_else(|| t.strip_prefix("0O")) {
        return i64::from_str_radix(oct, 8)
            .map(|n| n as f64)
            .unwrap_or(f64::NAN);
    }
    if let Some(bin) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
        return i64::from_str_radix(bin, 2)
            .map(|n| n as f64)
            .unwrap_or(f64::NAN);
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// ECMAScript Number::toString (radix 10).
pub fn number_to_string(n: f64) -> String {
    if n.is_nan() {
        return "NaN".to_string();
    }
    if n == 0.0 {
        return "0".to_string();
    }
    if n.is_infinite() {
        return if n > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    let neg = n < 0.0;
    let abs = n.abs();
    // Rust's `{:e}` is a correct shortest round-trip representation, giving the
    // minimal decimal digit string `s` and an exponent. We then format per the
    // ECMAScript Number::toString algorithm (digits `s`, `k = s.len()`, and
    // `n = exp + 1`).
    let sci = format!("{abs:e}");
    let (mant, exp_str) = sci.split_once('e').expect("scientific notation has 'e'");
    let exp: i32 = exp_str.parse().expect("valid exponent");
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    let nn = exp + 1;
    let body = format_decimal(&digits, k, nn);
    if neg {
        format!("-{body}")
    } else {
        body
    }
}

/// Format the digit string `s` (length `k`, value `s × 10^(n-k)`) per the
/// ECMAScript Number::toString rules.
fn format_decimal(s: &str, k: i32, n: i32) -> String {
    if k <= n && n <= 21 {
        // Integer: all digits followed by n-k zeros.
        let mut out = String::with_capacity(n as usize);
        out.push_str(s);
        out.extend(std::iter::repeat('0').take((n - k) as usize));
        out
    } else if 0 < n && n <= 21 {
        // Decimal point inside the digits.
        let i = n as usize;
        format!("{}.{}", &s[..i], &s[i..])
    } else if -6 < n && n <= 0 {
        // 0.00…0 followed by all digits.
        format!("0.{}{}", "0".repeat((-n) as usize), s)
    } else {
        // Exponential form.
        let e = n - 1;
        let sign = if e >= 0 { "+" } else { "-" };
        let mantissa = if k == 1 {
            s.to_string()
        } else {
            format!("{}.{}", &s[..1], &s[1..])
        };
        format!("{mantissa}e{sign}{}", e.abs())
    }
}
