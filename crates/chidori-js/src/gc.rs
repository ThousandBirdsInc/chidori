//! Cycle collection for the reference-counted object heap.
//!
//! The engine's objects are `Rc<RefCell<ObjectData>>`; plain reference
//! counting cannot reclaim cycles, and JavaScript creates them constantly:
//! constructor ↔ prototype, closure → upvalue cell → closure, promise →
//! reaction → suspended frame → promise, object → proto → method → scope.
//! Historically the only mitigation was [`Vm::dispose`]'s end-of-life graph
//! walk, which (a) only helps once the VM is finished and (b) only reaches
//! objects still connected to the realm roots — orphaned cycles leaked, which
//! is why long conformance runs had to be chunked across processes.
//!
//! This module adds the standard refcount-accounting collector (the same
//! family as CPython's `gc`):
//!
//! 1. every allocation is registered as a `Weak` in `Vm::all_objects`
//!    (see [`Vm::alloc`]); the registry never extends lifetimes;
//! 2. [`Vm::collect_cycles`] snapshots the live registered objects, sets
//!    `gc_refs(o) = strong_count(o)`, then subtracts one for every reference
//!    *traced between registered objects* (props, prototype, and every
//!    internal-slot edge — arrays, maps, closures' upvalue cells, generator
//!    frames, promise reactions, proxies, typed-array buffers, …);
//! 3. an object left with `gc_refs > 0` has a reference we cannot see — a
//!    host-held handle, a value on a native frame, a capture inside a native
//!    closure — and is therefore treated as a ROOT. Everything reachable from
//!    roots (through the same traced edges) is kept;
//! 4. what remains is unreachable garbage cycles: each such object's outgoing
//!    edges (props / proto / internal) are cleared, which collapses the cycle
//!    and lets plain `Rc` reclamation free the memory.
//!
//! Soundness leans on one invariant: **an edge kind is either subtracted in
//! step 2 AND traversed in step 3, or neither.** Untraceable references
//! (native-closure captures, host handles) are never subtracted, so their
//! targets always look externally referenced and survive. The one genuinely
//! shared interior type — a `Rc<RefCell<Value>>` binding cell, which holds a
//! single strong ref to its inner object no matter how many closures share
//! the cell — is deduplicated by pointer so its inner edge is counted exactly
//! once. Cells the HOST also holds (module export cells) must be registered
//! in `Vm::gc_cell_roots`; their contents are then rooted unconditionally.
//!
//! `collect_cycles` only runs at quiescence (no JS frames on the Rust stack,
//! empty microtask queue): queued `Microtask::Job` closures capture objects
//! invisibly, and an executing frame lives on the native stack where we
//! cannot see its operand stack.

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::rc::Rc;

use crate::value::{FunctionInner, Internal, JsObject, ObjectData, Property, PropertyKind, Value};
use crate::vm::{Completion, Frame, GeneratorState, PromiseState, Reaction, Vm};

impl Vm {
    /// Allocate (and register) an object owned by this VM. All engine
    /// allocation paths funnel here so `collect_cycles`/`dispose` see every
    /// object; an unregistered object is never *unsafe*, it just can't be
    /// cycle-collected.
    pub fn alloc(&self, data: ObjectData) -> JsObject {
        let o = JsObject::new(data);
        self.track_object(&o);
        o
    }

    /// Allocate (and register) a plain object with the given prototype.
    pub fn alloc_ordinary(&self, proto: Option<JsObject>) -> JsObject {
        self.alloc(ObjectData::new(proto, Internal::Ordinary))
    }

    /// Register an externally-created object with this VM's collector.
    pub fn track_object(&self, o: &JsObject) {
        let mut reg = self.all_objects.borrow_mut();
        reg.push(Rc::downgrade(&o.0));
        // Amortized compaction: prune dead weak entries when the registry
        // doubles past the last high-water mark.
        if reg.len() >= self.gc_compact_at.get() {
            reg.retain(|w| w.strong_count() > 0);
            self.gc_compact_at.set((reg.len() * 2).max(1 << 12));
        }
    }

    /// Number of live objects currently registered (test/diagnostic aid).
    pub fn gc_tracked_live(&self) -> usize {
        self.all_objects
            .borrow()
            .iter()
            .filter(|w| w.strong_count() > 0)
            .count()
    }

    /// Collect unreachable reference cycles. Returns the number of objects
    /// whose edges were cleared (0 when not at quiescence — an executing
    /// frame or a queued job makes collection unsound, so we refuse).
    pub fn collect_cycles(&mut self) -> usize {
        if self.call_depth > 0 || !self.microtasks.is_empty() {
            return 0;
        }
        // Snapshot the live registered objects. Each snapshot handle adds one
        // strong count, which the accounting below subtracts back out.
        let live: Vec<JsObject> = {
            let mut reg = self.all_objects.borrow_mut();
            reg.retain(|w| w.strong_count() > 0);
            reg.iter()
                .filter_map(|w| w.upgrade().map(JsObject))
                .collect()
        };
        let index: HashMap<usize, usize> = live
            .iter()
            .enumerate()
            .map(|(i, o)| (o.ptr_id(), i))
            .collect();

        // Pass 1+2: gc_refs = strong_count - 1 (our snapshot handle), minus
        // one per traced edge between registered objects.
        let mut gc_refs: Vec<isize> = live
            .iter()
            .map(|o| Rc::strong_count(&o.0) as isize - 1)
            .collect();
        let mut seen_cells: HashSet<usize> = HashSet::new();
        let mut seen_frames: HashSet<usize> = HashSet::new();
        // Host-shared cells: pre-seeding marks them "already counted", so the
        // inner edge is never subtracted and the contents stay rooted.
        let mut root_values: Vec<Value> = Vec::new();
        for cell in &self.gc_cell_roots {
            seen_cells.insert(Rc::as_ptr(cell) as usize);
            root_values.push(cell.borrow().clone());
        }
        if let Some(cells) = &self.module_capture {
            for cell in cells {
                seen_cells.insert(Rc::as_ptr(cell) as usize);
                root_values.push(cell.borrow().clone());
            }
        }
        for o in &live {
            trace_object(
                &o.borrow(),
                &mut seen_cells,
                &mut seen_frames,
                &mut |t: &JsObject| {
                    if let Some(&i) = index.get(&t.ptr_id()) {
                        gc_refs[i] -= 1;
                    }
                },
            );
        }

        // Pass 3: mark everything reachable from the roots. Roots are (a)
        // objects with unexplained strong counts — host handles, realm
        // intrinsic fields, values on native frames, native-closure captures —
        // and (b) the contents of host-registered cells. Traversal goes
        // THROUGH untracked objects too (reachability must not stop at an
        // object that happens not to be registered), with a pointer-keyed
        // visited set and fresh cell/frame memoization (a memoized cell's
        // contents are marked on its first visit, so sharing is safe here).
        let mut visited: HashSet<usize> = HashSet::new();
        let mut work: Vec<JsObject> = Vec::new();
        for (i, o) in live.iter().enumerate() {
            if gc_refs[i] > 0 {
                work.push(o.clone());
            }
        }
        for v in &root_values {
            if let Value::Object(o) = v {
                work.push(o.clone());
            }
        }
        let mut mark_cells: HashSet<usize> = HashSet::new();
        let mut mark_frames: HashSet<usize> = HashSet::new();
        while let Some(o) = work.pop() {
            if !visited.insert(o.ptr_id()) {
                continue;
            }
            let mut found: Vec<JsObject> = Vec::new();
            trace_object(
                &o.borrow(),
                &mut mark_cells,
                &mut mark_frames,
                &mut |t: &JsObject| found.push(t.clone()),
            );
            work.extend(found);
        }
        let marked: Vec<bool> = live.iter().map(|o| visited.contains(&o.ptr_id())).collect();

        // Pass 4: sweep — clear every unmarked object's outgoing edges so the
        // cycle collapses and Rc reclamation frees the subgraph.
        let mut swept = 0usize;
        for (i, o) in live.iter().enumerate() {
            if !marked[i] {
                clear_object_edges(o);
                swept += 1;
            }
        }
        if swept > 0 {
            let mut reg = self.all_objects.borrow_mut();
            reg.retain(|w| w.strong_count() > 0);
            self.gc_compact_at.set((reg.len() * 2).max(1 << 12));
        }
        swept
    }
}

/// Drop every outgoing edge of `o` (props, prototype, internal slots). The
/// object stays allocated until its own strong count reaches zero, but it can
/// no longer keep anything else alive.
pub(crate) fn clear_object_edges(o: &JsObject) {
    let mut b = o.borrow_mut();
    b.props.clear();
    b.proto = None;
    b.internal = Internal::Ordinary;
    b.privates = None;
}

/// Enumerate every traced strong `JsObject` reference held by `data`, exactly
/// once per reference. `seen_cells`/`seen_frames` deduplicate the two interior
/// `Rc` containers that can be SHARED between objects (binding cells and the
/// async-resume frame slot), whose inner references must be counted once
/// globally, not once per holder.
fn trace_object(
    data: &ObjectData,
    seen_cells: &mut HashSet<usize>,
    seen_frames: &mut HashSet<usize>,
    f: &mut dyn FnMut(&JsObject),
) {
    if let Some(p) = &data.proto {
        f(p);
    }
    for (_k, prop) in &data.props {
        trace_property(prop, f);
    }
    if let Some(privs) = &data.privates {
        for el in privs.values() {
            match el {
                crate::value::PrivateElement::Field(v)
                | crate::value::PrivateElement::Method(v) => trace_value(v, f),
                crate::value::PrivateElement::Accessor { get, set } => {
                    if let Some(g) = get {
                        trace_value(g, f);
                    }
                    if let Some(s) = set {
                        trace_value(s, f);
                    }
                }
            }
        }
    }
    match &data.internal {
        Internal::Array(v) => {
            for x in v {
                trace_value(x, f);
            }
        }
        Internal::Map(m) | Internal::WeakMap(m) => {
            for (k, v) in m {
                trace_value(&k.0, f);
                trace_value(v, f);
            }
        }
        Internal::Set(s) | Internal::WeakSet(s) => {
            for (k, _) in s {
                trace_value(&k.0, f);
            }
        }
        Internal::TypedArray(t) => f(&t.buffer),
        Internal::DataView(d) => f(&d.buffer),
        Internal::Proxy(p) => {
            f(&p.target);
            f(&p.handler);
        }
        Internal::Iterator(it) => {
            if let Some(t) = &it.target {
                f(t);
            }
        }
        Internal::ModuleNamespace(ns) => {
            for cell in ns.exports.values() {
                trace_cell(cell, seen_cells, f);
            }
        }
        Internal::Function(func) => match func {
            FunctionInner::Bytecode(bf) => {
                for cell in &bf.upvalues {
                    trace_cell(cell, seen_cells, f);
                }
                if let Some(h) = &bf.home_object {
                    f(h);
                }
                for o in &bf.captured_with {
                    f(o);
                }
            }
            FunctionInner::Bound(bound) => {
                f(&bound.target);
                trace_value(&bound.bound_this, f);
                for a in &bound.bound_args {
                    trace_value(a, f);
                }
            }
            // Native closures may capture objects we cannot see; those refs
            // are deliberately NOT subtracted, so their targets stay rooted.
            FunctionInner::Native(_) => {}
        },
        Internal::Promise(p) => {
            match &p.state {
                PromiseState::Fulfilled(v) | PromiseState::Rejected(v) => trace_value(v, f),
                PromiseState::Pending => {}
            }
            for r in p.fulfill_reactions.iter().chain(p.reject_reactions.iter()) {
                trace_reaction(r, seen_cells, seen_frames, f);
            }
        }
        Internal::Generator(g) => {
            match &g.state {
                GeneratorState::SuspendedStart(fr) | GeneratorState::SuspendedYield(fr) => {
                    trace_frame(fr, seen_cells, f);
                }
                GeneratorState::Executing | GeneratorState::Completed => {}
            }
            for req in &g.queue {
                trace_value(&req.value, f);
                f(&req.result);
            }
        }
        Internal::Arguments(map) => {
            for cell in map.iter().flatten() {
                trace_value(&cell.borrow(), f);
            }
        }
        Internal::Ordinary
        | Internal::Error
        | Internal::Boolean(_)
        | Internal::Number(_)
        | Internal::StringObj(_)
        | Internal::Symbol(_)
        | Internal::Date(_)
        | Internal::ArrayBuffer(_)
        | Internal::BigIntObj(_) => {}
    }
}

fn trace_property(prop: &Property, f: &mut dyn FnMut(&JsObject)) {
    match &prop.kind {
        PropertyKind::Data { value, .. } => trace_value(value, f),
        PropertyKind::Accessor { get, set } => {
            if let Some(g) = get {
                trace_value(g, f);
            }
            if let Some(s) = set {
                trace_value(s, f);
            }
        }
    }
}

fn trace_value(v: &Value, f: &mut dyn FnMut(&JsObject)) {
    if let Value::Object(o) = v {
        f(o);
    }
}

/// A binding cell holds ONE strong ref to its inner object regardless of how
/// many closures/frames share the cell — count it on first visit only.
fn trace_cell(
    cell: &Rc<RefCell<Value>>,
    seen_cells: &mut HashSet<usize>,
    f: &mut dyn FnMut(&JsObject),
) {
    if seen_cells.insert(Rc::as_ptr(cell) as usize) {
        trace_value(&cell.borrow(), f);
    }
}

fn trace_reaction(
    r: &Reaction,
    seen_cells: &mut HashSet<usize>,
    seen_frames: &mut HashSet<usize>,
    f: &mut dyn FnMut(&JsObject),
) {
    match r {
        Reaction::Then {
            handler,
            result_capability,
            ..
        } => {
            if let Some(h) = handler {
                trace_value(h, f);
            }
            f(result_capability);
        }
        Reaction::AsyncResume {
            frame, own_promise, ..
        } => {
            // The frame slot is shared between the fulfill and reject
            // reactions of the same await — trace its contents once.
            if seen_frames.insert(Rc::as_ptr(frame) as usize) {
                if let Some(fr) = frame.borrow().as_ref() {
                    trace_frame(fr, seen_cells, f);
                }
            }
            f(own_promise);
        }
    }
}

/// Every strong object reference a suspended frame holds: closure state,
/// operand stack, locals/args, binding cells, `this`/`new.target`, parked
/// completions, and the active `with` chain.
fn trace_frame(fr: &Frame, seen_cells: &mut HashSet<usize>, f: &mut dyn FnMut(&JsObject)) {
    let bf = &fr.func;
    for cell in &bf.upvalues {
        trace_cell(cell, seen_cells, f);
    }
    if let Some(h) = &bf.home_object {
        f(h);
    }
    for o in &bf.captured_with {
        f(o);
    }
    for v in fr
        .stack
        .iter()
        .chain(fr.locals.iter())
        .chain(fr.args.iter())
    {
        trace_value(v, f);
    }
    for cell in &fr.cells {
        trace_cell(cell, seen_cells, f);
    }
    trace_value(&fr.this, f);
    trace_value(&fr.new_target, f);
    trace_value(&fr.completion, f);
    if let Some(c) = &fr.pending_completion {
        match c {
            Completion::Return(v) | Completion::Throw(v) => trace_value(v, f),
            Completion::Jump { .. } => {}
        }
    }
    if let Some(v) = &fr.pending_throw {
        trace_value(v, f);
    }
    if let Some(v) = &fr.pending_return {
        trace_value(v, f);
    }
    for o in &fr.with_scope {
        f(o);
    }
    if let Some(o) = &fr.eval_vars {
        f(o);
    }
}
