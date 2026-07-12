//! Object shapes (hidden classes): shared own-property KEY layout.
//!
//! See docs/js-object-shapes-design.md. A [`Shape`] is one node in a
//! transition tree: the root describes "no own properties (yet)" and each
//! child appends exactly one property key to its parent. Every plain object
//! born shaped holds an `Rc<Shape>` (its current node) plus a flat slot
//! vector; N same-shape objects share ONE key table (the chain) instead of
//! owning N `IndexMap`s and N copies of every key string.
//!
//! Invariants (docs §3.2):
//! - A shape encodes ONLY the insertion-ordered key list. Property
//!   attributes/kinds live in the per-object slots, so attribute mutation
//!   never forks the tree and never demotes; only key REMOVAL (which would
//!   shift slot indices) demotes to dictionary mode.
//! - Shapes derive purely from program behavior (keys in insertion order):
//!   no addresses, no RNG, nothing serialized — replay determinism is
//!   untouched. Shape identity (`Rc::ptr_eq`) is a pure cache-verification
//!   signal, exactly like the proto-identity inline caches.
//! - Transitions hold `Weak` children (the child holds its parent
//!   STRONGLY). A parent↔child `Rc` cycle would never be reclaimed by the
//!   reference-counting GC; with weak transition edges, a shape subtree dies
//!   with its last object (or literal-site/IC cache entry), and a later
//!   re-transition simply rebuilds the node.

use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::hash::BuildHasherDefault;
use std::rc::{Rc, Weak};

use crate::fxhash::{FxHasher, FxIndexMap};
use crate::value::PropertyKey;

type FxHashMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;

/// Chain length at and above which key lookup goes through the per-shape
/// [`Shape::index`] table. Below it, walking the parent chain is cheaper
/// than a hash for the 2–5 key objects that dominate real code.
const INDEX_THRESHOLD: usize = 8;

/// Appending an integer-index key once an object already has this many
/// properties demotes to dictionary mode (docs §3.4): the object is being
/// used as a sparse array / number-keyed map, where per-object dictionary
/// storage beats minting a fresh transition chain per object.
const INDEX_KEY_BOUND: usize = 8;

/// One node in the shape tree. Immutable once created (the interior
/// mutability is memoization only); shared via `Rc`.
pub struct Shape {
    /// Parent shape; `None` for the empty root.
    parent: Option<Rc<Shape>>,
    /// The property key this node appends to its parent. For the root this
    /// is an unused sentinel (never compared: lookups stop above the root).
    key: PropertyKey,
    /// Slot index of `key` in the owning object's slot vector (== depth-1).
    slot: u32,
    /// key → child shape for the next appended property. Lazily allocated
    /// (most shapes have 0 or 1 transition); values are weak — see the
    /// module docs.
    transitions: RefCell<FxHashMap<PropertyKey, Weak<Shape>>>,
    /// key → slot over the whole chain, built lazily once the chain is long
    /// enough that walking beats hashing no more (see [`INDEX_THRESHOLD`]).
    index: OnceCell<FxIndexMap<PropertyKey, u32>>,
}

impl std::fmt::Debug for Shape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.keys_in_order()).finish()
    }
}

impl Shape {
    /// The empty root of a transition tree (one per realm, held by `Realm`).
    pub fn new_root() -> Rc<Shape> {
        Rc::new(Shape {
            parent: None,
            key: PropertyKey::Str(crate::value::JsString::new("")),
            slot: 0,
            transitions: RefCell::new(FxHashMap::default()),
            index: OnceCell::new(),
        })
    }

    /// Number of properties an object of this shape has (== its slot count).
    #[inline]
    pub fn len(&self) -> usize {
        if self.parent.is_none() {
            0
        } else {
            self.slot as usize + 1
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.parent.is_none()
    }

    /// Slot of `key`, if present in this shape.
    #[inline]
    pub fn lookup<Q>(&self, key: &Q) -> Option<u32>
    where
        Q: ?Sized + std::hash::Hash + indexmap::Equivalent<PropertyKey>,
    {
        self.lookup_full(key).map(|(slot, _)| slot)
    }

    /// Slot and canonical (shape-owned) key for `key`, if present.
    pub fn lookup_full<Q>(&self, key: &Q) -> Option<(u32, &PropertyKey)>
    where
        Q: ?Sized + std::hash::Hash + indexmap::Equivalent<PropertyKey>,
    {
        if self.len() >= INDEX_THRESHOLD {
            let index = self.index.get_or_init(|| {
                let mut m = FxIndexMap::with_capacity_and_hasher(self.len(), Default::default());
                let mut cur = self;
                while cur.parent.is_some() {
                    m.insert(cur.key.clone(), cur.slot);
                    cur = cur.parent.as_deref().expect("checked");
                }
                m
            });
            return index.get_full(key).map(|(_, k, slot)| (*slot, k));
        }
        let mut cur = self;
        while cur.parent.is_some() {
            if key.equivalent(&cur.key) {
                return Some((cur.slot, &cur.key));
            }
            cur = cur.parent.as_deref().expect("checked");
        }
        None
    }

    /// The key stored at slot `i`, if in range. O(len - i) parent hops —
    /// callers indexing hot (the pre-Phase-3 IC verification) sit on 2–5 key
    /// objects where this is a couple of pointer chases.
    pub fn key_at(&self, i: u32) -> Option<&PropertyKey> {
        let len = self.len() as u32;
        if i >= len {
            return None;
        }
        let mut cur = self;
        for _ in 0..(len - 1 - i) {
            cur = cur.parent.as_deref().expect("depth checked");
        }
        Some(&cur.key)
    }

    /// All keys, insertion order (slot order).
    pub fn keys_in_order(&self) -> Vec<&PropertyKey> {
        let mut keys = Vec::with_capacity(self.len());
        let mut cur = self;
        while cur.parent.is_some() {
            keys.push(&cur.key);
            cur = cur.parent.as_deref().expect("checked");
        }
        keys.reverse();
        keys
    }

    /// The root of this shape's tree (for resetting an emptied object).
    pub fn root(self: &Rc<Self>) -> Rc<Shape> {
        let mut cur = self.clone();
        while let Some(p) = &cur.parent {
            let p = p.clone();
            cur = p;
        }
        cur
    }

    /// The child shape appending `key`, memoized in the transition table.
    pub fn transition(self: &Rc<Self>, key: PropertyKey) -> Rc<Shape> {
        let mut tr = self.transitions.borrow_mut();
        if let Some(w) = tr.get(&key) {
            if let Some(child) = w.upgrade() {
                return child;
            }
        }
        let child = Rc::new(Shape {
            parent: Some(self.clone()),
            key: key.clone(),
            slot: self.len() as u32,
            transitions: RefCell::new(FxHashMap::default()),
            index: OnceCell::new(),
        });
        tr.insert(key, Rc::downgrade(&child));
        child
    }

    /// `true` when this shape is exactly `parent` + `key` — the O(1) cursor
    /// check of the JSON record-shape cache (parent pointer + key equality,
    /// where interned keys hit the `Rc` fast path).
    #[inline]
    pub fn appends(&self, parent: &Rc<Shape>, key: &PropertyKey) -> bool {
        self.parent.as_ref().is_some_and(|p| Rc::ptr_eq(p, parent)) && self.key == *key
    }

    /// The chain from the first appended key down to this shape (root
    /// excluded), i.e. `path[i]` has `i + 1` properties.
    pub fn path_from_root(self: &Rc<Self>) -> Vec<Rc<Shape>> {
        let mut path = Vec::with_capacity(self.len());
        let mut cur = self.clone();
        while cur.parent.is_some() {
            path.push(cur.clone());
            let p = cur.parent.clone().expect("checked");
            cur = p;
        }
        path.reverse();
        path
    }

    /// Whether appending `key` to a shaped object with `len` properties
    /// keeps it shaped (docs §3.4: integer-index spam past a small bound is
    /// an object used as a sparse array — dictionary mode fits better).
    #[inline]
    pub fn can_append(key: &PropertyKey, len: usize) -> bool {
        len < INDEX_KEY_BOUND || key.array_index().is_none()
    }
}
