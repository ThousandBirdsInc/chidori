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
    /// Constructor of a class with an `extends` clause: `this` is in TDZ until
    /// `super()` constructs it, and `return` follows the derived-constructor
    /// rules (object | undefined-becomes-this | else TypeError).
    DerivedCtor,
}

impl FuncKind {
    pub fn is_class_ctor(self) -> bool {
        matches!(self, FuncKind::ClassCtor | FuncKind::DerivedCtor)
    }
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
    /// For a MAPPED `arguments` object (sloppy, simple parameter list): the
    /// cell index of each positional parameter (`None` for an index shadowed
    /// by a later duplicate name). Empty when the function is unmapped.
    pub mapped_param_cells: Vec<Option<u32>>,
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
    /// Scope snapshots for the direct-`eval` call sites in this function,
    /// indexed by `Op::DirectEval`'s `scope` payload.
    pub eval_scopes: Vec<std::rc::Rc<EvalScopeDesc>>,
    /// For a [`FuncKind::DerivedCtor`]: the cell holding `%this`, listed in
    /// `stable_cells` so [[Construct]] can watch the SAME `Rc` across the call
    /// and apply the derived-constructor completion rules (object passes;
    /// undefined yields the bound `this` or a ReferenceError when `super()`
    /// never ran; other primitives are a TypeError) at frame exit — i.e.
    /// after `finally` blocks have run.
    pub this_cell: Option<u32>,
    /// Closures over this proto inherit the CREATING frame's [[HomeObject]]
    /// (arrows do this implicitly; set for synthetic in-class closures like a
    /// derived constructor's `%fieldinit`, so `super.x` in field initializers
    /// resolves against the class prototype).
    pub inherit_home: bool,
    /// Tagged-template literals in this function, indexed by
    /// [`Op::GetTemplateObject`]. The cached frozen template object is keyed
    /// at runtime by `(this proto's pointer, index)` — the spec's per-Parse
    /// Node template cache (a shared proto is the same Parse Node).
    pub templates: Vec<TemplateParts>,
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
            mapped_param_cells: Vec::new(),
            is_strict: false,
            stable_cells: Vec::new(),
            eval_scopes: Vec::new(),
            this_cell: None,
            inherit_home: false,
            templates: Vec::new(),
        }
    }
}

/// The compile-time parts of one tagged-template literal: the cooked strings
/// (`None` for an illegal escape, which cooks to `undefined`) and the raw
/// strings. Used by [`Op::GetTemplateObject`] to build the cached, frozen
/// template object on first evaluation.
#[derive(Clone, Debug)]
pub struct TemplateParts {
    pub cooked: Vec<Option<Rc<str>>>,
    pub raw: Vec<Rc<str>>,
}

/// A local binding slot — either a plain stack slot or a heap cell (captured).
#[derive(Clone, Copy, Debug)]
pub enum Slot {
    Local(u32),
    Cell(u32),
}

/// Where a caller binding visible at a direct-`eval` call site lives in the
/// CALLER's frame (see [`EvalScopeDesc`]).
#[derive(Clone, Copy, Debug)]
pub enum EvalSlot {
    Cell(u32),
    Upvalue(u32),
}

/// One caller binding visible at a direct-`eval` call site.
#[derive(Clone, Debug)]
pub struct EvalBinding {
    pub name: String,
    pub slot: EvalSlot,
    /// let/const/class (a sloppy eval `var` of the same name is a SyntaxError).
    pub is_lexical: bool,
    pub is_const: bool,
    /// A formal parameter (an eval var-declared `arguments` colliding with a
    /// parameter named `arguments` is a SyntaxError).
    pub is_param: bool,
}

/// The kind of a private class element, resolved lexically at compile time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PrivKind {
    Field,
    Method,
    Accessor,
    StaticField,
    StaticMethod,
    StaticAccessor,
}

/// One enclosing class body's private-name declarations, snapshotted at a
/// direct-`eval` call site so the eval body can compile `this.#x` against the
/// caller's private scope (the runtime names come from the caller frame's
/// private environment chain).
#[derive(Clone, Debug)]
pub struct EvalClassPriv {
    /// The compile-time class id (`#name@<id>` storage-key suffix).
    pub id: u32,
    pub names: Vec<(String, PrivKind)>,
}

/// Compile-time snapshot of the scope at a direct-`eval` call site. The
/// runtime compiles the eval source against these bindings (they become the
/// eval body's upvalues, wired to the caller frame's live cells) and uses the
/// context flags for the spec's eval early errors and var-target selection.
#[derive(Clone, Debug)]
pub struct EvalScopeDesc {
    pub bindings: Vec<EvalBinding>,
    /// Enclosing class bodies' private names (outermost first); empty outside
    /// class bodies.
    pub class_privs: Vec<EvalClassPriv>,
    /// The eval site is inside a class field initializer or class static
    /// block: the eval body may not contain `arguments` (early SyntaxError).
    pub in_field_initializer: bool,
    /// An enclosing NON-ARROW function exists (`new.target` is legal; the
    /// parse wrapper is a function).
    pub in_function: bool,
    /// The eval site is inside a PARAMETER list whose scope owns an
    /// `arguments` binding (non-arrow params, or any params that declare a
    /// parameter literally named `arguments`): a sloppy direct eval
    /// var-declaring `arguments` here is a SyntaxError.
    pub arguments_param_scope: bool,
    /// The caller's var scope is the global script (sloppy eval vars become
    /// global object properties).
    pub is_global_var_scope: bool,
    /// `super.x` resolves via the [[HomeObject]] path at the call site.
    pub home_super: bool,
    /// `super.x` is syntactically allowed at the call site at all.
    pub allow_super_prop: bool,
    /// Caller code is strict (the eval inherits strictness).
    pub strict: bool,
}

/// Which comparison a fused [`Op::CmpBranchFalse`] performs. Each maps 1:1 to a
/// standalone comparison opcode and is evaluated with the identical helper, so
/// fusion never changes coercion or thrown-error behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    StrictEq,
    StrictNe,
    Lt,
    Gt,
    Le,
    Ge,
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
    DeclareGlobal {
        name: u32,
        /// CreateGlobalVarBinding's D argument: eval-created globals are
        /// deletable (configurable); script-level ones are not.
        deletable: bool,
    },
    /// CanDeclareGlobalFunction check for a global function declaration
    /// (const index of name): throw a TypeError when an existing
    /// non-configurable own property can't be redefined as a writable,
    /// enumerable data property. Emitted before the closure is built so the
    /// spec's instantiation-time error precedes any function evaluation.
    CanDeclareGlobalFunc(u32),
    /// CreateGlobalFunctionBinding `[value] -> []` (const index of name): if
    /// the existing property is absent or configurable, (re)define it as
    /// `{value, writable, enumerable, configurable: deletable}`; otherwise
    /// keep its attributes and just set the value.
    DefineGlobalFunc {
        name: u32,
        deletable: bool,
    },
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
    /// handler (see `TryHandler::delegation`). The operand is the ip a
    /// `.return(v)` resumption jumps to (with `v` pushed) so the delegation
    /// loop can forward it to the inner iterator's `return` method;
    /// `u32::MAX` = no return delegation.
    MarkDelegationHandler(u32),
    /// Direct `eval(...)` call site: `[callee, arg0..argN-1] -> [result]`.
    /// When the callee is the %eval% intrinsic, the source compiles against
    /// the scope snapshot `FuncProto::eval_scopes[scope]` and runs with the
    /// caller's `this`/`new.target`/with-chain (spec PerformEval); any other
    /// callee gets an ordinary call.
    DirectEval {
        argc: u32,
        scope: u32,
    },
    /// Function-entry op for functions containing direct `eval`: create the
    /// frame's eval-vars object (where sloppy eval `var`s that don't match a
    /// visible binding live) and push it as the OUTERMOST with-scope, so the
    /// function's dynamic name ops — and closures created inside — see
    /// eval-introduced vars.
    InitEvalVars,

    // ---- private class elements ----
    /// ClassDefinitionEvaluation's NewPrivateEnvironment: mint a fresh runtime
    /// [`crate::value::PrivateName`] for each compile-time storage key (string
    /// const indices) and push the environment onto the frame's chain. Closures
    /// created while it is active capture the chain.
    PushPrivateEnv(Rc<[u32]>),
    /// Pop the innermost private environment (end of class definition).
    PopPrivateEnv,
    /// PrivateFieldAdd: `[obj, value] -> [obj]` — append a Field element for
    /// the resolved name; TypeError if the object already has it (a field
    /// initializer re-entered on the same object via return-override).
    PrivateFieldAdd(u32),
    /// PrivateMethodOrAccessorAdd for a method: `[obj, value] -> [obj]`;
    /// TypeError on a duplicate (double initialization).
    PrivateMethodAdd(u32),
    /// PrivateMethodOrAccessorAdd for a merged accessor pair:
    /// `[obj, getter, setter] -> [obj]` (`undefined` = absent side);
    /// TypeError on a duplicate.
    PrivateAccessorAdd(u32),
    /// `#x in obj`: `[obj] -> [bool]` — whether obj's [[PrivateElements]] has
    /// the resolved private name.
    PrivateHasOwn(u32),
    /// `[super, args.., newTarget] -> [instance]`: the construct step of
    /// `super(...)` — `Construct(super, args, newTarget)` (argc is the
    /// payload). The parent allocates `this` (so subclassing a builtin yields
    /// a real exotic instance) with `newTarget.prototype` as its prototype.
    ConstructSuper(u32),
    /// `[super, argsArray, newTarget] -> [instance]`: spread form.
    ConstructSuperSpread,
    /// `[instance] -> [instance]`: BindThisValue — initialize the derived
    /// constructor's `%this` cell (index payload) IN PLACE so closures that
    /// captured it before `super()` observe the value. Throws a
    /// ReferenceError if the cell is already initialized (`super()` twice).
    BindThisCell(u32),
    /// Same, for a `%this` captured as an upvalue (`super()` in an arrow).
    BindThisUpvalue(u32),
    /// `[ctor, superclass] -> []`: ClassDefinitionEvaluation prototype wiring:
    /// `ctor.prototype.[[Prototype]] = superclass.prototype` and
    /// `ctor.[[Prototype]] = superclass`, handling `extends null` (proto chain
    /// ends; ctor still inherits %Function.prototype%) and throwing TypeError
    /// when the heritage is not a constructor or its `prototype` is neither
    /// object nor null.
    ClassLinkSuper,

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
    /// Push the cached, frozen template object for tagged-template literal
    /// `index` (into the function's `templates`): `[] -> [templateObject]`.
    /// Built once per `(proto, index)` and reused on later evaluations.
    GetTemplateObject(u32),
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
    /// Stack unchanged; sets the top value closure's [[HomeObject]] to the
    /// object `n` slots below the top (MakeMethod for private static
    /// methods/accessors, whose stack shape carries no key).
    SetHomeObjectAt(u32),
    /// GetSuperBase: push `[[HomeObject]].[[GetPrototypeOf]]()` from the
    /// frame's function (`undefined` when the home prototype is null).
    GetSuperBase,
    /// `super.NAME` read: `[this, base] -> [value]` — Get(base, NAME) with
    /// receiver `this`.
    SuperGet(u32),
    /// `super[key]` read: `[this, base, key] -> [value]` — ToPropertyKey runs
    /// here (after GetSuperBase, per MakeSuperPropertyReference ordering).
    SuperGetDynamic,
    /// `super.NAME = v`: `[this, base, value] -> [value]` — Set(base, NAME,
    /// value, receiver=this); a failed write throws in strict code.
    SuperSet(u32),
    /// `super[key] = v`: `[this, base, key, value] -> [value]`.
    SuperSetDynamic,
    /// Spread source object into target ( target source -> target ).
    ObjectSpread,
    /// CopyDataProperties with excludedItems for object-rest destructuring:
    /// `[target, src, key1..keyN] -> [target]` (N is the payload; the keys
    /// are already property keys). Excluded keys are never read on the
    /// source — not even [[GetOwnProperty]].
    CopyDataPropertiesExcept(u32),
    GetProp(u32), // const index of name; obj -> value
    /// PrivateGet `obj.#x`: resolve the storage key (const index) through the
    /// frame's private environment, then read the element from the receiver's
    /// own [[PrivateElements]] (TypeError when absent — the brand check).
    /// obj -> value.
    PrivateGet(u32),
    SetProp(u32), // const index of name; obj value -> value
    /// PrivateSet `obj.#x = v`: brand-checks like [`Op::PrivateGet`], then
    /// writes the field / calls the setter (methods and setter-less accessors
    /// are TypeErrors). obj value -> value.
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
    /// Superinstruction (peephole fusion, see `fuse.rs`): a comparison
    /// immediately followed by `JumpIfFalse(target)` — the dominant loop-test
    /// idiom (`for (…; i < n; …)`). Pops `b` then `a`, computes the comparison
    /// using the **same** helpers as the standalone comparison op (identical
    /// coercion and thrown errors), and branches to `target` when the result is
    /// false. Exactly equivalent to the two-op sequence: the intermediate
    /// boolean the pair would push/pop is never observable to JS.
    CmpBranchFalse {
        cmp: CmpOp,
        target: u32,
    },
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
    /// `[.., key, fn] -> [.., key, fn]` (peek): SetFunctionName for a
    /// computed-key anonymous function/class value — name becomes the runtime
    /// key (a Symbol key yields "[description]" or ""), with the payload
    /// const ("", "get", "set") as prefix.
    SetFunctionNameFromKey(u32),
    /// `[obj, v] -> [obj]`: object-literal `__proto__: v` — set obj's
    /// [[Prototype]] to v when v is an Object or null; ignore otherwise.
    SetProtoFromLiteral,
    /// Open a `using` dispose capability for the entering block/body.
    PushDisposeScope,
    /// `[v] -> [v]` (peek): AddDisposableResource — record `v` and its
    /// dispose method (`@@dispose`; for `await using`, `@@asyncDispose`
    /// falling back to `@@dispose`) in the innermost dispose scope.
    /// Nullish `v` records nothing; a non-object or a missing/uncallable
    /// method is a TypeError (thrown BEFORE the binding initializes).
    TrackDisposable {
        is_await: bool,
    },
    /// DisposeResources: pop the innermost dispose scope and call each
    /// method (reverse order). Runs on a finally landing pad with the
    /// in-flight completion parked: a dispose error converts the parked
    /// completion to a throw, chaining prior throws via SuppressedError.
    DisposeScope,
    /// Async DisposeResources step: take the TOP resource off the innermost
    /// dispose scope and call its method (a call error merges like
    /// [`Op::DisposeScope`]); pushes `[result, more]` — when the scope is
    /// exhausted it is popped and `[undefined, false]` is pushed. The
    /// compiled landing pad Awaits `result` between steps (`await using`).
    DisposeAsyncNext,
    /// `[error] -> []`: merge an awaited dispose rejection into the parked
    /// completion (same chaining as [`Op::DisposeScope`]).
    MergeDisposeError,
    /// no-op / line marker
    Nop,
    /// Create the `arguments` object from current frame.
    LoadArguments,
}
