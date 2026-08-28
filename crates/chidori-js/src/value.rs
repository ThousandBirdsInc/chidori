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
    /// Short well-formed UTF-8 stored inline — no allocation, no refcount,
    /// clone is a 24-byte copy. This is the overwhelming majority of strings
    /// an agent program churns through: property keys, number→string
    /// conversions, JSON keys and small values, glue-code fragments. The cap
    /// is chosen so the enum stays the same 24 bytes the `Utf8` arm forces.
    /// Interned/constant strings deliberately do NOT take this arm (see
    /// [`JsString::from_rc_str`]): they keep the shared `Rc` so the
    /// pointer-equality fast paths in `eq` keep confirming in one compare.
    /// `meta` packs the byte length (low bits) with an is-ASCII flag
    /// ([`INLINE_ASCII`], computed once at construction) so the per-code-unit
    /// hot paths (`code_unit_at`, `len_utf16`) stay O(1) without a rescan.
    Inline { meta: u8, buf: [u8; INLINE_CAP] },
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
/// yet. Safe: [`MAX_STRING_LEN`] (2^28 units) keeps every real count far
/// below `u32::MAX`.
const UNITS_UNKNOWN: u32 = u32::MAX;

/// Byte capacity of [`Repr::Inline`]: the largest inline buffer that keeps
/// the `Repr` enum at the 24 bytes the `Utf8` arm (16-byte fat `Rc<str>` +
/// 4-byte cell + tag) already forces.
const INLINE_CAP: usize = 22;

/// Bit in `Repr::Inline::meta` marking a pure-ASCII buffer (unit count ==
/// byte count, and byte index == unit index). The low bits hold the length.
const INLINE_ASCII: u8 = 0x80;

/// Decode an inline `meta` byte into the byte length.
fn inline_len(meta: u8) -> usize {
    (meta & !INLINE_ASCII) as usize
}

/// A fresh `Utf8` arm with its unit count not yet computed. O(1) — callers on
/// hot paths (bytecode constant loads) must not pay a scan here.
fn utf8_repr(s: Rc<str>) -> Repr {
    Repr::Utf8(s, std::cell::Cell::new(UNITS_UNKNOWN))
}

/// The representation for freshly-built well-formed text: inline when it
/// fits, heap `Rc` otherwise.
fn well_formed_repr(s: &str) -> Repr {
    match inline_repr(s) {
        Some(r) => r,
        None => utf8_repr(Rc::from(s)),
    }
}

/// The inline representation of `s`, if it fits.
fn inline_repr(s: &str) -> Option<Repr> {
    if s.len() <= INLINE_CAP {
        let mut buf = [0u8; INLINE_CAP];
        buf[..s.len()].copy_from_slice(s.as_bytes());
        let ascii = if s.is_ascii() { INLINE_ASCII } else { 0 };
        Some(Repr::Inline {
            meta: s.len() as u8 | ascii,
            buf,
        })
    } else {
        None
    }
}

/// Assemble an inline repr directly from well-formed UTF-8 bytes (both
/// halves come out of existing `JsString`s, so no validation is needed —
/// only the ASCII scan for the meta bit).
fn inline_from_wf_bytes(a: &[u8], b: &[u8]) -> Repr {
    let mut buf = [0u8; INLINE_CAP];
    buf[..a.len()].copy_from_slice(a);
    buf[a.len()..a.len() + b.len()].copy_from_slice(b);
    let len = a.len() + b.len();
    let ascii = if buf[..len].is_ascii() {
        INLINE_ASCII
    } else {
        0
    };
    Repr::Inline {
        meta: len as u8 | ascii,
        buf,
    }
}

/// The `&str` view of an inline buffer. Inline strings are only ever built
/// from `&str` slices (or bytes copied out of existing well-formed
/// `JsString`s), so the check cannot fail. The default build re-validates —
/// on ≤[`INLINE_CAP`] bytes that is a handful of instructions, which
/// `#![forbid(unsafe_code)]` makes the right trade — but under the `jit`
/// feature (where the crate already carries audited `unsafe`) the
/// re-validation is skipped: `as_str` on inline strings is hot enough that
/// the checks showed up at ~3% of JSON round-trips. Byte-level consumers
/// (equality, hashing, JSON quoting, concat assembly) go through
/// `wtf8_bytes` instead and skip it entirely.
fn inline_str(buf: &[u8; INLINE_CAP], meta: u8) -> &str {
    let bytes = &buf[..inline_len(meta)];
    #[cfg(feature = "jit")]
    {
        // SAFETY: every `Repr::Inline` constructor copies its bytes from a
        // `&str` or from existing well-formed `JsString` storage, and
        // `meta`'s length was set from that same slice.
        #[expect(unsafe_code, reason = "inline strings are valid UTF-8 by construction")]
        return unsafe { std::str::from_utf8_unchecked(bytes) };
    }
    #[cfg(not(feature = "jit"))]
    {
        std::str::from_utf8(bytes).expect("inline string holds valid UTF-8")
    }
}

impl Rope {
    /// Append the whole tree's bytes to `out` without recursion (a build loop
    /// makes left-leaning chains as deep as the number of appends).
    fn append_to(&self, out: &mut String) {
        let mut stack: Vec<&JsString> = vec![&self.right, &self.left];
        while let Some(part) = stack.pop() {
            match &part.0 {
                Repr::Inline { meta, buf } => out.push_str(inline_str(buf, *meta)),
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
        let take = |slot: &mut JsString| {
            std::mem::replace(
                slot,
                JsString(Repr::Inline {
                    meta: INLINE_ASCII,
                    buf: [0; INLINE_CAP],
                }),
            )
        };
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
    /// number/JSON conversions, host input). Short text stays inline —
    /// allocation-free; longer text takes the `Utf8` arm.
    pub fn new(s: impl AsRef<str>) -> Self {
        JsString(well_formed_repr(s.as_ref()))
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
            // input length — record it rather than rediscovering it later
            // (inline strings recount on demand: ≤22 bytes).
            let s = String::from_utf16_lossy(units);
            if let Some(r) = inline_repr(&s) {
                return JsString(r);
            }
            JsString(Repr::Utf8(
                Rc::from(s.as_str()),
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
            Repr::Inline { meta, buf } => inline_str(buf, *meta),
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
            Repr::Inline { meta, .. } => inline_len(*meta),
            Repr::Utf8(s, _) => s.len(),
            Repr::Wtf8(w) => w.bytes.len(),
            Repr::Rope(r) => r.bytes,
        }
    }
    /// Canonical well-formed WTF-8 bytes — the basis for equality and hashing.
    pub fn wtf8_bytes(&self) -> &[u8] {
        match &self.0 {
            Repr::Inline { meta, buf } => &buf[..inline_len(*meta)],
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
            // ≤22 bytes: recounting beats carrying a cache. Pure-ASCII (the
            // common case) short-circuits to the byte length.
            Repr::Inline { meta, buf } => {
                if *meta & INLINE_ASCII != 0 {
                    inline_len(*meta)
                } else {
                    inline_str(buf, *meta).chars().map(|c| c.len_utf16()).sum()
                }
            }
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
            Repr::Inline { meta, buf } if *meta & INLINE_ASCII != 0 => {
                buf[..inline_len(*meta)].get(i).map(|&b| b as u16)
            }
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
            Repr::Inline { meta, buf } => CodeUnits::Utf8(inline_str(buf, *meta).encode_utf16()),
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
        !matches!(self.0, Repr::Wtf8(_))
    }
    /// Replace every unpaired surrogate with U+FFFD (`String.prototype.toWellFormed`).
    pub fn to_well_formed(&self) -> JsString {
        match &self.0 {
            Repr::Wtf8(w) => JsString::new(&*w.lossy),
            _ => self.clone(),
        }
    }
    /// The borrowed UTF-8 view IF this is a plain (non-rope, well-formed)
    /// string — O(1), never flattens a rope. `None` for ropes and WTF-8.
    pub fn as_flat_utf8(&self) -> Option<&str> {
        match &self.0 {
            Repr::Inline { meta, buf } => Some(inline_str(buf, *meta)),
            Repr::Utf8(s, _) => Some(s),
            _ => None,
        }
    }

    /// The flat UTF-8 view of a well-formed string, FLATTENING a rope on
    /// first use (the copy is cached in the rope node, so repeated calls —
    /// a kernel's per-access reads — are O(1)). `None` for WTF-8 strings.
    pub fn flatten_utf8(&self) -> Option<&str> {
        match &self.0 {
            Repr::Inline { meta, buf } => Some(inline_str(buf, *meta)),
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
            (
                Repr::Inline { .. } | Repr::Utf8(..) | Repr::Rope(_),
                Repr::Inline { .. } | Repr::Utf8(..) | Repr::Rope(_),
            ) => {
                let (lb, rb) = (self.byte_len(), other.byte_len());
                if lb == 0 {
                    return other.clone();
                }
                if rb == 0 {
                    return self.clone();
                }
                if lb + rb <= INLINE_CAP {
                    // Small + small: assemble inline from the raw well-formed
                    // bytes — no heap, no UTF-8 revalidation.
                    return JsString(inline_from_wf_bytes(self.wtf8_bytes(), other.wtf8_bytes()));
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
        JsString(well_formed_repr(s.as_str()))
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
// The primitive representation (RFC 2195: tag byte + union of C-laid-out
// variant structs) pins the layout the `jit` feature's dense-element fast
// path reads: the tag at byte 0 (`Value::JIT_NUMBER_TAG`) and a `Number`'s
// f64 payload at its natural alignment (`Value::JIT_NUMBER_PAYLOAD_OFFSET`).
// Same 24-byte size as the default representation measured here; the only
// cost is `Option<Value>`'s spare-tag niche (24 → 32 bytes), which appears
// in scalar positions only, never in bulk storage. Discriminants are
// explicit so the JIT contract cannot drift under variant reordering.
#[repr(u8)]
pub enum Value {
    Undefined = 0,
    Null = 1,
    Bool(bool) = 2,
    Number(f64) = 3,
    String(JsString) = 4,
    Symbol(JsSymbol) = 5,
    Object(JsObject) = 6,
    /// The BigInt primitive (arbitrary precision).
    BigInt(Rc<BigInt>) = 7,
    /// Temporal Dead Zone marker: the value stored in a `let`/`const`/`class`
    /// cell after hoisting but before its initializer runs. Reading it (via
    /// `LoadCell`/`LoadUpvalue`) throws a `ReferenceError`; it never escapes into
    /// user-observable positions.
    Uninitialized = 8,
    /// Array hole (elision): the slot in a dense `Internal::Array` for a missing
    /// index, e.g. index 1 of `[0, , 2]`. `HasProperty` is false at a hole and
    /// the iteration/own-key machinery skips it; reading it yields `undefined`
    /// (via the prototype chain). It never escapes into user-observable values.
    Hole = 9,
}

impl Value {
    /// The `jit` tier's raw dense-element contract (see `crate::jit`): the
    /// `#[repr(u8)]` tag value of [`Value::Number`], read by compiled code
    /// to test a dense slot before loading its payload. Must match the
    /// explicit discriminant above; `crate::jit::dense_layout_ok` verifies
    /// the whole contract against a live value before any compiled code
    /// relies on it.
    #[cfg(feature = "jit")]
    pub(crate) const JIT_NUMBER_TAG: u8 = 3;
    /// Byte offset of a [`Value::Number`]'s f64 payload under `#[repr(u8)]`
    /// (the variant struct is `{ tag: u8, payload: f64 }` with C layout, so
    /// the payload sits at the f64's natural alignment).
    #[cfg(feature = "jit")]
    pub(crate) const JIT_NUMBER_PAYLOAD_OFFSET: usize = 8;
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
        let PropertyKey::Str(s) = self else {
            return None;
        };
        // A canonical index starts with an ASCII digit; reject named keys on
        // the raw byte view before materializing the validated `&str` (the
        // prototype walks in `protos_allow_*_index_create` probe every own
        // key of `Array.prototype`-class objects through here).
        if !s.wtf8_bytes().first().is_some_and(u8::is_ascii_digit) {
            return None;
        }
        canonical_index(s.as_str())
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
/// throw `RangeError` rather than allocating (we use dense storage, not
/// sparse). Sized to match V8's fast-array ceiling (32M elements — beyond it
/// V8 switches to dictionary mode, which we don't have): real programs like
/// `new Array(2e6)` or `a[2_000_000] = x` must succeed, while a hostile
/// `new Array(2**32 - 1)` still fails loudly (~768 MiB at 24-byte slots is
/// the worst case a script can demand per allocation).
pub const MAX_DENSE_ARRAY: usize = 1 << 25; // 33.5M elements

/// Upper bound on a single eager string allocation (`repeat`, `padStart`/
/// `padEnd`, concatenation, …). Beyond this, those operations throw
/// `RangeError` *before* allocating, so a hostile/conformance input
/// (`"a".repeat(2**33)`, a doubling loop) cannot OOM the process. Sized near
/// V8's own string ceiling (2^29 - 24 units) so legitimate large strings —
/// multi-hundred-MB JSON payloads, log accumulations — succeed; the unit
/// count also must stay far below `u32::MAX` (see `UNITS_UNKNOWN`).
pub const MAX_STRING_LEN: usize = 1 << 28; // 268M code units

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
        // `has_idx_keys` is exact: a level with no index-keyed own props
        // cannot contain the specific probes below.
        if b.has_idx_keys() {
            if b.own_contains_key(&StrKeyRef(first)) {
                return false;
            }
            for j in 1..count {
                let mut buf = [0u8; 10];
                if b.own_contains_key(&StrKeyRef(fmt_index(idx + j, &mut buf))) {
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
        if b.has_idx_keys() {
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

/// Ordinary own-property storage (see docs/js-object-shapes-design.md §3.1).
///
/// `Shaped` is the append-only common case: the [`Shape`] holds the shared,
/// insertion-ordered key list and `slots[i]` holds the full [`Property`] for
/// the key at chain depth `i`. Keeping whole `Property` values in the slots
/// (rather than bare values) means attribute mutation and accessor
/// properties need NO demotion — the shape encodes key ORDER only, so the
/// only edges that demote are the order-destroying ones (`delete`) and
/// integer-key spam (see [`Shape::can_append`]). `Dict` is today's map,
/// verbatim: the battle-tested path every demoted object falls back to
/// (objects never re-promote), and the birth mode for exotics/intrinsics.
///
/// Mode is unobservable: enumeration order is insertion order in both.
pub(crate) enum PropStorage {
    Shaped {
        shape: Rc<crate::shape::Shape>,
        slots: Vec<Property>,
    },
    Dict(crate::fxhash::FxIndexMap<PropertyKey, Property>),
}

impl Default for PropStorage {
    fn default() -> Self {
        PropStorage::Dict(crate::fxhash::FxIndexMap::default())
    }
}

/// Chain length up to which shaped iteration steps positions in place
/// (`key_at` per step: O(len²) parent hops total, ZERO allocation) rather
/// than pre-collecting the key list. Stringify/own-keys enumerate every
/// object per pass, so the per-object `Vec` was a measurable slice of
/// `json_stringify`; the 2–8 key records that dominate stay alloc-free and
/// genuinely wide objects pay one collect instead of O(len²) hops.
const ITER_WALK_MAX: usize = 16;

/// Iterator over own properties in insertion order, across both storage
/// modes (see [`ObjectData::own_iter`]).
pub enum OwnIter<'a> {
    Dict(indexmap::map::Iter<'a, PropertyKey, Property>),
    /// Positional stepping over a short shaped chain (alloc-free).
    Shaped {
        shape: &'a crate::shape::Shape,
        slots: &'a [Property],
        pos: u32,
    },
    /// Pre-collected keys for a wide shaped chain.
    ShapedWide {
        keys: std::vec::IntoIter<&'a PropertyKey>,
        slots: std::slice::Iter<'a, Property>,
    },
}

impl<'a> Iterator for OwnIter<'a> {
    type Item = (&'a PropertyKey, &'a Property);
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            OwnIter::Dict(it) => it.next(),
            OwnIter::Shaped { shape, slots, pos } => {
                let i = *pos as usize;
                let prop = slots.get(i)?;
                *pos += 1;
                Some((shape.key_at(i as u32).expect("slot in range"), prop))
            }
            OwnIter::ShapedWide { keys, slots } => Some((keys.next()?, slots.next()?)),
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            OwnIter::Dict(it) => it.size_hint(),
            OwnIter::Shaped { slots, pos, .. } => {
                let n = slots.len().saturating_sub(*pos as usize);
                (n, Some(n))
            }
            OwnIter::ShapedWide { keys, .. } => keys.size_hint(),
        }
    }
}

impl ExactSizeIterator for OwnIter<'_> {}

/// Iterator over own property keys in insertion order, across both storage
/// modes (see [`ObjectData::own_keys_iter`]).
pub enum OwnKeys<'a> {
    Dict(indexmap::map::Keys<'a, PropertyKey, Property>),
    /// Positional stepping over a short shaped chain (alloc-free).
    Shaped {
        shape: &'a crate::shape::Shape,
        pos: u32,
        len: u32,
    },
    /// Pre-collected keys for a wide shaped chain.
    ShapedWide(std::vec::IntoIter<&'a PropertyKey>),
}

impl<'a> Iterator for OwnKeys<'a> {
    type Item = &'a PropertyKey;
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            OwnKeys::Dict(it) => it.next(),
            OwnKeys::Shaped { shape, pos, len } => {
                if pos < len {
                    let k = shape.key_at(*pos).expect("slot in range");
                    *pos += 1;
                    Some(k)
                } else {
                    None
                }
            }
            OwnKeys::ShapedWide(it) => it.next(),
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            OwnKeys::Dict(it) => it.size_hint(),
            OwnKeys::Shaped { pos, len, .. } => {
                let n = (*len - *pos) as usize;
                (n, Some(n))
            }
            OwnKeys::ShapedWide(it) => it.size_hint(),
        }
    }
}

impl ExactSizeIterator for OwnKeys<'_> {}

/// Owning iterator over property values (see
/// [`ObjectData::take_props_values`]).
pub enum PropsIntoValues {
    Dict(indexmap::map::IntoValues<PropertyKey, Property>),
    Slots(std::vec::IntoIter<Property>),
}

impl Iterator for PropsIntoValues {
    type Item = Property;
    #[inline]
    fn next(&mut self) -> Option<Property> {
        match self {
            PropsIntoValues::Dict(it) => it.next(),
            PropsIntoValues::Slots(it) => it.next(),
        }
    }
}

pub struct ObjectData {
    pub proto: Option<JsObject>,
    /// The ordinary own-property storage. Private to this module: every other
    /// part of the engine goes through the `own_*` accessor API below, so the
    /// backing representation can vary (shaped vs dictionary) without the
    /// 300+ call sites knowing.
    props: PropStorage,
    pub extensible: bool,
    /// Exactly "some own property key is an array index" — maintained by the
    /// own_* mutators so the prototype-chain guards in
    /// `protos_allow_*_index_create` answer per level in O(1) instead of
    /// probing every own key of `Array.prototype`-class objects. Kept EXACT
    /// (insert sets it, a delete under the flag recounts, bulk installs
    /// scan); drift could only ever be toward `true`, which merely declines
    /// a fast path — never toward an unsound `false`.
    has_idx_keys: bool,
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
            props: PropStorage::default(),
            extensible: true,
            has_idx_keys: false,
            internal,
            privates: None,
        }
    }

    /// A plain object born in shaped mode: `root` is the realm's empty root
    /// shape. Costs nothing until the first insert (no table, no slot vec
    /// allocation).
    pub fn new_shaped(proto: Option<JsObject>, root: Rc<crate::shape::Shape>) -> Self {
        ObjectData {
            proto,
            props: PropStorage::Shaped {
                shape: root,
                slots: Vec::new(),
            },
            extensible: true,
            has_idx_keys: false,
            internal: Internal::Ordinary,
            privates: None,
        }
    }

    /// A plain object born shaped at a KNOWN shape with its slots pre-built
    /// (object-literal template instantiation). `slots.len()` must equal
    /// `shape.len()`.
    pub fn new_shaped_with(
        proto: Option<JsObject>,
        shape: Rc<crate::shape::Shape>,
        slots: Vec<Property>,
        has_idx_keys: bool,
    ) -> Self {
        debug_assert_eq!(slots.len(), shape.len());
        debug_assert_eq!(
            has_idx_keys,
            shape
                .keys_in_order()
                .iter()
                .any(|k| k.array_index().is_some())
        );
        ObjectData {
            proto,
            props: PropStorage::Shaped { shape, slots },
            extensible: true,
            has_idx_keys,
            internal: Internal::Ordinary,
            privates: None,
        }
    }

    pub fn private_get(&self, id: u64) -> Option<&PrivateElement> {
        self.privates.as_ref().and_then(|p| p.get(&id))
    }

    /// Demote to dictionary mode: materialize the map from the shape chain +
    /// slots, preserving insertion order. One-way — a demoted object never
    /// re-promotes (docs §3.4). Idempotent; returns the map for follow-up
    /// mutation.
    fn demote(&mut self) -> &mut crate::fxhash::FxIndexMap<PropertyKey, Property> {
        if matches!(self.props, PropStorage::Shaped { .. }) {
            if let PropStorage::Shaped { shape, slots } = std::mem::take(&mut self.props) {
                let mut map = crate::fxhash::FxIndexMap::with_capacity_and_hasher(
                    slots.len() + 1,
                    Default::default(),
                );
                for (k, p) in shape.keys_in_order().into_iter().zip(slots) {
                    map.insert(k.clone(), p);
                }
                self.props = PropStorage::Dict(map);
            }
        }
        match &mut self.props {
            PropStorage::Dict(m) => m,
            PropStorage::Shaped { .. } => unreachable!("just demoted"),
        }
    }

    // =====================================================================
    // Own-property accessor API (see docs/js-object-shapes-design.md §4,
    // Phase 1). This is the ONLY way the rest of the engine touches the
    // ordinary own-property storage; the map itself is module-private.
    //
    // The API mirrors `IndexMap`'s surface deliberately: the inline caches
    // and the kernel prop machinery depend on stable insertion-order slot
    // indices (`own_get_full` hands them out, `own_get_index[_mut]` uses
    // them), and probes are generic over `Equivalent<PropertyKey>` so the
    // alloc-free `StrKeyRef` guards keep working unchanged.
    // =====================================================================

    /// Whether some own property key is an array index (exact — see the
    /// field's invariant note).
    #[inline]
    pub fn has_idx_keys(&self) -> bool {
        self.has_idx_keys
    }

    /// The own property for `key`, if present.
    #[inline]
    pub fn own_get<Q>(&self, key: &Q) -> Option<&Property>
    where
        Q: ?Sized + std::hash::Hash + indexmap::Equivalent<PropertyKey>,
    {
        match &self.props {
            PropStorage::Dict(m) => m.get(key),
            PropStorage::Shaped { shape, slots } => {
                shape.lookup(key).map(|slot| &slots[slot as usize])
            }
        }
    }

    /// Mutable access to the own property for `key`, if present.
    #[inline]
    pub fn own_get_mut<Q>(&mut self, key: &Q) -> Option<&mut Property>
    where
        Q: ?Sized + std::hash::Hash + indexmap::Equivalent<PropertyKey>,
    {
        match &mut self.props {
            PropStorage::Dict(m) => m.get_mut(key),
            PropStorage::Shaped { shape, slots } => {
                shape.lookup(key).map(|slot| &mut slots[slot as usize])
            }
        }
    }

    /// The own property for `key` with its insertion-order slot index (the
    /// index the inline caches and kernel prop localization cache; it is
    /// stable for the property's lifetime).
    #[inline]
    pub fn own_get_full<Q>(&self, key: &Q) -> Option<(usize, &PropertyKey, &Property)>
    where
        Q: ?Sized + std::hash::Hash + indexmap::Equivalent<PropertyKey>,
    {
        match &self.props {
            PropStorage::Dict(m) => m.get_full(key),
            PropStorage::Shaped { shape, slots } => {
                let (slot, k) = shape.lookup_full(key)?;
                Some((slot as usize, k, &slots[slot as usize]))
            }
        }
    }

    /// Mutable variant of [`Self::own_get_full`].
    #[inline]
    pub fn own_get_full_mut<Q>(&mut self, key: &Q) -> Option<(usize, &PropertyKey, &mut Property)>
    where
        Q: ?Sized + std::hash::Hash + indexmap::Equivalent<PropertyKey>,
    {
        match &mut self.props {
            PropStorage::Dict(m) => m.get_full_mut(key),
            PropStorage::Shaped { shape, slots } => {
                let (slot, k) = shape.lookup_full(key)?;
                Some((slot as usize, k, &mut slots[slot as usize]))
            }
        }
    }

    /// Whether an own property for `key` exists.
    #[inline]
    pub fn own_contains_key<Q>(&self, key: &Q) -> bool
    where
        Q: ?Sized + std::hash::Hash + indexmap::Equivalent<PropertyKey>,
    {
        match &self.props {
            PropStorage::Dict(m) => m.contains_key(key),
            PropStorage::Shaped { shape, .. } => shape.lookup(key).is_some(),
        }
    }

    /// Insert (or replace, preserving insertion order) the own property for
    /// `key`, returning the previous property if any — `IndexMap::insert`
    /// semantics. Spec checks (extensibility, writability) are the CALLER's
    /// job, exactly as with the raw map.
    #[inline]
    pub fn own_insert(&mut self, key: PropertyKey, prop: Property) -> Option<Property> {
        self.has_idx_keys |= key.array_index().is_some();
        match &mut self.props {
            PropStorage::Dict(m) => return m.insert(key, prop),
            PropStorage::Shaped { shape, slots } => {
                if let Some(slot) = shape.lookup(&key) {
                    // Replacing at an existing key keeps slot order in both
                    // modes; the shape is unchanged.
                    return Some(std::mem::replace(&mut slots[slot as usize], prop));
                }
                if crate::shape::Shape::can_append(&key, slots.len()) {
                    let next = shape.transition(key);
                    *shape = next;
                    slots.push(prop);
                    return None;
                }
            }
        }
        // Shaped, but the key must not join the transition tree
        // (integer-index spam): fall to dictionary mode.
        self.demote().insert(key, prop)
    }

    /// Remove the own property for `key`, preserving the insertion order of
    /// the remaining properties (`shift_remove` semantics — enumeration
    /// order is observable).
    #[inline]
    pub fn own_remove<Q>(&mut self, key: &Q) -> Option<Property>
    where
        Q: ?Sized + std::hash::Hash + indexmap::Equivalent<PropertyKey>,
    {
        if let PropStorage::Shaped { shape, .. } = &self.props {
            // Deleting an absent key changes nothing: stay shaped. A hit
            // shifts the slot indices of everything after it — the one edge
            // shapes cannot represent — so demote first (docs §3.4).
            shape.lookup(key)?;
            self.demote();
        }
        let removed = match &mut self.props {
            PropStorage::Dict(m) => m.shift_remove(key),
            PropStorage::Shaped { .. } => unreachable!("demoted above"),
        };
        if self.has_idx_keys && removed.is_some() {
            // Deletes are rare and already O(n) (shift_remove); keep the
            // flag exact by recounting.
            self.has_idx_keys = self.own_keys_iter().any(|k| k.array_index().is_some());
        }
        removed
    }

    /// The current shape, when the storage is shaped (`None` in dictionary
    /// mode). Shape identity (`Rc::ptr_eq`) pins the whole key layout — the
    /// basis of the (shape, slot) inline caches (docs §3.3).
    #[inline]
    pub fn own_shape(&self) -> Option<&Rc<crate::shape::Shape>> {
        match &self.props {
            PropStorage::Shaped { shape, .. } => Some(shape),
            PropStorage::Dict(_) => None,
        }
    }

    /// The property at slot `i` WITHOUT retrieving its key — for
    /// shape-verified cache hits, where shape identity already pins the key
    /// at every slot.
    #[inline]
    pub fn own_prop_at(&self, i: usize) -> Option<&Property> {
        match &self.props {
            PropStorage::Shaped { slots, .. } => slots.get(i),
            PropStorage::Dict(m) => m.get_index(i).map(|(_, p)| p),
        }
    }

    /// Mutable variant of [`Self::own_prop_at`].
    #[inline]
    pub fn own_prop_at_mut(&mut self, i: usize) -> Option<&mut Property> {
        match &mut self.props {
            PropStorage::Shaped { slots, .. } => slots.get_mut(i),
            PropStorage::Dict(m) => m.get_index_mut(i).map(|(_, p)| p),
        }
    }

    /// The own property at insertion-order slot `i` (IC/kernel verification:
    /// callers compare the returned key against the expected one).
    #[inline]
    pub fn own_get_index(&self, i: usize) -> Option<(&PropertyKey, &Property)> {
        match &self.props {
            PropStorage::Dict(m) => m.get_index(i),
            PropStorage::Shaped { shape, slots } => Some((shape.key_at(i as u32)?, slots.get(i)?)),
        }
    }

    /// Mutable variant of [`Self::own_get_index`].
    #[inline]
    pub fn own_get_index_mut(&mut self, i: usize) -> Option<(&PropertyKey, &mut Property)> {
        match &mut self.props {
            PropStorage::Dict(m) => m.get_index_mut(i),
            PropStorage::Shaped { shape, slots } => {
                Some((shape.key_at(i as u32)?, slots.get_mut(i)?))
            }
        }
    }

    /// Number of own properties in the ordinary storage.
    #[inline]
    pub fn own_len(&self) -> usize {
        match &self.props {
            PropStorage::Dict(m) => m.len(),
            PropStorage::Shaped { slots, .. } => slots.len(),
        }
    }

    /// `true` when the ordinary own-property storage is empty — the guard
    /// Whether this object's own storage provably lacks a `toJSON` key —
    /// `Shaped` answers from the shape's immutable cache, `Dict` does one
    /// hash probe. (The JSON stringifier's per-node fast path.)
    #[inline]
    pub fn own_lacks_to_json(&self) -> bool {
        match &self.props {
            PropStorage::Dict(m) => m.get(&PropertyKey::str("toJSON")).is_none(),
            PropStorage::Shaped { shape, .. } => shape.lacks_to_json(),
        }
    }

    /// every dense-array/exotic fast path checks ("nothing reified can
    /// shadow").
    #[inline]
    pub fn own_is_empty(&self) -> bool {
        match &self.props {
            PropStorage::Dict(m) => m.is_empty(),
            PropStorage::Shaped { slots, .. } => slots.is_empty(),
        }
    }

    /// Iterate own properties in insertion order.
    #[inline]
    pub fn own_iter(&self) -> OwnIter<'_> {
        match &self.props {
            PropStorage::Dict(m) => OwnIter::Dict(m.iter()),
            PropStorage::Shaped { shape, slots } if slots.len() <= ITER_WALK_MAX => {
                OwnIter::Shaped {
                    shape,
                    slots,
                    pos: 0,
                }
            }
            PropStorage::Shaped { shape, slots } => OwnIter::ShapedWide {
                keys: shape.keys_in_order().into_iter(),
                slots: slots.iter(),
            },
        }
    }

    /// Iterate own property keys in insertion order.
    #[inline]
    pub fn own_keys_iter(&self) -> OwnKeys<'_> {
        match &self.props {
            PropStorage::Dict(m) => OwnKeys::Dict(m.keys()),
            PropStorage::Shaped { shape, slots } if slots.len() <= ITER_WALK_MAX => {
                OwnKeys::Shaped {
                    shape,
                    pos: 0,
                    len: slots.len() as u32,
                }
            }
            PropStorage::Shaped { shape, .. } => {
                OwnKeys::ShapedWide(shape.keys_in_order().into_iter())
            }
        }
    }

    /// Pre-size the storage for `n` additional properties.
    #[inline]
    pub fn own_reserve(&mut self, n: usize) {
        match &mut self.props {
            PropStorage::Dict(m) => m.reserve(n),
            PropStorage::Shaped { slots, .. } => slots.reserve(n),
        }
    }

    /// Allocated capacity of the storage (0 ⇔ nothing allocated yet).
    #[inline]
    pub fn own_capacity(&self) -> usize {
        match &self.props {
            PropStorage::Dict(m) => m.capacity(),
            PropStorage::Shaped { slots, .. } => slots.capacity(),
        }
    }

    /// Drop every own property (GC edge-clearing).
    #[inline]
    pub fn own_clear(&mut self) {
        self.has_idx_keys = false;
        match &mut self.props {
            PropStorage::Dict(m) => m.clear(),
            PropStorage::Shaped { shape, slots } => {
                slots.clear();
                *shape = shape.root();
            }
        }
    }

    /// Replace the whole storage with a pre-built map (object-literal
    /// template instantiation).
    #[inline]
    pub fn set_props_map(&mut self, map: crate::fxhash::FxIndexMap<PropertyKey, Property>) {
        self.has_idx_keys = map.keys().any(|k| k.array_index().is_some());
        self.props = PropStorage::Dict(map);
    }

    /// Take every own property VALUE, leaving the storage empty (teardown /
    /// cycle breaking — the keys are not needed to break value edges, and a
    /// shaped object must not pay a key-cloning dictionary materialization
    /// just to be dropped).
    #[inline]
    pub fn take_props_values(&mut self) -> PropsIntoValues {
        self.has_idx_keys = false;
        match std::mem::take(&mut self.props) {
            PropStorage::Dict(m) => PropsIntoValues::Dict(m.into_values()),
            PropStorage::Shaped { slots, .. } => PropsIntoValues::Slots(slots.into_iter()),
        }
    }
    /// Does `props` hold a (reified) entry for the array index `idx`?
    /// Alloc-free: the index is formatted into a stack buffer and probed via
    /// [`StrKeyRef`], so hot fast-path guards can call this per element.
    pub fn has_index_prop(&self, idx: u32) -> bool {
        if self.own_is_empty() {
            return false;
        }
        let mut buf = [0u8; 10];
        self.own_contains_key(&StrKeyRef(fmt_index(idx, &mut buf)))
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

    /// An Array exotic object's `length`.
    ///
    /// `length` is normally DERIVED from the dense backing store, but it is
    /// reified into `props` whenever it can no longer be: a non-writable
    /// `length` (freeze/`defineProperty`) records its attributes there, and a
    /// `length` past the dense-storage ceiling records the SPARSE TAIL — the
    /// indices in `[dense.len(), length)`, which live in `props` (or nowhere,
    /// as holes) rather than in the vec. The reified entry always wins.
    pub fn array_length(&self) -> u32 {
        let dense = match &self.internal {
            Internal::Array(arr) => arr.len() as u32,
            _ => return 0,
        };
        // No props at all (the overwhelmingly common array) => derived. This
        // early-out keeps the generic index-write path free of a key
        // allocation + map probe.
        if self.own_is_empty() {
            return dense;
        }
        if let Some(p) = self.own_get(&PropertyKey::str("length")) {
            if let Some(Value::Number(n)) = p.value() {
                return *n as u32;
            }
        }
        dense
    }

    /// True when the array's whole `[0, length)` range is held in the dense
    /// backing store — i.e. there is no sparse tail. Every dense fast path
    /// that treats `arr.len()` as `length` requires this.
    pub fn array_is_dense(&self) -> bool {
        match &self.internal {
            Internal::Array(arr) => {
                self.own_is_empty() || self.array_length() as usize == arr.len()
            }
            _ => false,
        }
    }

    /// Reify (or clear) the `length` entry so it reports `len` with `writable`.
    /// Clearing is only possible when `len` is exactly the dense count and
    /// `length` stays writable — otherwise the entry is what carries the value.
    fn array_reify_length(&mut self, len: u32, writable: bool) {
        let dense = match &self.internal {
            Internal::Array(arr) => arr.len(),
            _ => 0,
        };
        if writable && len as usize == dense {
            // Derived again: drop any reified entry (nothing to drop when the
            // map is empty — the common `arr.length = n` shrink).
            if !self.own_is_empty() {
                self.own_remove(&PropertyKey::str("length"));
            }
        } else {
            self.own_insert(
                PropertyKey::str("length"),
                Property {
                    kind: PropertyKind::Data {
                        value: Value::Number(len as f64),
                        writable,
                    },
                    enumerable: false,
                    configurable: false,
                },
            );
        }
    }

    /// The storage half of `ArraySetLength`: resize the dense store, delete the
    /// index properties at or past `new_len` (the spec's "every property whose
    /// name is an array index not smaller than the new length is deleted"),
    /// and record the resulting `length`/`writable` state.
    pub fn array_set_length(&mut self, new_len: u32, writable: bool) {
        let n = new_len as usize;
        if let Internal::Array(arr) = &mut self.internal {
            if n < arr.len() {
                arr.truncate(n);
            } else if n <= MAX_DENSE_ARRAY {
                // Growing introduces HOLES, not undefined slots.
                arr.resize(n, Value::Hole);
            }
            // n > MAX_DENSE_ARRAY: the tail past `arr.len()` stays sparse.
        }
        if !self.own_is_empty() {
            let dropped: Vec<PropertyKey> = self
                .own_keys_iter()
                .filter(|k| k.array_index().is_some_and(|i| i >= new_len))
                .cloned()
                .collect();
            for k in dropped {
                self.own_remove(&k);
            }
        }
        self.array_reify_length(new_len, writable);
    }

    /// Grow an array's `length` to cover a newly created index, keeping the
    /// current `writable` state. No-op when `length` already covers it.
    pub fn array_grow_length(&mut self, needed: u32) {
        // With no props, `length` IS the dense count and the caller has
        // already grown the backing store to cover the index.
        if self.own_is_empty() || needed <= self.array_length() {
            return;
        }
        let writable = self
            .own_get(&PropertyKey::str("length"))
            .map(|p| matches!(&p.kind, PropertyKind::Data { writable, .. } if *writable))
            .unwrap_or(true);
        self.array_reify_length(needed, writable);
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
            Internal::IteratorHelper(_) => "Object",
            Internal::RegExpStringIterator(_) => "Object",
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
    /// An Iterator Helper (`Iterator.prototype.map/filter/take/drop/flatMap`
    /// result) or an `Iterator.from` wrapper: a generator-like object driving
    /// an underlying iterator record through one transformation.
    IteratorHelper(Box<IteratorHelperData>),
    /// A RegExp String Iterator (`String.prototype.matchAll` /
    /// `RegExp.prototype[@@matchAll]` result). The brand
    /// %RegExpStringIteratorPrototype%.next checks for.
    RegExpStringIterator(Box<RegExpStringIterData>),
}

/// State backing an `Internal::RegExpStringIterator` object (spec
/// CreateRegExpStringIterator's internal slots).
pub struct RegExpStringIterData {
    /// The matcher object ([[IteratingRegExp]]).
    pub matcher: Value,
    /// The subject string ([[IteratedString]]).
    pub string: JsString,
    pub global: bool,
    pub unicode: bool,
    pub done: bool,
}

/// State backing an `Internal::IteratorHelper` object.
pub struct IteratorHelperData {
    /// Underlying iterator record: the iterator object and its `next` method
    /// captured at helper creation (GetIteratorDirect).
    pub iter: Value,
    pub next: Value,
    /// Helper completed (a done result was produced, an error unwound it, or
    /// `return()` closed it).
    pub done: bool,
    /// Re-entrancy guard — resuming a helper from inside its own callback
    /// throws, matching generator semantics.
    pub running: bool,
    /// Zero-based count of values taken from the underlying iterator, passed
    /// as the second callback argument for map/filter/flatMap.
    pub counter: f64,
    pub kind: HelperKind,
}

/// Which transformation an iterator helper applies.
pub enum HelperKind {
    /// `map(mapper)`.
    Map(Value),
    /// `filter(predicate)`.
    Filter(Value),
    /// `take(limit)`: values remaining to yield (integer or +∞).
    Take(f64),
    /// `drop(limit)`: values still to skip (integer or +∞).
    Drop(f64),
    /// `flatMap(mapper)`: `inner` is the live inner iterator record, if any.
    FlatMap {
        mapper: Value,
        inner: Option<(Value, Value)>,
    },
    /// `Iterator.from` wrap: forwards `next`/`return` to the record verbatim.
    Wrap,
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
    /// Lazily-built UTF-16 view of `string` for `StringChars` stepping.
    /// Strings are immutable, so this is a pure cache: without it every
    /// `next()` re-converts the whole string and iteration goes quadratic.
    /// Not snapshotted — a decoded iterator rebuilds it on first step.
    pub string_units: Option<std::rc::Rc<Vec<u16>>>,
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
