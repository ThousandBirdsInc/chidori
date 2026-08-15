//! VM images: a serializable picture of live engine state.
//!
//! Until now the engine's only durable form of a suspended program was
//! `bundle + journal` ([`crate::replay::ReplayRuntime::to_blob`]), and restore
//! meant *re-executing* the whole run against the recorded call log. That is
//! correct and edit-tolerant, but it costs O(total run history) on every
//! resume — which is the whole reason a paused agent has to stay parked in the
//! process that started it. A VM image is the other half: it captures the
//! actual heap, closures, promises and suspended frames, so a *different*
//! process (or a different machine) can pick the program up where it stopped
//! in O(live state) instead of O(history).
//!
//! # The baseline trick
//!
//! Serializing a whole realm is neither possible nor useful: the intrinsics
//! are wired together with native Rust function pointers, and there are
//! thousands of them. But a realm is also *deterministic* — building an engine
//! and evaluating the same setup scripts always produces the same object
//! graph. So an image is **realm-relative**:
//!
//! 1. The embedder brings the VM to the state both sides can reproduce (fresh
//!    realm + whatever polyfills/preludes it always evaluates) and calls
//!    [`Vm::mark_image_baseline`]. That walks the object graph from the realm
//!    roots in a fixed order and hands every reachable object, binding cell
//!    and symbol a small integer id.
//! 2. [`Vm::snapshot_image`] serializes only what came *after* the baseline.
//!    Anything older is written as a reference to its baseline id — one `u32`
//!    instead of a subgraph.
//! 3. [`Vm::restore_image`] is called on a VM taken to the same baseline. Ids
//!    resolve against *that* VM's objects, and the post-baseline graph is
//!    rebuilt on top.
//!
//! Baseline objects are not frozen: top-level `var`s land on the global object,
//! and scripts do patch `Array.prototype`. Each baseline object's structure is
//! fingerprinted at step 1 and re-checked at step 2; anything that changed is
//! written as an **overlay** (its full property table) and reapplied on
//! restore. The common case — an untouched intrinsic — costs one hash.
//!
//! # Compiled code
//!
//! Bytecode is not serialized. `FuncProto`s are addressed as
//! `(unit, path-of-const-indices)` against the compilation units the embedder
//! registered with [`Vm::register_image_unit`], and the image records each
//! unit's key plus a digest of its shape. Restore recompiles (cheap, cached)
//! and resolves the same paths. Recompiling identical source must produce an
//! identical proto tree; the digest check turns a violation into a clean
//! `Mismatch` rather than a wrong resume.
//!
//! # Partiality is the design
//!
//! Some live state genuinely cannot be written down: a queued
//! `Microtask::Job` is a Rust closure, a native function created after the
//! baseline captures Rust state, a generator caught mid-step has its frame on
//! the native stack. Every such case returns
//! [`ImageError::Unsupported`] from `snapshot_image` rather than guessing. The
//! caller's move is to fall back to journal replay, which always works — the
//! image is a fast path over deterministic computation, never the source of
//! truth.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

use serde::{Deserialize, Serialize};

use crate::bytecode::{Const, FuncProto};
use crate::value::{
    BoundFunction, BytecodeFunction, DataViewData, FunctionInner, Internal, IterKind, IterState,
    JsObject, JsString, JsSymbol, MapKey, NamespaceData, PrivateElement, PrivateEnv, PrivateName,
    Property, PropertyKey, PropertyKind, ProxyData, SymbolData, TAKind, TypedArrayData, Value,
};
use crate::vm::{
    AsyncGenRequest, Completion, Frame, GeneratorData, GeneratorState, Microtask, PromiseData,
    PromiseState, Reaction, TryHandler, Vm,
};

/// Bumped whenever the encoding changes shape. Restore refuses anything else.
pub const IMAGE_VERSION: u32 = 1;

#[derive(Debug)]
pub enum ImageError {
    /// Live state this format cannot represent. The caller should fall back to
    /// journal replay; the reason names what stopped it.
    Unsupported(String),
    /// The restoring VM is not at the baseline the image was taken against.
    Mismatch(String),
    Decode(String),
}

impl std::fmt::Display for ImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImageError::Unsupported(m) => write!(f, "VM image unsupported: {m}"),
            ImageError::Mismatch(m) => write!(f, "VM image baseline mismatch: {m}"),
            ImageError::Decode(m) => write!(f, "VM image decode failed: {m}"),
        }
    }
}

impl std::error::Error for ImageError {}

type R<T> = Result<T, ImageError>;

fn unsupported<T>(what: impl Into<String>) -> R<T> {
    Err(ImageError::Unsupported(what.into()))
}

// ---------------------------------------------------------------------------
// Hashing (FNV-1a — stable across processes, unlike DefaultHasher)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Fnv(u64);

impl Fnv {
    fn new() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= *b as u64;
            self.0 = self.0.wrapping_mul(0x100_0000_01b3);
        }
    }
    fn u64(&mut self, v: u64) {
        self.write(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.write(&v.to_le_bytes());
    }
    fn u8(&mut self, v: u8) {
        self.write(&[v]);
    }
    fn str(&mut self, s: &str) {
        self.u64(s.len() as u64);
        self.write(s.as_bytes());
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

// ---------------------------------------------------------------------------
// Compilation units
// ---------------------------------------------------------------------------

/// One registered compilation unit: a stable key (the embedder's choice —
/// typically a source hash) and the root `FuncProto` compiling it produced.
pub struct ImageUnit {
    pub key: String,
    pub root: Rc<FuncProto>,
}

/// Address of a `FuncProto`: which unit, then the chain of `consts` indices
/// from that unit's root down to the proto.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Debug)]
pub struct ProtoRef {
    unit: u32,
    path: Vec<u32>,
}

/// Every proto reachable from a unit root, addressed by its const path.
/// Pre-order so the numbering is stable under recompilation of the same
/// source.
fn index_protos(root: &Rc<FuncProto>, unit: u32) -> HashMap<usize, ProtoRef> {
    let mut out = HashMap::new();
    let mut stack: Vec<(Rc<FuncProto>, Vec<u32>)> = vec![(root.clone(), Vec::new())];
    while let Some((proto, path)) = stack.pop() {
        let ptr = Rc::as_ptr(&proto) as *const () as usize;
        if out.contains_key(&ptr) {
            continue;
        }
        out.insert(
            ptr,
            ProtoRef {
                unit,
                path: path.clone(),
            },
        );
        for (i, k) in proto.consts.iter().enumerate() {
            if let Const::Func(child) = k {
                let mut child_path = path.clone();
                child_path.push(i as u32);
                stack.push((child.clone(), child_path));
            }
        }
    }
    out
}

fn resolve_proto(root: &Rc<FuncProto>, path: &[u32]) -> Option<Rc<FuncProto>> {
    let mut cur = root.clone();
    for step in path {
        let next = match cur.consts.get(*step as usize) {
            Some(Const::Func(f)) => f.clone(),
            _ => return None,
        };
        cur = next;
    }
    Some(cur)
}

/// Structural digest of a unit's proto tree — op counts, const shapes, cell
/// and upvalue counts. Two compilations of the same source agree; a compiler
/// change or a different source does not.
fn unit_digest(root: &Rc<FuncProto>) -> u64 {
    let mut h = Fnv::new();
    let mut stack = vec![root.clone()];
    let mut seen: HashSet<usize> = HashSet::new();
    while let Some(proto) = stack.pop() {
        if !seen.insert(Rc::as_ptr(&proto) as *const () as usize) {
            continue;
        }
        h.str(&proto.name);
        h.u64(proto.code.len() as u64);
        h.u32(proto.num_locals);
        h.u32(proto.num_cells);
        h.u32(proto.num_params);
        h.u64(proto.upvalues.len() as u64);
        h.u64(proto.consts.len() as u64);
        for (i, k) in proto.consts.iter().enumerate() {
            match k {
                Const::Func(child) => {
                    h.u8(1);
                    h.u32(i as u32);
                    stack.push(child.clone());
                }
                Const::String(s) => {
                    h.u8(2);
                    h.str(s.as_str());
                }
                Const::Number(n) => {
                    h.u8(3);
                    h.u64(n.to_bits());
                }
                Const::BigInt(b) => {
                    h.u8(4);
                    h.str(b);
                }
                Const::Bool(b) => {
                    h.u8(5);
                    h.u8(*b as u8);
                }
                Const::Null => h.u8(6),
                Const::Undefined => h.u8(7),
            }
        }
    }
    h.finish()
}

// ---------------------------------------------------------------------------
// Baseline
// ---------------------------------------------------------------------------

/// The deterministic prefix an image is written against: every object, binding
/// cell and symbol reachable from the realm roots at the moment
/// [`Vm::mark_image_baseline`] ran, numbered by a fixed-order walk.
pub struct Baseline {
    obj_ids: HashMap<usize, u32>,
    objs: Vec<JsObject>,
    cell_ids: HashMap<usize, u32>,
    cells: Vec<Rc<RefCell<Value>>>,
    sym_ids: HashMap<usize, u32>,
    syms: Vec<JsSymbol>,
    /// Per-object structural fingerprint, parallel to `objs`. Re-checked at
    /// snapshot time to find the objects that need overlays.
    prints: Vec<u64>,
    /// Fingerprint of the walk itself — object/cell/symbol counts and the
    /// shape of every baseline object. Both sides must agree.
    digest: u64,
}

impl Baseline {
    pub fn digest(&self) -> u64 {
        self.digest
    }

    pub fn object_count(&self) -> usize {
        self.objs.len()
    }
}

/// Deterministic BFS over the realm: assign ids in discovery order, visiting
/// each object's edges in a fixed sequence (prototype, own properties in
/// insertion order, private elements, then internal slots).
pub(crate) fn build_baseline(vm: &Vm) -> Baseline {
    // Pre-sized for a realm-shaped walk (~900 objects for the base realm with
    // every lazy section materialized). Only saves rehashing; the ids still
    // come from insertion order, so capacity cannot affect the digest.
    const OBJ_HINT: usize = 1024;
    let mut b = Baseline {
        obj_ids: HashMap::with_capacity(OBJ_HINT),
        objs: Vec::with_capacity(OBJ_HINT),
        cell_ids: HashMap::new(),
        cells: Vec::new(),
        sym_ids: HashMap::with_capacity(64),
        syms: Vec::with_capacity(64),
        prints: Vec::new(),
        digest: 0,
    };

    // Well-known symbols first, in declaration order, so their ids are stable
    // regardless of which intrinsic happens to mention them first.
    for s in well_known_symbols(vm) {
        intern_symbol(&mut b, &s);
    }

    let mut queue: VecDeque<JsObject> = VecDeque::new();
    for root in vm.realm.object_roots() {
        if intern_object(&mut b, &root) {
            queue.push_back(root);
        }
    }
    while let Some(obj) = queue.pop_front() {
        let data = obj.borrow();
        let push = |o: &JsObject, b: &mut Baseline, q: &mut VecDeque<JsObject>| {
            if intern_object(b, o) {
                q.push_back(o.clone());
            }
        };
        if let Some(p) = &data.proto {
            push(p, &mut b, &mut queue);
        }
        for (key, prop) in data.own_iter() {
            if let PropertyKey::Sym(s) = key {
                intern_symbol(&mut b, s);
            }
            for_each_prop_value(prop, |v| walk_value(v, &mut b, &mut queue));
        }
        if let Some(privs) = &data.privates {
            for el in privs.values() {
                for_each_private_value(el, |v| walk_value(v, &mut b, &mut queue));
            }
        }
        walk_internal(&data.internal, &mut b, &mut queue);
    }

    // Second pass: fingerprints (they reference ids, so every object must be
    // numbered first).
    b.prints = (0..b.objs.len())
        .map(|i| fingerprint(&b, &b.objs[i]))
        .collect();

    let mut h = Fnv::new();
    h.u64(b.objs.len() as u64);
    h.u64(b.cells.len() as u64);
    h.u64(b.syms.len() as u64);
    for p in &b.prints {
        h.u64(*p);
    }
    b.digest = h.finish();
    b
}

fn well_known_symbols(vm: &Vm) -> Vec<JsSymbol> {
    let r = &vm.realm;
    vec![
        r.symbol_iterator.clone(),
        r.symbol_async_iterator.clone(),
        r.symbol_to_primitive.clone(),
        r.symbol_to_string_tag.clone(),
        r.symbol_has_instance.clone(),
        r.symbol_match.clone(),
        r.symbol_replace.clone(),
        r.symbol_search.clone(),
        r.symbol_split.clone(),
        r.symbol_match_all.clone(),
        r.symbol_species.clone(),
        r.symbol_unscopables.clone(),
        r.symbol_is_concat_spreadable.clone(),
        r.symbol_dispose.clone(),
        r.symbol_async_dispose.clone(),
        r.symbol_disposable_state.clone(),
        r.symbol_async_disposable_state.clone(),
        r.symbol_sync_iterator_record.clone(),
        r.symbol_array_buffer_shared.clone(),
        r.symbol_intl_locale.clone(),
        r.symbol_intl_plural_rules.clone(),
        r.symbol_intl_number_format.clone(),
        r.symbol_stack_start.clone(),
    ]
}

fn intern_object(b: &mut Baseline, o: &JsObject) -> bool {
    let id = o.ptr_id();
    if b.obj_ids.contains_key(&id) {
        return false;
    }
    b.obj_ids.insert(id, b.objs.len() as u32);
    b.objs.push(o.clone());
    true
}

fn intern_cell(b: &mut Baseline, c: &Rc<RefCell<Value>>) -> bool {
    let id = Rc::as_ptr(c) as *const () as usize;
    if b.cell_ids.contains_key(&id) {
        return false;
    }
    b.cell_ids.insert(id, b.cells.len() as u32);
    b.cells.push(c.clone());
    true
}

fn intern_symbol(b: &mut Baseline, s: &JsSymbol) -> bool {
    let id = Rc::as_ptr(&s.0) as *const () as usize;
    if b.sym_ids.contains_key(&id) {
        return false;
    }
    b.sym_ids.insert(id, b.syms.len() as u32);
    b.syms.push(s.clone());
    true
}

/// Visit a property's values in a FIXED order: data value, else getter then
/// setter. The order is load-bearing — it decides the order the walk discovers
/// objects in, hence their baseline ids, hence the digest — so it must not
/// change even though nothing here reads it back.
fn for_each_prop_value(p: &Property, mut f: impl FnMut(&Value)) {
    match &p.kind {
        PropertyKind::Data { value, .. } => f(value),
        PropertyKind::Accessor { get, set } => {
            if let Some(g) = get {
                f(g);
            }
            if let Some(s) = set {
                f(s);
            }
        }
    }
}

/// As [`for_each_prop_value`], for a private element. Same fixed order, same
/// reason.
fn for_each_private_value(el: &PrivateElement, mut f: impl FnMut(&Value)) {
    match el {
        PrivateElement::Field(v) | PrivateElement::Method(v) => f(v),
        PrivateElement::Accessor { get, set } => {
            if let Some(g) = get {
                f(g);
            }
            if let Some(s) = set {
                f(s);
            }
        }
    }
}

fn walk_value(v: &Value, b: &mut Baseline, q: &mut VecDeque<JsObject>) {
    match v {
        Value::Object(o) => {
            if intern_object(b, o) {
                q.push_back(o.clone());
            }
        }
        Value::Symbol(s) => {
            intern_symbol(b, s);
        }
        _ => {}
    }
}

fn walk_cell(c: &Rc<RefCell<Value>>, b: &mut Baseline, q: &mut VecDeque<JsObject>) {
    if intern_cell(b, c) {
        let v = c.borrow().clone();
        walk_value(&v, b, q);
    }
}

fn walk_frame(f: &Frame, b: &mut Baseline, q: &mut VecDeque<JsObject>) {
    walk_bytecode_fn(&f.func, b, q);
    for v in f.stack.iter().chain(f.locals.iter()).chain(f.args.iter()) {
        walk_value(v, b, q);
    }
    for c in &f.cells {
        walk_cell(c, b, q);
    }
    walk_value(&f.this, b, q);
    walk_value(&f.new_target, b, q);
    walk_value(&f.completion, b, q);
    for v in [&f.pending_throw, &f.pending_return].into_iter().flatten() {
        walk_value(v, b, q);
    }
    if let Some(c) = &f.pending_completion {
        match c {
            Completion::Return(v) | Completion::Throw(v) => walk_value(v, b, q),
            Completion::Jump { .. } => {}
        }
    }
    for scope in &f.dispose_scopes {
        for (a, d) in scope {
            walk_value(a, b, q);
            walk_value(d, b, q);
        }
    }
    for o in f.with_scope.iter().chain(f.eval_vars.iter()) {
        if intern_object(b, o) {
            q.push_back(o.clone());
        }
    }
    if let Some(o) = &f.func_obj {
        if intern_object(b, o) {
            q.push_back(o.clone());
        }
    }
}

fn walk_bytecode_fn(bf: &Rc<BytecodeFunction>, b: &mut Baseline, q: &mut VecDeque<JsObject>) {
    for c in &bf.upvalues {
        walk_cell(c, b, q);
    }
    if let Some(h) = &bf.home_object {
        if intern_object(b, h) {
            q.push_back(h.clone());
        }
    }
    for w in &bf.captured_with {
        if intern_object(b, w) {
            q.push_back(w.clone());
        }
    }
}

fn walk_internal(internal: &Internal, b: &mut Baseline, q: &mut VecDeque<JsObject>) {
    match internal {
        Internal::Array(items) => {
            for v in items {
                walk_value(v, b, q);
            }
        }
        Internal::Arguments(slots) => {
            for c in slots.iter().flatten() {
                walk_cell(c, b, q);
            }
        }
        Internal::Map(m) | Internal::WeakMap(m) => {
            for (k, v) in m {
                walk_value(&k.0, b, q);
                walk_value(v, b, q);
            }
        }
        Internal::Set(s) | Internal::WeakSet(s) => {
            for (k, _) in s {
                walk_value(&k.0, b, q);
            }
        }
        Internal::Function(FunctionInner::Bytecode(bf)) => walk_bytecode_fn(bf, b, q),
        Internal::Function(FunctionInner::Bound(bound)) => {
            if intern_object(b, &bound.target) {
                q.push_back(bound.target.clone());
            }
            walk_value(&bound.bound_this, b, q);
            for v in &bound.bound_args {
                walk_value(v, b, q);
            }
        }
        Internal::Function(FunctionInner::Native(_)) => {}
        Internal::Promise(p) => {
            match &p.state {
                PromiseState::Fulfilled(v) | PromiseState::Rejected(v) => walk_value(v, b, q),
                PromiseState::Pending => {}
            }
            for r in p.fulfill_reactions.iter().chain(p.reject_reactions.iter()) {
                match r {
                    Reaction::Then {
                        handler,
                        result_capability,
                        ..
                    } => {
                        if let Some(h) = handler {
                            walk_value(h, b, q);
                        }
                        if intern_object(b, result_capability) {
                            q.push_back(result_capability.clone());
                        }
                    }
                    Reaction::AsyncResume {
                        frame, own_promise, ..
                    } => {
                        if intern_object(b, own_promise) {
                            q.push_back(own_promise.clone());
                        }
                        if let Some(f) = frame.borrow().as_ref() {
                            walk_frame(f, b, q);
                        }
                    }
                }
            }
        }
        Internal::Generator(g) => {
            match &g.state {
                GeneratorState::SuspendedStart(f) | GeneratorState::SuspendedYield(f) => {
                    walk_frame(f, b, q)
                }
                GeneratorState::Executing | GeneratorState::Completed => {}
            }
            for req in &g.queue {
                walk_value(&req.value, b, q);
                if intern_object(b, &req.result) {
                    q.push_back(req.result.clone());
                }
            }
        }
        Internal::TypedArray(t) => {
            if intern_object(b, &t.buffer) {
                q.push_back(t.buffer.clone());
            }
        }
        Internal::DataView(d) => {
            if intern_object(b, &d.buffer) {
                q.push_back(d.buffer.clone());
            }
        }
        Internal::Proxy(p) => {
            if intern_object(b, &p.target) {
                q.push_back(p.target.clone());
            }
            if intern_object(b, &p.handler) {
                q.push_back(p.handler.clone());
            }
        }
        Internal::Iterator(it) => {
            if let Some(t) = &it.target {
                if intern_object(b, t) {
                    q.push_back(t.clone());
                }
            }
        }
        Internal::ModuleNamespace(ns) => {
            for c in ns.exports.values() {
                walk_cell(c, b, q);
            }
        }
        Internal::Symbol(s) => {
            intern_symbol(b, s);
        }
        _ => {}
    }
}

/// Structural hash of one object: enough to notice a script adding, removing
/// or replacing a property, swapping a prototype, or freezing it.
fn fingerprint(b: &Baseline, o: &JsObject) -> u64 {
    let mut h = Fnv::new();
    let data = o.borrow();
    match &data.proto {
        Some(p) => {
            h.u8(1);
            h.u32(b.obj_ids.get(&p.ptr_id()).copied().unwrap_or(u32::MAX));
        }
        None => h.u8(0),
    }
    h.u8(data.extensible as u8);
    for (key, prop) in data.own_iter() {
        match key {
            PropertyKey::Str(s) => {
                h.u8(1);
                h.str(s.as_str());
            }
            PropertyKey::Sym(s) => {
                h.u8(2);
                h.u32(
                    b.sym_ids
                        .get(&(Rc::as_ptr(&s.0) as *const () as usize))
                        .copied()
                        .unwrap_or(u32::MAX),
                );
            }
        }
        h.u8(prop.enumerable as u8);
        h.u8(prop.configurable as u8);
        match &prop.kind {
            PropertyKind::Data { value, writable } => {
                h.u8(0);
                h.u8(*writable as u8);
                hash_value(b, value, &mut h);
            }
            PropertyKind::Accessor { get, set } => {
                h.u8(1);
                for v in [get, set] {
                    match v {
                        Some(v) => {
                            h.u8(1);
                            hash_value(b, v, &mut h);
                        }
                        None => h.u8(0),
                    }
                }
            }
        }
    }
    h.u8(internal_tag(&data.internal));
    h.u64(data.privates.as_ref().map_or(0, |p| p.len() as u64));
    h.finish()
}

fn hash_value(b: &Baseline, v: &Value, h: &mut Fnv) {
    match v {
        Value::Undefined => h.u8(0),
        Value::Null => h.u8(1),
        Value::Bool(x) => {
            h.u8(2);
            h.u8(*x as u8);
        }
        Value::Number(n) => {
            h.u8(3);
            h.u64(n.to_bits());
        }
        Value::String(s) => {
            h.u8(4);
            h.str(s.as_str());
        }
        Value::Symbol(s) => {
            h.u8(5);
            h.u32(
                b.sym_ids
                    .get(&(Rc::as_ptr(&s.0) as *const () as usize))
                    .copied()
                    .unwrap_or(u32::MAX),
            );
        }
        Value::Object(o) => {
            h.u8(6);
            h.u32(b.obj_ids.get(&o.ptr_id()).copied().unwrap_or(u32::MAX));
        }
        Value::BigInt(n) => {
            h.u8(7);
            h.str(&n.to_string());
        }
        Value::Uninitialized => h.u8(8),
        Value::Hole => h.u8(9),
    }
}

fn internal_tag(i: &Internal) -> u8 {
    match i {
        Internal::Ordinary => 0,
        Internal::Array(_) => 1,
        Internal::Function(_) => 2,
        Internal::Error => 3,
        Internal::Boolean(_) => 4,
        Internal::Number(_) => 5,
        Internal::StringObj(_) => 6,
        Internal::Symbol(_) => 7,
        Internal::Map(_) => 8,
        Internal::Set(_) => 9,
        Internal::WeakMap(_) => 10,
        Internal::WeakSet(_) => 11,
        Internal::Promise(_) => 12,
        Internal::Generator(_) => 13,
        Internal::Date(_) => 14,
        Internal::Arguments(_) => 15,
        Internal::Iterator(_) => 16,
        Internal::ArrayBuffer(_) => 17,
        Internal::TypedArray(_) => 18,
        Internal::DataView(_) => 19,
        Internal::BigIntObj(_) => 20,
        Internal::Proxy(_) => 21,
        Internal::ModuleNamespace(_) => 22,
        Internal::Temporal(_) => 23,
        Internal::IteratorHelper(_) => 24,
    }
}

// ---------------------------------------------------------------------------
// The on-disk shape
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ORef {
    Base(u32),
    Img(u32),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum CRef {
    Base(u32),
    Img(u32),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SRef {
    Base(u32),
    Img(u32),
}

/// A `Value`. Numbers travel as raw bits — JSON has no NaN or infinity, and a
/// silently-`null`ed NaN would be a wrong resume rather than a failed one.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum VImg {
    Undef,
    Null,
    Bool(bool),
    Num(u64),
    /// Well-formed text.
    Str(String),
    /// Text containing unpaired surrogates: raw UTF-16 code units.
    StrU(Vec<u16>),
    Sym(SRef),
    Obj(ORef),
    BigInt(String),
    Uninit,
    Hole,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum KeyImg {
    Str(String),
    StrU(Vec<u16>),
    Sym(SRef),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum PropImg {
    Data {
        value: VImg,
        writable: bool,
        enumerable: bool,
        configurable: bool,
    },
    Accessor {
        get: Option<VImg>,
        set: Option<VImg>,
        enumerable: bool,
        configurable: bool,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum PrivImg {
    Field(VImg),
    Method(VImg),
    Accessor {
        get: Option<VImg>,
        set: Option<VImg>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PrivEnvImg {
    parent: Option<u32>,
    names: Vec<(String, u64, String)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FuncImg {
    proto: ProtoRef,
    upvalues: Vec<CRef>,
    home_object: Option<ORef>,
    is_class_ctor: bool,
    captured_with: Vec<ORef>,
    captured_priv_env: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HandlerImg {
    catch_ip: Option<u32>,
    finally_ip: Option<u32>,
    stack_depth: u32,
    with_depth: u32,
    priv_env: Option<u32>,
    delegation: bool,
    delegation_return_ip: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum CompletionImg {
    Return(VImg),
    Throw(VImg),
    Jump { target: u32, boundary: u32 },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FrameImg {
    func: u32,
    ip: u64,
    stack: Vec<VImg>,
    locals: Vec<VImg>,
    cells: Vec<CRef>,
    this: VImg,
    new_target: VImg,
    handlers: Vec<HandlerImg>,
    pending_completion: Option<CompletionImg>,
    pending_throw: Option<VImg>,
    unwind_pos: Option<u32>,
    pending_return: Option<VImg>,
    args: Vec<VImg>,
    func_obj: Option<ORef>,
    dispose_scopes: Vec<Vec<(VImg, VImg)>>,
    completion: VImg,
    enumerators: Vec<(Vec<KeyStrImg>, u64)>,
    with_scope: Vec<ORef>,
    skip_delegation_throw: bool,
    eval_vars: Option<ORef>,
    priv_env: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum KeyStrImg {
    Str(String),
    StrU(Vec<u16>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ReactionImg {
    Then {
        handler: Option<VImg>,
        result_capability: ORef,
        is_reject: bool,
    },
    AsyncResume {
        slot: u32,
        own_promise: ORef,
        is_reject: bool,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum PromStateImg {
    Pending,
    Fulfilled(VImg),
    Rejected(VImg),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PromImg {
    state: PromStateImg,
    fulfill: Vec<ReactionImg>,
    reject: Vec<ReactionImg>,
    handled: bool,
    host_id: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum GenStateImg {
    SuspendedStart(u32),
    SuspendedYield(u32),
    Completed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GenReqImg {
    kind: u8,
    value: VImg,
    result: ORef,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GenImg {
    state: GenStateImg,
    is_async: bool,
    queue: Vec<GenReqImg>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IterImg {
    target: Option<ORef>,
    string: Option<KeyStrImg>,
    index: u64,
    kind: u8,
    done: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum IntImg {
    Ordinary,
    Array(Vec<VImg>),
    Bytecode(u32),
    Bound {
        target: ORef,
        this: VImg,
        args: Vec<VImg>,
    },
    Error,
    Boolean(bool),
    Number(u64),
    StringObj(KeyStrImg),
    Symbol(SRef),
    Map(Vec<(VImg, VImg)>),
    Set(Vec<VImg>),
    WeakMap(Vec<(VImg, VImg)>),
    WeakSet(Vec<VImg>),
    Promise(PromImg),
    Generator(GenImg),
    Date(u64),
    Arguments(Vec<Option<CRef>>),
    Iterator(IterImg),
    ArrayBuffer(Option<Vec<u8>>),
    TypedArray {
        buffer: ORef,
        byte_offset: u64,
        length: u64,
        kind: u8,
        length_tracking: bool,
    },
    DataView {
        buffer: ORef,
        byte_offset: u64,
        byte_length: u64,
        length_tracking: bool,
    },
    BigIntObj(String),
    Proxy {
        target: ORef,
        handler: ORef,
        revoked: bool,
        callable: bool,
    },
    ModuleNamespace(Vec<(KeyStrImg, CRef)>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObjImg {
    proto: Option<ORef>,
    extensible: bool,
    props: Vec<(KeyImg, PropImg)>,
    privates: Vec<(u64, PrivImg)>,
    internal: IntImg,
}

/// A baseline object whose structure changed after the baseline was taken —
/// the global object always, anything monkey-patched sometimes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Overlay {
    id: u32,
    proto: Option<ORef>,
    extensible: bool,
    props: Vec<(KeyImg, PropImg)>,
    privates: Vec<(u64, PrivImg)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SymImg {
    description: Option<String>,
    id: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UnitImg {
    key: String,
    digest: u64,
}

/// The image itself.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VmImage {
    pub version: u32,
    pub baseline_digest: u64,
    units: Vec<UnitImg>,
    objects: Vec<ObjImg>,
    cells: Vec<VImg>,
    symbols: Vec<SymImg>,
    funcs: Vec<FuncImg>,
    priv_envs: Vec<PrivEnvImg>,
    frames: Vec<FrameImg>,
    /// Async-resume frame slots. `None` once the settlement that owned it has
    /// already reclaimed the frame.
    frame_slots: Vec<Option<u32>>,
    overlays: Vec<Overlay>,
    microtasks: Vec<(ReactionImg, VImg)>,
    pending_host: Vec<(u64, ORef)>,
    next_host_id: u64,
    symbol_counter: u64,
    private_name_counter: u64,
    rng_state: u64,
    unhandled_rejections: Vec<VImg>,
    console_log: Vec<String>,
    symbol_registry: Vec<(String, SRef)>,
}

impl VmImage {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    pub fn from_bytes(bytes: &[u8]) -> R<VmImage> {
        serde_json::from_slice(bytes).map_err(|e| ImageError::Decode(e.to_string()))
    }

    /// Objects written into the image (baseline references excluded) — the
    /// size term that replaces O(history).
    pub fn live_object_count(&self) -> usize {
        self.objects.len()
    }
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

struct Encoder<'a> {
    base: &'a Baseline,
    protos: HashMap<usize, ProtoRef>,
    obj_ids: HashMap<usize, u32>,
    objects: Vec<Option<ObjImg>>,
    obj_order: Vec<JsObject>,
    cell_ids: HashMap<usize, u32>,
    cells: Vec<Option<VImg>>,
    cell_order: Vec<Rc<RefCell<Value>>>,
    sym_ids: HashMap<usize, u32>,
    symbols: Vec<SymImg>,
    func_ids: HashMap<usize, u32>,
    funcs: Vec<Option<FuncImg>>,
    func_order: Vec<Rc<BytecodeFunction>>,
    penv_ids: HashMap<usize, u32>,
    priv_envs: Vec<Option<PrivEnvImg>>,
    penv_order: Vec<Rc<PrivateEnv>>,
    frames: Vec<Option<FrameImg>>,
    slot_ids: HashMap<usize, u32>,
    frame_slots: Vec<Option<u32>>,
}

impl<'a> Encoder<'a> {
    fn new(base: &'a Baseline, protos: HashMap<usize, ProtoRef>) -> Encoder<'a> {
        Encoder {
            base,
            protos,
            obj_ids: HashMap::new(),
            objects: Vec::new(),
            obj_order: Vec::new(),
            cell_ids: HashMap::new(),
            cells: Vec::new(),
            cell_order: Vec::new(),
            sym_ids: HashMap::new(),
            symbols: Vec::new(),
            func_ids: HashMap::new(),
            funcs: Vec::new(),
            func_order: Vec::new(),
            penv_ids: HashMap::new(),
            priv_envs: Vec::new(),
            penv_order: Vec::new(),
            frames: Vec::new(),
            slot_ids: HashMap::new(),
            frame_slots: Vec::new(),
        }
    }

    fn obj_ref(&mut self, o: &JsObject) -> ORef {
        let ptr = o.ptr_id();
        if let Some(id) = self.base.obj_ids.get(&ptr) {
            return ORef::Base(*id);
        }
        if let Some(id) = self.obj_ids.get(&ptr) {
            return ORef::Img(*id);
        }
        let id = self.objects.len() as u32;
        self.obj_ids.insert(ptr, id);
        self.objects.push(None);
        self.obj_order.push(o.clone());
        ORef::Img(id)
    }

    fn cell_ref(&mut self, c: &Rc<RefCell<Value>>) -> CRef {
        let ptr = Rc::as_ptr(c) as *const () as usize;
        if let Some(id) = self.base.cell_ids.get(&ptr) {
            return CRef::Base(*id);
        }
        if let Some(id) = self.cell_ids.get(&ptr) {
            return CRef::Img(*id);
        }
        let id = self.cells.len() as u32;
        self.cell_ids.insert(ptr, id);
        self.cells.push(None);
        self.cell_order.push(c.clone());
        CRef::Img(id)
    }

    fn sym_ref(&mut self, s: &JsSymbol) -> SRef {
        let ptr = Rc::as_ptr(&s.0) as *const () as usize;
        if let Some(id) = self.base.sym_ids.get(&ptr) {
            return SRef::Base(*id);
        }
        if let Some(id) = self.sym_ids.get(&ptr) {
            return SRef::Img(*id);
        }
        let id = self.symbols.len() as u32;
        self.sym_ids.insert(ptr, id);
        self.symbols.push(SymImg {
            description: s.description().map(|d| d.to_string()),
            id: s.0.id,
        });
        SRef::Img(id)
    }

    fn func_ref(&mut self, f: &Rc<BytecodeFunction>) -> R<u32> {
        let ptr = Rc::as_ptr(f) as *const () as usize;
        if let Some(id) = self.func_ids.get(&ptr) {
            return Ok(*id);
        }
        let id = self.funcs.len() as u32;
        self.func_ids.insert(ptr, id);
        self.funcs.push(None);
        self.func_order.push(f.clone());
        Ok(id)
    }

    fn penv_ref(&mut self, e: &Rc<PrivateEnv>) -> u32 {
        let ptr = Rc::as_ptr(e) as *const () as usize;
        if let Some(id) = self.penv_ids.get(&ptr) {
            return *id;
        }
        let id = self.priv_envs.len() as u32;
        self.penv_ids.insert(ptr, id);
        self.priv_envs.push(None);
        self.penv_order.push(e.clone());
        id
    }

    fn slot_ref(&mut self, slot: &Rc<RefCell<Option<Box<Frame>>>>) -> R<u32> {
        let ptr = Rc::as_ptr(slot) as *const () as usize;
        if let Some(id) = self.slot_ids.get(&ptr) {
            return Ok(*id);
        }
        let id = self.frame_slots.len() as u32;
        self.slot_ids.insert(ptr, id);
        self.frame_slots.push(None);
        let frame_id = match slot.borrow().as_ref() {
            Some(f) => Some(self.frame(f)?),
            None => None,
        };
        self.frame_slots[id as usize] = frame_id;
        Ok(id)
    }

    fn value(&mut self, v: &Value) -> R<VImg> {
        Ok(match v {
            Value::Undefined => VImg::Undef,
            Value::Null => VImg::Null,
            Value::Bool(b) => VImg::Bool(*b),
            Value::Number(n) => VImg::Num(n.to_bits()),
            Value::String(s) => self.string(s),
            Value::Symbol(s) => VImg::Sym(self.sym_ref(s)),
            Value::Object(o) => VImg::Obj(self.obj_ref(o)),
            Value::BigInt(n) => VImg::BigInt(n.to_string()),
            Value::Uninitialized => VImg::Uninit,
            Value::Hole => VImg::Hole,
        })
    }

    fn string(&self, s: &JsString) -> VImg {
        match str_img(s) {
            KeyStrImg::Str(t) => VImg::Str(t),
            KeyStrImg::StrU(u) => VImg::StrU(u),
        }
    }

    fn key(&mut self, k: &PropertyKey) -> KeyImg {
        match k {
            PropertyKey::Str(s) => match str_img(s) {
                KeyStrImg::Str(t) => KeyImg::Str(t),
                KeyStrImg::StrU(u) => KeyImg::StrU(u),
            },
            PropertyKey::Sym(s) => KeyImg::Sym(self.sym_ref(s)),
        }
    }

    fn prop(&mut self, p: &Property) -> R<PropImg> {
        Ok(match &p.kind {
            PropertyKind::Data { value, writable } => PropImg::Data {
                value: self.value(value)?,
                writable: *writable,
                enumerable: p.enumerable,
                configurable: p.configurable,
            },
            PropertyKind::Accessor { get, set } => PropImg::Accessor {
                get: match get {
                    Some(g) => Some(self.value(g)?),
                    None => None,
                },
                set: match set {
                    Some(s) => Some(self.value(s)?),
                    None => None,
                },
                enumerable: p.enumerable,
                configurable: p.configurable,
            },
        })
    }

    fn privates(&mut self, data: &crate::value::ObjectData) -> R<Vec<(u64, PrivImg)>> {
        let mut out = Vec::new();
        if let Some(privs) = &data.privates {
            for (id, el) in privs.iter() {
                let img = match el {
                    PrivateElement::Field(v) => PrivImg::Field(self.value(v)?),
                    PrivateElement::Method(v) => PrivImg::Method(self.value(v)?),
                    PrivateElement::Accessor { get, set } => PrivImg::Accessor {
                        get: match get {
                            Some(g) => Some(self.value(g)?),
                            None => None,
                        },
                        set: match set {
                            Some(s) => Some(self.value(s)?),
                            None => None,
                        },
                    },
                };
                out.push((*id, img));
            }
        }
        Ok(out)
    }

    fn props(&mut self, data: &crate::value::ObjectData) -> R<Vec<(KeyImg, PropImg)>> {
        let mut out = Vec::new();
        for (key, prop) in data.own_iter() {
            let k = self.key(key);
            let p = self.prop(prop)?;
            out.push((k, p));
        }
        Ok(out)
    }

    fn object(&mut self, o: &JsObject) -> R<ObjImg> {
        let data = o.borrow();
        let proto = data.proto.as_ref().map(|p| self.obj_ref(p));
        let props = self.props(&data)?;
        let privates = self.privates(&data)?;
        let internal = self.internal(&data.internal)?;
        Ok(ObjImg {
            proto,
            extensible: data.extensible,
            props,
            privates,
            internal,
        })
    }

    fn internal(&mut self, i: &Internal) -> R<IntImg> {
        Ok(match i {
            Internal::Ordinary => IntImg::Ordinary,
            Internal::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for v in items {
                    out.push(self.value(v)?);
                }
                IntImg::Array(out)
            }
            Internal::Function(FunctionInner::Bytecode(bf)) => IntImg::Bytecode(self.func_ref(bf)?),
            Internal::Function(FunctionInner::Bound(b)) => {
                let target = self.obj_ref(&b.target);
                let this = self.value(&b.bound_this)?;
                let mut args = Vec::with_capacity(b.bound_args.len());
                for v in &b.bound_args {
                    args.push(self.value(v)?);
                }
                IntImg::Bound { target, this, args }
            }
            Internal::Function(FunctionInner::Native(nf)) => {
                return unsupported(format!(
                    "native function `{}` created after the image baseline (its Rust \
                     closure state cannot be written down)",
                    nf.name
                ))
            }
            Internal::Error => IntImg::Error,
            Internal::Boolean(b) => IntImg::Boolean(*b),
            Internal::Number(n) => IntImg::Number(n.to_bits()),
            Internal::StringObj(s) => IntImg::StringObj(str_img(s)),
            Internal::Symbol(s) => IntImg::Symbol(self.sym_ref(s)),
            Internal::Map(m) => {
                let mut out = Vec::with_capacity(m.len());
                for (k, v) in m {
                    out.push((self.value(&k.0)?, self.value(v)?));
                }
                IntImg::Map(out)
            }
            Internal::WeakMap(m) => {
                let mut out = Vec::with_capacity(m.len());
                for (k, v) in m {
                    out.push((self.value(&k.0)?, self.value(v)?));
                }
                IntImg::WeakMap(out)
            }
            Internal::Set(s) => {
                let mut out = Vec::with_capacity(s.len());
                for (k, _) in s {
                    out.push(self.value(&k.0)?);
                }
                IntImg::Set(out)
            }
            Internal::WeakSet(s) => {
                let mut out = Vec::with_capacity(s.len());
                for (k, _) in s {
                    out.push(self.value(&k.0)?);
                }
                IntImg::WeakSet(out)
            }
            Internal::Promise(p) => IntImg::Promise(self.promise(p)?),
            Internal::Generator(g) => IntImg::Generator(self.generator(g)?),
            Internal::Date(t) => IntImg::Date(t.to_bits()),
            Internal::Arguments(slots) => {
                let mut out = Vec::with_capacity(slots.len());
                for s in slots {
                    out.push(s.as_ref().map(|c| self.cell_ref(c)));
                }
                IntImg::Arguments(out)
            }
            Internal::Iterator(it) => IntImg::Iterator(IterImg {
                target: it.target.as_ref().map(|t| self.obj_ref(t)),
                string: it.string.as_ref().map(str_img),
                index: it.index as u64,
                kind: iter_kind_tag(it.kind),
                done: it.done,
            }),
            Internal::ArrayBuffer(b) => IntImg::ArrayBuffer(b.clone()),
            Internal::TypedArray(t) => IntImg::TypedArray {
                buffer: self.obj_ref(&t.buffer),
                byte_offset: t.byte_offset as u64,
                length: t.length as u64,
                kind: ta_kind_tag(t.kind),
                length_tracking: t.length_tracking,
            },
            Internal::DataView(d) => IntImg::DataView {
                buffer: self.obj_ref(&d.buffer),
                byte_offset: d.byte_offset as u64,
                byte_length: d.byte_length as u64,
                length_tracking: d.length_tracking,
            },
            Internal::BigIntObj(n) => IntImg::BigIntObj(n.to_string()),
            Internal::Proxy(p) => IntImg::Proxy {
                target: self.obj_ref(&p.target),
                handler: self.obj_ref(&p.handler),
                revoked: p.revoked,
                callable: p.callable,
            },
            Internal::ModuleNamespace(ns) => {
                let mut out = Vec::with_capacity(ns.exports.len());
                for (k, c) in ns.exports.iter() {
                    let key = str_img(k);
                    out.push((key, self.cell_ref(c)));
                }
                IntImg::ModuleNamespace(out)
            }
            Internal::Temporal(_) => {
                return unsupported("a Temporal object is live (its backing slot is opaque)")
            }
            Internal::IteratorHelper(_) => {
                return unsupported(
                    "an Iterator helper (map/filter/take/drop/flatMap) is live in the heap",
                )
            }
        })
    }

    fn promise(&mut self, p: &PromiseData) -> R<PromImg> {
        let state = match &p.state {
            PromiseState::Pending => PromStateImg::Pending,
            PromiseState::Fulfilled(v) => PromStateImg::Fulfilled(self.value(v)?),
            PromiseState::Rejected(v) => PromStateImg::Rejected(self.value(v)?),
        };
        let mut fulfill = Vec::with_capacity(p.fulfill_reactions.len());
        for r in &p.fulfill_reactions {
            fulfill.push(self.reaction(r)?);
        }
        let mut reject = Vec::with_capacity(p.reject_reactions.len());
        for r in &p.reject_reactions {
            reject.push(self.reaction(r)?);
        }
        Ok(PromImg {
            state,
            fulfill,
            reject,
            handled: p.handled,
            host_id: p.host_id,
        })
    }

    fn reaction(&mut self, r: &Reaction) -> R<ReactionImg> {
        Ok(match r {
            Reaction::Then {
                handler,
                result_capability,
                is_reject,
            } => ReactionImg::Then {
                handler: match handler {
                    Some(h) => Some(self.value(h)?),
                    None => None,
                },
                result_capability: self.obj_ref(result_capability),
                is_reject: *is_reject,
            },
            Reaction::AsyncResume {
                frame,
                own_promise,
                is_reject,
            } => ReactionImg::AsyncResume {
                slot: self.slot_ref(frame)?,
                own_promise: self.obj_ref(own_promise),
                is_reject: *is_reject,
            },
        })
    }

    fn generator(&mut self, g: &GeneratorData) -> R<GenImg> {
        let state =
            match &g.state {
                GeneratorState::SuspendedStart(f) => GenStateImg::SuspendedStart(self.frame(f)?),
                GeneratorState::SuspendedYield(f) => GenStateImg::SuspendedYield(self.frame(f)?),
                GeneratorState::Completed => GenStateImg::Completed,
                GeneratorState::Executing => return unsupported(
                    "a generator is mid-step (its frame lives on the native stack, not the heap)",
                ),
            };
        let mut queue = Vec::with_capacity(g.queue.len());
        for req in &g.queue {
            queue.push(GenReqImg {
                kind: resume_kind_tag(req.kind),
                value: self.value(&req.value)?,
                result: self.obj_ref(&req.result),
            });
        }
        Ok(GenImg {
            state,
            is_async: g.is_async,
            queue,
        })
    }

    fn frame(&mut self, f: &Frame) -> R<u32> {
        let id = self.frames.len() as u32;
        self.frames.push(None);
        let func = self.func_ref(&f.func)?;
        let mut stack = Vec::with_capacity(f.stack.len());
        for v in &f.stack {
            stack.push(self.value(v)?);
        }
        let mut locals = Vec::with_capacity(f.locals.len());
        for v in &f.locals {
            locals.push(self.value(v)?);
        }
        let cells = f.cells.iter().map(|c| self.cell_ref(c)).collect();
        let this = self.value(&f.this)?;
        let new_target = self.value(&f.new_target)?;
        let handlers = f
            .handlers
            .iter()
            .map(|h| HandlerImg {
                catch_ip: h.catch_ip,
                finally_ip: h.finally_ip,
                stack_depth: h.stack_depth as u32,
                with_depth: h.with_depth as u32,
                priv_env: h.priv_env.as_ref().map(|e| self.penv_ref(e)),
                delegation: h.delegation,
                delegation_return_ip: h.delegation_return_ip,
            })
            .collect::<Vec<_>>();
        let pending_completion = match &f.pending_completion {
            Some(Completion::Return(v)) => Some(CompletionImg::Return(self.value(v)?)),
            Some(Completion::Throw(v)) => Some(CompletionImg::Throw(self.value(v)?)),
            Some(Completion::Jump { target, boundary }) => Some(CompletionImg::Jump {
                target: *target,
                boundary: *boundary,
            }),
            None => None,
        };
        let pending_throw = match &f.pending_throw {
            Some(v) => Some(self.value(v)?),
            None => None,
        };
        let pending_return = match &f.pending_return {
            Some(v) => Some(self.value(v)?),
            None => None,
        };
        let mut args = Vec::with_capacity(f.args.len());
        for v in &f.args {
            args.push(self.value(v)?);
        }
        let func_obj = f.func_obj.as_ref().map(|o| self.obj_ref(o));
        let mut dispose_scopes = Vec::with_capacity(f.dispose_scopes.len());
        for scope in &f.dispose_scopes {
            let mut out = Vec::with_capacity(scope.len());
            for (a, d) in scope {
                out.push((self.value(a)?, self.value(d)?));
            }
            dispose_scopes.push(out);
        }
        let completion = self.value(&f.completion)?;
        let enumerators = f
            .enumerators
            .iter()
            .map(|(keys, cursor)| (keys.iter().map(str_img).collect(), *cursor as u64))
            .collect();
        let with_scope = f
            .with_scope
            .iter()
            .map(|o| self.obj_ref(o))
            .collect::<Vec<_>>();
        let eval_vars = f.eval_vars.as_ref().map(|o| self.obj_ref(o));
        let priv_env = f.priv_env.as_ref().map(|e| self.penv_ref(e));

        self.frames[id as usize] = Some(FrameImg {
            func,
            ip: f.ip as u64,
            stack,
            locals,
            cells,
            this,
            new_target,
            handlers,
            pending_completion,
            pending_throw,
            unwind_pos: f.unwind_pos,
            pending_return,
            args,
            func_obj,
            dispose_scopes,
            completion,
            enumerators,
            with_scope,
            skip_delegation_throw: f.skip_delegation_throw,
            eval_vars,
            priv_env,
        });
        Ok(id)
    }

    fn bytecode_fn(&mut self, bf: &Rc<BytecodeFunction>) -> R<FuncImg> {
        let ptr = Rc::as_ptr(&bf.proto) as *const () as usize;
        let proto = self.protos.get(&ptr).cloned().ok_or_else(|| {
            ImageError::Unsupported(format!(
                "closure over `{}` comes from a compilation unit that was never registered \
                 with register_image_unit",
                bf.proto.name
            ))
        })?;
        Ok(FuncImg {
            proto,
            upvalues: bf.upvalues.iter().map(|c| self.cell_ref(c)).collect(),
            home_object: bf.home_object.as_ref().map(|h| self.obj_ref(h)),
            is_class_ctor: bf.is_class_ctor,
            captured_with: bf.captured_with.iter().map(|w| self.obj_ref(w)).collect(),
            captured_priv_env: bf.captured_priv_env.as_ref().map(|e| self.penv_ref(e)),
        })
    }

    fn priv_env(&mut self, e: &Rc<PrivateEnv>) -> PrivEnvImg {
        PrivEnvImg {
            parent: e.parent.as_ref().map(|p| self.penv_ref(p)),
            names: e
                .names
                .iter()
                .map(|(k, n)| {
                    (
                        k.as_str().to_string(),
                        n.id,
                        n.description.as_str().to_string(),
                    )
                })
                .collect(),
        }
    }

    /// Work the queues to fixpoint: encoding one object can discover more.
    fn drain(&mut self) -> R<()> {
        loop {
            let mut progress = false;
            for i in 0..self.objects.len() {
                if self.objects[i].is_none() {
                    let o = self.obj_order[i].clone();
                    let img = self.object(&o)?;
                    self.objects[i] = Some(img);
                    progress = true;
                }
            }
            for i in 0..self.cells.len() {
                if self.cells[i].is_none() {
                    let v = self.cell_order[i].borrow().clone();
                    let img = self.value(&v)?;
                    self.cells[i] = Some(img);
                    progress = true;
                }
            }
            for i in 0..self.funcs.len() {
                if self.funcs[i].is_none() {
                    let f = self.func_order[i].clone();
                    let img = self.bytecode_fn(&f)?;
                    self.funcs[i] = Some(img);
                    progress = true;
                }
            }
            for i in 0..self.priv_envs.len() {
                if self.priv_envs[i].is_none() {
                    let e = self.penv_order[i].clone();
                    let img = self.priv_env(&e);
                    self.priv_envs[i] = Some(img);
                    progress = true;
                }
            }
            if !progress {
                return Ok(());
            }
        }
    }

    /// Baseline objects whose fingerprint moved — write their whole property
    /// table so restore can reapply it.
    fn overlays(&mut self) -> R<Vec<Overlay>> {
        let mut out = Vec::new();
        for i in 0..self.base.objs.len() {
            let o = self.base.objs[i].clone();
            if fingerprint(self.base, &o) == self.base.prints[i] {
                continue;
            }
            let data = o.borrow();
            if internal_tag(&data.internal) != 0
                && !matches!(
                    &data.internal,
                    Internal::Function(_) | Internal::Error | Internal::Array(_)
                )
            {
                // A baseline object whose *internal slot* changed kind is a
                // shape we do not attempt to patch (nothing in script can do
                // it today; refusing keeps the format honest if that changes).
                let tag = internal_tag(&data.internal);
                if tag != internal_tag(&Internal::Ordinary) && overlay_forbidden(tag) {
                    return unsupported(format!(
                        "baseline object #{i} changed in a way overlays cannot express \
                         (internal slot kind {tag})"
                    ));
                }
            }
            let proto = data.proto.as_ref().map(|p| self.obj_ref(p));
            let props = self.props(&data)?;
            let privates = self.privates(&data)?;
            out.push(Overlay {
                id: i as u32,
                proto,
                extensible: data.extensible,
                props,
                privates,
            });
        }
        Ok(out)
    }
}

/// Internal slots an overlay cannot rebuild (they carry state beyond
/// properties). Baseline objects with these are refused if they went dirty.
fn overlay_forbidden(tag: u8) -> bool {
    matches!(tag, 12 | 13 | 21 | 23 | 24)
}

fn str_img(s: &JsString) -> KeyStrImg {
    if s.is_well_formed() {
        KeyStrImg::Str(s.as_str().to_string())
    } else {
        KeyStrImg::StrU(s.to_utf16_vec())
    }
}

fn iter_kind_tag(k: IterKind) -> u8 {
    match k {
        IterKind::ArrayKeys => 0,
        IterKind::ArrayValues => 1,
        IterKind::ArrayEntries => 2,
        IterKind::StringChars => 3,
        IterKind::MapKeys => 4,
        IterKind::MapValues => 5,
        IterKind::MapEntries => 6,
        IterKind::SetValues => 7,
        IterKind::SetEntries => 8,
    }
}

fn iter_kind_of(tag: u8) -> R<IterKind> {
    Ok(match tag {
        0 => IterKind::ArrayKeys,
        1 => IterKind::ArrayValues,
        2 => IterKind::ArrayEntries,
        3 => IterKind::StringChars,
        4 => IterKind::MapKeys,
        5 => IterKind::MapValues,
        6 => IterKind::MapEntries,
        7 => IterKind::SetValues,
        8 => IterKind::SetEntries,
        _ => return Err(ImageError::Decode(format!("bad iterator kind {tag}"))),
    })
}

fn ta_kind_tag(k: TAKind) -> u8 {
    match k {
        TAKind::I8 => 0,
        TAKind::U8 => 1,
        TAKind::U8Clamped => 2,
        TAKind::I16 => 3,
        TAKind::U16 => 4,
        TAKind::I32 => 5,
        TAKind::U32 => 6,
        TAKind::F32 => 7,
        TAKind::F64 => 8,
        TAKind::I64 => 9,
        TAKind::U64 => 10,
    }
}

fn ta_kind_of(tag: u8) -> R<TAKind> {
    Ok(match tag {
        0 => TAKind::I8,
        1 => TAKind::U8,
        2 => TAKind::U8Clamped,
        3 => TAKind::I16,
        4 => TAKind::U16,
        5 => TAKind::I32,
        6 => TAKind::U32,
        7 => TAKind::F32,
        8 => TAKind::F64,
        9 => TAKind::I64,
        10 => TAKind::U64,
        _ => return Err(ImageError::Decode(format!("bad typed-array kind {tag}"))),
    })
}

fn resume_kind_tag(k: crate::generator::ResumeKind) -> u8 {
    match k {
        crate::generator::ResumeKind::Next => 0,
        crate::generator::ResumeKind::Throw => 1,
        crate::generator::ResumeKind::Return => 2,
    }
}

fn resume_kind_of(tag: u8) -> R<crate::generator::ResumeKind> {
    Ok(match tag {
        0 => crate::generator::ResumeKind::Next,
        1 => crate::generator::ResumeKind::Throw,
        2 => crate::generator::ResumeKind::Return,
        _ => return Err(ImageError::Decode(format!("bad resume kind {tag}"))),
    })
}

/// Take an image of `vm`, which must have been through
/// [`Vm::mark_image_baseline`].
pub(crate) fn encode(vm: &Vm) -> R<VmImage> {
    let base = vm
        .image_baseline
        .as_ref()
        .ok_or_else(|| ImageError::Mismatch("no image baseline was marked on this VM".into()))?;

    for t in &vm.microtasks {
        if matches!(t, Microtask::Job(_)) {
            return unsupported(
                "a plain microtask job (queueMicrotask / a thenable job) is queued — \
                 Rust closures have no serialized form; snapshot at a drained point",
            );
        }
    }

    let mut protos = HashMap::new();
    let mut units = Vec::new();
    for (i, unit) in vm.image_units.iter().enumerate() {
        protos.extend(index_protos(&unit.root, i as u32));
        units.push(UnitImg {
            key: unit.key.clone(),
            digest: unit_digest(&unit.root),
        });
    }

    let mut enc = Encoder::new(base, protos);

    // Roots of the post-baseline graph: pending host promises, the microtask
    // queue, unhandled rejections, and every baseline object that changed.
    let mut pending_host = Vec::with_capacity(vm.pending_host.len());
    for (id, promise) in vm.pending_host.iter() {
        let r = enc.obj_ref(promise);
        pending_host.push((*id, r));
    }
    let mut microtasks = Vec::new();
    for t in &vm.microtasks {
        if let Microtask::Reaction { reaction, argument } = t {
            let r = enc.reaction(reaction)?;
            let a = enc.value(argument)?;
            microtasks.push((r, a));
        }
    }
    let mut unhandled = Vec::with_capacity(vm.unhandled_rejections.len());
    for v in &vm.unhandled_rejections {
        unhandled.push(enc.value(v)?);
    }
    let mut symbol_registry = Vec::with_capacity(vm.realm.symbol_registry.len());
    for (k, s) in vm.realm.symbol_registry.iter() {
        let r = enc.sym_ref(s);
        symbol_registry.push((k.clone(), r));
    }

    let overlays = enc.overlays()?;
    enc.drain()?;

    let objects = enc
        .objects
        .into_iter()
        .map(|o| o.expect("drain fills every object slot"))
        .collect();
    let cells = enc
        .cells
        .into_iter()
        .map(|c| c.expect("drain fills every cell slot"))
        .collect();
    let funcs = enc
        .funcs
        .into_iter()
        .map(|f| f.expect("drain fills every function slot"))
        .collect();
    let priv_envs = enc
        .priv_envs
        .into_iter()
        .map(|e| e.expect("drain fills every private-env slot"))
        .collect();
    let frames = enc
        .frames
        .into_iter()
        .map(|f| f.expect("every frame slot is written before drain returns"))
        .collect();

    Ok(VmImage {
        version: IMAGE_VERSION,
        baseline_digest: base.digest,
        units,
        objects,
        cells,
        symbols: enc.symbols,
        funcs,
        priv_envs,
        frames,
        frame_slots: enc.frame_slots,
        overlays,
        microtasks,
        pending_host,
        next_host_id: vm.next_host_id,
        symbol_counter: vm.symbol_counter,
        private_name_counter: vm.private_name_counter,
        rng_state: vm.rng_state,
        unhandled_rejections: unhandled,
        console_log: vm.console_log.clone(),
        symbol_registry,
    })
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

struct Decoder<'a> {
    img: &'a VmImage,
    base_objs: Vec<JsObject>,
    base_cells: Vec<Rc<RefCell<Value>>>,
    base_syms: Vec<JsSymbol>,
    objs: Vec<JsObject>,
    cells: Vec<Rc<RefCell<Value>>>,
    syms: Vec<JsSymbol>,
    funcs: Vec<Option<Rc<BytecodeFunction>>>,
    penvs: Vec<Option<Rc<PrivateEnv>>>,
    slots: Vec<Rc<RefCell<Option<Box<Frame>>>>>,
    roots: Vec<Rc<FuncProto>>,
}

impl<'a> Decoder<'a> {
    fn obj(&self, r: &ORef) -> R<JsObject> {
        match r {
            ORef::Base(i) => self
                .base_objs
                .get(*i as usize)
                .cloned()
                .ok_or_else(|| ImageError::Mismatch(format!("baseline object #{i} is missing"))),
            ORef::Img(i) => self
                .objs
                .get(*i as usize)
                .cloned()
                .ok_or_else(|| ImageError::Decode(format!("image object #{i} is missing"))),
        }
    }

    fn cell(&self, r: &CRef) -> R<Rc<RefCell<Value>>> {
        match r {
            CRef::Base(i) => self
                .base_cells
                .get(*i as usize)
                .cloned()
                .ok_or_else(|| ImageError::Mismatch(format!("baseline cell #{i} is missing"))),
            CRef::Img(i) => self
                .cells
                .get(*i as usize)
                .cloned()
                .ok_or_else(|| ImageError::Decode(format!("image cell #{i} is missing"))),
        }
    }

    fn sym(&self, r: &SRef) -> R<JsSymbol> {
        match r {
            SRef::Base(i) => self
                .base_syms
                .get(*i as usize)
                .cloned()
                .ok_or_else(|| ImageError::Mismatch(format!("baseline symbol #{i} is missing"))),
            SRef::Img(i) => self
                .syms
                .get(*i as usize)
                .cloned()
                .ok_or_else(|| ImageError::Decode(format!("image symbol #{i} is missing"))),
        }
    }

    fn value(&self, v: &VImg) -> R<Value> {
        Ok(match v {
            VImg::Undef => Value::Undefined,
            VImg::Null => Value::Null,
            VImg::Bool(b) => Value::Bool(*b),
            VImg::Num(bits) => Value::Number(f64::from_bits(*bits)),
            VImg::Str(s) => Value::String(JsString::new(s)),
            VImg::StrU(u) => Value::String(JsString::from_code_units(u)),
            VImg::Sym(s) => Value::Symbol(self.sym(s)?),
            VImg::Obj(o) => Value::Object(self.obj(o)?),
            VImg::BigInt(s) => Value::BigInt(Rc::new(
                s.parse()
                    .map_err(|_| ImageError::Decode(format!("bad BigInt `{s}`")))?,
            )),
            VImg::Uninit => Value::Uninitialized,
            VImg::Hole => Value::Hole,
        })
    }

    fn jsstring(&self, s: &KeyStrImg) -> JsString {
        match s {
            KeyStrImg::Str(t) => JsString::new(t),
            KeyStrImg::StrU(u) => JsString::from_code_units(u),
        }
    }

    fn key(&self, k: &KeyImg) -> R<PropertyKey> {
        Ok(match k {
            KeyImg::Str(s) => PropertyKey::Str(JsString::new(s)),
            KeyImg::StrU(u) => PropertyKey::Str(JsString::from_code_units(u)),
            KeyImg::Sym(s) => PropertyKey::Sym(self.sym(s)?),
        })
    }

    fn prop(&self, p: &PropImg) -> R<Property> {
        Ok(match p {
            PropImg::Data {
                value,
                writable,
                enumerable,
                configurable,
            } => Property {
                kind: PropertyKind::Data {
                    value: self.value(value)?,
                    writable: *writable,
                },
                enumerable: *enumerable,
                configurable: *configurable,
            },
            PropImg::Accessor {
                get,
                set,
                enumerable,
                configurable,
            } => Property {
                kind: PropertyKind::Accessor {
                    get: match get {
                        Some(g) => Some(self.value(g)?),
                        None => None,
                    },
                    set: match set {
                        Some(s) => Some(self.value(s)?),
                        None => None,
                    },
                },
                enumerable: *enumerable,
                configurable: *configurable,
            },
        })
    }

    fn privates(
        &self,
        list: &[(u64, PrivImg)],
    ) -> R<Option<Box<indexmap::IndexMap<u64, PrivateElement>>>> {
        if list.is_empty() {
            return Ok(None);
        }
        let mut map = indexmap::IndexMap::new();
        for (id, el) in list {
            let e = match el {
                PrivImg::Field(v) => PrivateElement::Field(self.value(v)?),
                PrivImg::Method(v) => PrivateElement::Method(self.value(v)?),
                PrivImg::Accessor { get, set } => PrivateElement::Accessor {
                    get: match get {
                        Some(g) => Some(self.value(g)?),
                        None => None,
                    },
                    set: match set {
                        Some(s) => Some(self.value(s)?),
                        None => None,
                    },
                },
            };
            map.insert(*id, e);
        }
        Ok(Some(Box::new(map)))
    }

    fn func(&mut self, id: u32) -> R<Rc<BytecodeFunction>> {
        if let Some(f) = self.funcs.get(id as usize).and_then(|f| f.clone()) {
            return Ok(f);
        }
        let img = self
            .img
            .funcs
            .get(id as usize)
            .ok_or_else(|| ImageError::Decode(format!("image function #{id} is missing")))?;
        let root = self.roots.get(img.proto.unit as usize).ok_or_else(|| {
            ImageError::Mismatch(format!("compilation unit #{} is missing", img.proto.unit))
        })?;
        let proto = resolve_proto(root, &img.proto.path).ok_or_else(|| {
            ImageError::Mismatch(format!(
                "function proto path {:?} does not resolve in unit #{}",
                img.proto.path, img.proto.unit
            ))
        })?;
        let mut upvalues = Vec::with_capacity(img.upvalues.len());
        for c in &img.upvalues {
            upvalues.push(self.cell(c)?);
        }
        let home_object = match &img.home_object {
            Some(h) => Some(self.obj(h)?),
            None => None,
        };
        let mut captured_with = Vec::with_capacity(img.captured_with.len());
        for w in &img.captured_with {
            captured_with.push(self.obj(w)?);
        }
        let captured_priv_env = match img.captured_priv_env {
            Some(e) => Some(self.penv(e)?),
            None => None,
        };
        let bf = Rc::new(BytecodeFunction {
            proto,
            upvalues,
            home_object,
            is_class_ctor: img.is_class_ctor,
            captured_with,
            captured_priv_env,
        });
        self.funcs[id as usize] = Some(bf.clone());
        Ok(bf)
    }

    fn penv(&mut self, id: u32) -> R<Rc<PrivateEnv>> {
        if let Some(e) = self.penvs.get(id as usize).and_then(|e| e.clone()) {
            return Ok(e);
        }
        let img = self
            .img
            .priv_envs
            .get(id as usize)
            .ok_or_else(|| ImageError::Decode(format!("image private env #{id} is missing")))?;
        let parent = match img.parent {
            Some(p) if p == id => {
                return Err(ImageError::Decode("private env is its own parent".into()))
            }
            Some(p) => Some(self.penv(p)?),
            None => None,
        };
        let env = Rc::new(PrivateEnv {
            parent,
            names: img
                .names
                .iter()
                .map(|(k, nid, desc)| {
                    (
                        JsString::new(k),
                        PrivateName {
                            id: *nid,
                            description: JsString::new(desc),
                        },
                    )
                })
                .collect(),
        });
        self.penvs[id as usize] = Some(env.clone());
        Ok(env)
    }

    fn frame(&mut self, id: u32) -> R<Box<Frame>> {
        let img = self
            .img
            .frames
            .get(id as usize)
            .ok_or_else(|| ImageError::Decode(format!("image frame #{id} is missing")))?
            .clone();
        let func = self.func(img.func)?;
        let mut stack = Vec::with_capacity(img.stack.len());
        for v in &img.stack {
            stack.push(self.value(v)?);
        }
        let mut locals = Vec::with_capacity(img.locals.len());
        for v in &img.locals {
            locals.push(self.value(v)?);
        }
        let mut cells = Vec::with_capacity(img.cells.len());
        for c in &img.cells {
            cells.push(self.cell(c)?);
        }
        let mut handlers = Vec::with_capacity(img.handlers.len());
        for h in &img.handlers {
            handlers.push(TryHandler {
                catch_ip: h.catch_ip,
                finally_ip: h.finally_ip,
                stack_depth: h.stack_depth as usize,
                with_depth: h.with_depth as usize,
                priv_env: match h.priv_env {
                    Some(e) => Some(self.penv(e)?),
                    None => None,
                },
                delegation: h.delegation,
                delegation_return_ip: h.delegation_return_ip,
            });
        }
        let pending_completion = match &img.pending_completion {
            Some(CompletionImg::Return(v)) => Some(Completion::Return(self.value(v)?)),
            Some(CompletionImg::Throw(v)) => Some(Completion::Throw(self.value(v)?)),
            Some(CompletionImg::Jump { target, boundary }) => Some(Completion::Jump {
                target: *target,
                boundary: *boundary,
            }),
            None => None,
        };
        let mut args = Vec::with_capacity(img.args.len());
        for v in &img.args {
            args.push(self.value(v)?);
        }
        let mut dispose_scopes = Vec::with_capacity(img.dispose_scopes.len());
        for scope in &img.dispose_scopes {
            let mut out = Vec::with_capacity(scope.len());
            for (a, d) in scope {
                out.push((self.value(a)?, self.value(d)?));
            }
            dispose_scopes.push(out);
        }
        let mut with_scope = Vec::with_capacity(img.with_scope.len());
        for o in &img.with_scope {
            with_scope.push(self.obj(o)?);
        }
        Ok(Box::new(Frame {
            func,
            ip: img.ip as usize,
            stack,
            locals,
            cells,
            this: self.value(&img.this)?,
            new_target: self.value(&img.new_target)?,
            handlers,
            pending_completion,
            pending_throw: match &img.pending_throw {
                Some(v) => Some(self.value(v)?),
                None => None,
            },
            unwind_pos: img.unwind_pos,
            pending_return: match &img.pending_return {
                Some(v) => Some(self.value(v)?),
                None => None,
            },
            args,
            func_obj: match &img.func_obj {
                Some(o) => Some(self.obj(o)?),
                None => None,
            },
            dispose_scopes,
            completion: self.value(&img.completion)?,
            enumerators: img
                .enumerators
                .iter()
                .map(|(keys, cursor)| {
                    (
                        keys.iter().map(|k| self.jsstring(k)).collect(),
                        *cursor as usize,
                    )
                })
                .collect(),
            with_scope,
            trace_token: None,
            skip_delegation_throw: img.skip_delegation_throw,
            eval_vars: match &img.eval_vars {
                Some(o) => Some(self.obj(o)?),
                None => None,
            },
            priv_env: match img.priv_env {
                Some(e) => Some(self.penv(e)?),
                None => None,
            },
        }))
    }

    fn reaction(&mut self, r: &ReactionImg) -> R<Reaction> {
        Ok(match r {
            ReactionImg::Then {
                handler,
                result_capability,
                is_reject,
            } => Reaction::Then {
                handler: match handler {
                    Some(h) => Some(self.value(h)?),
                    None => None,
                },
                result_capability: self.obj(result_capability)?,
                is_reject: *is_reject,
            },
            ReactionImg::AsyncResume {
                slot,
                own_promise,
                is_reject,
            } => Reaction::AsyncResume {
                frame: self
                    .slots
                    .get(*slot as usize)
                    .cloned()
                    .ok_or_else(|| ImageError::Decode(format!("frame slot #{slot} missing")))?,
                own_promise: self.obj(own_promise)?,
                is_reject: *is_reject,
            },
        })
    }

    fn internal(&mut self, i: &IntImg) -> R<Internal> {
        Ok(match i {
            IntImg::Ordinary => Internal::Ordinary,
            IntImg::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for v in items {
                    out.push(self.value(v)?);
                }
                Internal::Array(out)
            }
            IntImg::Bytecode(id) => Internal::Function(FunctionInner::Bytecode(self.func(*id)?)),
            IntImg::Bound { target, this, args } => {
                let mut bound_args = Vec::with_capacity(args.len());
                for v in args {
                    bound_args.push(self.value(v)?);
                }
                Internal::Function(FunctionInner::Bound(BoundFunction {
                    target: self.obj(target)?,
                    bound_this: self.value(this)?,
                    bound_args,
                }))
            }
            IntImg::Error => Internal::Error,
            IntImg::Boolean(b) => Internal::Boolean(*b),
            IntImg::Number(bits) => Internal::Number(f64::from_bits(*bits)),
            IntImg::StringObj(s) => Internal::StringObj(self.jsstring(s)),
            IntImg::Symbol(s) => Internal::Symbol(self.sym(s)?),
            IntImg::Map(entries) => {
                let mut m = crate::fxhash::FxIndexMap::default();
                for (k, v) in entries {
                    m.insert(MapKey(self.value(k)?), self.value(v)?);
                }
                Internal::Map(m)
            }
            IntImg::WeakMap(entries) => {
                let mut m = crate::fxhash::FxIndexMap::default();
                for (k, v) in entries {
                    m.insert(MapKey(self.value(k)?), self.value(v)?);
                }
                Internal::WeakMap(m)
            }
            IntImg::Set(items) => {
                let mut m = crate::fxhash::FxIndexMap::default();
                for v in items {
                    m.insert(MapKey(self.value(v)?), ());
                }
                Internal::Set(m)
            }
            IntImg::WeakSet(items) => {
                let mut m = crate::fxhash::FxIndexMap::default();
                for v in items {
                    m.insert(MapKey(self.value(v)?), ());
                }
                Internal::WeakSet(m)
            }
            IntImg::Promise(p) => {
                let state = match &p.state {
                    PromStateImg::Pending => PromiseState::Pending,
                    PromStateImg::Fulfilled(v) => PromiseState::Fulfilled(self.value(v)?),
                    PromStateImg::Rejected(v) => PromiseState::Rejected(self.value(v)?),
                };
                let mut fulfill = Vec::with_capacity(p.fulfill.len());
                for r in &p.fulfill {
                    fulfill.push(self.reaction(r)?);
                }
                let mut reject = Vec::with_capacity(p.reject.len());
                for r in &p.reject {
                    reject.push(self.reaction(r)?);
                }
                Internal::Promise(Box::new(PromiseData {
                    state,
                    fulfill_reactions: fulfill,
                    reject_reactions: reject,
                    handled: p.handled,
                    host_id: p.host_id,
                }))
            }
            IntImg::Generator(g) => {
                let state = match &g.state {
                    GenStateImg::SuspendedStart(f) => {
                        GeneratorState::SuspendedStart(self.frame(*f)?)
                    }
                    GenStateImg::SuspendedYield(f) => {
                        GeneratorState::SuspendedYield(self.frame(*f)?)
                    }
                    GenStateImg::Completed => GeneratorState::Completed,
                };
                let mut queue = VecDeque::with_capacity(g.queue.len());
                for req in &g.queue {
                    queue.push_back(AsyncGenRequest {
                        kind: resume_kind_of(req.kind)?,
                        value: self.value(&req.value)?,
                        result: self.obj(&req.result)?,
                    });
                }
                Internal::Generator(GeneratorData {
                    state,
                    is_async: g.is_async,
                    queue,
                })
            }
            IntImg::Date(bits) => Internal::Date(f64::from_bits(*bits)),
            IntImg::Arguments(slots) => {
                let mut out = Vec::with_capacity(slots.len());
                for s in slots {
                    out.push(match s {
                        Some(c) => Some(self.cell(c)?),
                        None => None,
                    });
                }
                Internal::Arguments(out)
            }
            IntImg::Iterator(it) => Internal::Iterator(IterState {
                target: match &it.target {
                    Some(t) => Some(self.obj(t)?),
                    None => None,
                },
                string: it.string.as_ref().map(|s| self.jsstring(s)),
                index: it.index as usize,
                kind: iter_kind_of(it.kind)?,
                done: it.done,
            }),
            IntImg::ArrayBuffer(b) => Internal::ArrayBuffer(b.clone()),
            IntImg::TypedArray {
                buffer,
                byte_offset,
                length,
                kind,
                length_tracking,
            } => Internal::TypedArray(TypedArrayData {
                buffer: self.obj(buffer)?,
                byte_offset: *byte_offset as usize,
                length: *length as usize,
                kind: ta_kind_of(*kind)?,
                length_tracking: *length_tracking,
            }),
            IntImg::DataView {
                buffer,
                byte_offset,
                byte_length,
                length_tracking,
            } => Internal::DataView(DataViewData {
                buffer: self.obj(buffer)?,
                byte_offset: *byte_offset as usize,
                byte_length: *byte_length as usize,
                length_tracking: *length_tracking,
            }),
            IntImg::BigIntObj(s) => Internal::BigIntObj(Rc::new(
                s.parse()
                    .map_err(|_| ImageError::Decode(format!("bad BigInt `{s}`")))?,
            )),
            IntImg::Proxy {
                target,
                handler,
                revoked,
                callable,
            } => Internal::Proxy(ProxyData {
                target: self.obj(target)?,
                handler: self.obj(handler)?,
                revoked: *revoked,
                callable: *callable,
            }),
            IntImg::ModuleNamespace(exports) => {
                let mut map = indexmap::IndexMap::new();
                for (k, c) in exports {
                    map.insert(self.jsstring(k), self.cell(c)?);
                }
                Internal::ModuleNamespace(Box::new(NamespaceData { exports: map }))
            }
        })
    }
}

/// Rebuild `vm`'s post-baseline state from `img`. `vm` must be at the same
/// baseline (same realm construction, same setup scripts) and must have the
/// same compilation units registered.
pub(crate) fn decode(vm: &mut Vm, img: &VmImage) -> R<()> {
    if img.version != IMAGE_VERSION {
        return Err(ImageError::Mismatch(format!(
            "image version {} but this engine writes {IMAGE_VERSION}",
            img.version
        )));
    }
    let base = vm
        .image_baseline
        .as_ref()
        .ok_or_else(|| ImageError::Mismatch("no image baseline was marked on this VM".into()))?;
    if base.digest != img.baseline_digest {
        return Err(ImageError::Mismatch(
            "the restoring VM's baseline differs from the one the image was taken against \
             (different engine build, realm setup, or prelude)"
                .into(),
        ));
    }
    if vm.image_units.len() != img.units.len() {
        return Err(ImageError::Mismatch(format!(
            "image expects {} compilation units, this VM registered {}",
            img.units.len(),
            vm.image_units.len()
        )));
    }
    for (i, (unit, want)) in vm.image_units.iter().zip(img.units.iter()).enumerate() {
        if unit.key != want.key {
            return Err(ImageError::Mismatch(format!(
                "compilation unit #{i} is `{}`, image expects `{}`",
                unit.key, want.key
            )));
        }
        if unit_digest(&unit.root) != want.digest {
            return Err(ImageError::Mismatch(format!(
                "compilation unit `{}` recompiled to different bytecode",
                unit.key
            )));
        }
    }

    let base_objs = base.objs.clone();
    let base_cells = base.cells.clone();
    let base_syms = base.syms.clone();
    let roots: Vec<Rc<FuncProto>> = vm.image_units.iter().map(|u| u.root.clone()).collect();

    // Allocate every image object and cell empty first, so the graph can be
    // wired up with cycles intact.
    let objs: Vec<JsObject> = img
        .objects
        .iter()
        .map(|_| vm.alloc(crate::value::ObjectData::new(None, Internal::Ordinary)))
        .collect();
    let cells: Vec<Rc<RefCell<Value>>> = img
        .cells
        .iter()
        .map(|_| Rc::new(RefCell::new(Value::Undefined)))
        .collect();
    let syms: Vec<JsSymbol> = img
        .symbols
        .iter()
        .map(|s| {
            JsSymbol(Rc::new(SymbolData {
                description: s.description.as_deref().map(Rc::from),
                id: s.id,
            }))
        })
        .collect();
    let slots: Vec<Rc<RefCell<Option<Box<Frame>>>>> = img
        .frame_slots
        .iter()
        .map(|_| Rc::new(RefCell::new(None)))
        .collect();

    let mut dec = Decoder {
        img,
        base_objs,
        base_cells,
        base_syms,
        objs,
        cells,
        syms,
        funcs: vec![None; img.funcs.len()],
        penvs: vec![None; img.priv_envs.len()],
        slots,
        roots,
    };

    // Fill cells.
    for (i, v) in img.cells.iter().enumerate() {
        let value = dec.value(v)?;
        *dec.cells[i].borrow_mut() = value;
    }

    // Fill objects.
    for (i, o) in img.objects.iter().enumerate() {
        let proto = match &o.proto {
            Some(p) => Some(dec.obj(p)?),
            None => None,
        };
        let internal = dec.internal(&o.internal)?;
        let privates = dec.privates(&o.privates)?;
        let mut props = Vec::with_capacity(o.props.len());
        for (k, p) in &o.props {
            props.push((dec.key(k)?, dec.prop(p)?));
        }
        let target = dec.objs[i].clone();
        let mut data = target.borrow_mut();
        data.proto = proto;
        data.extensible = o.extensible;
        data.internal = internal;
        data.privates = privates;
        for (k, p) in props {
            data.own_insert(k, p);
        }
    }

    // Fill async-resume frame slots.
    for (i, f) in img.frame_slots.iter().enumerate() {
        if let Some(fid) = f {
            let frame = dec.frame(*fid)?;
            *dec.slots[i].borrow_mut() = Some(frame);
        }
    }

    // Reapply overlays onto baseline objects.
    for ov in &img.overlays {
        let target =
            dec.base_objs.get(ov.id as usize).cloned().ok_or_else(|| {
                ImageError::Mismatch(format!("baseline object #{} missing", ov.id))
            })?;
        let proto = match &ov.proto {
            Some(p) => Some(dec.obj(p)?),
            None => None,
        };
        let privates = dec.privates(&ov.privates)?;
        let mut props = Vec::with_capacity(ov.props.len());
        for (k, p) in &ov.props {
            props.push((dec.key(k)?, dec.prop(p)?));
        }
        let mut data = target.borrow_mut();
        data.own_clear();
        data.proto = proto;
        data.extensible = ov.extensible;
        data.privates = privates;
        for (k, p) in props {
            data.own_insert(k, p);
        }
    }

    // VM-level state.
    let mut microtasks = VecDeque::with_capacity(img.microtasks.len());
    for (r, a) in &img.microtasks {
        let reaction = dec.reaction(r)?;
        let argument = dec.value(a)?;
        microtasks.push_back(Microtask::Reaction { reaction, argument });
    }
    let mut pending_host = indexmap::IndexMap::new();
    for (id, o) in &img.pending_host {
        pending_host.insert(*id, dec.obj(o)?);
    }
    let mut unhandled = Vec::with_capacity(img.unhandled_rejections.len());
    for v in &img.unhandled_rejections {
        unhandled.push(dec.value(v)?);
    }
    let mut registry = indexmap::IndexMap::new();
    for (k, s) in &img.symbol_registry {
        registry.insert(k.clone(), dec.sym(s)?);
    }

    vm.microtasks = microtasks;
    vm.pending_host = pending_host;
    vm.unhandled_rejections = unhandled;
    vm.realm.symbol_registry = registry;
    vm.next_host_id = img.next_host_id;
    vm.symbol_counter = img.symbol_counter;
    vm.private_name_counter = img.private_name_counter;
    vm.rng_state = img.rng_state;
    vm.console_log = img.console_log.clone();
    Ok(())
}
