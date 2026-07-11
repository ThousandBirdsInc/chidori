//! Core value and object model for the chidori-js engine.
//!
//! GC strategy (initial, per the plan): reference counting via `Rc<RefCell<_>>`.
//! Cycles leak within a single execution; that is acceptable for run-to-suspend
//! agent programs and is documented as the deferred GC decision. Determinism is
//! preserved because there are no program-observable finalizers (`WeakRef`/
//! `FinalizationRegistry` are denied for durable agents).
//!
//! Iteration order is deterministic and address-independent by construction:
//! ordinary property maps are insertion-ordered (`IndexMap`), and own-key
//! enumeration applies the spec ordering (integer indices ascending, then string
//! keys in insertion order, then symbols).

use indexmap::IndexMap;
use num_bigint::BigInt;
use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use crate::bytecode::FuncProto;

/// A JS string: a sequence of UTF-16 code units (the spec's string model),
/// stored as WTF-8 bytes so lone surrogates and astral `.length`/indexing are
/// representable. The overwhelmingly common case — a string with no unpaired
/// surrogate — takes the `Utf8` arm, which is byte-identical to the old
/// `Rc<str>` model: cheap clone, `as_str()` borrows directly, zero overhead.
/// Only strings that actually contain an unpaired surrogate pay for the `Wtf8`
/// arm. See [`crate::wtf8`].
#[derive(Clone)]
pub struct JsString(Repr);

#[derive(Clone)]
enum Repr {
    /// No unpaired surrogate: the bytes are valid UTF-8 (== the legacy model).
    /// The `Cell` caches the UTF-16 code-unit count ([`UNITS_UNKNOWN`] until
    /// first computed by `len_utf16`), so repeated `.length` reads are O(1)
    /// and — because `units == byte len` iff the string is pure ASCII —
    /// `code_unit_at` can index bytes directly on the overwhelmingly common
    /// ASCII case instead of walking the prefix per access.
    Utf8(Rc<str>, std::cell::Cell<u32>),
    /// Contains ≥1 unpaired surrogate. `bytes` is well-formed WTF-8; `lossy`
    /// is the U+FFFD-replaced UTF-8 view that backs `as_str()` (and the host
    /// boundary); `units` is the exact UTF-16 code-unit count.
    Wtf8(Rc<Wtf8Buf>),
    /// Lazy concatenation of two WELL-FORMED strings (`Utf8`/`Rope` children
    /// only — a `Wtf8` operand takes the eager code-unit path so surrogate
    /// re-pairing stays correct). Makes the `s += chunk` build loop O(total)
    /// instead of O(total²): each `+` is one rope node; the bytes are copied
    /// exactly once, when something first observes them (`as_str`), into the
    /// one-shot `flat` cache. `bytes`/`units` are stored so `.length` and the
    /// engine's string-size guard stay O(1) without flattening.
    Rope(Rc<Rope>),
}

struct Wtf8Buf {
    bytes: Box<[u8]>,
    lossy: Box<str>,
    units: u32,
}

struct Rope {
    left: JsString,
    right: JsString,
    /// Total UTF-8 byte length of the tree (children are well-formed).
    bytes: usize,
    /// Total UTF-16 code-unit length (`.length`), precomputed.
    units: usize,
    /// The flattened form, built once on first byte-level observation.
    flat: std::cell::OnceCell<Rc<str>>,
}

/// Minimum combined size before `concat` builds a rope node instead of
/// copying. Below this, an eager copy is cheaper than the node + eventual
/// flatten bookkeeping, and short-string behavior stays exactly as before.
pub(crate) const ROPE_MIN_BYTES: usize = 64;

/// Sentinel for a `Repr::Utf8` whose code-unit count has not been computed
/// yet. Safe: [`MAX_STRING_LEN`] (16M units) keeps every real count far below
/// `u32::MAX`.
const UNITS_UNKNOWN: u32 = u32::MAX;

/// A fresh `Utf8` arm with its unit count not yet computed. O(1) — callers on
/// hot paths (bytecode constant loads) must not pay a scan here.
fn utf8_repr(s: Rc<str>) -> Repr {
    Repr::Utf8(s, std::cell::Cell::new(UNITS_UNKNOWN))
}

thread_local! {
    /// Shared empty backing for [`Rope`]'s iterative `Drop` (placeholder the
    /// children are replaced with while dismantling — an `Rc` bump, not an
    /// allocation per node).
    static EMPTY_RC_STR: Rc<str> = Rc::from("");
}

impl Rope {
    /// Append the whole tree's bytes to `out` without recursion (a build loop
    /// makes left-leaning chains as deep as the number of appends).
    fn append_to(&self, out: &mut String) {
        let mut stack: Vec<&JsString> = vec![&self.right, &self.left];
        while let Some(part) = stack.pop() {
            match &part.0 {
                Repr::Utf8(s, _) => out.push_str(s),
                Repr::Rope(r) => match r.flat.get() {
                    Some(f) => out.push_str(f),
                    None => {
                        stack.push(&r.right);
                        stack.push(&r.left);
                    }
                },
                // Ropes are built from well-formed parts only.
                Repr::Wtf8(_) => unreachable!("rope over non-well-formed string"),
            }
        }
    }
}

impl Drop for Rope {
    /// Dismantle iteratively: dropping a chain of N appends must not recurse
    /// N deep through nested `Rc<Rope>` drops.
    fn drop(&mut self) {
        let empty = EMPTY_RC_STR.with(|e| e.clone());
        let take =
            |slot: &mut JsString| std::mem::replace(slot, JsString(utf8_repr(empty.clone())));
        let mut stack = vec![take(&mut self.left), take(&mut self.right)];
        while let Some(part) = stack.pop() {
            if let Repr::Rope(r) = part.0 {
                // Only dismantle a uniquely-owned node; a shared one is kept
                // alive by its other owner and must not be gutted.
                if let Some(rope) = Rc::into_inner(r) {
                    let mut rope = rope;
                    stack.push(take(&mut rope.left));
                    stack.push(take(&mut rope.right));
                    // `rope` drops here with empty children: no recursion.
                }
            }
        }
    }
}

/// Code-unit index one past the code point starting at `i`: `i + 2` when
/// `units[i]` begins a surrogate pair, else `i + 1`. Used to step a code-unit
/// buffer by code point (the String iterator / `codePointAt`).
pub fn next_code_point_boundary(units: &[u16], i: usize) -> usize {
    if (0xD800..=0xDBFF).contains(&units[i])
        && i + 1 < units.len()
        && (0xDC00..=0xDFFF).contains(&units[i + 1])
    {
        i + 2
    } else {
        i + 1
    }
}

/// Iterator over a `JsString`'s UTF-16 code units (`s.code_units()`).
pub enum CodeUnits<'a> {
    Utf8(std::str::EncodeUtf16<'a>),
    Wtf8(crate::wtf8::Wtf8Units<'a>),
}
impl Iterator for CodeUnits<'_> {
    type Item = u16;
    #[inline]
    fn next(&mut self) -> Option<u16> {
        match self {
            CodeUnits::Utf8(it) => it.next(),
            CodeUnits::Wtf8(it) => it.next(),
        }
    }
}

impl JsString {
    /// Build from valid UTF-8 (the source of nearly every string: literals,
    /// number/JSON conversions, host input). Always the cheap `Utf8` arm.
    pub fn new(s: impl AsRef<str>) -> Self {
        JsString(utf8_repr(Rc::from(s.as_ref())))
    }
    /// Adopt an existing `Rc<str>` without reallocating — used for bytecode
    /// string-constant loads, which are a hot path.
    pub fn from_rc_str(s: Rc<str>) -> Self {
        JsString(utf8_repr(s))
    }
    /// Build from a UTF-16 code-unit sequence, re-pairing adjacent surrogates.
    /// Takes the `Utf8` arm when the result is well-formed.
    pub fn from_code_units(units: &[u16]) -> Self {
        if crate::wtf8::is_well_formed(units) {
            // Well-formed ⇒ `from_utf16` cannot fail. The unit count is the
            // input length — record it rather than rediscovering it later.
            JsString(Repr::Utf8(
                Rc::from(String::from_utf16_lossy(units).as_str()),
                std::cell::Cell::new(units.len() as u32),
            ))
        } else {
            let bytes = crate::wtf8::encode_wtf8(units);
            let lossy = crate::wtf8::to_string_lossy(&bytes);
            JsString(Repr::Wtf8(Rc::new(Wtf8Buf {
                bytes: bytes.into_boxed_slice(),
                lossy: lossy.into_boxed_str(),
                units: units.len() as u32,
            })))
        }
    }
    /// A `&str` view. For well-formed strings this is the exact contents and a
    /// free borrow; for strings holding unpaired surrogates it is the
    /// U+FFFD-replaced (lossy) view — which is precisely what every UTF-8-only
    /// consumer (the host JSON boundary) wants. Internal operations that must
    /// preserve surrogates use the code-unit API instead.
    pub fn as_str(&self) -> &str {
        match &self.0 {
            Repr::Utf8(s, _) => s,
            Repr::Wtf8(w) => &w.lossy,
            Repr::Rope(r) => r.flat.get_or_init(|| {
                let mut out = String::with_capacity(r.bytes);
                r.append_to(&mut out);
                Rc::from(out.as_str())
            }),
        }
    }
    /// Total byte length WITHOUT flattening a rope — the basis for the
    /// engine's string-size guard on every concatenation.
    pub fn byte_len(&self) -> usize {
        match &self.0 {
            Repr::Utf8(s, _) => s.len(),
            Repr::Wtf8(w) => w.bytes.len(),
            Repr::Rope(r) => r.bytes,
        }
    }
    /// Canonical well-formed WTF-8 bytes — the basis for equality and hashing.
    pub fn wtf8_bytes(&self) -> &[u8] {
        match &self.0 {
            Repr::Utf8(s, _) => s.as_bytes(),
            Repr::Wtf8(w) => &w.bytes,
            // A rope is well-formed UTF-8; observing its bytes flattens once.
            Repr::Rope(_) => self.as_str().as_bytes(),
        }
    }
    /// Length in UTF-16 code units (the JS `.length`). O(n) once for the
    /// `Utf8` arm, then served from the per-handle cache — an `s.length`
    /// read in a loop condition must not rescan the string per iteration.
    pub fn len_utf16(&self) -> usize {
        match &self.0 {
            Repr::Utf8(s, units) => {
                let cached = units.get();
                if cached != UNITS_UNKNOWN {
                    return cached as usize;
                }
                let n: usize = s.chars().map(|c| c.len_utf16()).sum();
                units.set(n as u32);
                n
            }
            Repr::Wtf8(w) => w.units as usize,
            Repr::Rope(r) => r.units,
        }
    }
    /// The UTF-16 code unit at index `i`, or `None` if out of range. O(1) for
    /// pure-ASCII strings (unit count == byte count ⇔ every unit is one
    /// ASCII byte); O(i) otherwise. The `charCodeAt`-class builtins loop over
    /// this, so the ASCII case must not walk the prefix per call.
    pub fn code_unit_at(&self, i: usize) -> Option<u16> {
        match &self.0 {
            Repr::Utf8(s, units) if units.get() as usize == s.len() => {
                s.as_bytes().get(i).map(|&b| b as u16)
            }
            Repr::Rope(r) if r.units == r.bytes => {
                self.as_str().as_bytes().get(i).map(|&b| b as u16)
            }
            _ => self.code_units().nth(i),
        }
    }
    /// Iterate the UTF-16 code units.
    pub fn code_units(&self) -> CodeUnits<'_> {
        match &self.0 {
            Repr::Utf8(s, _) => CodeUnits::Utf8(s.encode_utf16()),
            Repr::Wtf8(w) => CodeUnits::Wtf8(crate::wtf8::decode_units(&w.bytes)),
            Repr::Rope(_) => CodeUnits::Utf8(self.as_str().encode_utf16()),
        }
    }
    /// Collect the UTF-16 code units (the regexp / split boundary).
    pub fn to_utf16_vec(&self) -> Vec<u16> {
        self.code_units().collect()
    }
    /// Split into code points — combining surrogate pairs — each as a
    /// `JsString` of its 1–2 code units. This is the String iterator's
    /// granularity (`for..of`, spread, `[...s]`): code-point-wise, but a lone
    /// surrogate is preserved as a single one-unit string (not U+FFFD).
    pub fn code_point_strings(&self) -> Vec<JsString> {
        let units = self.to_utf16_vec();
        let mut out = Vec::new();
        let mut i = 0;
        while i < units.len() {
            let end = next_code_point_boundary(&units, i);
            out.push(JsString::from_code_units(&units[i..end]));
            i = end;
        }
        out
    }
    /// `true` if the string contains no unpaired surrogate.
    pub fn is_well_formed(&self) -> bool {
        matches!(self.0, Repr::Utf8(..) | Repr::Rope(_))
    }
    /// Replace every unpaired surrogate with U+FFFD (`String.prototype.toWellFormed`).
    pub fn to_well_formed(&self) -> JsString {
        match &self.0 {
            Repr::Utf8(..) | Repr::Rope(_) => self.clone(),
            Repr::Wtf8(w) => JsString::new(&*w.lossy),
        }
    }
    /// The borrowed UTF-8 view IF this is a plain (non-rope, well-formed)
    /// string — O(1), never flattens a rope. `None` for ropes and WTF-8.
    pub fn as_flat_utf8(&self) -> Option<&str> {
        match &self.0 {
            Repr::Utf8(s, _) => Some(s),
            _ => None,
        }
    }

    /// The flat UTF-8 view of a well-formed string, FLATTENING a rope on
    /// first use (the copy is cached in the rope node, so repeated calls —
    /// a kernel's per-access reads — are O(1)). `None` for WTF-8 strings.
    pub fn flatten_utf8(&self) -> Option<&str> {
        match &self.0 {
            Repr::Utf8(s, _) => Some(s),
            Repr::Rope(_) => Some(self.as_str()),
            Repr::Wtf8(_) => None,
        }
    }

    /// Concatenate, preserving code units. Two well-formed strings concatenate
    /// as plain UTF-8; otherwise we route through code units so a high+low
    /// surrogate straddling the boundary re-pairs into one astral code point.
    pub fn concat(&self, other: &JsString) -> JsString {
        match (&self.0, &other.0) {
            // Both sides well-formed: O(1) rope node once the result is big
            // enough to matter; eager copy below the threshold (small-string
            // behavior unchanged, no node overhead). This turns the
            // `s += chunk` build loop from O(total²) into O(total).
            (Repr::Utf8(..) | Repr::Rope(_), Repr::Utf8(..) | Repr::Rope(_)) => {
                let (lb, rb) = (self.byte_len(), other.byte_len());
                if lb == 0 {
                    return other.clone();
                }
                if rb == 0 {
                    return self.clone();
                }
                if lb + rb >= ROPE_MIN_BYTES {
                    // `len_utf16` is O(1) for a rope child (stored), so the
                    // accumulator side of a build loop never rescans.
                    return JsString(Repr::Rope(Rc::new(Rope {
                        bytes: lb + rb,
                        units: self.len_utf16() + other.len_utf16(),
                        left: self.clone(),
                        right: other.clone(),
                        flat: std::cell::OnceCell::new(),
                    })));
                }
                let mut s = String::with_capacity(lb + rb);
                s.push_str(self.as_str());
                s.push_str(other.as_str());
                JsString(utf8_repr(Rc::from(s.as_str())))
            }
            _ => {
                let mut units = self.to_utf16_vec();
                units.extend(other.code_units());
                JsString::from_code_units(&units)
            }
        }
    }
}

impl PartialEq for JsString {
    fn eq(&self, other: &Self) -> bool {
        // Pointer-equality fast path: strings are shared by `Rc`, and the hot
        // comparisons (property-map probes against bytecode-constant keys)
        // usually compare a clone of the very same allocation. Content equality
        // is unchanged — `ptr_eq` can only confirm, never deny.
        match (&self.0, &other.0) {
            (Repr::Utf8(a, _), Repr::Utf8(b, _)) if Rc::ptr_eq(a, b) => true,
            (Repr::Wtf8(a), Repr::Wtf8(b)) if Rc::ptr_eq(a, b) => true,
            (Repr::Rope(a), Repr::Rope(b)) if Rc::ptr_eq(a, b) => true,
            _ => self.wtf8_bytes() == other.wtf8_bytes(),
        }
    }
}
impl Eq for JsString {}
impl std::hash::Hash for JsString {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.wtf8_bytes().hash(state)
    }
}
impl fmt::Debug for JsString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}
impl From<&str> for JsString {
    fn from(s: &str) -> Self {
        JsString::new(s)
    }
}
impl From<String> for JsString {
    fn from(s: String) -> Self {
        JsString(utf8_repr(Rc::from(s.as_str())))
    }
}

/// Symbol identity is by pointer (each `Symbol()` is unique). Well-known symbols
/// are allocated once in the realm and shared.
#[derive(Clone)]
pub struct JsSymbol(pub Rc<SymbolData>);

pub struct SymbolData {
    pub description: Option<Rc<str>>,
    /// Stable identifier for deterministic ordering / debugging.
    pub id: u64,
}

impl JsSymbol {
    pub fn description(&self) -> Option<&str> {
        self.0.description.as_deref()
    }
}
impl PartialEq for JsSymbol {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for JsSymbol {}
impl std::hash::Hash for JsSymbol {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (Rc::as_ptr(&self.0) as usize).hash(state)
    }
}
impl fmt::Debug for JsSymbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Symbol({:?})", self.0.description)
    }
}

/// A reference-counted, mutable JS object handle. Clone is a cheap `Rc` bump and
/// shares identity (`==` is pointer identity).
#[derive(Clone)]
pub struct JsObject(pub Rc<RefCell<ObjectData>>);

impl JsObject {
    pub fn new(data: ObjectData) -> Self {
        JsObject(Rc::new(RefCell::new(data)))
    }
    pub fn ordinary(proto: Option<JsObject>) -> Self {
        JsObject::new(ObjectData::new(proto, Internal::Ordinary))
    }
    pub fn borrow(&self) -> std::cell::Ref<'_, ObjectData> {
        self.0.borrow()
    }
    pub fn borrow_mut(&self) -> std::cell::RefMut<'_, ObjectData> {
        self.0.borrow_mut()
    }
    /// Pointer identity (same heap object). Basis of the inline caches'
    /// holder verification.
    pub fn ptr_eq(&self, other: &JsObject) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
    pub fn ptr_id(&self) -> usize {
        Rc::as_ptr(&self.0) as *const () as usize
    }
    pub fn same(&self, other: &JsObject) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}
impl PartialEq for JsObject {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for JsObject {}
impl fmt::Debug for JsObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JsObject@{:x}", self.ptr_id())
    }
}

/// A JS value. `Clone` is cheap for all variants (scalars or `Rc` bumps).
#[derive(Clone)]
pub enum Value {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    String(JsString),
    Symbol(JsSymbol),
    Object(JsObject),
    /// The BigInt primitive (arbitrary precision).
    BigInt(Rc<BigInt>),
    /// Temporal Dead Zone marker: the value stored in a `let`/`const`/`class`
    /// cell after hoisting but before its initializer runs. Reading it (via
    /// `LoadCell`/`LoadUpvalue`) throws a `ReferenceError`; it never escapes into
    /// user-observable positions.
    Uninitialized,
    /// Array hole (elision): the slot in a dense `Internal::Array` for a missing
    /// index, e.g. index 1 of `[0, , 2]`. `HasProperty` is false at a hole and
    /// the iteration/own-key machinery skips it; reading it yields `undefined`
    /// (via the prototype chain). It never escapes into user-observable values.
    Hole,
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Undefined => write!(f, "undefined"),
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Number(n) => write!(f, "{n}"),
            Value::String(s) => write!(f, "{s:?}"),
            Value::Symbol(s) => write!(f, "{s:?}"),
            Value::Object(o) => write!(f, "{o:?}"),
            Value::BigInt(n) => write!(f, "{n}n"),
            Value::Uninitialized | Value::Hole => write!(f, "<uninitialized>"),
        }
    }
}

impl Value {
    pub fn str(s: impl AsRef<str>) -> Value {
        Value::String(JsString::new(s))
    }
    pub fn number(n: f64) -> Value {
        Value::Number(n)
    }
    pub fn int(n: i64) -> Value {
        Value::Number(n as f64)
    }
    pub fn bigint(n: BigInt) -> Value {
        Value::BigInt(Rc::new(n))
    }
    pub fn is_undefined(&self) -> bool {
        matches!(self, Value::Undefined)
    }
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
    pub fn is_nullish(&self) -> bool {
        matches!(self, Value::Undefined | Value::Null)
    }
    pub fn as_object(&self) -> Option<&JsObject> {
        match self {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }
    pub fn as_number(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s.as_str()),
            _ => None,
        }
    }
    /// `typeof` operator.
    pub fn type_of(&self) -> &'static str {
        match self {
            Value::Undefined => "undefined",
            Value::Uninitialized | Value::Hole => "undefined",
            Value::Null => "object",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Symbol(_) => "symbol",
            Value::BigInt(_) => "bigint",
            Value::Object(o) => {
                if o.borrow().is_callable() {
                    "function"
                } else {
                    "object"
                }
            }
        }
    }
}

/// A spec Private Name: the runtime identity of a `#name` class element.
/// A fresh one is allocated per name per class *evaluation* (not per class
/// source text), so two evaluations of the same `class` literal mint
/// distinct names — the basis of brand checks.
#[derive(Clone)]
pub struct PrivateName {
    /// VM-unique identity (allocated from `Vm::next_private_id`).
    pub id: u64,
    /// Source-visible spelling (`#x`) for error messages.
    pub description: JsString,
}

/// One entry in an object's `[[PrivateElements]]` list.
#[derive(Clone)]
pub enum PrivateElement {
    Field(Value),
    Method(Value),
    Accessor {
        get: Option<Value>,
        set: Option<Value>,
    },
}

/// A PrivateEnvironment record: maps the compiler's per-class-body storage
/// keys (`#x@<class id>`) to the runtime [`PrivateName`]s minted when the
/// class definition evaluated. Chained towards the outer class bodies;
/// closures created inside a class body capture the chain.
pub struct PrivateEnv {
    pub parent: Option<Rc<PrivateEnv>>,
    /// Compile-time storage key -> runtime name. Small per class; linear scan.
    pub names: Vec<(JsString, PrivateName)>,
}

impl PrivateEnv {
    /// Resolve a compile-time storage key through the chain (innermost first).
    pub fn resolve(env: &Option<Rc<PrivateEnv>>, key: &str) -> Option<PrivateName> {
        let mut cur = env.as_ref();
        while let Some(e) = cur {
            if let Some((_, n)) = e.names.iter().find(|(k, _)| k.as_str() == key) {
                return Some(n.clone());
            }
            cur = e.parent.as_ref();
        }
        None
    }
}

/// A property key: string or symbol. Integer-index keys are stored as their
/// string form; enumeration re-derives integer ordering.
#[derive(Clone, PartialEq, Eq)]
pub enum PropertyKey {
    Str(JsString),
    Sym(JsSymbol),
}

/// Manual (not derived) so [`StrKeyRef`] — the alloc-free `&str` probe — can
/// reproduce the exact same stream for string keys.
impl std::hash::Hash for PropertyKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            PropertyKey::Str(s) => {
                state.write_u8(0);
                s.hash(state);
            }
            PropertyKey::Sym(s) => {
                state.write_u8(1);
                s.hash(state);
            }
        }
    }
}

/// A borrowed string key for probing a property map WITHOUT allocating a
/// `PropertyKey` (whose `JsString` is heap-backed). Hashes exactly like
/// `PropertyKey::Str` of a well-formed string, so
/// `props.contains_key(&StrKeyRef(s))` is equivalent to building the key.
pub struct StrKeyRef<'a>(pub &'a str);

impl std::hash::Hash for StrKeyRef<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // `PropertyKey::Str` hashes tag 0 + the string's canonical WTF-8
        // bytes; a well-formed &str's bytes ARE its WTF-8 bytes.
        state.write_u8(0);
        self.0.as_bytes().hash(state);
    }
}

impl indexmap::Equivalent<PropertyKey> for StrKeyRef<'_> {
    fn equivalent(&self, key: &PropertyKey) -> bool {
        matches!(key, PropertyKey::Str(s) if s.wtf8_bytes() == self.0.as_bytes())
    }
}

impl PropertyKey {
    pub fn str(s: impl AsRef<str>) -> PropertyKey {
        PropertyKey::Str(JsString::new(s))
    }
    pub fn from_index(i: u32) -> PropertyKey {
        // Stack-format the digits so the key costs one allocation (the
        // `Rc<str>`), not two — this runs per element in `own_keys` and the
        // array builtins' generic paths.
        let mut buf = [0u8; 10];
        PropertyKey::Str(JsString::new(fmt_index(i, &mut buf)))
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            PropertyKey::Str(s) => Some(s.as_str()),
            PropertyKey::Sym(_) => None,
        }
    }
    /// Returns the array-index interpretation of this key if it is a canonical
    /// integer index in `[0, 2^32-1)`.
    pub fn array_index(&self) -> Option<u32> {
        let s = self.as_str()?;
        canonical_index(s)
    }
}

impl fmt::Debug for PropertyKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PropertyKey::Str(s) => write!(f, "{s:?}"),
            PropertyKey::Sym(s) => write!(f, "{s:?}"),
        }
    }
}

/// Upper bound on eager dense-array allocation. Beyond this, length operations
/// throw `RangeError` rather than allocating (we use dense storage, not sparse).
pub const MAX_DENSE_ARRAY: usize = 1_000_000;

/// Upper bound on a single eager string allocation (`repeat`, `padStart`/
/// `padEnd`, …). Beyond this, those builtins throw `RangeError` *before*
/// allocating, so a hostile/conformance input (`"a".repeat(2**33)`) cannot OOM
/// the process. Well above any legitimate string a program builds.
pub const MAX_STRING_LEN: usize = 1 << 24; // 16M code units

/// Canonical numeric index per spec `CanonicalNumericIndexString` restricted to
/// array indices (used for ordering and array fast-paths).
pub fn canonical_index(s: &str) -> Option<u32> {
    if s == "0" {
        return Some(0);
    }
    if s.is_empty() || s.as_bytes()[0] == b'0' {
        return None; // no leading zeros
    }
    if !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    match s.parse::<u32>() {
        Ok(n) if (n as u64) < (u32::MAX as u64) => Some(n),
        _ => None,
    }
}

/// Format an array index into `buf`, returning the digit string. Alloc-free
/// backing for the property-map probes below.
fn fmt_index(idx: u32, buf: &mut [u8; 10]) -> &str {
    let mut i = buf.len();
    let mut n = idx;
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    std::str::from_utf8(&buf[i..]).expect("ASCII digits")
}

/// True when no object on the PROTOTYPE chain starting at `proto` could
/// observably intercept the CREATION of the array-index properties
/// `idx..idx+count` on the receiver (an append or in-bounds hole fill, whose
/// own property is absent — so the spec's OrdinarySet consults the chain):
/// every prototype must be a plain `Ordinary` or dense-`Array` object with
/// no reified `props` entry at those indices. A dense proto ELEMENT is a
/// plain writable data property — OrdinarySet then creates the property on
/// the receiver anyway, so skipping it is unobservable; only a reified entry
/// (which may be an accessor or non-writable) can intercept or veto the
/// write. The hot dense-array write fast paths call this before creating;
/// anything else declines to the generic path.
pub fn protos_allow_index_create(proto: Option<JsObject>, idx: u32, count: u32) -> bool {
    // Stack-format the first index once — the common `count == 1` case
    // (a push of one element, one `a[i] = v` store) reuses it at every level.
    let mut buf = [0u8; 10];
    let first = fmt_index(idx, &mut buf);
    let mut cur = proto;
    while let Some(p) = cur {
        let b = p.borrow();
        match &b.internal {
            Internal::Ordinary | Internal::Array(_) => {}
            // Exotic protos (Proxy traps, TypedArray index absorption,
            // String character slots, mapped Arguments, …) own their [[Set]].
            _ => return false,
        }
        if !b.props.is_empty() {
            if b.props.contains_key(&StrKeyRef(first)) {
                return false;
            }
            for j in 1..count {
                let mut buf = [0u8; 10];
                if b.props
                    .contains_key(&StrKeyRef(fmt_index(idx + j, &mut buf)))
                {
                    return false;
                }
            }
        }
        let next = b.proto.clone();
        drop(b);
        cur = next;
    }
    true
}

/// The activation-scoped variant of [`protos_allow_index_create`] for the
/// kernel `StoreElem` fast path: true when no object on `start`'s prototype
/// chain could observably intercept the creation of ANY array-index property
/// on `start` — every prototype a plain `Ordinary`/dense-`Array` object with
/// no reified index-keyed `props` entry at all. Checked ONCE per kernel
/// activation (nothing inside a kernel region can run user code or
/// restructure a property map, so the verdict holds for the whole
/// activation); the per-key probes stay in the per-write helper above.
pub fn protos_allow_any_index_create(start: &JsObject) -> bool {
    let mut cur = start.borrow().proto.clone();
    while let Some(p) = cur {
        let b = p.borrow();
        match &b.internal {
            Internal::Ordinary | Internal::Array(_) => {}
            _ => return false,
        }
        if !b.props.is_empty() && b.props.keys().any(|k| k.array_index().is_some()) {
            return false;
        }
        let next = b.proto.clone();
        drop(b);
        cur = next;
    }
    true
}

#[derive(Clone)]
pub struct Property {
    pub kind: PropertyKind,
    pub enumerable: bool,
    pub configurable: bool,
}

#[derive(Clone)]
pub enum PropertyKind {
    Data {
        value: Value,
        writable: bool,
    },
    Accessor {
        get: Option<Value>,
        set: Option<Value>,
    },
}

impl Property {
    /// Default data property (writable, enumerable, configurable) — used for
    /// ordinary assignment.
    pub fn data(value: Value) -> Property {
        Property {
            kind: PropertyKind::Data {
                value,
                writable: true,
            },
            enumerable: true,
            configurable: true,
        }
    }
    /// Non-enumerable method/builtin property (writable, configurable).
    pub fn builtin(value: Value) -> Property {
        Property {
            kind: PropertyKind::Data {
                value,
                writable: true,
            },
            enumerable: false,
            configurable: true,
        }
    }
    /// Frozen data property: non-writable, non-enumerable, non-configurable
    /// (e.g. `Math.PI`, `Number.MAX_VALUE`).
    pub fn frozen(value: Value) -> Property {
        Property {
            kind: PropertyKind::Data {
                value,
                writable: false,
            },
            enumerable: false,
            configurable: false,
        }
    }
    pub fn value(&self) -> Option<&Value> {
        match &self.kind {
            PropertyKind::Data { value, .. } => Some(value),
            PropertyKind::Accessor { .. } => None,
        }
    }
}

pub struct ObjectData {
    pub proto: Option<JsObject>,
    pub props: crate::fxhash::FxIndexMap<PropertyKey, Property>,
    pub extensible: bool,
    pub internal: Internal,
    /// The spec `[[PrivateElements]]` list, keyed by [`PrivateName::id`].
    /// Boxed so the common no-privates object pays one pointer. Attached
    /// directly to the receiver — even a Proxy — with no traps and no
    /// extensibility check.
    pub privates: Option<Box<IndexMap<u64, PrivateElement>>>,
}

impl ObjectData {
    pub fn new(proto: Option<JsObject>, internal: Internal) -> Self {
        ObjectData {
            proto,
            props: crate::fxhash::FxIndexMap::default(),
            extensible: true,
            internal,
            privates: None,
        }
    }
    pub fn private_get(&self, id: u64) -> Option<&PrivateElement> {
        self.privates.as_ref().and_then(|p| p.get(&id))
    }
    /// Does `props` hold a (reified) entry for the array index `idx`?
    /// Alloc-free: the index is formatted into a stack buffer and probed via
    /// [`StrKeyRef`], so hot fast-path guards can call this per element.
    pub fn has_index_prop(&self, idx: u32) -> bool {
        if self.props.is_empty() {
            return false;
        }
        let mut buf = [0u8; 10];
        self.props
            .contains_key(&StrKeyRef(fmt_index(idx, &mut buf)))
    }
    /// Append a private element; `false` (no insert) when `id` is already
    /// present — the caller's duplicate-initialization TypeError.
    pub fn private_add(&mut self, id: u64, el: PrivateElement) -> bool {
        let table = self.privates.get_or_insert_with(Default::default);
        if table.contains_key(&id) {
            return false;
        }
        table.insert(id, el);
        true
    }
    pub fn is_callable(&self) -> bool {
        match &self.internal {
            Internal::Function(_) => true,
            // A proxy is callable iff it captured `[[Call]]` at creation.
            Internal::Proxy(p) => p.callable,
            _ => false,
        }
    }
    pub fn is_array(&self) -> bool {
        matches!(self.internal, Internal::Array(_))
    }
    pub fn class_name(&self) -> &'static str {
        match &self.internal {
            Internal::Ordinary => "Object",
            Internal::Array(_) => "Array",
            Internal::Function(_) => "Function",
            Internal::Error => "Error",
            Internal::Boolean(_) => "Boolean",
            Internal::Number(_) => "Number",
            Internal::StringObj(_) => "String",
            Internal::Symbol(_) => "Symbol",
            Internal::Map(_) => "Map",
            Internal::Set(_) => "Set",
            Internal::WeakMap(_) => "WeakMap",
            Internal::WeakSet(_) => "WeakSet",
            Internal::Promise(_) => "Promise",
            Internal::Generator(_) => "Generator",
            Internal::Date(_) => "Date",
            Internal::Arguments(_) => "Arguments",
            Internal::Iterator(_) => "Iterator",
            Internal::ArrayBuffer(_) => "ArrayBuffer",
            Internal::TypedArray(_) => "TypedArray",
            Internal::DataView(_) => "DataView",
            Internal::BigIntObj(_) => "BigInt",
            Internal::Proxy(_) => "Proxy",
            Internal::ModuleNamespace(_) => "Module",
            Internal::Temporal(_) => "Temporal",
        }
    }
}

/// Exotic behaviors / internal slots.
pub enum Internal {
    Ordinary,
    /// Dense array storage. The `length` property is derived from this vec.
    Array(Vec<Value>),
    Function(FunctionInner),
    Error,
    Boolean(bool),
    Number(f64),
    StringObj(JsString),
    Symbol(JsSymbol),
    Map(crate::fxhash::FxIndexMap<MapKey, Value>),
    Set(crate::fxhash::FxIndexMap<MapKey, ()>),
    /// WeakMap/WeakSet. Our GC is reference-counting with no weak references, so
    /// these hold strong refs — observationally identical for all of Test262
    /// (which cannot force collection); only `WeakRef`/`FinalizationRegistry`
    /// expose collection and remain unsupported (determinism contract).
    WeakMap(crate::fxhash::FxIndexMap<MapKey, Value>),
    WeakSet(crate::fxhash::FxIndexMap<MapKey, ()>),
    /// Boxed: `PromiseData` is the largest inline payload (104 bytes) and
    /// promises are allocation-rare next to plain objects — boxing it (and
    /// `NamespaceData`) shrinks EVERY `ObjectData` by ~32 bytes.
    Promise(Box<crate::vm::PromiseData>),
    Generator(crate::vm::GeneratorData),
    Date(f64),
    /// The `arguments` exotic object. For a MAPPED one (sloppy, simple
    /// parameter list) the vec aliases each index to its parameter's live
    /// cell (`None` = unmapped index); empty for unmapped arguments.
    Arguments(Vec<Option<Rc<RefCell<Value>>>>),
    /// A built-in iterator over an array/string/Map/Set.
    Iterator(IterState),
    /// Raw byte buffer backing typed arrays / data views. `None` = detached.
    ArrayBuffer(Option<Vec<u8>>),
    /// A typed-array view onto an `ArrayBuffer`.
    TypedArray(TypedArrayData),
    /// A `DataView` onto an `ArrayBuffer`.
    DataView(DataViewData),
    /// Boxed BigInt (Object(new BigInt-wrapper)); holds the primitive.
    BigIntObj(Rc<BigInt>),
    /// A Proxy exotic object: forwards internal methods to `handler` traps,
    /// defaulting to `target`. `revoked` clears both once `revoke()` is called.
    Proxy(ProxyData),
    /// A Module Namespace exotic object (`import * as ns` / dynamic
    /// `import()` result): null prototype, non-extensible, exports exposed as
    /// live {writable:true, enumerable:true, configurable:false} data
    /// properties whose [[Set]] always fails and whose [[Delete]] refuses.
    ModuleNamespace(Box<NamespaceData>),
    /// A `Temporal.*` object. The spec arithmetic lives in `temporal_rs`; the
    /// slot holds the immutable backing value (no JS references, so the GC
    /// treats it as a leaf).
    Temporal(Box<TemporalSlot>),
}

/// The backing value of a `Temporal.*` object (see `Internal::Temporal`).
pub enum TemporalSlot {
    Instant(temporal_rs::Instant),
    Duration(temporal_rs::Duration),
    PlainDate(temporal_rs::PlainDate),
    PlainTime(temporal_rs::PlainTime),
    PlainDateTime(temporal_rs::PlainDateTime),
    PlainYearMonth(temporal_rs::PlainYearMonth),
    PlainMonthDay(temporal_rs::PlainMonthDay),
    ZonedDateTime(temporal_rs::ZonedDateTime),
}

/// Backing slots for a Module Namespace exotic object: export name → the
/// module's live binding cell, pre-sorted by name (spec: ascending code-unit
/// order). Reads go through the cell so post-snapshot reassignment in the
/// module is observable; an uninitialized (TDZ) cell read throws.
pub struct NamespaceData {
    pub exports: IndexMap<JsString, Rc<RefCell<Value>>>,
}

/// Backing slots for a Proxy exotic object.
pub struct ProxyData {
    pub target: JsObject,
    pub handler: JsObject,
    pub revoked: bool,
    /// Whether the proxy exposes `[[Call]]` — fixed at creation from the
    /// target's callability (spec ProxyCreate). It survives revocation, so
    /// `IsCallable`/`typeof` of a revoked function proxy stays `"function"`.
    pub callable: bool,
}

/// Element type of a typed array.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TAKind {
    I8,
    U8,
    U8Clamped,
    I16,
    U16,
    I32,
    U32,
    F32,
    F64,
    I64,
    U64,
}

impl TAKind {
    pub fn bytes(self) -> usize {
        match self {
            TAKind::I8 | TAKind::U8 | TAKind::U8Clamped => 1,
            TAKind::I16 | TAKind::U16 => 2,
            TAKind::I32 | TAKind::U32 | TAKind::F32 => 4,
            TAKind::F64 | TAKind::I64 | TAKind::U64 => 8,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            TAKind::I8 => "Int8Array",
            TAKind::U8 => "Uint8Array",
            TAKind::U8Clamped => "Uint8ClampedArray",
            TAKind::I16 => "Int16Array",
            TAKind::U16 => "Uint16Array",
            TAKind::I32 => "Int32Array",
            TAKind::U32 => "Uint32Array",
            TAKind::F32 => "Float32Array",
            TAKind::F64 => "Float64Array",
            TAKind::I64 => "BigInt64Array",
            TAKind::U64 => "BigUint64Array",
        }
    }
    /// Whether elements are BigInt values (vs. JS numbers).
    pub fn is_bigint(self) -> bool {
        matches!(self, TAKind::I64 | TAKind::U64)
    }
    pub fn all() -> [TAKind; 11] {
        [
            TAKind::I8,
            TAKind::U8,
            TAKind::U8Clamped,
            TAKind::I16,
            TAKind::U16,
            TAKind::I32,
            TAKind::U32,
            TAKind::F32,
            TAKind::F64,
            TAKind::I64,
            TAKind::U64,
        ]
    }
}

pub struct TypedArrayData {
    pub buffer: JsObject,
    pub byte_offset: usize,
    pub length: usize,
    pub kind: TAKind,
    /// True for an auto-length view on a resizable buffer: its length tracks the
    /// buffer's current byte length rather than being fixed at construction.
    pub length_tracking: bool,
}

pub struct DataViewData {
    pub buffer: JsObject,
    pub byte_offset: usize,
    pub byte_length: usize,
    /// True for an auto-length view on a resizable buffer (no explicit length):
    /// its byteLength tracks the buffer's current byte length.
    pub length_tracking: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IterKind {
    ArrayKeys,
    ArrayValues,
    ArrayEntries,
    StringChars,
    MapKeys,
    MapValues,
    MapEntries,
    SetValues,
    SetEntries,
}

pub struct IterState {
    pub target: Option<JsObject>,
    pub string: Option<JsString>,
    pub index: usize,
    pub kind: IterKind,
    pub done: bool,
}

/// A `SameValueZero`-keyed map/set key (NaN equal to NaN, +0 equal to -0).
#[derive(Clone)]
pub struct MapKey(pub Value);

impl PartialEq for MapKey {
    fn eq(&self, other: &Self) -> bool {
        same_value_zero(&self.0, &other.0)
    }
}
impl Eq for MapKey {}
impl std::hash::Hash for MapKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match &self.0 {
            Value::Undefined | Value::Uninitialized | Value::Hole => 0u8.hash(state),
            Value::Null => 1u8.hash(state),
            Value::Bool(b) => {
                2u8.hash(state);
                b.hash(state);
            }
            Value::Number(n) => {
                3u8.hash(state);
                // Normalize -0 to +0 and NaN to a canonical bit pattern.
                let norm = if *n == 0.0 {
                    0.0f64
                } else if n.is_nan() {
                    f64::NAN
                } else {
                    *n
                };
                norm.to_bits().hash(state);
            }
            Value::String(s) => {
                4u8.hash(state);
                s.hash(state);
            }
            Value::Symbol(s) => {
                5u8.hash(state);
                s.hash(state);
            }
            Value::BigInt(n) => {
                7u8.hash(state);
                n.hash(state);
            }
            Value::Object(o) => {
                6u8.hash(state);
                o.ptr_id().hash(state);
            }
        }
    }
}

/// `SameValueZero`: like `===` but NaN equals NaN. Used for Map/Set keys,
/// `Array.prototype.includes`, etc.
pub fn same_value_zero(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => (x.is_nan() && y.is_nan()) || x == y,
        _ => strict_equals_nonnumeric(a, b),
    }
}

/// `SameValue`: like SameValueZero but distinguishes +0/-0. Used by
/// `Object.is`.
pub fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            if x.is_nan() && y.is_nan() {
                true
            } else if *x == 0.0 && *y == 0.0 {
                x.is_sign_negative() == y.is_sign_negative()
            } else {
                x == y
            }
        }
        _ => strict_equals_nonnumeric(a, b),
    }
}

/// The non-numeric portion of `===` (numbers handled by callers).
pub fn strict_equals_nonnumeric(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Undefined, Value::Undefined) => true,
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Symbol(x), Value::Symbol(y)) => x == y,
        (Value::BigInt(x), Value::BigInt(y)) => x == y,
        (Value::Object(x), Value::Object(y)) => x.same(y),
        _ => false,
    }
}

/// A boxed native function: a Rust closure callable from JS. `Err` carries the
/// thrown JS value.
pub type NativeFn = Rc<dyn Fn(&mut crate::vm::Vm, Value, &[Value]) -> Result<Value, Value>>;

pub enum FunctionInner {
    Native(NativeFunction),
    Bytecode(Rc<BytecodeFunction>),
    Bound(BoundFunction),
}

pub struct NativeFunction {
    pub name: Rc<str>,
    pub length: u32,
    pub func: NativeFn,
    /// Native constructor (e.g. `new Map()`), if this is constructable.
    pub construct: Option<NativeFn>,
}

#[derive(Clone)]
pub struct BytecodeFunction {
    pub proto: Rc<FuncProto>,
    /// Captured variables (closure environment), one cell per upvalue descriptor.
    pub upvalues: Vec<Rc<RefCell<Value>>>,
    /// For methods: the object the method was defined on (used by `super`).
    pub home_object: Option<JsObject>,
    /// For class constructors only.
    pub is_class_ctor: bool,
    /// The `with`-scope chain active when this closure was created (innermost
    /// last). A function defined inside `with (o) { … }` resolves its free
    /// identifiers against `o` even when called after the block; the chain
    /// seeds the callee frame's with-scope stack.
    pub captured_with: Vec<JsObject>,
    /// The PrivateEnvironment chain active when this closure was created.
    /// Methods/initializers defined inside a class body resolve `#x` storage
    /// keys against it; `None` outside class bodies.
    pub captured_priv_env: Option<Rc<PrivateEnv>>,
}

pub struct BoundFunction {
    pub target: JsObject,
    pub bound_this: Value,
    pub bound_args: Vec<Value>,
}

impl ObjectData {
    pub fn as_function(&self) -> Option<&FunctionInner> {
        match &self.internal {
            Internal::Function(f) => Some(f),
            _ => None,
        }
    }
}
