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

/// An interned-ish JS string. We start with `Rc<str>` (cheap clone, value
/// equality); ropes/atoms are a later optimization.
#[derive(Clone)]
pub struct JsString(pub Rc<str>);

impl JsString {
    pub fn new(s: impl AsRef<str>) -> Self {
        JsString(Rc::from(s.as_ref()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl PartialEq for JsString {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl Eq for JsString {}
impl std::hash::Hash for JsString {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state)
    }
}
impl fmt::Debug for JsString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", &self.0)
    }
}
impl From<&str> for JsString {
    fn from(s: &str) -> Self {
        JsString::new(s)
    }
}
impl From<String> for JsString {
    fn from(s: String) -> Self {
        JsString(Rc::from(s.as_str()))
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

/// A property key: string or symbol. Integer-index keys are stored as their
/// string form; enumeration re-derives integer ordering.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum PropertyKey {
    Str(JsString),
    Sym(JsSymbol),
}

impl PropertyKey {
    pub fn str(s: impl AsRef<str>) -> PropertyKey {
        PropertyKey::Str(JsString::new(s))
    }
    pub fn from_index(i: u32) -> PropertyKey {
        PropertyKey::Str(JsString::new(i.to_string()))
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
    pub props: IndexMap<PropertyKey, Property>,
    pub extensible: bool,
    pub internal: Internal,
}

impl ObjectData {
    pub fn new(proto: Option<JsObject>, internal: Internal) -> Self {
        ObjectData {
            proto,
            props: IndexMap::new(),
            extensible: true,
            internal,
        }
    }
    pub fn is_callable(&self) -> bool {
        matches!(self.internal, Internal::Function(_))
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
            Internal::Arguments => "Arguments",
            Internal::Iterator(_) => "Iterator",
            Internal::ArrayBuffer(_) => "ArrayBuffer",
            Internal::TypedArray(_) => "TypedArray",
            Internal::DataView(_) => "DataView",
            Internal::BigIntObj(_) => "BigInt",
            Internal::Proxy(_) => "Proxy",
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
    Map(IndexMap<MapKey, Value>),
    Set(IndexMap<MapKey, ()>),
    /// WeakMap/WeakSet. Our GC is reference-counting with no weak references, so
    /// these hold strong refs — observationally identical for all of Test262
    /// (which cannot force collection); only `WeakRef`/`FinalizationRegistry`
    /// expose collection and remain unsupported (determinism contract).
    WeakMap(IndexMap<MapKey, Value>),
    WeakSet(IndexMap<MapKey, ()>),
    Promise(crate::vm::PromiseData),
    Generator(crate::vm::GeneratorData),
    Date(f64),
    Arguments,
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
}

/// Backing slots for a Proxy exotic object.
pub struct ProxyData {
    pub target: JsObject,
    pub handler: JsObject,
    pub revoked: bool,
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
    Bytecode(BytecodeFunction),
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
