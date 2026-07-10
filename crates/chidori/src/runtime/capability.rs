#![allow(dead_code)]
//! Capability flags raised when agent code touches a captured-effect surface.
//!
//! The captured-effects model (`docs/captured-effects-vfs-crypto-timers.md`)
//! provides filesystem, crypto, and timer surfaces that the runtime previously
//! rejected outright. Providing them is safe because every nondeterministic
//! operation is captured into the call log and replayed, but operators still
//! need *visibility* into which surfaces an agent reached. A `Capability` flag
//! is raised on first touch of a surface — regardless of whether the
//! individual call was deterministic (an inline hash) or captured (random
//! bytes) — and the accumulated set is surfaced on the snapshot manifest and as
//! OTEL span attributes.
//!
//! Flags are advisory-for-visibility and monotonic: raising one never blocks
//! execution. Blocking, when desired, is a separate policy decision layered on
//! top (reusing the existing approval-pause path).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A surface an agent touched that the runtime captures and flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Read from the virtual filesystem.
    FsRead,
    /// Wrote to the virtual filesystem.
    FsWrite,
    /// Deleted from the virtual filesystem.
    FsDelete,
    /// Computed a hash/HMAC/cipher over caller-supplied input (deterministic).
    CryptoHash,
    /// Drew random bytes / UUID / filled a typed array (captured).
    CryptoRandom,
    /// Generated a key (captured).
    CryptoKeygen,
    /// Scheduled a macrotask via `setTimeout`/`setInterval`/`setImmediate`.
    Timer,
    /// Scheduled a microtask via `queueMicrotask`.
    Microtask,
}

impl Capability {
    /// Stable string identifier used for OTEL span attribute keys, e.g.
    /// `chidori.capability.crypto_random`.
    pub fn as_str(self) -> &'static str {
        match self {
            Capability::FsRead => "fs_read",
            Capability::FsWrite => "fs_write",
            Capability::FsDelete => "fs_delete",
            Capability::CryptoHash => "crypto_hash",
            Capability::CryptoRandom => "crypto_random",
            Capability::CryptoKeygen => "crypto_keygen",
            Capability::Timer => "timer",
            Capability::Microtask => "microtask",
        }
    }

    /// Parse the JS-side flag string emitted by the prelude host shims back
    /// into a `Capability`. Returns `None` for unknown strings so a forward-
    /// compatible prelude can't crash an older runtime. (Named `parse`, not
    /// `from_str`, because the fallible-by-`Option` shape doesn't fit the
    /// `FromStr` trait contract.)
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "fs_read" => Some(Capability::FsRead),
            "fs_write" => Some(Capability::FsWrite),
            "fs_delete" => Some(Capability::FsDelete),
            "crypto_hash" => Some(Capability::CryptoHash),
            "crypto_random" => Some(Capability::CryptoRandom),
            "crypto_keygen" => Some(Capability::CryptoKeygen),
            "timer" => Some(Capability::Timer),
            "microtask" => Some(Capability::Microtask),
            _ => None,
        }
    }
}

/// Accumulates the set of capabilities an agent has touched, recording the
/// sequence number at which each was first seen. `BTreeMap` keeps both
/// serialization and iteration deterministic so the same run always produces
/// the same ledger — which lets replay assert the recomputed set matches the
/// stored manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityLedger {
    /// Capability -> sequence number of first touch.
    first_seen: BTreeMap<Capability, u64>,
}

impl CapabilityLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `cap` was touched at sequence `seq`. The first touch wins;
    /// later touches don't move the recorded `seq`. Returns `true` if this was
    /// the first time the capability was seen.
    pub fn note(&mut self, cap: Capability, seq: u64) -> bool {
        if self.first_seen.contains_key(&cap) {
            return false;
        }
        self.first_seen.insert(cap, seq);
        true
    }

    pub fn is_empty(&self) -> bool {
        self.first_seen.is_empty()
    }

    pub fn contains(&self, cap: Capability) -> bool {
        self.first_seen.contains_key(&cap)
    }

    /// The capabilities touched, in stable order.
    pub fn capabilities(&self) -> impl Iterator<Item = Capability> + '_ {
        self.first_seen.keys().copied()
    }

    /// First-touch sequence for `cap`, if any.
    pub fn first_seen(&self, cap: Capability) -> Option<u64> {
        self.first_seen.get(&cap).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_records_first_seen_only() {
        let mut ledger = CapabilityLedger::new();
        assert!(ledger.note(Capability::FsRead, 3));
        assert!(!ledger.note(Capability::FsRead, 9));
        assert_eq!(ledger.first_seen(Capability::FsRead), Some(3));
    }

    #[test]
    fn string_round_trips() {
        for cap in [
            Capability::FsRead,
            Capability::FsWrite,
            Capability::FsDelete,
            Capability::CryptoHash,
            Capability::CryptoRandom,
            Capability::CryptoKeygen,
            Capability::Timer,
            Capability::Microtask,
        ] {
            assert_eq!(Capability::parse(cap.as_str()), Some(cap));
        }
        assert_eq!(Capability::parse("nope"), None);
    }

    #[test]
    fn ledger_iteration_is_stable() {
        let mut ledger = CapabilityLedger::new();
        ledger.note(Capability::Timer, 5);
        ledger.note(Capability::FsRead, 1);
        ledger.note(Capability::CryptoRandom, 3);
        let order: Vec<_> = ledger.capabilities().collect();
        // BTreeMap orders by the enum's declared (derived Ord) order.
        assert_eq!(
            order,
            vec![
                Capability::FsRead,
                Capability::CryptoRandom,
                Capability::Timer
            ]
        );
    }
}
