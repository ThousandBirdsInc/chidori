//! Bytecode definitions: the compact instruction set the compiler lowers the
//! oxc AST into, and `FuncProto`, the per-function compiled artifact.
//!
//! The VM is a stack machine: most opcodes consume operands from and push
//! results to a per-frame operand stack. Per-frame operand stacks (rather than
//! one shared stack) make generator/async frame suspension trivial — a suspended
//! frame is just its `{ip, locals, stack}` frozen in memory, never serialized.

use std::rc::Rc;

/// A compile-time constant referenced by index from `OpLoadConst`.
#[derive(Clone, Debug)]
pub enum Const {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    String(Rc<str>),
    /// A nested function template (compiled). `OpClosure` instantiates it.
    Func(Rc<FuncProto>),
    /// A `BigInt` literal stored as its decimal string (parsed on load).
    BigInt(Rc<str>),
}

/// How a free variable referenced by a nested function is captured at closure
/// creation time.
#[derive(Clone, Copy, Debug)]
pub enum UpvalueSource {
    /// Capture cell slot `index` from the *enclosing* frame's cell array.
    ParentCell(u32),
    /// Capture upvalue `index` from the *enclosing* function's upvalue array
    /// (transitive capture).
    ParentUpvalue(u32),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FuncKind {
    Normal,
    Arrow,
    Method,
    Getter,
    Setter,
    Async,
    AsyncArrow,
    AsyncMethod,
    Generator,
    GeneratorMethod,
    AsyncGenerator,
    AsyncGeneratorMethod,
    ClassCtor,
}

impl FuncKind {
    pub fn is_async(self) -> bool {
        matches!(
            self,
            FuncKind::Async
                | FuncKind::AsyncArrow
                | FuncKind::AsyncMethod
                | FuncKind::AsyncGenerator
                | FuncKind::AsyncGeneratorMethod
        )
    }
    pub fn is_generator(self) -> bool {
        matches!(
            self,
            FuncKind::Generator
                | FuncKind::GeneratorMethod
                | FuncKind::AsyncGenerator
                | FuncKind::AsyncGeneratorMethod
        )
    }
    pub fn is_arrow(self) -> bool {
        matches!(self, FuncKind::Arrow | FuncKind::AsyncArrow)
    }
    /// Concise methods / accessors are callable but not constructors.
    pub fn is_method(self) -> bool {
        matches!(
            self,
            FuncKind::Method
                | FuncKind::Getter
                | FuncKind::Setter
                | FuncKind::AsyncMethod
                | FuncKind::GeneratorMethod
                | FuncKind::AsyncGeneratorMethod
        )
    }
}

/// A compiled function (or the top-level script, which compiles as a function).
#[derive(Debug)]
pub struct FuncProto {
    pub name: String,
    pub code: Vec<Op>,
    pub consts: Vec<Const>,
    /// Number of plain (non-captured) local slots.
    pub num_locals: u32,
    /// Number of cell (captured-by-closure) slots.
    pub num_cells: u32,
    /// Declared positional parameter count (for `Function.prototype.length`).
    pub num_params: u32,
    /// Whether the last param is a rest param.
    pub has_rest: bool,
    /// Capture descriptors used by `OpClosure` to build a child's upvalues.
    pub upvalues: Vec<UpvalueSource>,
    pub kind: FuncKind,
    /// Source span for stack traces (start byte offset).
    pub source_start: u32,
    /// Whether this function references `arguments`.
    pub uses_arguments: bool,
    /// Names of the positional params, for `arguments`/debug.
    pub param_names: Vec<String>,
    /// Strict-mode code: assignment to a non-writable / setter-less / non-existent
    /// property of a non-extensible object (and to a primitive) throws a TypeError
    /// rather than silently failing (PutValue with Throw=true).
    pub is_strict: bool,
    /// Cell indices that must be STABLE: `InitCell`/`InitCellTdz` mutate them in
    /// place instead of replacing the `Rc`. Used for a module body's top-level
    /// bindings so the linker can pre-allocate them, wire imports to the
    /// exporter's cell, and have the body fill them without breaking the link.
    /// Empty for ordinary scripts/functions (default replace-the-`Rc` semantics).
    pub stable_cells: Vec<u32>,
}

impl FuncProto {
    pub fn empty(name: &str, kind: FuncKind) -> FuncProto {
        FuncProto {
            name: name.to_string(),
            code: Vec::new(),
            consts: Vec::new(),
            num_locals: 0,
            num_cells: 0,
            num_params: 0,
            has_rest: false,
            upvalues: Vec::new(),
            kind,
            source_start: 0,
            uses_arguments: false,
            param_names: Vec::new(),
            is_strict: false,
            stable_cells: Vec::new(),
        }
    }
}

/// A local binding slot — either a plain stack slot or a heap cell (captured).
#[derive(Clone, Copy, Debug)]
pub enum Slot {
    Local(u32),
    Cell(u32),
}

/// The instruction set. Jump targets are absolute indices into `code`, patched
/// by the compiler.
#[derive(Clone, Debug)]
pub enum Op {
    // ---- constants / literals ----
    LoadConst(u32),
    LoadUndefined,
    /// Push an array hole sentinel (`Value::Hole`) for an elision in an array
    /// literal. Stored directly into the dense backing Vec by `NewArray`.
    LoadHole,
    LoadNull,
    LoadTrue,
    LoadFalse,
    LoadThis,
    /// `RequireObjectCoercible`: throw a TypeError if the top of stack is
    /// `null`/`undefined`, otherwise leave it in place. Emitted at the start of
    /// object-pattern destructuring (which must reject a nullish source even when
    /// the pattern is empty).
    RequireObjectCoercible,
    /// `OrdinaryCallBindThis` for a sloppy (non-strict) function: replace the
    /// top-of-stack `this` with the global object when it is `undefined`/`null`,
    /// or `ToObject(this)` when it is a primitive. (Strict functions skip this.)
    BindThisSloppy,
    /// Push `new.target` (undefined unless in a [[Construct]] call).
    LoadNewTarget,
    /// Push positional argument `i` (undefined if not supplied).
    LoadArg(u32),
    /// Push an array of arguments from index `n` onward (rest parameter).
    LoadRestArgs(u32),

    // ---- locals / cells / upvalues / globals ----
    LoadLocal(u32),
    StoreLocal(u32),
    LoadCell(u32),
    StoreCell(u32),
    /// As [`StoreCell`], but throws a ReferenceError if the cell is still in the
    /// Temporal Dead Zone (assignment to a `let`/`const`/`class` binding before
    /// its initializer runs). Emitted for assignment *expressions* only; binding
    /// *initialization* keeps using `StoreCell`/`InitCell`.
    StoreCellChecked(u32),
    /// Initialize a fresh cell slot with the top of stack (used when entering a
    /// scope / per-iteration binding).
    InitCell(u32),
    /// Initialize a fresh cell slot to the Temporal Dead Zone marker (a hoisted
    /// `let`/`const`/`class` binding); reading it before its initializer throws.
    InitCellTdz(u32),
    LoadUpvalue(u32),
    StoreUpvalue(u32),
    /// As [`StoreUpvalue`], but throws a ReferenceError if the captured cell is
    /// still in the Temporal Dead Zone (assignment-expression path only).
    StoreUpvalueChecked(u32),
    LoadGlobal(u32),  // const index of name
    StoreGlobal(u32), // const index of name
    /// Declare a global var/function binding (const index of name).
    DeclareGlobal(u32),
    /// Throw ReferenceError if the named global is not defined (TDZ-ish for
    /// `typeof`-safe reads we use LoadGlobalOrUndefined instead).
    LoadGlobalTypeof(u32),

    // ---- `with` statement scope chain ----
    /// Pop the top of stack (an object, after ToObject) and push it as a new
    /// dynamic `with` scope on the frame's with-scope stack.
    PushWithScope,
    /// Pop the innermost `with` scope off the frame's with-scope stack.
    PopWithScope,
    /// Dynamic identifier read inside a `with` block. `name` is the const index
    /// of the identifier. At runtime, the with-scope stack is consulted
    /// (innermost first, honoring @@unscopables); if a scope HasProperty(name)
    /// the value is read from it, otherwise `fallback` (the static
    /// Load{Cell,Upvalue,Global,GlobalTypeof}) is executed instead.
    LoadName {
        name: u32,
        fallback: Box<Op>,
    },
    /// Dynamic identifier write inside a `with` block. Consumes the value on top
    /// of the stack. If a with-scope HasProperty(name) the value is written to
    /// it; otherwise `fallback` (the static Store{Cell,Upvalue,Global}) runs.
    StoreName {
        name: u32,
        fallback: Box<Op>,
    },
    /// `delete name` inside a `with` block. If a with-scope HasProperty(name),
    /// deletes the property from that object and pushes the result; otherwise
    /// pushes `true` (deleting an unresolvable/lexical reference is reported as
    /// success in sloppy mode).
    DeleteName(u32),
    /// Resolve the Reference base for `name` ONCE (spec: compound assignment /
    /// update expressions evaluate the LHS reference a single time). Pushes the
    /// with-object holding `name` per the current with-scope chain, or
    /// `undefined` when the name resolves statically. The captured base is then
    /// consumed by `LoadFromBase`/`StoreToBase`, so a getter/RHS side effect
    /// that deletes the binding can't redirect the later read/write.
    ResolveNameBase(u32),
    /// `[base] -> [value]`: object-environment GetBindingValue against a base
    /// captured by `ResolveNameBase`; a non-object base runs `fallback` (the
    /// static Load{Cell,Upvalue,Global}).
    LoadFromBase {
        name: u32,
        fallback: Box<Op>,
    },
    /// `[base, value] -> []`: PutValue against a captured base; a non-object
    /// base re-pushes the value and runs `fallback` (the static checked store).
    StoreToBase {
        name: u32,
        fallback: Box<Op>,
    },
    /// Pop a base value and throw TypeError if it is null/undefined
    /// (RequireObjectCoercible, run before a computed key's ToPropertyKey).
    RequireCoercible,
    /// Peek the top of stack and throw TypeError unless it is an Object —
    /// the iterator protocol's "result must be an Object" check.
    RequireIterResult,
    /// Mark the most recently pushed try-handler as a `yield*` delegation
    /// handler (see `TryHandler::delegation`).
    MarkDelegationHandler,

    // ---- private class elements ----
    /// Brand-checked private METHOD/ACCESSOR read: `[obj] -> [value]`. The
    /// receiver must OWN the `brand` key (instances are branded at
    /// construction); the element itself is then read through the prototype
    /// chain (methods/accessors live on the class prototype under `key`).
    PrivateGetB {
        brand: u32,
        key: u32,
    },
    /// Brand-checked private METHOD/ACCESSOR write: `[obj, value] -> [value]`.
    /// A method is never writable (TypeError); an accessor routes through its
    /// setter (TypeError when it has none).
    PrivateSetB {
        brand: u32,
        key: u32,
        is_method: bool,
    },
    /// `#x in obj`: `[obj] -> [bool]` — whether obj OWNS the key (a field's
    /// storage key, or a method/accessor's brand key).
    PrivateHasOwn(u32),
    /// `[superResult, superCtor] -> []`: constructor return-override — when
    /// the parent is a BYTECODE constructor (a JS class/function) that
    /// returned an object, substitute it as `this`; the `%this` cell (index
    /// payload) is updated IN PLACE so closures that captured it before
    /// `super()` observe the adopted object. Native parents (Error, Object,
    /// Set, …) initialize the pre-created `this` via the super_target pattern
    /// instead, so their call-handler results stay discarded.
    AdoptSuperThis(u32),

    // ---- stack manipulation ----
    Pop,
    Dup,
    /// Duplicate the value 1 below the top (a b -> a a b ... actually a b -> b a b)
    Swap,
    /// Rotate: bring stack[len-3] to top ( a b c -> b c a ).
    Rot3,

    // ---- objects / arrays ----
    NewObject,
    NewArray(u32), // number of initial elements popped
    /// Push a hole into array-literal construction (for elisions).
    ArrayPushElision,
    /// Spread the iterable on top into the array being built (array literal).
    ArraySpread,
    /// obj key value -> ; defines an own (enumerable) data property.
    DefineField,
    /// obj key value -> ; defines a non-enumerable data property (class methods).
    DefineMethod,
    /// obj key getter -> ; defines an enumerable getter (object literals).
    DefineGetter,
    /// obj key setter -> ; defines an enumerable setter (object literals).
    DefineSetter,
    /// obj key getter -> ; defines a non-enumerable getter (class accessors).
    DefineMethodGetter,
    /// obj key setter -> ; defines a non-enumerable setter (class accessors).
    DefineMethodSetter,
    /// `[obj, key, value]` unchanged on the stack; sets the value closure's
    /// [[HomeObject]] to `obj` (MakeMethod). Emitted for object-literal concise
    /// methods/accessors so their `super.prop` resolves against the object.
    SetHomeObject,
    /// `super.prop` read in an object method: pushes
    /// `getPrototypeOf([[HomeObject]])[name]` with `this` as the get receiver.
    GetSuperProp(u32),
    /// `super[expr]` read: pops the key, then like [`GetSuperProp`].
    GetSuperPropDynamic,
    /// Spread source object into target ( target source -> target ).
    ObjectSpread,
    GetProp(u32), // const index of name; obj -> value
    /// Private field/method read `obj.#x`: brand-checks (the object must have the
    /// private name in its chain, else a TypeError) then reads it. obj -> value.
    PrivateGet(u32),
    SetProp(u32), // const index of name; obj value -> value
    /// Private field write `obj.#x = v`: brand-checks then writes. obj value -> value.
    PrivateSet(u32),
    GetPropDynamic, // obj key -> value
    SetPropDynamic, // obj key value -> value
    /// Delete: obj key -> bool
    DeleteProp(u32),
    DeletePropDynamic,
    /// `in` operator: key obj -> bool
    HasProp,
    /// optional-chain short-circuit: if top is nullish, jump (leaving undefined).
    JumpIfNullish(u32),

    // ---- calls ----
    /// func this argc -> result. Args are on the stack above `this`.
    Call(u32),
    /// func argc -> result (this = undefined). Args on the stack above func.
    CallMethodless(u32),
    /// Construct: ctor argc -> instance.
    New(u32),
    /// Call with a spread-collected argument array: func this argsArray -> result
    CallSpread,
    NewSpread,
    /// super(...) call inside a derived constructor: argc on stack.
    SuperCall(u32),
    SuperCallSpread,
    Return,
    /// Implicit return of `this`/last completion for scripts.
    ReturnUndefined,

    // ---- closures ----
    Closure(u32), // const index of FuncProto

    // ---- arithmetic / unary ----
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Neg,
    Pos,
    /// ToNumeric coercion (keeps BigInt; used by `++`/`--` for the old value).
    ToNumeric,
    /// Throw a `TypeError` for assignment to a `const` binding.
    ThrowConstAssign,
    /// `import(specifier)`: pops the (already-evaluated) specifier and pushes a
    /// Promise. Module loading is unsupported, so the Promise is rejected — but
    /// returning a real Promise lets `import(x).then(...)`/`.catch(...)` work.
    DynamicImport,
    BitNot,
    Not,
    Inc,
    Dec,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    UShr,
    TypeofExpr, // typeof of value on stack

    // ---- comparison ----
    Eq, // ==
    Ne, // !=
    StrictEq,
    StrictNe,
    Lt,
    Le,
    Gt,
    Ge,
    InstanceOf,

    // ---- control flow ----
    Jump(u32),
    JumpIfTrue(u32),
    JumpIfFalse(u32),
    /// Pop; jump if falsy but leave the value if truthy (for `&&`). Actually we
    /// implement `&&`/`||`/`??` with peek-based jumps below.
    JumpIfFalsyPeek(u32), // peek top; if falsy jump (keep), else pop
    JumpIfTruthyPeek(u32),
    JumpIfNullishPeek(u32),

    // ---- exceptions ----
    Throw,
    /// Push a try handler: catch target, finally target (u32::MAX if none).
    PushTryHandler {
        catch: u32,
        finally: u32,
    },
    PopTryHandler,
    /// `break`/`continue` that crosses one or more enclosing `finally` regions:
    /// park a `Jump` completion and run the crossed finallys (down to the target
    /// loop's handler depth `boundary`) before jumping to `target`.
    CompletionJump {
        target: u32,
        boundary: u32,
    },
    /// End of a `finally` body: if a non-local completion is parked, resume it
    /// (run the next outer finally, or perform return/throw/break/continue);
    /// otherwise the finalizer ran on the normal path and execution falls through.
    EndFinally,

    // ---- iteration ----
    /// Get iterator from iterable on stack (calls Symbol.iterator).
    GetIterator,
    GetAsyncIterator,
    /// iterator -> iterator done? value pushed; advances. Pushes (value, done).
    /// We implement as: IteratorNext leaves [iterator] and pushes result obj;
    /// the compiler then reads .done/.value.
    IteratorNext,
    /// Close the iterator (calls return()) — used on early loop exit.
    IteratorClose,
    /// for-in: build a list of enumerable keys from object; push an enumerator.
    ForInEnumerate,
    /// Advance enumerator: push next key + has_next flag.
    ForInNext,
    /// Pop the current for-in enumerator (at loop end).
    ForInPop,

    // ---- generators / async ----
    /// Mark current frame as a generator/async and yield the initial suspended
    /// generator object (used at generator function entry). Emitted by the VM,
    /// not normally by the compiler.
    Yield,
    YieldStar, // delegate yield*
    Await,
    /// Suspend a generator/async-generator after its parameter prologue has run
    /// (parameters are evaluated at call time per spec; the body runs lazily).
    GeneratorStart,
    /// Async/generator function epilogue.
    AsyncReturn,

    // ---- misc ----
    /// Convert top to property key (ToPropertyKey).
    ToPropertyKey,
    /// Convert top to string (template parts).
    ToStringOp,
    /// Concatenate `n` strings on the stack into one (template literals).
    ConcatStrings(u32),
    /// Build a RegExp from (pattern, flags) consts.
    NewRegExp {
        pattern: u32,
        flags: u32,
    },
    /// no-op / line marker
    Nop,
    /// Push the current function's `home object`'s prototype for `super.x`.
    LoadSuperBase,
    /// `super.prop` get: base key -> value (uses this for receiver)
    SuperGet(u32),
    SuperGetDynamic,
    SuperSet(u32),
    /// spread an iterable onto the stack as call args (used with CallSpread prep)
    SpreadIntoArray,
    /// Create the `arguments` object from current frame.
    LoadArguments,
}
