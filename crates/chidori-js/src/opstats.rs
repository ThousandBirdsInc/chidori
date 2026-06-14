//! Phase-0 dynamic opcode-frequency instrumentation (feature `op-histogram`).
//!
//! This module exists **only** when the `op-histogram` Cargo feature is enabled.
//! In the default build it is not compiled in at all, and the single
//! `opstats::record(..)` call site in `exec.rs::run_frame` is `#[cfg]`-removed —
//! so the shipping interpreter loop is byte-identical and pays nothing. See the
//! Phase-0 plan in [`docs/interpreter-optimization.md`].
//!
//! When enabled, the interpreter records, in a process-global thread-local
//! histogram, every *executed* opcode and every adjacent opcode *pair* (the
//! current op together with the immediately-preceding one in execution order).
//! Pair counts are what drive superinstruction / op-fusion candidate selection
//! in Phase 2: a frequently-executed `(Prev, Cur)` pair is a fusion candidate.
//!
//! Usage (from the `opstats` example):
//! ```ignore
//! opstats::reset();
//! engine.eval(src);                 // execution feeds the histogram
//! let report = opstats::take();      // drain + read the counts
//! ```
//!
//! This is pure instrumentation: it never influences a value, an error, an
//! ordering, or the journal, and it is excluded from the default build, so it
//! cannot affect deterministic replay.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::bytecode::Op;

thread_local! {
    static HIST: RefCell<Hist> = RefCell::new(Hist::default());
}

#[derive(Default)]
struct Hist {
    total: u64,
    ops: HashMap<&'static str, u64>,
    pairs: HashMap<(&'static str, &'static str), u64>,
    prev: Option<&'static str>,
}

/// A drained snapshot of the histogram, sorted for reporting.
pub struct Report {
    /// Total opcodes executed.
    pub total: u64,
    /// `(opcode, count)`, descending by count.
    pub ops: Vec<(&'static str, u64)>,
    /// `((prev, cur), count)`, descending by count.
    pub pairs: Vec<((&'static str, &'static str), u64)>,
}

/// Record one executed opcode. Called once per dispatched instruction from the
/// interpreter loop when the `op-histogram` feature is on.
#[inline]
pub fn record(op: &Op) {
    let name = variant_name(op);
    HIST.with(|h| {
        let mut h = h.borrow_mut();
        h.total += 1;
        *h.ops.entry(name).or_insert(0) += 1;
        if let Some(prev) = h.prev {
            *h.pairs.entry((prev, name)).or_insert(0) += 1;
        }
        h.prev = Some(name);
    });
}

/// Clear the histogram (call before a workload you want to measure in isolation).
pub fn reset() {
    HIST.with(|h| *h.borrow_mut() = Hist::default());
}

/// Drain the histogram into a sorted [`Report`] and clear it.
pub fn take() -> Report {
    HIST.with(|h| {
        let h = std::mem::take(&mut *h.borrow_mut());
        let mut ops: Vec<_> = h.ops.into_iter().collect();
        ops.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let mut pairs: Vec<_> = h.pairs.into_iter().collect();
        pairs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        Report {
            total: h.total,
            ops,
            pairs,
        }
    })
}

/// The `Op` variant name without its payload, interned to `&'static str`.
///
/// `Op` is `#[derive(Debug)]`, so `{:?}` yields `Variant`, `Variant(..)`, or
/// `Variant { .. }`; we keep the leading identifier. The result is interned so
/// the histogram can key on `&'static str` (cheap to hash/copy) rather than
/// allocating a `String` per executed opcode.
fn variant_name(op: &Op) -> &'static str {
    let dbg = format!("{op:?}");
    let end = dbg
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(dbg.len());
    intern(&dbg[..end])
}

fn intern(s: &str) -> &'static str {
    thread_local! {
        static POOL: RefCell<HashMap<String, &'static str>> = RefCell::new(HashMap::new());
    }
    POOL.with(|p| {
        if let Some(&v) = p.borrow().get(s) {
            return v;
        }
        let leaked: &'static str = Box::leak(s.to_string().into_boxed_str());
        p.borrow_mut().insert(s.to_string(), leaked);
        leaked
    })
}
