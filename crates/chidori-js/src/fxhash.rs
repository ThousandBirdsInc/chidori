//! A fast, deterministic, non-cryptographic hasher for the engine's internal
//! hash tables (property maps, Map/Set backing stores).
//!
//! This is the Fx hash function (rustc's `FxHasher` algorithm — a rotate/xor/
//! multiply over 8-byte chunks), reimplemented in-tree so the engine adds no
//! dependency. It replaces `IndexMap`'s default `RandomState`/SipHash, which
//! the callgrind profiles showed costing ~49% of *all* executed instructions
//! on property-heavy code (docs/interpreter-optimization.md §15).
//!
//! Determinism: hashes are used only for bucket placement inside `IndexMap`;
//! iteration order is insertion order and lookup results are decided by `Eq`,
//! so the hash function is unobservable to JS and to the replay journal. This
//! hasher is nonetheless fully deterministic (no per-process random seed,
//! little-endian chunk reads on every platform) so bucket layout — and thus
//! allocation/probe patterns — are identical across runs by construction.
//!
//! Security trade (deliberate): SipHash's random seed defends a hash table
//! whose keys an adversary controls against collision flooding (HashDoS).
//! Property keys here come from the agent program itself, and the engine
//! already bounds runaway execution with the uncatchable op budget, so the
//! flooding attack buys an attacker nothing a plain `while(1)` loop doesn't.
//! Every production JS engine (V8, JSC, SpiderMonkey) makes the same call.

use std::hash::{BuildHasherDefault, Hasher};

use indexmap::IndexMap;

/// `IndexMap` keyed with [`FxHasher`] — drop-in for the engine's hot maps.
pub type FxIndexMap<K, V> = IndexMap<K, V, BuildHasherDefault<FxHasher>>;

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

/// The rustc `FxHasher`: one rotate/xor/multiply per 8-byte chunk.
#[derive(Default)]
pub struct FxHasher {
    hash: u64,
}

impl FxHasher {
    #[inline]
    fn add_to_hash(&mut self, i: u64) {
        self.hash = (self.hash.rotate_left(5) ^ i).wrapping_mul(SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            // unwrap: chunks_exact(8) yields exactly 8 bytes.
            self.add_to_hash(u64::from_le_bytes(c.try_into().unwrap()));
        }
        let rem = chunks.remainder();
        if !rem.is_empty() {
            let mut buf = [0u8; 8];
            buf[..rem.len()].copy_from_slice(rem);
            // Fold the remainder length in so "ab" and "ab\0" differ.
            self.add_to_hash(u64::from_le_bytes(buf) ^ (rem.len() as u64));
        }
    }
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add_to_hash(i as u64);
    }
    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.add_to_hash(i as u64);
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add_to_hash(i as u64);
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add_to_hash(i);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add_to_hash(i as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::BuildHasher;

    fn hash_of(bytes: &[u8]) -> u64 {
        let mut h = FxHasher::default();
        h.write(bytes);
        h.finish()
    }

    #[test]
    fn deterministic_across_builders() {
        // No random state: two independent builders agree (unlike RandomState).
        let b1: BuildHasherDefault<FxHasher> = Default::default();
        let b2: BuildHasherDefault<FxHasher> = Default::default();
        assert_eq!(b1.hash_one("prototype"), b2.hash_one("prototype"));
    }

    #[test]
    fn distinguishes_padding() {
        assert_ne!(hash_of(b"ab"), hash_of(b"ab\0"));
        assert_ne!(hash_of(b""), hash_of(b"\0"));
    }

    #[test]
    fn fx_index_map_basics() {
        let mut m: FxIndexMap<&str, i32> = FxIndexMap::default();
        m.insert("a", 1);
        m.insert("b", 2);
        assert_eq!(m.get("a"), Some(&1));
        // Insertion order is preserved regardless of hasher.
        assert_eq!(m.keys().copied().collect::<Vec<_>>(), vec!["a", "b"]);
    }
}
