---
title: "Object Shapes Design"
---

# Object shapes (hidden classes): design + phased migration plan

> **Status:** IMPLEMENTED (2026-07-12) — Phases 0–3 landed on this branch;
> every phase gated on the full test262 language+built-ins baseline with
> zero regressions (47,291 tests). See §6.5 for the deltas between this design
> and what actually landed (slots hold whole `Property` values so attribute
> edges never demote; transition edges are `Weak`; the per-shape `index` is
> an `FxIndexMap` for `Equivalent`-probe parity).
>
> Original design note (2026-07-06): this answers the question
> [`js-performance-roadmap.md`](./js-performance-roadmap.md) §3.7 deferred —
> "revisit with fresh callgrind data; if `get_index_of` still dominates
> property-heavy code, shapes are the next step" — with fresh data, and it
> comes to a more nuanced conclusion: hashing is no longer the headline cost;
> **per-object construction is**. Shapes are still the right structural
> answer, but their value here is structure *sharing* (allocation + key
> interning + enumeration for free) more than lookup acceleration, and the
> phasing below is chosen accordingly.
>
> The three invariants are unchanged: **zero `unsafe`**, **no new
> heavyweight dependencies**, **byte-identical deterministic replay**.
> Shapes are derived purely from insertion order and property attributes,
> are never serialized, and never influence observable enumeration order —
> the determinism story is identical to the inline caches'.

---

## 1. Fresh data (callgrind, 2026-07-06, current branch head)

`json_roundtrip` (20k stringify+parse round-trips of a 5-key record with
nested objects/arrays), 1.20 G instructions total:

| cost center | Ir share | what it is |
| --- | ---: | --- |
| `malloc`/`free`/`memcpy` family | **~25%** | per-object `IndexMap` tables, per-key strings, `Vec`s, `Value` boxes |
| `IndexMap::get_index_of` + `insert_full` | ~8.4% | key hashing on parse-insert and stringify/get lookups |
| `json_stringify` + `json_quote_into` | ~7.7% | inherent serialization work |
| `JsonParser::parse_*` | ~6.2% | inherent parsing work |
| `Vm::get_from_object` + `get_prop` + `step` | ~11% | generic property walks + interpreter dispatch |
| `own_keys` + `enumerable_own_keys_excluding` | ~3.2% | stringify enumeration (clones every key per object per pass) |

Two conclusions:

1. **The kernel tier already ate the lookup problem where it matters.**
   Monomorphic property loops (`property_access`) run as register programs
   with entry-resolved slots (kernel prop localization); `get_index_of` no
   longer dominates any loop-shaped workload. What's left of it lives in
   *cold* structure — one lookup per key per object during parse/stringify,
   IC-verified interpreter accesses outside loops.
2. **Construction is the bottleneck shapes actually fix.** Every
   `JSON.parse` object allocates its own hash table and (until the Phase-0
   interning below) its own copies of key strings that every sibling record
   spells identically; every `{x, y}` literal in a loop does the same.
   Structure sharing turns "N same-shape objects" into "one shared shape +
   N slot vectors."

Phase 0 (landed with this doc): interning parse keys by source slice cut
~4% of `json_roundtrip` — confirming keys were a real but minor slice, and
that the per-object *table* churn is the dominant remainder.

## 2. Current model, and what 309 touch points mean

`ObjectData` today:

```rust
pub struct ObjectData {
    pub props: FxIndexMap<PropertyKey, Property>, // insertion-ordered
    pub internal: Internal,                       // exotic payloads
    pub proto: Option<JsObject>,
    pub extensible: bool,
}
```

`props` is public and directly manipulated at **309 sites** across the
engine (`.props` field accesses: gets, `get_full`/`get_index[_mut]`,
inserts, `shift_remove`, iteration, `contains_key`). The inline caches and
the kernel prop machinery both already exploit `IndexMap`'s stable
insertion-order slot indices — that is, the engine has organically grown
*half* a shape system: slot-indexed access with identity verified per use.
What's missing is the sharable identity (one pointer comparison instead of
key equality checks) and the shared storage (no per-object table).

A big-bang replacement of `props` is therefore not the plan. The plan is to
make the existing map *cheap to have* for shaped objects, by splitting the
key→slot index (shared, in the shape) from the slot values (per-object).

## 3. Design

### 3.1 Representation

```rust
/// One node in the shape tree. Immutable once created; shared via Rc.
pub struct Shape {
    /// Parent shape (None = the empty root for a given birth proto).
    parent: Option<Rc<Shape>>,
    /// The property this node appends to the parent, and its attributes.
    /// Attribute CHANGES (defineProperty) demote to dictionary mode rather
    /// than fork attribute-variant shape chains (rare; keeps the tree small).
    key: PropertyKey,
    enumerable: bool,
    // writable/configurable: shaped properties are always plain data
    // properties with default attributes; anything else demotes (§3.4).
    /// Slot index of `key` in the owning object's slot vector (== depth-1).
    slot: u32,
    /// key → child shape for the next appended property.
    /// Lazily allocated; most shapes have 0 or 1 transition.
    transitions: RefCell<FxHashMap<PropertyKey, Rc<Shape>>>,
    /// key → slot for O(1) lookup once the chain grows past a threshold;
    /// below it, lookup walks the parent chain (shorter than a hash for the
    /// 2-5 key objects that dominate real code). Built lazily per shape.
    index: OnceCell<FxHashMap<PropertyKey, u32>>,
}
```

Object storage becomes an enum behind accessors:

```rust
pub enum PropStorage {
    /// Plain data properties with default attributes, insertion-ordered:
    /// the shape holds the keys, `slots[i]` holds the value for the
    /// property at chain depth i.
    Shaped { shape: Rc<Shape>, slots: Vec<Value> },
    /// Everything else: accessors, non-default attributes, deleted keys,
    /// symbol-keyed exotics mid-mutation — today's map, verbatim.
    Dict(FxIndexMap<PropertyKey, Property>),
}
```

### 3.2 Invariants

- **Enumeration order** is insertion order in both modes (shape chain order
  == insertion order by construction; spec integer-key ordering is applied
  at enumeration time exactly as today). Mode is unobservable.
- **Replay determinism**: shapes are derived from program behavior only
  (keys, insertion order, attribute operations). No addresses, no RNG. The
  transition tree is per-`Vm` (rooted in `Realm`), so cross-realm sharing
  never happens and identity checks stay realm-local — same policy as the
  proto-identity ICs.
- **A shaped object's slot indices are stable for its lifetime in that
  shape** — append-only transitions; any destructive change demotes.

### 3.3 Fast paths enabled

- **Construction**: object literals compile to "reserve slots, walk the
  transition chain once per site" — after warm-up the chain walk is N
  pointer hops with no hashing and ONE allocation (the slot vec).
  `JSON.parse` gets the same via a per-parser cursor cache: record shapes
  repeat, so each record after the first is `Vec::with_capacity(n)` + n
  value writes.
- **ICs**: today's key-verified ICs upgrade to (shape ptr, slot) —
  verification is one `Rc::ptr_eq` instead of a key compare + map probe.
  Misses re-resolve exactly as now; no invalidation protocol is added
  (same "verify on every use" discipline).
- **Kernel prop localization**: entry resolution becomes a shape-chain walk
  (or `index` probe) instead of `get_full`; the per-activation invariants
  are unchanged. Slot vec replaces `get_index_mut` in the write-back.
- **`own_keys`/stringify**: the key list is the shape chain — no per-object
  key cloning for the common all-enumerable case.

### 3.4 Demotion (dictionary mode) triggers

`delete`, `defineProperty` with any non-default attribute or accessor,
`preventExtensions`/`seal`/`freeze` (reify then mark), index-keyed
properties on non-arrays past a small bound, and proto mutation do NOT
demote (shape is keyed by birth proto only for the root; proto lives on
`ObjectData` as today and IC/kernel guards already verify it). Demotion
materializes the map from the chain once; objects never re-promote.

### 3.5 What does NOT change

- `Internal` exotics (arrays' dense storage, typed arrays, proxies) — all
  already bypass `props` on their fast paths.
- The array kernel ops, dense-element machinery, prototype-chain guards.
- The journal/replay surface: nothing shape-related is recorded.

## 4. Phased migration (each phase gates test262 language+built-ins)

1. **Phase 0 — parse-key interning** *(landed with this doc)*: JSON object
   keys interned by source slice; ~4% on `json_roundtrip`, zero structural
   risk.
2. **Phase 1 — accessor extraction**: replace direct `.props` field access
   with a narrow `ObjectData` API (`own_get`, `own_get_full`, `own_insert`,
   `own_remove`, `own_iter`, `own_index/get_index_mut` for the IC/kernel
   paths). Pure mechanical refactor of the 309 sites, zero behavior change,
   landable in slices. This is the bulk of the diff and de-risks everything
   after it.
3. **Phase 2 — `PropStorage` behind the API**: introduce the enum with
   `Dict` as the only constructor (still zero behavior change), then flip
   plain-object birth to `Shaped` with demotion triggers per §3.4.
   Differential coverage: the kernels corpus + a new shapes-focused corpus
   (delete/defineProperty/freeze mid-loop, enumeration order, proxies).
4. **Phase 3 — consumers**: IC upgrade to (shape, slot); kernel prop entry
   resolution via shape; literal-site transition caching; `JSON.parse`
   record-shape cursor; stringify keys-from-shape.
5. **Phase 4 — measurement + demotion tuning**: re-profile json/closures/
   property-heavy workloads; tune the chain-walk vs `index` threshold and
   the literal-site cache.

Expected wins (from the profile shares): json_roundtrip construction+lookup
slices total ~35% of its instructions today; shapes address most of the
map/key allocation and hashing within it — a 1.3–1.6× on json is realistic,
plus across-the-board gains on object-literal-heavy code (closures
workload allocates a record per iteration) and colder property access.

## 5. Risks and mitigations

- **The 309-site refactor** is where mistakes hide. Mitigation: Phase 1 is
  semantically inert and mechanical; land it in reviewable slices, each
  gated on the full test262 baseline (the gate caught two real bugs during
  the kernel work on this branch; it is the effective spec oracle).
- **Engines grow their worst bugs in transitions** (`delete`, redefinition,
  proto swaps). Mitigation: demote on every destructive edge — the
  dictionary path IS today's battle-tested code; shapes only ever serve the
  append-only common case.
- **Memory**: transition trees can retain dead shapes. Per-`Vm` rooting +
  the existing cycle collector's registry covers reclamation at quiescence;
  shapes hold no `Value`s, only keys.
- **`u128` liveness / slot bounds**: slot vecs cap at the same bound as
  dictionary properties today (no new limit).

## 6.5 Implementation deltas (what actually landed, 2026-07-12)

The landed implementation follows §3–§4 with four refinements, each chosen
to shrink the correctness surface rather than to chase speed:

1. **Slots hold whole `Property` values** (`Shaped { shape, slots:
   Vec<Property> }`), not bare `Value`s, and the shape does NOT carry an
   `enumerable` bit. The shape therefore encodes exactly one thing — the
   insertion-ordered key list — and every attribute/kind mutation
   (`defineProperty` with any attributes, accessors, `seal`/`freeze`) works
   identically in both modes with NO demotion. The §3.4 demotion set
   shrinks to the order-destroying edges only: `delete` of a present key,
   and integer-index keys past a small bound (8). This also let the
   Phase-1 accessor API keep `IndexMap`-shaped signatures
   (`&Property`/`&mut Property`, 3-tuple `get_full`), which is what made
   the ~340-site refactor mechanical. The cost is `size_of::<Property>()`
   per slot instead of `size_of::<Value>()` — the allocation-count win
   (one slot vec vs. a per-object `IndexMap` + key strings) is unchanged.
2. **Transition edges are `Weak`** (`transitions:
   RefCell<FxHashMap<PropertyKey, Weak<Shape>>>`); the child holds its
   parent strongly. The §3.1 sketch's strong child edges would form
   parent↔child `Rc` cycles that the reference-counting GC can never
   reclaim. With weak edges a shape subtree dies with its last object /
   cache entry, and a later transition simply rebuilds the node — no
   registry, no §5 quiescence sweep needed.
3. **The per-shape `index` is an `FxIndexMap<PropertyKey, u32>`** (not a
   plain `FxHashMap`) so it accepts the same `Equivalent<PropertyKey>`
   probes (`StrKeyRef`) as the dictionary path.
4. **ICs hold `Rc<Shape>` strongly** in the new `IcEntry::{own_shape,
   proto_shape}` fields — one small key chain pinned per monomorphic site,
   in exchange for upgrade-free verification. The JSON.parse cursor is a
   per-nesting-depth path (`Vec<Rc<Shape>>` per depth) so nested records
   don't clobber their parent's cursor mid-object.

**Measured (callgrind instruction counts, examples/shapebench, vs the
fork point efb16a0):** `object_literals` −19%, `mixed_helpers` −5.8%,
`for-in` over per-iteration records −3.8%, `JSON.parse` −1.9%,
`json_roundtrip` −0.8%, `JSON.stringify` +1.7% — of which ~+0.5M
instructions in every workload is one-time realm setup (intrinsic
container objects now mint shape chains; ≈0.15 ms per `Engine::new`, and
two rounds of tuning already halved it twice: an inline single-entry
transition slot, and two-touch arming of the per-shape key index so
grow-by-insert singletons never pay O(n²) index builds). The §4 1.3–1.6×
json estimate did NOT materialize on this corpus: with Phase-0 interning
and the pre-reserved parse maps already landed, per-record construction
was a smaller slice of `json_roundtrip` than the 2026-07-06 table
suggested, and stringify/parse inherent work dominates. The structural
wins concentrate where construction actually dominates (object literals
in loops, record-processing helpers).

Phase-4 items still open: stringify-from-shape (serializing slots
directly for all-plain-data records — deliberately skipped because the
spec's per-key re-lookup during serialization is observable under
mutating `toJSON`), and shrinking the residual realm-setup delta if
`Engine::new` latency ever matters.

## 6. Relationship to register bytecode (§3.5 of the roadmap)

Independent. Register bytecode removes interpreter dispatch/operand-stack
costs; shapes remove per-object storage costs. The kernel tier's typed-slot
experience (entry-resolved indices, verify-per-use, decline-to-generic)
is the proven pattern both reuse. Shapes first: they also make the
register-bytecode ICs cheaper when that lands.
