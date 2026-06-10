//! The engine-agnostic host-function seam.
//!
//! This is the Rust-side counterpart of the plan's `HostFn` seam. The 33
//! `unsafe extern "C"` QuickJS callbacks in the existing engine marshal
//! `serde_json::Value` and delegate to `host_core`; the host *logic* is already
//! engine-agnostic. Here we express the same boundary natively for the pure-Rust
//! engine: a host effect is a JS-visible async operation whose result is either
//! produced live (record mode) or replayed from the journal (replay mode).
//!
//! `HostDispatch` is what the replay runtime installs on the VM. When JS calls a
//! registered host function, the VM asks the dispatcher what to do:
//!   * `Replay(value)` — a journal entry exists; resolve synchronously with it.
//!   * `Suspend(id)`   — no entry; register a pending host op and block.
//!
//! Determinism contract (see the plan): every non-deterministic source — time,
//! randomness, all I/O, prompts, tools — must flow through a host effect so it is
//! captured in record mode and reproduced in replay mode. Object/Map/Set order is
//! deterministic by construction in `value.rs`.

use serde_json::Value as JsonValue;

/// What the host layer decides should happen for a given host call.
pub enum HostDecision {
    /// A journal entry already exists for this call: resolve immediately with
    /// the recorded (fulfilled) value.
    ReplayResolve(JsonValue),
    /// A journal entry exists and recorded a rejection.
    ReplayReject(String),
    /// No entry yet — this is the live frontier. The VM registers a pending host
    /// promise under this id and blocks on it.
    Suspend,
}

/// A host effect request: a named operation plus its JSON-marshalled arguments
/// and a deterministic key (call-site id + per-site invocation index).
pub struct HostRequest<'a> {
    pub name: &'a str,
    pub args: &'a JsonValue,
    /// Deterministic addressing key for the journal.
    pub key: HostKey,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HostKey {
    /// Stable identifier of the call site (operation name + a monotonic per-name
    /// counter is the default; an author may supply an explicit key).
    pub site: String,
    /// Per-site invocation index, so repeated calls to the same site are
    /// distinguishable across replay.
    pub seq: u64,
}

/// Installed on the VM by the replay runtime. Pure record/replay policy; the VM
/// owns the promise/suspension mechanics.
pub trait HostDispatch {
    /// Decide record-vs-replay for a host call and (in record mode) note that the
    /// op is now pending under `op_id`.
    fn dispatch(&mut self, req: &HostRequest, op_id: u64) -> HostDecision;

    /// Called when a host op is resolved (record mode) so the journal can append
    /// the result. `op_id` is the VM's pending id; `key` is the deterministic
    /// address recorded alongside the value.
    fn record_resolve(&mut self, op_id: u64, key: &HostKey, value: &JsonValue);

    /// Called when a host op is rejected (record mode).
    fn record_reject(&mut self, op_id: u64, key: &HostKey, error: &str);

    /// Allocate the deterministic key for the next call to `name`.
    fn next_key(&mut self, name: &str) -> HostKey;
}
