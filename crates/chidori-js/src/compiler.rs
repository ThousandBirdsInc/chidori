//! The bytecode compiler: lowers the oxc AST to chidori-js bytecode.
//!
//! Binding model: every source-level binding is a heap **cell**
//! (`Rc<RefCell<Value>>`). This makes closure capture correct without
//! retroactively patching local-slot accesses — a variable that turns out to be
//! captured is already a cell. It costs an allocation per binding, which is an
//! acceptable v1 trade (the plan defers perf tuning). Per-iteration `let`
//! bindings get fresh cells via re-`InitCell`, giving correct closure-in-loop
//! semantics. `this`/`arguments`/`new.target` are modeled as implicit cells so
//! arrow functions capture them lexically.
//!
//! TDZ for `let`/`const` is not enforced (bindings read as `undefined` before
//! initialization) — a documented conformance gap (plan P5).

use std::rc::Rc;

use oxc::allocator::Allocator;
use oxc::ast::ast::*;
use oxc::parser::Parser;
use oxc::span::SourceType;

use crate::bytecode::*;
use crate::value::JsString;

/// Decode oxc's lone-surrogate string encoding (see
/// `oxc StringLiteral::lone_surrogates`): U+FFFD is an escape — `U+FFFD XXXX`
/// (four lowercase hex digits) denotes the single code unit `XXXX` (a lone
/// surrogate, or U+FFFD itself when `XXXX == fffd`); all other characters are
/// literal. Only called when the literal is flagged as containing surrogates.
fn decode_lone_surrogates(value: &str) -> JsString {
    let mut units: Vec<u16> = Vec::new();
    let mut it = value.chars();
    while let Some(c) = it.next() {
        if c == '\u{FFFD}' {
            let mut hex = 0u16;
            let mut ok = true;
            for _ in 0..4 {
                match it.next().and_then(|d| d.to_digit(16)) {
                    Some(d) => hex = (hex << 4) | d as u16,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            units.push(if ok { hex } else { 0xFFFD });
        } else {
            let mut buf = [0u16; 2];
            units.extend_from_slice(c.encode_utf16(&mut buf));
        }
    }
    JsString::from_code_units(&units)
}

pub fn compile_script(src: &str) -> Result<FuncProto, String> {
    compile_script_impl(src, false, true, true)
}

/// Compile a script through a thread-local source→proto cache, so repeated
/// compilations of the same source (a durable restore/resume re-compiling its
/// bundle, or the fixed runtime prelude scripts evaluated on every fresh
/// engine) reuse one compiled [`FuncProto`] instead of re-running the whole
/// oxc parse → lower pipeline.
///
/// Sharing one proto across VMs on the same thread is sound: a `FuncProto` is
/// immutable after compilation — closures instantiated from it, the per-VM
/// tagged-template cache (keyed by proto pointer *per VM*), and the module
/// hook all hold their own per-VM state. The cache is keyed by the FULL
/// source string (hash + equality via `HashMap`), so a hit can never alias two
/// distinct programs, and it is a pure performance side effect: compilation is
/// deterministic, so a cached proto is byte-for-byte the proto a fresh compile
/// would produce. Errors are not cached (they are cheap to recompute and keep
/// the cache success-only).
pub fn compile_script_cached(src: &str) -> Result<Rc<FuncProto>, String> {
    // Bound the cache: a process hosts a handful of distinct bundles/preludes;
    // clearing wholesale at the cap is simpler than LRU and keeps worst-case
    // memory proportional to the cap without order-dependent behavior.
    const CACHE_CAP: usize = 64;
    thread_local! {
        static SCRIPT_CACHE: std::cell::RefCell<std::collections::HashMap<String, Rc<FuncProto>>> =
            std::cell::RefCell::new(std::collections::HashMap::new());
    }
    SCRIPT_CACHE.with(|cache| {
        if let Some(proto) = cache.borrow().get(src) {
            return Ok(proto.clone());
        }
        let proto = Rc::new(compile_script(src)?);
        let mut cache = cache.borrow_mut();
        if cache.len() >= CACHE_CAP {
            cache.clear();
        }
        cache.insert(src.to_string(), proto.clone());
        Ok(proto)
    })
}

/// Compile a script with the peephole op-fusion pass (`fuse.rs`) toggled. Used by
/// the differential test that runs the same program with `fuse = true` and
/// `fuse = false` and asserts byte-identical observable behavior. Production code
/// uses [`compile_script`] (fusion on).
pub fn compile_script_opts(src: &str, fuse: bool) -> Result<FuncProto, String> {
    compile_script_impl(src, false, fuse, true)
}

/// Compile with BOTH optimization passes toggled — used by the localization
/// differential test (`tests/localize.rs`), which asserts that every
/// combination executes identically. Production uses [`compile_script`]
/// (both passes on).
pub fn compile_script_passes(src: &str, fuse: bool, localize: bool) -> Result<FuncProto, String> {
    compile_script_impl(src, false, fuse, localize)
}

/// Compile global eval code — `(0,eval)(src)`: identical to a script except
/// `return` is illegal and the global `var`/function bindings it creates are
/// DELETABLE (CreateGlobalVarBinding with D=true).
pub fn compile_indirect_eval(src: &str) -> Result<FuncProto, String> {
    compile_script_impl(src, true, true, true)
}

fn compile_script_impl(
    src: &str,
    as_eval: bool,
    fuse: bool,
    localize: bool,
) -> Result<FuncProto, String> {
    let allocator = Allocator::default();
    // Parse as a *script* (the JS default): sloppy unless a `"use strict"`
    // directive (or a class/`module` context) makes it strict. The conformance
    // runner prepends `"use strict"` for the strict test variant, so this honors
    // the real per-variant strictness instead of forcing strict on everything
    // (which previously broke sloppy-mode tests). Strict early-errors still
    // surface as SyntaxError via the semantic pass below when strict.
    let source_type = SourceType::script();
    let ret = Parser::new(&allocator, src, source_type).parse();
    if !ret.errors.is_empty() {
        let msg = ret
            .errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!("SyntaxError: {msg}"));
    }
    let program = ret.program;
    // Semantic early-errors (duplicate lexical declarations, illegal `await`,
    // etc.) that the parser doesn't flag. Surfacing them as SyntaxError makes the
    // many `negative: { phase: parse, type: SyntaxError }` tests pass.
    let sem = oxc::semantic::SemanticBuilder::new()
        .with_check_syntax_error(true)
        .build(&program);
    if !sem.errors.is_empty() {
        return Err(format!(
            "SyntaxError: {}",
            sem.errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    let mut c = Compiler::new();
    c.source = src.to_string();
    c.toplevel_is_eval = as_eval;
    c.fuse = fuse;
    c.localize = localize;
    // A construct the lowering step rejects is, for our purposes, a syntax/early
    // error — surface it as SyntaxError so it is reported consistently.
    c.compile_toplevel(&program).map_err(|e| {
        if e.starts_with("SyntaxError") {
            e
        } else {
            format!("SyntaxError: {e}")
        }
    })
}

/// The compiled artifact of a direct-`eval` source text: the body proto (its
/// upvalues are `ParentCell(i)` indices into the scope snapshot's `bindings`,
/// which the runtime wires to the caller frame's live cells), the sloppy
/// escaping `var`/function names for EvalDeclarationInstantiation, and the
/// effective strictness.
pub struct CompiledEval {
    pub proto: FuncProto,
    pub var_names: Vec<String>,
    pub strict: bool,
}

/// Compile a DIRECT `eval` source against the scope snapshot taken at its call
/// site (see [`EvalScopeDesc`]). The source is parsed inside a wrapper picked
/// from the call-site context so the parser's placement rules match the spec:
/// a method wrapper permits `super.x`, a function wrapper permits
/// `new.target`, and global eval parses as a bare script (rejecting both).
/// `return` is gated back out (it is never legal in eval code). Visible
/// caller bindings are declared in a synthetic parent scope, so the body's
/// free references compile to ordinary upvalues.
pub fn compile_direct_eval(src: &str, desc: &EvalScopeDesc) -> Result<CompiledEval, String> {
    let allocator = Allocator::default();
    enum Wrap {
        None,
        Function,
        Method,
    }
    let wrap = if desc.allow_super_prop {
        Wrap::Method
    } else if desc.in_function {
        Wrap::Function
    } else {
        Wrap::None
    };
    // A hashbang comment is only valid at position 0 — strip it before the
    // source goes inside a wrapper (it is still a comment per the spec).
    let src = if let Some(rest) = src.strip_prefix("#!") {
        match rest.find('\n') {
            Some(i) => &rest[i..],
            None => "",
        }
    } else {
        src
    };
    // A strict caller's eval inherits strictness, including its EARLY errors —
    // carry it into the parse via a synthetic directive (skipped below when
    // computing completion values).
    let strict_prefix = if desc.strict { "\"use strict\";\n" } else { "" };
    let wrapped = match wrap {
        Wrap::None => format!("{strict_prefix}{src}"),
        Wrap::Function => format!("(function(){{\n{strict_prefix}{src}\n}})"),
        Wrap::Method => format!("({{ m(){{\n{strict_prefix}{src}\n}} }})"),
    };
    let source_type = SourceType::script();
    let ret = Parser::new(&allocator, &wrapped, source_type).parse();
    if !ret.errors.is_empty() {
        let msg = ret
            .errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!("SyntaxError: {msg}"));
    }
    let program = ret.program;
    let sem = oxc::semantic::SemanticBuilder::new()
        .with_check_syntax_error(true)
        .build(&program);
    // oxc checks the eval body standalone, so `this.#x` references that
    // legally resolve against the CALLER's private scopes (seeded from the
    // call-site snapshot) are misreported as undeclared — suppress exactly
    // those two diagnostics for seeded names.
    let seeded: std::collections::HashSet<&str> = desc
        .class_privs
        .iter()
        .flat_map(|p| p.names.iter().map(|(n, _)| n.as_str()))
        .collect();
    let sem_errors: Vec<String> = sem
        .errors
        .iter()
        .map(|e| e.to_string())
        .filter(|msg| {
            let name = msg
                .strip_prefix("Private identifier '#")
                .or_else(|| msg.strip_prefix("Private field '#"))
                .and_then(|r| r.split('\'').next());
            match name {
                Some(n) => !seeded.contains(n),
                None => true,
            }
        })
        .collect();
    if !sem_errors.is_empty() {
        return Err(format!("SyntaxError: {}", sem_errors.join("; ")));
    }
    // Extract the real body statements + directives from the wrapper.
    let (stmts, directives): (&[Statement], &[Directive]) = match wrap {
        Wrap::None => (&program.body, &program.directives),
        Wrap::Function | Wrap::Method => {
            let func = (|| -> Option<&Function> {
                let st = program.body.first()?;
                let Statement::ExpressionStatement(es) = st else {
                    return None;
                };
                let mut e: &Expression = &es.expression;
                if let Expression::ParenthesizedExpression(pe) = e {
                    e = &pe.expression;
                }
                match e {
                    Expression::FunctionExpression(f) => Some(f),
                    Expression::ObjectExpression(o) => {
                        let prop = o.properties.first()?;
                        let oxc::ast::ast::ObjectPropertyKind::ObjectProperty(p) = prop else {
                            return None;
                        };
                        match &p.value {
                            Expression::FunctionExpression(f) => Some(f),
                            _ => None,
                        }
                    }
                    _ => None,
                }
            })()
            .ok_or_else(|| "SyntaxError: eval wrapper shape".to_string())?;
            let body = func
                .body
                .as_deref()
                .ok_or_else(|| "SyntaxError: eval wrapper body".to_string())?;
            (&body.statements, &body.directives)
        }
    };
    let body_strict = desc.strict
        || directives
            .iter()
            .any(|d| d.directive.as_str() == "use strict");

    let mut c = Compiler::new();
    c.source = wrapped.clone();
    // Seed the caller's enclosing class private scopes (outermost first) so
    // `this.#x` in the eval body resolves to the caller's storage keys; the
    // runtime names come from the caller frame's private environment chain.
    // Classes declared INSIDE the eval get ids above the seeded range.
    c.class_privs = desc
        .class_privs
        .iter()
        .map(|p| ClassPrivCtx {
            id: p.id,
            names: p.names.iter().cloned().collect(),
            order: p.names.iter().map(|(n, _)| n.clone()).collect(),
            instance_groups: Vec::new(),
        })
        .collect();
    c.next_class_id = desc.class_privs.iter().map(|p| p.id + 1).max().unwrap_or(0);
    c.in_field_initializer = desc.in_field_initializer;

    // Synthetic parent scope: one cell per visible caller binding, in order —
    // the body's upvalue descriptors index straight into `desc.bindings`.
    let mut env = FnCtx::new("<eval-env>", FuncKind::Normal);
    env.is_toplevel = true;
    c.fns.push(env);
    c.enter_scope(true);
    for b in &desc.bindings {
        c.declare_kind(&b.name, true, b.is_const);
    }

    // The eval body itself.
    let mut fc = FnCtx::new("<eval>", FuncKind::Normal);
    fc.strict = body_strict;
    fc.track_completion = true;
    fc.is_eval_body = true;
    // For nested direct evals inside this body, the context flags are
    // inherited through `is_toplevel` (in_function detection).
    fc.is_toplevel = !desc.in_function;
    fc.script_global = desc.is_global_var_scope && !body_strict;
    fc.eval_sloppy = !body_strict && !(desc.is_global_var_scope && !body_strict);
    fc.enclosed_in_with = true;
    fc.contains_eval = wrapped.contains("eval");
    fc.home_super = desc.home_super;
    let body_script_global = fc.script_global;
    let body_contains_eval = fc.contains_eval;
    c.fns.push(fc);
    c.enter_scope(true);
    if body_contains_eval && !body_script_global {
        c.emit(Op::InitEvalVars);
    }
    // Completion register (eval's result is its completion value).
    c.emit(Op::LoadUndefined);
    let comp_cell = c.declare("%completion", true);
    c.emit(Op::InitCell(comp_cell));
    // `this`/`new.target`: prefer the caller's own %this/%newtarget bindings
    // from the snapshot (a sloppy caller's boxed `this` lives there); only
    // declare locals when the snapshot doesn't carry them (global eval).
    if !desc.bindings.iter().any(|b| b.name == "%this") {
        let this_cell = c.declare("%this", true);
        if body_script_global {
            let gt = c.str_const("globalThis");
            c.emit(Op::LoadGlobal(gt));
        } else {
            c.emit(Op::LoadThis);
        }
        c.emit(Op::InitCell(this_cell));
    }
    if !desc.bindings.iter().any(|b| b.name == "%newtarget") {
        let nt_cell = c.declare("%newtarget", true);
        c.emit(Op::LoadNewTarget);
        c.emit(Op::InitCell(nt_cell));
    }

    // Directive prologue strings are completion values too (`eval("'1'")`).
    // The synthetic "use strict" prefix is not part of the user's source.
    let mut user_directives = directives.iter();
    if desc.strict {
        let _ = user_directives.next();
    }
    for d in user_directives {
        let v = c.str_const(&d.expression.value);
        c.emit(Op::LoadConst(v));
        c.store_binding("%completion");
    }

    for st in stmts {
        if let Statement::VariableDeclaration(d) = st {
            if matches!(
                d.kind,
                VariableDeclarationKind::Using | VariableDeclarationKind::AwaitUsing
            ) {
                return Err(
                    "SyntaxError: 'using' declarations are not allowed at the top level of eval"
                        .to_string(),
                );
            }
        }
    }
    let compile_body = |c: &mut Compiler| -> Result<(), String> {
        c.hoist_lexical(stmts);
        c.predeclare_global_funcs(stmts);
        c.hoist_vars_all(stmts);
        c.hoist_funcs(stmts)?;
        for stmt in stmts {
            c.compile_stmt(stmt)?;
        }
        Ok(())
    };
    compile_body(&mut c).map_err(|e| {
        if e.starts_with("SyntaxError") {
            e
        } else {
            format!("SyntaxError: {e}")
        }
    })?;
    c.load_binding("%completion");
    c.emit(Op::Return);
    c.exit_scope();
    let fc = c.fns.pop().unwrap();
    let proto = c.finish(fc);
    Ok(CompiledEval {
        proto,
        var_names: std::mem::take(&mut c.eval_var_names),
        strict: body_strict,
    })
}

/// Compile a module's source text into a [`CompiledModule`] (its body proto plus
/// import/export entries). Parse + semantic early-errors surface as `SyntaxError`
/// exactly as for scripts, so `negative: { phase: parse }` module tests pass with
/// no evaluation. The body is compiled with module semantics: always strict,
/// top-level `this` is `undefined`, and top-level declarations are lexical cells
/// (NOT global-object properties).
pub fn compile_module(src: &str) -> Result<crate::module::CompiledModule, String> {
    use crate::module::*;
    let allocator = Allocator::default();
    let source_type = SourceType::default().with_module(true);
    let ret = Parser::new(&allocator, src, source_type).parse();
    if !ret.errors.is_empty() {
        let msg = ret
            .errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!("SyntaxError: {msg}"));
    }
    let program = ret.program;
    let sem = oxc::semantic::SemanticBuilder::new()
        .with_check_syntax_error(true)
        .build(&program);
    if !sem.errors.is_empty() {
        return Err(format!(
            "SyntaxError: {}",
            sem.errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    let mut c = Compiler::new();
    c.source = src.to_string();
    c.is_module = true;
    let (proto, cell_of_name) = c.compile_module_toplevel(&program).map_err(|e| {
        if e.starts_with("SyntaxError") {
            e
        } else {
            format!("SyntaxError: {e}")
        }
    })?;
    let num_cells = proto.num_cells;
    // Top-level await iff the module body's own code awaits (nested async
    // functions are separate protos, so this does not over-detect).
    let has_tla = proto.code.iter().any(|op| matches!(op, Op::Await));
    Ok(CompiledModule {
        proto: std::rc::Rc::new(proto),
        imports: c.module_imports,
        exports: c.module_exports,
        requested: c.module_requested,
        cell_of_name,
        num_cells,
        has_tla,
    })
}

struct Binding {
    name: String,
    cell: u32,
    /// var/function bindings live in function scope; let/const in block scope.
    function_scoped: bool,
    /// `const` bindings: reassignment is a runtime TypeError.
    is_const: bool,
    /// A named function expression's self-binding: immutable, but assignment
    /// is silently IGNORED in sloppy mode (TypeError only in strict).
    is_fn_name: bool,
}

struct Scope {
    bindings: Vec<Binding>,
    is_function_scope: bool,
    /// `with` nesting depth in effect when this scope was entered. A binding in
    /// a scope at the CURRENT depth has no `with` between it and a reference,
    /// so the inner declarative binding shadows any with-object and compiles to
    /// the static op.
    with_depth: u32,
}

struct LoopCtx {
    label: Option<String>,
    break_jumps: Vec<usize>,
    continue_jumps: Vec<usize>,
    /// True for loops (continue allowed); false for plain labeled blocks/switch.
    is_loop: bool,
    /// `with` nesting depth in effect when this loop/label was entered. A
    /// `break`/`continue` targeting it must pop the with-scopes entered since.
    with_depth: u32,
    /// Try-handler / try-finally depths a `continue` targeting this loop unwinds
    /// to. For a `for-of`/`for-await` loop these are *inside* the iterator's
    /// close handler, so `continue` re-iterates without closing the iterator.
    cont_handler_depth: u32,
    cont_finally_depth: u32,
    /// Try-handler / try-finally depths a `break` targeting this loop unwinds to.
    /// For a `for-of`/`for-await` loop these are *outside* the close handler, so
    /// `break` runs `IteratorClose`. Equal to the `cont_*` depths for other loops.
    brk_handler_depth: u32,
    brk_finally_depth: u32,
}

struct FnCtx {
    code: Vec<Op>,
    consts: Vec<Const>,
    scopes: Vec<Scope>,
    num_cells: u32,
    upvalues: Vec<UpvalueSource>,
    upvalue_keys: Vec<String>,
    kind: FuncKind,
    name: String,
    num_params: u32,
    has_rest: bool,
    param_names: Vec<String>,
    mapped_param_cells: Vec<Option<u32>>,
    uses_arguments: bool,
    /// Cell index of the implicit `this` binding (for non-arrow functions).
    this_cell: Option<u32>,
    new_target_cell: Option<u32>,
    arguments_cell: Option<u32>,
    /// Names captured by nested functions (conservative): such bindings become
    /// cells (always true here since all bindings are cells, but retained for
    /// clarity / future optimization).
    loops: Vec<LoopCtx>,
    track_completion: bool,
    /// Static count of try-handlers active at the current point (each
    /// `PushTryHandler` that is still in scope). Recorded per loop so a
    /// `break`/`continue` can compute how many handlers it crosses.
    handler_depth: u32,
    /// Static count of enclosing try-*finally* regions active at the current
    /// point. A `break`/`continue` routes through the completion machinery only
    /// when it crosses one (`finally_depth` at the break > at the target loop).
    finally_depth: u32,
    /// True only for the top-level `<script>` function: its `var`/`function`
    /// declarations become own properties of the global object (not cells), so
    /// `globalThis.x` / `hasOwnProperty` observe them.
    script_global: bool,
    /// Nesting depth of enclosing `with` statements within this function. When
    /// > 0, unqualified identifier reads/writes compile to dynamic name ops that
    /// consult the runtime with-scope stack before the static binding.
    with_depth: u32,
    /// True when this function is textually nested inside a `with` block of an
    /// enclosing function: its free identifiers must also resolve dynamically
    /// (against the closure's captured with-scope chain).
    enclosed_in_with: bool,
    /// The function's source region mentions `eval`: the classic direct-eval
    /// deopt. Free identifiers compile to dynamic name ops (so reads observe
    /// eval-introduced vars on the frame's eval-vars object), `arguments` is
    /// force-materialized, and the prologue creates the eval-vars object.
    contains_eval: bool,
    /// This is a top-level program context (`<script>`, `<module>`, or a
    /// direct-eval body) rather than a real function.
    is_toplevel: bool,
    /// A direct-eval BODY (gates `return` as a SyntaxError).
    is_eval_body: bool,
    /// A SLOPPY direct-eval body whose `var`/function declarations escape to
    /// the caller's var scope: hoisting collects their names (for the
    /// runtime's EvalDeclarationInstantiation) instead of declaring local
    /// cells, and initializers compile as dynamic name stores.
    eval_sloppy: bool,
    /// Currently compiling this function's parameter initializers (a sloppy
    /// direct eval here that var-declares `arguments` is a SyntaxError when
    /// the function is a non-arrow — its param scope owns an `arguments`
    /// binding; the BODY's var scope does not, so body evals are fine).
    in_params: bool,
    /// ALL simple parameter names, prescanned before initializers compile
    /// (an eval in an earlier default must see later parameter names).
    all_param_names: Vec<String>,
    /// Strict-mode code (a `"use strict"` directive here, an enclosing strict
    /// function, or a class body). Propagated to `FuncProto.is_strict` so the VM
    /// makes assignment failures throw.
    strict: bool,
    /// Module top-level cell indices that must be stable (see `FuncProto`).
    stable_cells: Vec<u32>,
    /// True for an object-literal concise method / accessor: `super.prop` in its
    /// own body resolves against the method's [[HomeObject]] (the object
    /// literal) via `getPrototypeOf`, rather than the class `%superclass`
    /// binding. (Nested arrows are separate functions and keep the class path.)
    home_super: bool,
    /// Scope snapshots for this function's direct-eval call sites.
    eval_scopes: Vec<std::rc::Rc<EvalScopeDesc>>,
    /// Synthetic in-class closure (e.g. `%fieldinit`): inherits the creating
    /// frame's [[HomeObject]] at closure creation (see `FuncProto`).
    inherit_home: bool,
    /// Tagged-template literals compiled in this function (see `FuncProto`).
    templates: Vec<TemplateParts>,
}

impl FnCtx {
    fn new(name: &str, kind: FuncKind) -> FnCtx {
        FnCtx {
            code: Vec::new(),
            consts: Vec::new(),
            scopes: Vec::new(),
            num_cells: 0,
            upvalues: Vec::new(),
            upvalue_keys: Vec::new(),
            kind,
            name: name.to_string(),
            num_params: 0,
            has_rest: false,
            param_names: Vec::new(),
            mapped_param_cells: Vec::new(),
            uses_arguments: false,
            this_cell: None,
            new_target_cell: None,
            arguments_cell: None,
            script_global: false,
            loops: Vec::new(),
            track_completion: false,
            with_depth: 0,
            enclosed_in_with: false,
            contains_eval: false,
            is_toplevel: false,
            is_eval_body: false,
            eval_sloppy: false,
            in_params: false,
            all_param_names: Vec::new(),
            handler_depth: 0,
            finally_depth: 0,
            strict: false,
            stable_cells: Vec::new(),
            inherit_home: false,
            templates: Vec::new(),
            home_super: false,
            eval_scopes: Vec::new(),
        }
    }
    fn alloc_cell(&mut self) -> u32 {
        let i = self.num_cells;
        self.num_cells += 1;
        i
    }
}

enum Resolved {
    Cell(u32),
    Upvalue(u32),
    Global,
}

/// Private-name scope of one `class` body being compiled. Each class body
/// gets a compile-time-unique id; a private element's COMPILE-TIME storage
/// key is `#name@<id>` (so same-named privates of nested/sibling classes can
/// never collide within a compilation). At runtime, `Op::PushPrivateEnv`
/// mints a fresh spec Private Name per key per class *evaluation*; the keys
/// only index into that environment chain.
struct ClassPrivCtx {
    id: u32,
    names: std::collections::HashMap<String, PrivKind>,
    /// Declaration order of the private names (deterministic key list for
    /// `Op::PushPrivateEnv`).
    order: Vec<String>,
    /// Private INSTANCE methods/accessors in declaration order: installed on
    /// `this` at construction (InitializeInstanceElements) from the class
    /// scope cells the element walk fills (`%privm#x` / `%privg#x`+`%privs#x`).
    instance_groups: Vec<(String, PrivKind)>,
}

struct Compiler {
    fns: Vec<FnCtx>,
    /// The toplevel being compiled is global EVAL code (indirect eval), not a
    /// script: `return` is illegal and global var bindings are deletable.
    toplevel_is_eval: bool,
    /// Enclosing `class` bodies (innermost last) for private-name resolution.
    class_privs: Vec<ClassPrivCtx>,
    /// Allocator for `ClassPrivCtx::id`.
    next_class_id: u32,
    /// A label captured by the immediately-following loop's `push_loop`.
    pending_label: Option<String>,
    /// Set just before compiling an object-literal concise method/accessor so the
    /// next function `FnCtx` is flagged `home_super` (one-shot, mirrors
    /// `pending_label`). Consumed by `emit_function_core`.
    pending_home_super: bool,
    /// Pending optional-chaining short-circuit fixups for the current chain.
    chain_jumps: Vec<usize>,
    /// Module-mode collection (populated only by `compile_module`): the import
    /// entries, export entries, and requested specifiers of the module body.
    module_imports: Vec<crate::module::ImportEntry>,
    module_exports: Vec<crate::module::ExportEntry>,
    module_requested: Vec<String>,
    is_module: bool,
    /// The full source text, so a function's body region can be cheaply scanned
    /// for the word `arguments` — when absent, the per-call `arguments` object is
    /// not materialized (a hot-path win for the common case).
    source: String,
    /// Escaping `var`/function names collected while compiling a SLOPPY
    /// direct-eval body (see `FnCtx::eval_sloppy`).
    eval_var_names: Vec<String>,
    /// One-shot: the next `compile_function` compiles a METHOD (class or
    /// object-literal concise method/accessor): the closure is a
    /// non-constructor with no `prototype` property.
    pending_method: bool,
    /// Lexically inside a `class` body (heritage, keys, methods,
    /// initializers): ALL class code is strict-mode code.
    in_class_body: bool,
    /// Compiling a class field initializer (or static block): direct eval
    /// here may not contain `arguments` (spec early error). Cleared on entry
    /// to non-arrow nested functions, which own their own `arguments`.
    in_field_initializer: bool,
    /// Run the peephole op-fusion pass (`fuse.rs`) on every finished function's
    /// bytecode. Always on in production; disabled only by `compile_script_opts`
    /// for the differential test that proves fused and unfused bytecode execute
    /// identically.
    fuse: bool,
    /// Run the cells→locals localization pass (`localize.rs`) on every finished
    /// function. Always on in production; disabled only for the differential
    /// test that proves localized and cell-only bytecode execute identically.
    localize: bool,
}

impl Compiler {
    fn new() -> Compiler {
        Compiler {
            fns: Vec::new(),
            toplevel_is_eval: false,
            class_privs: Vec::new(),
            next_class_id: 0,
            pending_label: None,
            pending_home_super: false,
            chain_jumps: Vec::new(),
            module_imports: Vec::new(),
            module_exports: Vec::new(),
            module_requested: Vec::new(),
            is_module: false,
            source: String::new(),
            eval_var_names: Vec::new(),
            pending_method: false,
            in_class_body: false,
            in_field_initializer: false,
            fuse: true,
            localize: true,
        }
    }

    /// Whether the source region `[start, end)` mentions `arguments` (the word).
    /// Conservative: an unreadable span returns `true` (materialize, to be safe).
    fn region_has_arguments(&self, start: u32, end: u32) -> bool {
        self.source
            .get(start as usize..end as usize)
            .map_or(true, |s| s.contains("arguments"))
    }

    /// Whether the source region mentions `eval` — the conservative trigger
    /// for the direct-eval deopt (dynamic name ops + eval-vars object). A
    /// false positive only costs speed, never correctness.
    fn region_has_eval(&self, start: u32, end: u32) -> bool {
        self.source
            .get(start as usize..end as usize)
            .map_or(true, |s| s.contains("eval"))
    }
}

type R = Result<(), String>;

impl Compiler {
    fn cur(&mut self) -> &mut FnCtx {
        self.fns.last_mut().unwrap()
    }

    fn cur_ref(&self) -> &FnCtx {
        self.fns.last().unwrap()
    }

    fn emit(&mut self, op: Op) -> usize {
        let c = self.cur();
        c.code.push(op);
        c.code.len() - 1
    }

    fn here(&mut self) -> u32 {
        self.cur().code.len() as u32
    }

    fn patch_jump(&mut self, at: usize, target: u32) {
        let op = &mut self.cur().code[at];
        match op {
            Op::Jump(t)
            | Op::JumpIfTrue(t)
            | Op::JumpIfFalse(t)
            | Op::JumpIfFalsyPeek(t)
            | Op::JumpIfTruthyPeek(t)
            | Op::JumpIfNullishPeek(t)
            | Op::JumpIfNullish(t) => *t = target,
            Op::PushTryHandler { catch, .. } => *catch = target,
            Op::MarkDelegationHandler(t) => *t = target,
            Op::CompletionJump { target: t, .. } => *t = target,
            _ => panic!("patch_jump on non-jump op"),
        }
    }

    fn konst(&mut self, k: Const) -> u32 {
        let c = self.cur();
        c.consts.push(k);
        (c.consts.len() - 1) as u32
    }

    fn intern_str(&mut self, js: JsString) -> u32 {
        // Dedup string constants.
        let c = self.cur();
        for (i, k) in c.consts.iter().enumerate() {
            if let Const::String(existing) = k {
                if *existing == js {
                    return i as u32;
                }
            }
        }
        c.consts.push(Const::String(js));
        (c.consts.len() - 1) as u32
    }

    fn str_const(&mut self, s: &str) -> u32 {
        self.intern_str(JsString::new(s))
    }

    fn load_str(&mut self, s: &str) {
        let i = self.str_const(s);
        self.emit(Op::LoadConst(i));
    }

    /// Load a string literal, decoding oxc's lone-surrogate encoding when the
    /// literal contains unpaired surrogates (e.g. `'\uD83D'`). oxc cannot store
    /// such a value as UTF-8, so it escapes it: U+FFFD followed by four hex
    /// digits is one code unit (the surrogate, or U+FFFD itself for `fffd`).
    fn load_string_literal(&mut self, s: &StringLiteral) {
        if s.lone_surrogates {
            let js = decode_lone_surrogates(s.value.as_str());
            let i = self.intern_str(js);
            self.emit(Op::LoadConst(i));
        } else {
            self.load_str(s.value.as_str());
        }
    }

    // ---- scopes & bindings ----

    fn enter_scope(&mut self, is_function: bool) {
        let with_depth = self.cur_ref().with_depth;
        self.cur().scopes.push(Scope {
            bindings: Vec::new(),
            is_function_scope: is_function,
            with_depth,
        });
    }
    fn exit_scope(&mut self) {
        self.cur().scopes.pop();
    }

    /// Declare a binding in the current (block) scope, or the nearest function
    /// scope for `var`. Returns the cell index.
    fn declare(&mut self, name: &str, function_scoped: bool) -> u32 {
        self.declare_kind(name, function_scoped, false)
    }

    /// As [`declare`], recording whether the binding is `const`.
    fn declare_kind(&mut self, name: &str, function_scoped: bool, is_const: bool) -> u32 {
        let cell = self.cur().alloc_cell();
        let fc = self.cur();
        let scope_idx = if function_scoped {
            fc.scopes
                .iter()
                .rposition(|s| s.is_function_scope)
                .unwrap_or(0)
        } else {
            fc.scopes.len() - 1
        };
        // If a var binding with this name already exists, reuse it.
        if function_scoped {
            if let Some(b) = fc.scopes[scope_idx]
                .bindings
                .iter()
                .find(|b| b.name == name)
            {
                let existing = b.cell;
                fc.num_cells -= 1; // undo alloc
                return existing;
            }
        }
        fc.scopes[scope_idx].bindings.push(Binding {
            name: name.to_string(),
            cell,
            function_scoped,
            is_const,
            is_fn_name: false,
        });
        cell
    }

    /// Declare a named function expression's self-binding: visible only inside
    /// the function (the caller wraps it in its own scope), immutable —
    /// assignment throws a TypeError in strict code and is silently ignored in
    /// sloppy code.
    fn declare_fn_name(&mut self, name: &str) -> u32 {
        let cell = self.cur().alloc_cell();
        let fc = self.cur();
        let scope_idx = fc.scopes.len() - 1;
        fc.scopes[scope_idx].bindings.push(Binding {
            name: name.to_string(),
            cell,
            function_scoped: false,
            is_const: false,
            is_fn_name: true,
        });
        cell
    }

    /// True while compiling directly in the top-level script body, where
    /// `var`/`function` declarations create global-object properties.
    fn in_global_scope(&self) -> bool {
        self.fns.last().map_or(false, |f| f.script_global)
    }

    /// Hoist one `var` binding pattern: a top-level simple identifier becomes a
    /// global property (`DeclareGlobal`, defined to `undefined` if absent);
    /// everything else (nested functions, or top-level destructuring) keeps the
    /// existing cell-based binding.
    fn hoist_var_pattern(&mut self, pat: &BindingPattern) {
        // Sloppy direct-eval body: simple `var name`s escape to the caller's
        // var scope — collect the name for the runtime's instantiation and
        // leave references dynamic (no local cell). Destructuring patterns
        // keep eval-local cells (a documented approximation).
        if self.cur_ref().eval_sloppy {
            if let BindingPattern::BindingIdentifier(id) = pat {
                let name = id.name.as_str().to_string();
                if !self.eval_var_names.contains(&name) {
                    self.eval_var_names.push(name);
                }
                return;
            }
        }
        match pat {
            BindingPattern::BindingIdentifier(id) if self.in_global_scope() => {
                let n = self.str_const(id.name.as_str());
                let deletable = self.cur_ref().is_eval_body;
                self.emit(Op::DeclareGlobal { name: n, deletable });
            }
            _ => self.declare_pattern_names(pat, true),
        }
    }

    /// Is the nearest in-scope binding named `name` a `const`? Searches the
    /// current function and enclosing functions (for captured upvalues).
    fn binding_is_const(&self, name: &str) -> bool {
        for fi in (0..self.fns.len()).rev() {
            for scope in self.fns[fi].scopes.iter().rev() {
                for b in scope.bindings.iter().rev() {
                    if b.name == name {
                        return b.is_const;
                    }
                }
            }
        }
        false
    }

    /// Whether `name` resolves (innermost-first, respecting shadowing) to a
    /// named function expression's immutable self-binding.
    fn binding_is_fn_name(&self, name: &str) -> bool {
        for fi in (0..self.fns.len()).rev() {
            for scope in self.fns[fi].scopes.iter().rev() {
                for b in scope.bindings.iter().rev() {
                    if b.name == name {
                        return b.is_fn_name;
                    }
                }
            }
        }
        false
    }

    fn resolve(&mut self, name: &str) -> Resolved {
        let top = self.fns.len() - 1;
        if let Some(cell) = self.find_cell(top, name) {
            return Resolved::Cell(cell);
        }
        if let Some(up) = self.resolve_upvalue(top, name) {
            return Resolved::Upvalue(up);
        }
        Resolved::Global
    }

    fn find_cell(&self, fi: usize, name: &str) -> Option<u32> {
        for scope in self.fns[fi].scopes.iter().rev() {
            for b in scope.bindings.iter().rev() {
                if b.name == name {
                    return Some(b.cell);
                }
            }
        }
        None
    }

    fn resolve_upvalue(&mut self, fi: usize, name: &str) -> Option<u32> {
        if fi == 0 {
            return None;
        }
        let parent = fi - 1;
        if let Some(cell) = self.find_cell(parent, name) {
            return Some(self.add_upvalue(fi, UpvalueSource::ParentCell(cell), name));
        }
        if let Some(up) = self.resolve_upvalue(parent, name) {
            return Some(self.add_upvalue(fi, UpvalueSource::ParentUpvalue(up), name));
        }
        None
    }

    fn add_upvalue(&mut self, fi: usize, src: UpvalueSource, key: &str) -> u32 {
        let fc = &mut self.fns[fi];
        // Dedup by (key) — name uniquely identifies the captured var per function.
        for (i, k) in fc.upvalue_keys.iter().enumerate() {
            if k == key {
                return i as u32;
            }
        }
        fc.upvalues.push(src);
        fc.upvalue_keys.push(key.to_string());
        (fc.upvalues.len() - 1) as u32
    }

    /// True when the current identifier reference could resolve against a
    /// `with`-object at runtime — textually inside a `with` block in this
    /// function, or in a function nested inside one (whose closure captures the
    /// with-scope chain). We skip synthetic compiler-internal names (`%this`,
    /// `%completion`, ...) which can never be shadowed by a real object
    /// property, and bindings of THIS function declared with no intervening
    /// `with`: the inner declarative binding shadows any with-object, so the
    /// static op is both correct and faster.
    fn in_with(&self, name: &str) -> bool {
        let fc = self.fns.last().unwrap();
        if name.starts_with('%')
            || (fc.with_depth == 0 && !fc.enclosed_in_with && !fc.contains_eval)
        {
            return false;
        }
        for scope in fc.scopes.iter().rev() {
            if scope.bindings.iter().any(|b| b.name == name) {
                return scope.with_depth != fc.with_depth;
            }
        }
        true
    }

    /// The static (non-`with`) store op for `name`: plain store, used by
    /// declaration/initialization paths.
    fn store_fallback(&mut self, name: &str) -> Op {
        // Assignment to a `const` binding is a runtime TypeError. Inside a
        // `with` the const cell is still the fallback target, so the dynamic op
        // carries the const-assign throw as its fallback.
        if self.binding_is_const(name) {
            Op::ThrowConstAssign
        } else if self.binding_is_fn_name(name) {
            // A named function expression's self-binding is immutable:
            // TypeError in strict code, silently dropped in sloppy code.
            if self.cur_ref().strict {
                Op::ThrowConstAssign
            } else {
                Op::Pop
            }
        } else {
            match self.resolve(name) {
                Resolved::Cell(i) => Op::StoreCell(i),
                Resolved::Upvalue(i) => Op::StoreUpvalue(i),
                Resolved::Global => {
                    let n = self.str_const(name);
                    Op::StoreGlobal(n)
                }
            }
        }
    }

    /// The static store op for assignment *expressions*: TDZ-checked, so
    /// `x = 1; let x;` throws (PutValue → SetMutableBinding on an
    /// uninitialized binding).
    fn store_assign_fallback(&mut self, name: &str) -> Op {
        if self.binding_is_const(name) {
            Op::ThrowConstAssign
        } else if self.binding_is_fn_name(name) {
            if self.cur_ref().strict {
                Op::ThrowConstAssign
            } else {
                Op::Pop
            }
        } else {
            match self.resolve(name) {
                Resolved::Cell(i) => Op::StoreCellChecked(i),
                Resolved::Upvalue(i) => Op::StoreUpvalueChecked(i),
                Resolved::Global => {
                    let n = self.str_const(name);
                    Op::StoreGlobal(n)
                }
            }
        }
    }

    /// The static load op for `name`.
    fn load_fallback(&mut self, name: &str) -> Op {
        match self.resolve(name) {
            Resolved::Cell(i) => Op::LoadCell(i),
            Resolved::Upvalue(i) => Op::LoadUpvalue(i),
            Resolved::Global => {
                let n = self.str_const(name);
                Op::LoadGlobal(n)
            }
        }
    }

    /// Snapshot the scope visible at a direct-`eval` call site: every binding
    /// of every enclosing function (innermost spelling wins), resolved FROM the
    /// current context — which forces upvalue capture, so the caller frame can
    /// hand the eval body live cells. `%`-internal names are excluded except
    /// `%superclass` (so `super.x` inside the eval can resolve).
    /// Whether `super.prop` is syntactically legal here: the innermost
    /// non-arrow function context is a method, accessor, class constructor,
    /// or a context flagged `home_super` (object methods, static blocks,
    /// eval bodies at such sites). Arrows are transparent.
    fn super_prop_allowed(&self) -> bool {
        for fc in self.fns.iter().rev() {
            if fc.kind.is_arrow() {
                continue;
            }
            return fc.home_super
                || fc.inherit_home
                || fc.kind.is_method()
                || fc.kind.is_class_ctor();
        }
        false
    }

    /// Emit `throw new <ErrorCtor>(<message>)` (the error constructor is
    /// resolved from the global at runtime).
    fn emit_throw_error(&mut self, ctor: &str, message: &str) {
        let tk = self.str_const(ctor);
        self.emit(Op::LoadGlobal(tk));
        let mk = self.str_const(message);
        self.emit(Op::LoadConst(mk));
        self.emit(Op::New(1));
        self.emit(Op::Throw);
    }

    /// Emit `[this, base]` for a super property reference: GetThisBinding
    /// (with the derived-constructor TDZ ReferenceError) then GetSuperBase.
    fn emit_super_ref(&mut self) -> R {
        if !self.super_prop_allowed() {
            return Err("'super' keyword is only valid inside a class or method".to_string());
        }
        self.load_binding("%this");
        self.emit(Op::GetSuperBase);
        Ok(())
    }

    fn collect_eval_scope(&mut self) -> std::rc::Rc<EvalScopeDesc> {
        use std::collections::HashSet;
        // Phase 1: gather candidate names + kind metadata (innermost first).
        let mut seen: HashSet<String> = HashSet::new();
        // (name, is_lexical, is_const, is_param)
        let mut metas: Vec<(String, bool, bool, bool)> = Vec::new();
        for fi in (0..self.fns.len()).rev() {
            let fc = &self.fns[fi];
            for scope in fc.scopes.iter().rev() {
                for b in scope.bindings.iter().rev() {
                    // `%this`/`%newtarget`/`%superclass` ride along so the eval
                    // body's `this`/`new.target`/`super` resolve to the exact
                    // caller bindings (a sloppy caller's boxed `this` lives in
                    // its %this cell, not in the raw frame value).
                    if b.name.starts_with('%')
                        && !matches!(
                            b.name.as_str(),
                            "%this" | "%newtarget" | "%superclass" | "%fieldinit"
                        )
                    {
                        continue;
                    }
                    if !seen.insert(b.name.clone()) {
                        continue;
                    }
                    // Params are declared block-scoped but are var-like for the
                    // eval var-shadow check.
                    let is_param = fc.param_names.iter().any(|p| p == &b.name);
                    // `arguments` and params are var-like even though they are
                    // declared block-scoped internally.
                    let is_lexical = !b.function_scoped
                        && !is_param
                        && b.name != "arguments"
                        && !b.name.starts_with('%');
                    metas.push((b.name.clone(), is_lexical, b.is_const, is_param));
                }
            }
            for k in fc.upvalue_keys.clone() {
                if k.starts_with('%')
                    && !matches!(
                        k.as_str(),
                        "%this" | "%newtarget" | "%superclass" | "%fieldinit"
                    )
                {
                    continue;
                }
                if seen.insert(k.clone()) {
                    let is_const = self.binding_is_const(&k);
                    metas.push((k, false, is_const, false));
                }
            }
        }
        // Phase 2: resolve each from the current context (capturing upvalues).
        let mut bindings: Vec<EvalBinding> = Vec::new();
        for (name, is_lexical, is_const, is_param) in metas {
            let slot = match self.resolve(&name) {
                Resolved::Cell(i) => EvalSlot::Cell(i),
                Resolved::Upvalue(i) => EvalSlot::Upvalue(i),
                Resolved::Global => continue,
            };
            bindings.push(EvalBinding {
                name,
                slot,
                is_lexical,
                is_const,
                is_param,
            });
        }
        let in_function = self
            .fns
            .iter()
            .any(|f| !f.is_toplevel && !f.kind.is_arrow());
        // The spec's `var arguments` SyntaxError fires only when the eval runs
        // in a PARAMETER scope owning an `arguments` binding: a non-arrow
        // function's params, or any params declaring one named `arguments`.
        let arguments_param_scope = self
            .fns
            .iter()
            .rev()
            .find(|f| f.in_params)
            .map(|f| !f.kind.is_arrow() || f.all_param_names.iter().any(|p| p == "arguments"))
            .unwrap_or(false);
        let is_global_var_scope = self.fns.len() == 1;
        let home_super = self.super_prop_allowed();
        let allow_super_prop = home_super;
        let strict = self.cur_ref().strict;
        // Enclosing class bodies' private names (outermost first), so the
        // eval body can compile `this.#x` against the caller's private scope.
        let class_privs = self
            .class_privs
            .iter()
            .map(|c| EvalClassPriv {
                id: c.id,
                names: c.order.iter().map(|n| (n.clone(), c.names[n])).collect(),
            })
            .collect();
        std::rc::Rc::new(EvalScopeDesc {
            bindings,
            class_privs,
            in_field_initializer: self.in_field_initializer,
            in_function,
            arguments_param_scope,
            is_global_var_scope,
            home_super,
            allow_super_prop,
            strict,
        })
    }

    fn store_binding(&mut self, name: &str) {
        let fallback = self.store_fallback(name);
        if self.in_with(name) {
            let n = self.str_const(name);
            self.emit(Op::StoreName {
                name: n,
                fallback: Box::new(fallback),
            });
        } else {
            self.emit(fallback);
        }
    }

    /// As [`store_binding`], but for assignment *expressions*: a cell/upvalue
    /// target is written with the TDZ-checked store so `x = 1; let x;` throws a
    /// ReferenceError (PutValue → SetMutableBinding on an uninitialized binding).
    /// Declaration/initialization paths keep calling `store_binding` (plain
    /// store) so they can fill a binding that is intentionally still in TDZ.
    fn store_binding_assign(&mut self, name: &str) {
        let fallback = self.store_assign_fallback(name);
        if self.in_with(name) {
            let n = self.str_const(name);
            self.emit(Op::StoreName {
                name: n,
                fallback: Box::new(fallback),
            });
        } else {
            self.emit(fallback);
        }
    }

    fn load_binding(&mut self, name: &str) {
        let fallback = self.load_fallback(name);
        if self.in_with(name) {
            let n = self.str_const(name);
            self.emit(Op::LoadName {
                name: n,
                fallback: Box::new(fallback),
            });
        } else {
            self.emit(fallback);
        }
    }

    // ---- once-resolved references (with-scope) ----
    //
    // Assignment/update expressions evaluate their LHS Reference ONCE; a side
    // effect during the RHS (or a with-object getter) that deletes/shadows the
    // binding must not redirect the final write. Inside a `with` we therefore
    // capture the resolved base object up front and read/write through it.

    /// Capture the with-aware Reference base for `name` into a fresh temp cell
    /// (the with-object holding `name`, or `undefined` for a static binding).
    fn capture_name_base(&mut self, name: &str) -> u32 {
        let n = self.str_const(name);
        let t = self.temp();
        self.emit(Op::ResolveNameBase(n));
        self.emit(Op::InitCell(t));
        t
    }

    /// Read `name` through the captured base in `t_base`.
    fn load_via_base(&mut self, name: &str, t_base: u32) {
        let fallback = self.load_fallback(name);
        let n = self.str_const(name);
        self.emit(Op::LoadCell(t_base));
        self.emit(Op::LoadFromBase {
            name: n,
            fallback: Box::new(fallback),
        });
    }

    /// Store the value on top of the stack to `name` through the captured base
    /// in `t_base`, leaving the stored value on the stack (the assignment
    /// expression's result).
    fn store_via_base_keep(&mut self, name: &str, t_base: u32) {
        let fallback = self.store_assign_fallback(name);
        let n = self.str_const(name);
        let t_val = self.temp();
        self.emit(Op::InitCell(t_val));
        self.emit(Op::LoadCell(t_base));
        self.emit(Op::LoadCell(t_val));
        self.emit(Op::StoreToBase {
            name: n,
            fallback: Box::new(fallback),
        });
        self.emit(Op::LoadCell(t_val));
    }

    // ---- top level ----

    fn compile_toplevel(&mut self, program: &Program) -> Result<FuncProto, String> {
        let mut fc = FnCtx::new("<script>", FuncKind::Normal);
        fc.track_completion = true;
        fc.script_global = true;
        fc.is_toplevel = true;
        fc.is_eval_body = self.toplevel_is_eval;
        fc.contains_eval = self.source.contains("eval");
        fc.strict = program
            .directives
            .iter()
            .any(|d| d.directive.as_str() == "use strict");
        self.fns.push(fc);
        self.enter_scope(true);
        // completion slot starts undefined.
        self.emit(Op::LoadUndefined);
        // store into a synthetic completion cell.
        let comp_cell = self.declare("%completion", true);
        self.emit(Op::InitCell(comp_cell));
        // Script-level `this` is the global object (non-module sloppy semantics),
        // so top-level `this` and arrows that capture it resolve correctly.
        let this_cell = self.declare("%this", true);
        let gt = self.str_const("globalThis");
        self.emit(Op::LoadGlobal(gt));
        self.emit(Op::InitCell(this_cell));
        let nt_cell = self.declare("%newtarget", true);
        self.emit(Op::LoadUndefined);
        self.emit(Op::InitCell(nt_cell));
        self.hoist_lexical(&program.body);
        self.predeclare_global_funcs(&program.body);
        self.hoist_vars_all(&program.body);
        self.hoist_funcs(&program.body)?;
        for stmt in &program.body {
            self.compile_stmt(stmt)?;
        }
        // return completion value
        self.load_binding("%completion");
        self.emit(Op::Return);
        self.exit_scope();
        let fc = self.fns.pop().unwrap();
        Ok(self.finish(fc))
    }

    /// Compile a module body. Like `compile_toplevel` but: always strict, `this`
    /// is `undefined`, declarations are lexical cells (no global props), and the
    /// import/export declarations populate `self.module_*`. Returns the body proto
    /// plus a map from each top-level binding name to its cell index (the linker
    /// uses it to wire live import bindings and build namespace objects).
    fn compile_module_toplevel(
        &mut self,
        program: &Program,
    ) -> Result<(FuncProto, std::collections::HashMap<String, u32>), String> {
        let mut fc = FnCtx::new("<module>", FuncKind::Normal);
        fc.strict = true;
        fc.script_global = false;
        fc.is_toplevel = true;
        fc.contains_eval = self.source.contains("eval");
        let module_has_eval = fc.contains_eval;
        self.fns.push(fc);
        self.enter_scope(true);
        if module_has_eval {
            self.emit(Op::InitEvalVars);
        }
        // Module `this` is undefined; new.target likewise.
        let this_cell = self.declare("%this", true);
        self.emit(Op::LoadUndefined);
        self.emit(Op::InitCell(this_cell));
        let nt_cell = self.declare("%newtarget", true);
        self.emit(Op::LoadUndefined);
        self.emit(Op::InitCell(nt_cell));
        self.module_hoist(&program.body)?;
        for stmt in &program.body {
            self.compile_stmt(stmt)?;
        }
        // Capture the cell index of every top-level binding before the scope is
        // torn down — the linker needs these to share export cells, and they are
        // the module's stable cells (filled in place, never Rc-replaced).
        let cell_of_name = self.capture_toplevel_cells();
        self.cur().stable_cells = cell_of_name.values().copied().collect();
        self.emit(Op::LoadUndefined);
        self.emit(Op::Return);
        self.exit_scope();
        let fc = self.fns.pop().unwrap();
        Ok((self.finish(fc), cell_of_name))
    }

    /// Snapshot `name -> cell` for every binding visible in the module's function
    /// (top) scope.
    fn capture_toplevel_cells(&self) -> std::collections::HashMap<String, u32> {
        let mut map = std::collections::HashMap::new();
        let fc = self.fns.last().unwrap();
        for scope in &fc.scopes {
            for b in &scope.bindings {
                map.insert(b.name.clone(), b.cell);
            }
        }
        map
    }

    /// Module top-level hoisting: declare imported locals (as cells the linker
    /// later rebinds to the exporter's cell — so NO initializer is emitted), then
    /// the usual lexical/function/var hoisting (which is export-aware).
    fn module_hoist(&mut self, stmts: &[Statement]) -> R {
        for s in stmts {
            if let Statement::ImportDeclaration(d) = s {
                if let Some(specs) = &d.specifiers {
                    for spec in specs {
                        let local = import_local_name(spec);
                        if self.current_scope_cell(local).is_none() {
                            // const-like binding; cell value supplied by the linker.
                            self.declare_kind(local, false, true);
                        }
                    }
                }
            }
        }
        self.hoist_lexical(stmts);
        self.hoist_funcs(stmts)?;
        self.hoist_vars_all(stmts);
        // `var` cells must start as `undefined`, not TDZ. In a normal function the
        // frame's fresh cells default to `undefined`, but a module's cells are
        // pre-allocated in TDZ by the linker, so initialize each top-level `var`
        // binding explicitly (in place, since module top cells are stable).
        let mut var_names: Vec<String> = Vec::new();
        for s in stmts {
            collect_module_var_names(s, &mut var_names);
        }
        for name in &var_names {
            if let Some(cell) = self.current_scope_cell(name) {
                self.emit(Op::LoadUndefined);
                self.emit(Op::InitCell(cell));
            }
        }
        Ok(())
    }

    /// `import … from request` — record the requested specifier and one
    /// [`ImportEntry`] per specifier. Emits no code (linking binds the cells).
    fn compile_import_decl(&mut self, d: &ImportDeclaration) -> R {
        use crate::module::{ImportEntry, ImportName};
        let request = d.source.value.as_str().to_string();
        self.add_requested(&request);
        if let Some(specs) = &d.specifiers {
            for spec in specs {
                let (local, name) = match spec {
                    ImportDeclarationSpecifier::ImportSpecifier(s) => (
                        s.local.name.as_str().to_string(),
                        ImportName::Named(module_export_name(&s.imported)),
                    ),
                    ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                        (s.local.name.as_str().to_string(), ImportName::Default)
                    }
                    ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                        (s.local.name.as_str().to_string(), ImportName::Namespace)
                    }
                };
                self.module_imports.push(ImportEntry {
                    module_request: request.clone(),
                    import_name: name,
                    local_name: local,
                });
            }
        }
        Ok(())
    }

    /// `export <decl>` / `export {…}` / `export {…} from request`.
    fn compile_export_named(&mut self, d: &ExportNamedDeclaration) -> R {
        use crate::module::{ExportEntry, ExportKind};
        if let Some(decl) = &d.declaration {
            // `export const x = …` / `export function f(){}` / `export class C{}`:
            // compile the inner declaration normally, then record a local export
            // per bound name.
            self.compile_declaration(decl)?;
            for name in declaration_bound_names(decl) {
                self.module_exports.push(ExportEntry {
                    export_name: Some(name.clone()),
                    kind: ExportKind::Local { local_name: name },
                });
            }
            return Ok(());
        }
        // `export { a, b as c }` (no source) or `export { a } from 'm'` (re-export).
        let source = d.source.as_ref().map(|s| s.value.as_str().to_string());
        if let Some(req) = &source {
            self.add_requested(req);
        }
        for spec in &d.specifiers {
            let local = module_export_name(&spec.local);
            let exported = module_export_name(&spec.exported);
            let kind = match &source {
                Some(req) => ExportKind::Indirect {
                    module_request: req.clone(),
                    import_name: local,
                },
                None => ExportKind::Local { local_name: local },
            };
            self.module_exports.push(ExportEntry {
                export_name: Some(exported),
                kind,
            });
        }
        Ok(())
    }

    /// `export default <function|class|expression>` — bind `*default*` to the
    /// value and record a local export named `default`.
    fn compile_export_default(&mut self, d: &ExportDefaultDeclaration) -> R {
        use crate::module::{ExportEntry, ExportKind};
        let star = self.declare_kind("*default*", false, true);
        match &d.declaration {
            ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                // A named `export default function f(){}` also binds `f` locally.
                // Declare that binding BEFORE compiling the body so a
                // self-reference inside (e.g. `f = 2`) captures the module-level
                // cell instead of falling through to the global scope.
                let local =
                    f.id.as_ref()
                        .map(|id| self.declare(id.name.as_str(), false));
                // An anonymous `export default function(){}` gets the name "default".
                self.compile_function(
                    f,
                    Some(f.id.as_ref().map_or("default", |i| i.name.as_str())),
                )?;
                if let Some(c) = local {
                    self.emit(Op::Dup);
                    self.emit(Op::InitCell(c));
                }
                self.emit(Op::InitCell(star));
            }
            ExportDefaultDeclarationKind::ClassDeclaration(c) => {
                self.compile_class(
                    c,
                    Some(c.id.as_ref().map_or("default", |i| i.name.as_str())),
                )?;
                if let Some(id) = &c.id {
                    let cell = self.declare(id.name.as_str(), false);
                    self.emit(Op::Dup);
                    self.emit(Op::InitCell(cell));
                }
                self.emit(Op::InitCell(star));
            }
            other => {
                // `export default <AssignmentExpression>`: NamedEvaluation
                // gives an anonymous function/class the name "default".
                let expr: &Expression = other.as_expression().unwrap();
                self.compile_named_expr(expr, "default")?;
                self.emit(Op::InitCell(star));
            }
        }
        // A NAMED default function/class declaration has ONE live binding (the
        // name); the `default` export resolves to it, so later reassignment of
        // the name is visible through the export (live bindings). Anonymous
        // defaults export the synthetic `*default*` cell.
        let local_name = match &d.declaration {
            ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                f.id.as_ref()
                    .map(|i| i.name.as_str().to_string())
                    .unwrap_or_else(|| "*default*".to_string())
            }
            ExportDefaultDeclarationKind::ClassDeclaration(c) => {
                c.id.as_ref()
                    .map(|i| i.name.as_str().to_string())
                    .unwrap_or_else(|| "*default*".to_string())
            }
            _ => "*default*".to_string(),
        };
        self.module_exports.push(ExportEntry {
            export_name: Some("default".to_string()),
            kind: ExportKind::Local { local_name },
        });
        Ok(())
    }

    /// `export * from request` / `export * as ns from request`.
    fn compile_export_all(&mut self, d: &ExportAllDeclaration) -> R {
        use crate::module::{ExportEntry, ExportKind};
        let request = d.source.value.as_str().to_string();
        self.add_requested(&request);
        let kind = ExportKind::Star {
            module_request: request,
        };
        // `export * as ns` names a single export; bare `export *` has no name.
        let export_name = d.exported.as_ref().map(module_export_name);
        self.module_exports.push(ExportEntry { export_name, kind });
        Ok(())
    }

    fn add_requested(&mut self, request: &str) {
        if !self.module_requested.iter().any(|r| r == request) {
            self.module_requested.push(request.to_string());
        }
    }

    /// Compile an inner `Declaration` (the body of `export <decl>`). Mirrors the
    /// `compile_stmt` arms for the equivalent bare declarations; functions are
    /// hoisted (by `hoist_funcs`) so nothing is emitted here.
    fn compile_declaration(&mut self, decl: &Declaration) -> R {
        match decl {
            Declaration::VariableDeclaration(d) => self.compile_var_decl(d)?,
            Declaration::FunctionDeclaration(_) => { /* hoisted */ }
            Declaration::ClassDeclaration(c) => {
                let name = c.id.as_ref().map(|i| i.name.as_str().to_string());
                self.compile_class(c, name.as_deref())?;
                if let Some(n) = name {
                    if self.current_scope_cell(&n).is_none() {
                        self.declare(&n, false);
                    }
                    self.store_binding(&n);
                } else {
                    self.emit(Op::Pop);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(&self, fc: FnCtx) -> FuncProto {
        // Cells→locals localization (docs/js-performance-roadmap.md §3.2):
        // provably-uncaptured bindings move from heap cells to pooled
        // `frame.locals` slots. Runs BEFORE fusion so the fusion pass sees
        // (and superinstructs) the rewritten local ops.
        let loc = if self.localize {
            crate::localize::localize(
                fc.code,
                fc.num_cells,
                &fc.consts,
                &fc.stable_cells,
                fc.this_cell,
                &fc.mapped_param_cells,
                fc.uses_arguments,
                !fc.eval_scopes.is_empty(),
            )
        } else {
            crate::localize::Localized {
                code: fc.code,
                num_locals: 0,
                localized: vec![false; fc.num_cells as usize].into_boxed_slice(),
            }
        };
        // Peephole op-fusion (Phase 2): every finished function — top-level and
        // nested — flows through here, so applying it once covers the whole
        // proto tree. Disabled only by the differential test.
        let code = if self.fuse {
            crate::fuse::fuse_code_fixpoint(loc.code)
        } else {
            loc.code
        };
        FuncProto {
            eval_scopes: fc.eval_scopes.clone(),
            name: fc.name,
            ic: code
                .iter()
                .map(|_| crate::bytecode::IcEntry {
                    slot: std::cell::Cell::new(u32::MAX),
                    holder: std::cell::RefCell::new(None),
                })
                .collect(),
            code,
            consts: fc.consts,
            num_locals: loc.num_locals,
            num_cells: fc.num_cells,
            num_params: fc.num_params,
            has_rest: fc.has_rest,
            upvalues: fc.upvalues,
            kind: fc.kind,
            source_start: 0,
            uses_arguments: fc.uses_arguments,
            param_names: fc.param_names,
            mapped_param_cells: fc.mapped_param_cells,
            is_strict: fc.strict,
            stable_flags: {
                let mut flags = vec![false; fc.num_cells as usize].into_boxed_slice();
                for &c in &fc.stable_cells {
                    flags[c as usize] = true;
                }
                flags
            },
            stable_cells: fc.stable_cells.clone(),
            localized: loc.localized,
            this_cell: fc.this_cell,
            inherit_home: fc.inherit_home,
            templates: fc.templates,
        }
    }

    // ---- hoisting ----

    /// Recursively hoist `var` bindings (not into nested functions). Called once
    /// at function/script entry.
    fn hoist_vars_all(&mut self, stmts: &[Statement]) {
        for s in stmts {
            self.hoist_vars(s);
        }
    }

    /// The cell of a binding `name` declared in the *innermost* (current) scope.
    fn current_scope_cell(&self, name: &str) -> Option<u32> {
        let fc = self.fns.last()?;
        let scope = fc.scopes.last()?;
        scope
            .bindings
            .iter()
            .rev()
            .find(|b| b.name == name)
            .map(|b| b.cell)
    }

    /// Pre-declare the block-scoped lexical bindings (`let`/`const`/`class`, simple
    /// identifiers) of `stmts` at scope entry, each initialized to `undefined`, so
    /// forward references resolve to the binding rather than the global object
    /// (e.g. `const f = () => g(); const g = ...;`, or a class method naming a
    /// `const` declared later). Must run *before* `hoist_funcs` so hoisted function
    /// closures capture these cells. (Destructuring patterns keep the
    /// declare-at-statement path.)
    fn hoist_lexical(&mut self, stmts: &[Statement]) {
        for s in stmts {
            match s {
                Statement::VariableDeclaration(d)
                    if matches!(
                        d.kind,
                        VariableDeclarationKind::Let
                            | VariableDeclarationKind::Const
                            | VariableDeclarationKind::Using
                            | VariableDeclarationKind::AwaitUsing
                    ) =>
                {
                    // `using` bindings are const-like: reassignment TypeErrors.
                    let is_const = !matches!(d.kind, VariableDeclarationKind::Let);
                    for decl in &d.declarations {
                        if let BindingPattern::BindingIdentifier(id) = &decl.id {
                            if self.current_scope_cell(id.name.as_str()).is_none() {
                                let cell = self.declare_kind(id.name.as_str(), false, is_const);
                                self.emit(Op::InitCellTdz(cell));
                            }
                        }
                    }
                }
                Statement::ClassDeclaration(c) => {
                    if let Some(id) = &c.id {
                        if self.current_scope_cell(id.name.as_str()).is_none() {
                            let cell = self.declare_kind(id.name.as_str(), false, false);
                            self.emit(Op::InitCellTdz(cell));
                        }
                    }
                }
                // `export const/let/class …` hoists like the bare form.
                Statement::ExportNamedDeclaration(e) => match e.declaration.as_ref() {
                    Some(Declaration::VariableDeclaration(d))
                        if matches!(
                            d.kind,
                            VariableDeclarationKind::Let | VariableDeclarationKind::Const
                        ) =>
                    {
                        let is_const = matches!(d.kind, VariableDeclarationKind::Const);
                        for decl in &d.declarations {
                            if let BindingPattern::BindingIdentifier(id) = &decl.id {
                                if self.current_scope_cell(id.name.as_str()).is_none() {
                                    let cell = self.declare_kind(id.name.as_str(), false, is_const);
                                    self.emit(Op::InitCellTdz(cell));
                                }
                            }
                        }
                    }
                    Some(Declaration::ClassDeclaration(c)) => {
                        if let Some(id) = &c.id {
                            if self.current_scope_cell(id.name.as_str()).is_none() {
                                let cell = self.declare_kind(id.name.as_str(), false, false);
                                self.emit(Op::InitCellTdz(cell));
                            }
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }

    /// Declare + emit closures for function declarations directly in `stmts`
    /// (hoisted to the top of the current scope).
    /// Emit the CanDeclareGlobalFunction checks for every top-level function
    /// name (declaration order, deduped), to run BEFORE any global var/
    /// function binding is created — so a non-definable name (e.g.
    /// `function NaN(){}`, or a name shadowing a writable non-enumerable
    /// non-configurable property) aborts GlobalDeclarationInstantiation
    /// before it leaks a `var` binding. No-op outside the global scope.
    fn predeclare_global_funcs(&mut self, stmts: &[Statement]) {
        if !self.in_global_scope() || self.cur_ref().eval_sloppy {
            return;
        }
        let mut seen: Vec<String> = Vec::new();
        for s in stmts {
            if let Statement::FunctionDeclaration(f) = s {
                if let Some(id) = &f.id {
                    let name = id.name.as_str().to_string();
                    if !seen.contains(&name) {
                        seen.push(name.clone());
                        let n = self.str_const(&name);
                        self.emit(Op::CanDeclareGlobalFunc(n));
                    }
                }
            }
        }
    }

    fn hoist_funcs(&mut self, stmts: &[Statement]) -> R {
        // Sloppy direct-eval body: function declarations escape to the caller's
        // var scope. Collect the names (runtime pre-creates the bindings) and
        // install each closure with a dynamic name store; calls resolve
        // dynamically, so mutual recursion still works once both are stored.
        if self.cur_ref().eval_sloppy {
            for s in stmts {
                if let Statement::FunctionDeclaration(f) = s {
                    if let Some(id) = &f.id {
                        let name = id.name.as_str().to_string();
                        if !self.eval_var_names.contains(&name) {
                            self.eval_var_names.push(name.clone());
                        }
                        self.compile_function(f, Some(&name))?;
                        self.store_binding(&name);
                    }
                }
            }
            return Ok(());
        }
        // Top-level function declarations become global-object properties. Their
        // bodies reference each other via `LoadGlobal` (resolved at call time), so
        // a single definition pass suffices.
        if self.in_global_scope() {
            let deletable = self.cur_ref().is_eval_body;
            // The CanDeclareGlobalFunction checks are emitted earlier by
            // `predeclare_global_funcs` (before ANY binding is created), so a
            // non-definable name aborts instantiation before vars/functions
            // exist. Here we only build and bind the closures (last
            // declaration of a duplicate name wins via DefineGlobalFunc).
            for s in stmts {
                if let Statement::FunctionDeclaration(f) = s {
                    if let Some(id) = &f.id {
                        self.compile_function(f, Some(id.name.as_str()))?;
                        // CreateGlobalFunctionBinding: (re)define the property
                        // with function-binding attributes (or keep a
                        // non-configurable existing property's and just set it).
                        let n = self.str_const(id.name.as_str());
                        self.emit(Op::DefineGlobalFunc { name: n, deletable });
                    }
                }
            }
            return Ok(());
        }
        // Two passes so functions can reference each other (mutual recursion) and
        // themselves: first establish every declaration's cell (a stable `Rc`),
        // then build the closures (which capture those cells) and store into them
        // *in place* via `StoreCell`. Using `InitCell` after `Closure` would
        // replace the `Rc` the closure already captured, breaking self/forward
        // references — the bug behind e.g. `function F(){ this instanceof F }`.
        let mut cells = Vec::new();
        for s in stmts {
            if let Some(f) = stmt_function_decl(s) {
                if let Some(id) = &f.id {
                    let cell = self.declare(id.name.as_str(), true);
                    self.emit(Op::LoadUndefined);
                    self.emit(Op::InitCell(cell));
                    cells.push(cell);
                }
            }
        }
        let mut i = 0;
        for s in stmts {
            if let Some(f) = stmt_function_decl(s) {
                if let Some(id) = &f.id {
                    let cell = cells[i];
                    i += 1;
                    self.compile_function(f, Some(id.name.as_str()))?;
                    self.emit(Op::StoreCell(cell));
                }
            }
        }
        Ok(())
    }

    fn hoist_vars(&mut self, stmt: &Statement) {
        match stmt {
            Statement::VariableDeclaration(d) => {
                if matches!(d.kind, VariableDeclarationKind::Var) {
                    for decl in &d.declarations {
                        self.hoist_var_pattern(&decl.id);
                    }
                }
            }
            Statement::BlockStatement(b) => {
                for s in &b.body {
                    self.hoist_vars(s);
                }
            }
            Statement::IfStatement(i) => {
                self.hoist_vars(&i.consequent);
                if let Some(a) = &i.alternate {
                    self.hoist_vars(a);
                }
            }
            Statement::ForStatement(f) => {
                if let Some(ForStatementInit::VariableDeclaration(d)) = &f.init {
                    if matches!(d.kind, VariableDeclarationKind::Var) {
                        for decl in &d.declarations {
                            self.hoist_var_pattern(&decl.id);
                        }
                    }
                }
                self.hoist_vars(&f.body);
            }
            Statement::ForInStatement(f) => {
                if let ForStatementLeft::VariableDeclaration(d) = &f.left {
                    if matches!(d.kind, VariableDeclarationKind::Var) {
                        for decl in &d.declarations {
                            self.hoist_var_pattern(&decl.id);
                        }
                    }
                }
                self.hoist_vars(&f.body);
            }
            Statement::ForOfStatement(f) => {
                if let ForStatementLeft::VariableDeclaration(d) = &f.left {
                    if matches!(d.kind, VariableDeclarationKind::Var) {
                        for decl in &d.declarations {
                            self.hoist_var_pattern(&decl.id);
                        }
                    }
                }
                self.hoist_vars(&f.body);
            }
            Statement::WhileStatement(w) => self.hoist_vars(&w.body),
            Statement::DoWhileStatement(w) => self.hoist_vars(&w.body),
            Statement::TryStatement(t) => {
                for s in &t.block.body {
                    self.hoist_vars(s);
                }
                if let Some(h) = &t.handler {
                    for s in &h.body.body {
                        self.hoist_vars(s);
                    }
                }
                if let Some(f) = &t.finalizer {
                    for s in &f.body {
                        self.hoist_vars(s);
                    }
                }
            }
            Statement::LabeledStatement(l) => self.hoist_vars(&l.body),
            Statement::WithStatement(w) => self.hoist_vars(&w.body),
            Statement::SwitchStatement(s) => {
                for case in &s.cases {
                    for st in &case.consequent {
                        self.hoist_vars(st);
                    }
                }
            }
            // `export var x` hoists like a bare `var`.
            Statement::ExportNamedDeclaration(e) => {
                if let Some(Declaration::VariableDeclaration(d)) = &e.declaration {
                    if matches!(d.kind, VariableDeclarationKind::Var) {
                        for decl in &d.declarations {
                            self.hoist_var_pattern(&decl.id);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn declare_pattern_names(&mut self, pat: &BindingPattern, function_scoped: bool) {
        self.declare_pattern_names_kind(pat, function_scoped, false)
    }

    fn declare_pattern_names_kind(
        &mut self,
        pat: &BindingPattern,
        function_scoped: bool,
        is_const: bool,
    ) {
        match pat {
            BindingPattern::BindingIdentifier(id) => {
                self.declare_kind(id.name.as_str(), function_scoped, is_const);
            }
            BindingPattern::ObjectPattern(o) => {
                for p in &o.properties {
                    self.declare_pattern_names_kind(&p.value, function_scoped, is_const);
                }
                if let Some(r) = &o.rest {
                    self.declare_pattern_names_kind(&r.argument, function_scoped, is_const);
                }
            }
            BindingPattern::ArrayPattern(a) => {
                for el in a.elements.iter().flatten() {
                    self.declare_pattern_names_kind(el, function_scoped, is_const);
                }
                if let Some(r) = &a.rest {
                    self.declare_pattern_names_kind(&r.argument, function_scoped, is_const);
                }
            }
            BindingPattern::AssignmentPattern(a) => {
                self.declare_pattern_names_kind(&a.left, function_scoped, is_const);
            }
        }
    }
}

// =========================================================================
// Statements
// =========================================================================

impl Compiler {
    fn compile_stmt(&mut self, stmt: &Statement) -> R {
        match stmt {
            Statement::ExpressionStatement(e) => {
                self.compile_expr(&e.expression)?;
                if self.cur().track_completion {
                    self.store_binding("%completion");
                } else {
                    self.emit(Op::Pop);
                }
            }
            Statement::EmptyStatement(_) => {}
            Statement::BlockStatement(b) => self.compile_block(&b.body)?,
            Statement::VariableDeclaration(d) => self.compile_var_decl(d)?,
            Statement::FunctionDeclaration(_) => { /* hoisted */ }
            Statement::ClassDeclaration(c) => {
                let name = c.id.as_ref().map(|i| i.name.as_str().to_string());
                self.compile_class(c, name.as_deref())?;
                if let Some(n) = name {
                    // The class binding is pre-declared by `hoist_lexical`; reuse
                    // that cell (fallback declares it for non-hoisted contexts).
                    if self.current_scope_cell(&n).is_none() {
                        self.declare(&n, false);
                    }
                    self.store_binding(&n);
                } else {
                    self.emit(Op::Pop);
                }
            }
            Statement::IfStatement(i) => {
                self.zero_completion();
                self.compile_if(i)?
            }
            Statement::WhileStatement(w) => {
                self.zero_completion();
                self.compile_while(w)?
            }
            Statement::DoWhileStatement(w) => {
                self.zero_completion();
                self.compile_do_while(w)?
            }
            Statement::ForStatement(f) => {
                self.zero_completion();
                // `for (using x = ...; ;)`: the dispose capability spans the
                // whole statement (disposed when the loop completes/aborts).
                let head_using = matches!(
                    &f.init,
                    Some(ForStatementInit::VariableDeclaration(d)) if matches!(
                        d.kind,
                        VariableDeclarationKind::Using | VariableDeclarationKind::AwaitUsing
                    )
                );
                if head_using {
                    let pad_async = matches!(
                        &f.init,
                        Some(ForStatementInit::VariableDeclaration(d))
                            if matches!(d.kind, VariableDeclarationKind::AwaitUsing)
                    );
                    self.compile_with_dispose_scope(pad_async, |c| c.compile_for(f))?;
                } else {
                    self.compile_for(f)?;
                }
            }
            Statement::ForInStatement(f) => {
                self.zero_completion();
                self.compile_for_in(f)?
            }
            Statement::ForOfStatement(f) => {
                self.zero_completion();
                self.compile_for_of(f)?
            }
            Statement::ReturnStatement(_) if self.cur_ref().is_eval_body => {
                return Err("Illegal return statement".into());
            }
            Statement::ReturnStatement(r) => {
                if let Some(arg) = &r.argument {
                    self.compile_expr(arg)?;
                } else {
                    self.emit(Op::LoadUndefined);
                }
                // Leave every active `with` environment before returning.
                self.emit_pop_with_to(0);
                self.emit(Op::Return);
                // (A derived constructor's return-value rules apply at frame
                // exit, in [[Construct]], so `finally` blocks — which may call
                // super() — run first.)
            }
            Statement::ThrowStatement(t) => {
                self.compile_expr(&t.argument)?;
                self.emit(Op::Throw);
            }
            Statement::BreakStatement(b) => {
                let label = b.label.as_ref().map(|l| l.name.as_str().to_string());
                if let Some(td) = self.target_with_depth(label.as_deref(), false) {
                    self.emit_pop_with_to(td);
                }
                let at = self.emit_break_continue_jump(label.as_deref(), false);
                self.register_break(at, label)?;
            }
            Statement::ContinueStatement(c) => {
                let label = c.label.as_ref().map(|l| l.name.as_str().to_string());
                if let Some(td) = self.target_with_depth(label.as_deref(), true) {
                    self.emit_pop_with_to(td);
                }
                let at = self.emit_break_continue_jump(label.as_deref(), true);
                self.register_continue(at, label)?;
            }
            Statement::TryStatement(t) => {
                self.zero_completion();
                self.compile_try(t)?
            }
            Statement::SwitchStatement(s) => {
                self.zero_completion();
                self.compile_switch(s)?
            }
            Statement::WithStatement(w) => {
                self.zero_completion();
                self.compile_with(w)?
            }
            Statement::LabeledStatement(l) => {
                self.zero_completion();
                self.compile_labeled(l)?
            }
            Statement::DebuggerStatement(_) => {}
            Statement::ImportDeclaration(d) => self.compile_import_decl(d)?,
            Statement::ExportNamedDeclaration(d) => self.compile_export_named(d)?,
            Statement::ExportDefaultDeclaration(d) => self.compile_export_default(d)?,
            Statement::ExportAllDeclaration(d) => self.compile_export_all(d)?,
            // TypeScript-only statements: types are stripped before us.
            _ => {}
        }
        Ok(())
    }

    fn compile_block(&mut self, body: &[Statement]) -> R {
        self.enter_scope(false);
        if Self::stmts_have_using(body) {
            self.compile_with_dispose_scope(Self::stmts_have_await_using(body), |c| {
                c.hoist_lexical(body);
                c.hoist_funcs(body)?;
                for s in body {
                    c.compile_stmt(s)?;
                }
                Ok(())
            })?;
        } else {
            self.hoist_lexical(body);
            self.hoist_funcs(body)?;
            for s in body {
                self.compile_stmt(s)?;
            }
        }
        self.exit_scope();
        Ok(())
    }

    /// Whether any DIRECT statement is a `using` / `await using` declaration
    /// (nested blocks manage their own dispose scopes).
    fn stmts_have_using(stmts: &[Statement]) -> bool {
        stmts.iter().any(|s| {
            matches!(
                s,
                Statement::VariableDeclaration(d) if matches!(
                    d.kind,
                    VariableDeclarationKind::Using | VariableDeclarationKind::AwaitUsing
                )
            )
        })
    }

    /// Whether any DIRECT statement is an `await using` declaration — its
    /// dispose landing pad must Await each disposal.
    fn stmts_have_await_using(stmts: &[Statement]) -> bool {
        stmts.iter().any(|s| {
            matches!(
                s,
                Statement::VariableDeclaration(d)
                    if matches!(d.kind, VariableDeclarationKind::AwaitUsing)
            )
        })
    }

    /// Compile `f`'s statements inside a `using` dispose capability: a
    /// finally-style region whose landing pad runs DisposeResources, so EVERY
    /// exit — normal fall-through, throw, return, break/continue — disposes
    /// the scope's resources (in reverse), merging dispose errors into the
    /// in-flight completion (SuppressedError chaining). An `is_async` pad
    /// (any `await using` present) Awaits each disposal result, merging
    /// rejections the same way.
    fn compile_with_dispose_scope(&mut self, is_async: bool, f: impl FnOnce(&mut Self) -> R) -> R {
        self.emit(Op::PushDisposeScope);
        let push = self.emit(Op::PushTryHandler {
            catch: u32::MAX,
            finally: u32::MAX,
        });
        self.cur().handler_depth += 1;
        self.cur().finally_depth += 1;
        f(self)?;
        self.emit(Op::PopTryHandler);
        self.cur().handler_depth -= 1;
        self.cur().finally_depth -= 1;
        let normal = self.emit(Op::Jump(0));
        let fin = self.here();
        self.patch_finally(push, fin);
        self.patch_jump(normal, fin);
        if is_async {
            // The try handler is (re)installed at EMPTY stack depth each
            // iteration, so an Await rejection truncates to a clean base
            // before pushing the error:
            //   top:  PushTryHandler{catch}
            //         DisposeAsyncNext           [result, more]
            //         JumpIfFalse done           [result]
            //         Await; Pop                 []
            //         PopTryHandler; Jump top
            //   done: Pop; PopTryHandler; EndFinally   ([result] dropped)
            //   catch:                           [error]
            //         MergeDisposeError; Jump top
            let top = self.here();
            let inner = self.emit(Op::PushTryHandler {
                catch: 0,
                finally: u32::MAX,
            });
            self.emit(Op::DisposeAsyncNext);
            let jdone = self.emit(Op::JumpIfFalse(0));
            self.emit(Op::Await);
            self.emit(Op::Pop);
            self.emit(Op::PopTryHandler);
            let back = self.emit(Op::Jump(0));
            let catch_ip = self.here();
            self.patch_jump(inner, catch_ip);
            self.emit(Op::MergeDisposeError);
            let back2 = self.emit(Op::Jump(0));
            let done = self.here();
            self.patch_jump(jdone, done);
            self.patch_jump(back, top);
            self.patch_jump(back2, top);
            self.emit(Op::Pop); // drop the exhausted step's undefined result
            self.emit(Op::PopTryHandler);
        } else {
            self.emit(Op::DisposeScope);
        }
        self.emit(Op::EndFinally);
        Ok(())
    }

    fn compile_var_decl(&mut self, d: &VariableDeclaration) -> R {
        let function_scoped = matches!(d.kind, VariableDeclarationKind::Var);
        let is_const = matches!(
            d.kind,
            VariableDeclarationKind::Const
                | VariableDeclarationKind::Using
                | VariableDeclarationKind::AwaitUsing
        );
        // `using x = v` / `await using x = v`: after the initializer
        // evaluates, AddDisposableResource records it (and resolves its
        // dispose method — a TypeError here leaves the binding in TDZ)
        // BEFORE the binding initializes.
        let track_using = match d.kind {
            VariableDeclarationKind::Using => Some(false),
            VariableDeclarationKind::AwaitUsing => Some(true),
            _ => None,
        };
        // Sloppy direct-eval body: a simple `var name = init` is an assignment
        // to the caller-scope binding (dynamic name store; the binding was
        // pre-created by EvalDeclarationInstantiation or is a visible one).
        if function_scoped && self.cur_ref().eval_sloppy {
            for decl in &d.declarations {
                if let BindingPattern::BindingIdentifier(id) = &decl.id {
                    let name = id.name.as_str().to_string();
                    if let Some(init) = &decl.init {
                        self.compile_named_expr(init, &name)?;
                        self.store_binding(&name);
                    }
                    continue;
                }
                // Destructuring var in eval: eval-local cells (approximation).
                self.declare_pattern_names(&decl.id, true);
                if let Some(init) = &decl.init {
                    self.compile_expr(init)?;
                    self.bind_pattern(&decl.id, true)?;
                }
            }
            return Ok(());
        }
        // A top-level `var name` is a global property (already created by hoisting);
        // its initializer is an ordinary assignment to that global.
        let global_var = function_scoped && self.in_global_scope();
        for decl in &d.declarations {
            if global_var {
                if let BindingPattern::BindingIdentifier(id) = &decl.id {
                    let name = id.name.as_str().to_string();
                    if let Some(init) = &decl.init {
                        self.compile_named_expr(init, &name)?;
                        self.store_binding(&name); // resolves to StoreGlobal
                    }
                    continue;
                }
            }
            if let BindingPattern::BindingIdentifier(id) = &decl.id {
                let name = id.name.as_str();
                // `let`/`const` simple bindings are pre-declared (and `InitCell`'d
                // to undefined) by `hoist_lexical` at scope entry; reuse that cell
                // so forward references already resolved to it. `var` declares its
                // cell here. The fallback covers contexts without a lexical hoist
                // pass (e.g. a `switch` case body).
                let cell = if !function_scoped {
                    match self.current_scope_cell(name) {
                        Some(c) => c,
                        None => {
                            let c = self.declare_kind(name, false, is_const);
                            self.emit(Op::LoadUndefined);
                            self.emit(Op::InitCell(c));
                            c
                        }
                    }
                } else {
                    self.declare_kind(name, function_scoped, is_const)
                };
                if let Some(init) = &decl.init {
                    self.compile_named_expr(init, name)?;
                    if let Some(is_await) = track_using {
                        self.emit(Op::TrackDisposable { is_await });
                    }
                    // The declaration's own initialization is allowed even for
                    // `const`; store directly into the cell (clearing any TDZ).
                    // Inside a `with`, a *var* initializer is an ordinary
                    // assignment to the resolved reference, so it may land on the
                    // with-object instead of the cell. (`let`/`const` are
                    // block-scoped and never intercepted.)
                    if function_scoped && self.in_with(name) {
                        let n = self.str_const(name);
                        self.emit(Op::StoreName {
                            name: n,
                            fallback: Box::new(Op::StoreCell(cell)),
                        });
                    } else {
                        self.emit(Op::StoreCell(cell));
                    }
                } else if !function_scoped {
                    // `let x;` (no initializer) is initialized to `undefined` at
                    // the declaration, clearing the TDZ marker set by hoisting.
                    self.emit(Op::LoadUndefined);
                    self.emit(Op::StoreCell(cell));
                }
            } else if let Some(init) = &decl.init {
                self.compile_expr(init)?;
                self.bind_pattern_kind(&decl.id, function_scoped, is_const)?;
            } else {
                self.declare_pattern_names_kind(&decl.id, function_scoped, is_const);
            }
        }
        Ok(())
    }

    /// Emit code to pull the next value from the iterator in cell `itc` onto the
    /// stack, substituting `undefined` when the iterator is done. Leaves
    /// `[..., value]`.
    /// Like [`emit_iter_step`] but threads a `done` cell: once the iterator
    /// reports done, the cell latches true and further steps push `undefined`
    /// without calling `next()` again (spec IteratorDestructuringAssignment, so a
    /// pattern with more targets than the iterator yields doesn't over-call
    /// `next()`). The cell also drives IteratorClose at the end of the pattern.
    fn emit_iter_step_tracked(&mut self, itc: u32, done_cell: u32) {
        self.emit(Op::LoadCell(done_cell));
        let jskip = self.emit(Op::JumpIfTrue(0)); // already done -> undefined
                                                  // Latch done=true BEFORE stepping: an abrupt completion from next(),
                                                  // the `done` getter, or the `value` getter sets [[Done]] (spec
                                                  // IteratorStepValue), so the enclosing close handler must NOT call
                                                  // `return()`. The latch is cleared only once a value is extracted.
        self.emit(Op::LoadTrue);
        self.emit(Op::StoreCell(done_cell));
        self.emit(Op::LoadCell(itc));
        self.emit(Op::IteratorNext); // [iter, result]
        self.emit(Op::Swap);
        self.emit(Op::Pop); // [result]
        self.emit(Op::Dup);
        let dk = self.str_const("done");
        self.emit(Op::GetProp(dk)); // [result, done]
        let jdone = self.emit(Op::JumpIfTrue(0));
        let vk = self.str_const("value");
        self.emit(Op::GetProp(vk)); // [value]
        self.emit(Op::LoadFalse);
        self.emit(Op::StoreCell(done_cell)); // normal step: un-latch
        let jhave = self.emit(Op::Jump(0));
        // result.done: done_cell stays latched, drop result, push undefined.
        let donelbl = self.here();
        self.patch_jump(jdone, donelbl);
        self.emit(Op::Pop);
        self.emit(Op::LoadUndefined);
        let jhave2 = self.emit(Op::Jump(0));
        // done_cell already true: push undefined.
        let skiplbl = self.here();
        self.patch_jump(jskip, skiplbl);
        self.emit(Op::LoadUndefined);
        let have = self.here();
        self.patch_jump(jhave, have);
        self.patch_jump(jhave2, have);
    }

    fn emit_iter_step(&mut self, itc: u32) {
        self.emit(Op::LoadCell(itc)); // [iter]
        self.emit(Op::IteratorNext); // [iter, result]
        self.emit(Op::Swap);
        self.emit(Op::Pop); // [result]
        self.emit(Op::Dup);
        let dk = self.str_const("done");
        self.emit(Op::GetProp(dk)); // [result, done]
        let jdone = self.emit(Op::JumpIfTrue(0));
        let vk = self.str_const("value");
        self.emit(Op::GetProp(vk)); // [value]
        let jhave = self.emit(Op::Jump(0));
        let done = self.here();
        self.patch_jump(jdone, done);
        self.emit(Op::Pop); // drop result
        self.emit(Op::LoadUndefined);
        let have = self.here();
        self.patch_jump(jhave, have);
    }

    fn copy_cells(&mut self, cells: &[u32]) {
        for &c in cells {
            self.emit(Op::LoadCell(c));
            self.emit(Op::InitCell(c));
        }
    }

    /// Destructure the value on top of the stack into `pat`, declaring bindings.
    fn bind_pattern(&mut self, pat: &BindingPattern, function_scoped: bool) -> R {
        self.bind_pattern_kind(pat, function_scoped, false)
    }

    /// As [`bind_pattern`], recording whether the declared bindings are `const`.
    fn bind_pattern_kind(
        &mut self,
        pat: &BindingPattern,
        function_scoped: bool,
        is_const: bool,
    ) -> R {
        match pat {
            BindingPattern::BindingIdentifier(id) => {
                let cell = self.declare_kind(id.name.as_str(), function_scoped, is_const);
                self.emit(Op::InitCell(cell));
            }
            BindingPattern::AssignmentPattern(a) => {
                // value on stack; if undefined use default.
                self.emit(Op::Dup);
                self.emit(Op::LoadUndefined);
                self.emit(Op::StrictEq);
                let jf = self.emit(Op::JumpIfFalse(0));
                self.emit(Op::Pop); // drop undefined value
                                    // Named evaluation: when the target is a plain identifier, an
                                    // anonymous function/class/arrow default takes that name (spec
                                    // BindingElement : SingleNameBinding Initializer).
                if let BindingPattern::BindingIdentifier(id) = &a.left {
                    self.compile_named_expr(&a.right, id.name.as_str())?;
                } else {
                    self.compile_expr(&a.right)?;
                }
                let target = self.here();
                self.patch_jump(jf, target);
                self.bind_pattern_kind(&a.left, function_scoped, is_const)?;
            }
            BindingPattern::ArrayPattern(a) => {
                // Iterator-protocol destructuring (spec-correct: works for any
                // iterable, and accesses Symbol.iterator exactly as required).
                self.emit(Op::GetIterator); // [iter]  (consumes the source)
                let itc = self.temp();
                self.emit(Op::InitCell(itc)); // []
                let done_cell = self.temp();
                self.emit(Op::LoadFalse);
                self.emit(Op::InitCell(done_cell));

                // Wrap the binding in a finally that runs IteratorClose iff the
                // iterator isn't done — covering both normal completion with
                // leftover elements and an abrupt throw during element/default
                // evaluation. A trailing rest consumes the iterator to done, so
                // the close is then skipped. (Reuses the completion machinery:
                // a throw routes through this handler's `finally`.)
                let push = self.emit(Op::PushTryHandler {
                    catch: u32::MAX,
                    finally: u32::MAX,
                });
                self.cur().handler_depth += 1;
                self.cur().finally_depth += 1;

                for el in &a.elements {
                    self.emit_iter_step_tracked(itc, done_cell); // [value]
                    match el {
                        Some(p) => self.bind_pattern_kind(p, function_scoped, is_const)?,
                        None => {
                            self.emit(Op::Pop); // elision
                        }
                    }
                }
                if let Some(rest) = &a.rest {
                    self.emit(Op::NewArray(0)); // [arr]
                    let top = self.here();
                    self.emit(Op::LoadCell(done_cell));
                    let jdone_rest = self.emit(Op::JumpIfTrue(0)); // already done -> [arr]
                    self.emit(Op::LoadCell(itc));
                    self.emit(Op::IteratorNext); // [arr, iter, result]
                    self.emit(Op::Swap);
                    self.emit(Op::Pop); // [arr, result]
                    self.emit(Op::Dup);
                    let dk = self.str_const("done");
                    self.emit(Op::GetProp(dk)); // [arr, result, done]
                    let jend = self.emit(Op::JumpIfTrue(0));
                    let vk = self.str_const("value");
                    self.emit(Op::GetProp(vk)); // [arr, value]
                    let tv = self.temp();
                    self.emit(Op::InitCell(tv)); // [arr]
                    self.array_push_value(|c| {
                        c.emit(Op::LoadCell(tv));
                        Ok(())
                    })?; // [arr]
                    self.emit(Op::Jump(top));
                    // result.done: latch done_cell, drop result.
                    let end = self.here();
                    self.patch_jump(jend, end);
                    self.emit(Op::Pop); // drop result -> [arr]
                    self.emit(Op::LoadTrue);
                    self.emit(Op::StoreCell(done_cell));
                    let jafter = self.emit(Op::Jump(0));
                    let drest = self.here();
                    self.patch_jump(jdone_rest, drest); // [arr]
                    let after_rest = self.here();
                    self.patch_jump(jafter, after_rest);
                    self.bind_pattern_kind(&rest.argument, function_scoped, is_const)?;
                }

                self.emit(Op::PopTryHandler);
                self.cur().handler_depth -= 1;
                self.cur().finally_depth -= 1;
                let normal_to_close = self.emit(Op::Jump(0));
                // Close landing (reached on normal completion and, via the
                // handler's `finally`, on a throw): IteratorClose iff not done.
                let close_ip = self.here();
                self.patch_finally(push, close_ip);
                self.patch_jump(normal_to_close, close_ip);
                self.emit(Op::LoadCell(done_cell));
                let skip_close = self.emit(Op::JumpIfTrue(0));
                self.emit(Op::LoadCell(itc));
                self.emit(Op::IteratorClose);
                let after_close = self.here();
                self.patch_jump(skip_close, after_close);
                self.emit(Op::EndFinally);
                // Iterator consumed; nothing left on the stack to drop.
            }
            BindingPattern::ObjectPattern(o) => {
                // RequireObjectCoercible(source): reject a nullish value before any
                // property access (so an empty `{}`/`{...r}` pattern still throws).
                self.emit(Op::RequireObjectCoercible);
                let mut taken: Vec<String> = Vec::new();
                let mut taken_cells: Vec<u32> = Vec::new();
                let has_rest = o.rest.is_some();
                for p in &o.properties {
                    self.emit(Op::Dup);
                    if p.computed {
                        self.compile_property_key_expr(&p.key)?;
                        if has_rest {
                            // The coerced computed key joins the rest
                            // exclusion set (CopyDataProperties excludedNames).
                            self.emit(Op::ToPropertyKey);
                            let t = self.temp();
                            self.emit(Op::InitCell(t));
                            self.emit(Op::LoadCell(t));
                            taken_cells.push(t);
                        }
                        self.emit(Op::GetPropDynamic);
                    } else {
                        let name = property_key_name(&p.key);
                        taken.push(name.clone());
                        let idx = self.str_const(&name);
                        self.emit(Op::GetProp(idx));
                    }
                    self.bind_pattern_kind(&p.value, function_scoped, is_const)?;
                }
                if let Some(rest) = &o.rest {
                    // rest object: own-enumerable copy excluding the taken keys.
                    self.emit(Op::Dup);
                    self.compile_object_rest(&taken, &taken_cells)?;
                    self.bind_pattern_kind(&rest.argument, function_scoped, is_const)?;
                }
                self.emit(Op::Pop); // drop source
            }
        }
        Ok(())
    }

    fn compile_object_rest(&mut self, taken: &[String], taken_cells: &[u32]) -> R {
        // Object rest `{ ...rest }`: CopyDataProperties with excludedNames —
        // the source's own enumerable properties minus the keys already bound
        // by preceding pattern properties (static names plus the coerced
        // computed keys parked in `taken_cells`). stack: src -> restObj
        self.emit(Op::NewObject);
        self.emit(Op::Swap); // [restObj, src]
        for k in taken {
            self.load_str(k);
        }
        for &c in taken_cells {
            self.emit(Op::LoadCell(c));
        }
        let n = (taken.len() + taken_cells.len()) as u32;
        self.emit(Op::CopyDataPropertiesExcept(n)); // [restObj]
        Ok(())
    }

    fn compile_if(&mut self, i: &IfStatement) -> R {
        self.compile_expr(&i.test)?;
        let jf = self.emit(Op::JumpIfFalse(0));
        self.compile_stmt(&i.consequent)?;
        if let Some(alt) = &i.alternate {
            let je = self.emit(Op::Jump(0));
            let else_t = self.here();
            self.patch_jump(jf, else_t);
            self.compile_stmt(alt)?;
            let end = self.here();
            self.patch_jump(je, end);
        } else {
            let end = self.here();
            self.patch_jump(jf, end);
        }
        Ok(())
    }

    fn compile_while(&mut self, w: &WhileStatement) -> R {
        let top = self.here();
        self.compile_expr(&w.test)?;
        let jf = self.emit(Op::JumpIfFalse(0));
        self.push_loop(None, true);
        self.compile_stmt(&w.body)?;
        let cont = self.here();
        self.emit(Op::Jump(top));
        let end = self.here();
        self.patch_jump(jf, end);
        self.pop_loop(end, cont);
        Ok(())
    }

    fn compile_do_while(&mut self, w: &DoWhileStatement) -> R {
        let top = self.here();
        self.push_loop(None, true);
        self.compile_stmt(&w.body)?;
        let cont = self.here();
        self.compile_expr(&w.test)?;
        self.emit(Op::JumpIfTrue(top));
        let end = self.here();
        self.pop_loop(end, cont);
        Ok(())
    }

    fn compile_for(&mut self, f: &ForStatement) -> R {
        self.enter_scope(false);
        // init
        let mut per_iter: Vec<u32> = Vec::new();
        if let Some(init) = &f.init {
            match init {
                ForStatementInit::VariableDeclaration(d) => {
                    self.compile_var_decl(d)?;
                    // Per-iteration environment for `let`/`const` loop bindings, so
                    // closures created in the body capture a distinct binding each
                    // iteration (the classic `for (let i...)` case).
                    if !matches!(d.kind, VariableDeclarationKind::Var) {
                        let top = self.fns.len() - 1;
                        for decl in &d.declarations {
                            if let BindingPattern::BindingIdentifier(id) = &decl.id {
                                if let Some(c) = self.find_cell(top, id.name.as_str()) {
                                    per_iter.push(c);
                                }
                            }
                        }
                    }
                }
                _ => {
                    let e = init.as_expression().unwrap();
                    self.compile_expr(e)?;
                    self.emit(Op::Pop);
                }
            }
        }
        // Initial CreatePerIterationEnvironment.
        self.copy_cells(&per_iter);
        let top = self.here();
        let mut jf = None;
        if let Some(test) = &f.test {
            self.compile_expr(test)?;
            jf = Some(self.emit(Op::JumpIfFalse(0)));
        }
        self.push_loop(None, true);
        self.compile_stmt(&f.body)?;
        let cont = self.here();
        // Copy bindings before the increment (spec ForBodyEvaluation order).
        self.copy_cells(&per_iter);
        if let Some(update) = &f.update {
            self.compile_expr(update)?;
            self.emit(Op::Pop);
        }
        self.emit(Op::Jump(top));
        let end = self.here();
        if let Some(j) = jf {
            self.patch_jump(j, end);
        }
        self.pop_loop(end, cont);
        self.exit_scope();
        Ok(())
    }

    fn compile_for_in(&mut self, f: &ForInStatement) -> R {
        // Enumerator state lives in the frame (frame.enumerators), so the operand
        // stack baseline inside the body is empty — break/continue need no cleanup.
        self.enter_scope(false);
        // ForIn/OfHeadEvaluation: a lexical head's bound names are in TDZ while
        // the enumerated expression is evaluated (`for (let x in [x])`), then
        // discarded — the per-iteration binding is created fresh in the body.
        let head_lexical = matches!(
            &f.left,
            ForStatementLeft::VariableDeclaration(d)
                if !matches!(d.kind, VariableDeclarationKind::Var)
        );
        if head_lexical {
            self.enter_scope(false);
            if let ForStatementLeft::VariableDeclaration(d) = &f.left {
                let is_const = matches!(d.kind, VariableDeclarationKind::Const);
                let mut names = Vec::new();
                collect_pattern_names(&d.declarations[0].id, &mut names);
                for n in &names {
                    let cell = self.declare_kind(n, false, is_const);
                    self.emit(Op::InitCellTdz(cell));
                }
            }
        }
        self.compile_expr(&f.right)?;
        if head_lexical {
            self.exit_scope();
        }
        self.emit(Op::ForInEnumerate); // pushes enumerator id
        self.emit(Op::Pop); // id discarded; enumerator tracked on the frame
        self.push_loop(None, true);
        let top = self.here();
        self.emit(Op::ForInNext); // pushes (key, has_next)
        let jf = self.emit(Op::JumpIfFalse(0)); // consumes has_next; key remains
                                                // not-done: bind key, run body
        self.enter_scope(false);
        self.bind_for_target(&f.left)?; // consumes key
        self.compile_stmt(&f.body)?;
        self.exit_scope();
        self.emit(Op::Jump(top));
        let after_pop = self.here();
        self.patch_jump(jf, after_pop);
        self.emit(Op::Pop); // pop the leftover key (exhausted: undefined)
        let end = self.here();
        self.emit(Op::ForInPop);
        self.pop_loop(end, top);
        self.exit_scope();
        Ok(())
    }

    fn compile_for_of(&mut self, f: &ForOfStatement) -> R {
        // Keep the iterator in a synthetic cell so the body's stack baseline is
        // empty (clean break/continue).
        self.enter_scope(false);
        // ForIn/OfHeadEvaluation: a lexical (`let`/`const`/`using`) head
        // instantiates its bound names in TDZ *before* the iterable expression
        // is evaluated, so an iterable that references a loop variable throws a
        // ReferenceError (`for (let x of [x]) {}`). The per-iteration binding is
        // a separate, fresh declaration created in the body scope below, so this
        // head environment is discarded right after the iterable is evaluated.
        let head_lexical = matches!(
            &f.left,
            ForStatementLeft::VariableDeclaration(d)
                if !matches!(d.kind, VariableDeclarationKind::Var)
        );
        if head_lexical {
            self.enter_scope(false);
            if let ForStatementLeft::VariableDeclaration(d) = &f.left {
                let is_const = matches!(d.kind, VariableDeclarationKind::Const);
                let mut names = Vec::new();
                collect_pattern_names(&d.declarations[0].id, &mut names);
                for n in &names {
                    let cell = self.declare_kind(n, false, is_const);
                    self.emit(Op::InitCellTdz(cell));
                }
            }
        }
        self.compile_expr(&f.right)?;
        if head_lexical {
            self.exit_scope();
        }
        if f.r#await {
            self.emit(Op::GetAsyncIterator);
        } else {
            self.emit(Op::GetIterator);
        }
        let iter_cell = self.declare("%iter", false);
        self.emit(Op::InitCell(iter_cell));
        // Iterator record: the next method is read ONCE, at GetIterator time
        // (a `next` getter must not fire per iteration), and a non-callable
        // one already fails here via the call below.
        let next_k = self.str_const("next");
        let next_cell = self.temp();
        self.emit(Op::LoadCell(iter_cell));
        self.emit(Op::GetProp(next_k));
        self.emit(Op::InitCell(next_cell));

        // A finally-style close handler covers the BINDING + BODY of each
        // iteration, so an abrupt completion there (break / return / throw /
        // continue to an outer loop) runs IteratorClose. The `next()` call and
        // the done/value reads run OUTSIDE it: per spec an error from the
        // iterator protocol itself does NOT close the iterator. Normal
        // exhaustion doesn't close either. `continue` to THIS loop jumps to
        // `top`, which pops the handler before the next protocol round.
        let outer_hd = self.cur().handler_depth;
        let outer_fd = self.cur().finally_depth;
        let entry = self.emit(Op::Jump(0)); // first iteration: handler not yet pushed
        let top = self.here();
        self.emit(Op::PopTryHandler);
        let call_next = self.here();
        self.patch_jump(entry, call_next);
        self.emit(Op::LoadCell(next_cell));
        self.emit(Op::LoadCell(iter_cell)); // [next, iter]
        self.emit(Op::Call(0)); // [result]
        if f.r#await {
            // for-await: the iterator's next() returns a promise of the result;
            // await it before reading done/value (await of a non-promise is a
            // no-op, so this also works for sync iterables of plain values).
            self.emit(Op::Await);
        }
        self.emit(Op::RequireIterResult);
        self.emit(Op::Dup);
        let done_k = self.str_const("done");
        self.emit(Op::GetProp(done_k)); // [result, done]
        let jt = self.emit(Op::JumpIfTrue(0)); // consumes done; [result]
        let value_k = self.str_const("value");
        self.emit(Op::GetProp(value_k)); // [value]
        let close_push = self.emit(Op::PushTryHandler {
            catch: u32::MAX,
            finally: u32::MAX,
        });
        self.cur().handler_depth += 1;
        self.cur().finally_depth += 1;
        self.push_loop(None, true);
        // `break` unwinds to *outside* the close handler (so it closes); `continue`
        // stays inside (so it reaches `top`, which pops without closing).
        if let Some(lp) = self.cur().loops.last_mut() {
            lp.brk_handler_depth = outer_hd;
            lp.brk_finally_depth = outer_fd;
        }
        self.enter_scope(false);
        // `for (using x of …)`: a fresh dispose capability per ITERATION —
        // the resource is recorded before the binding initializes and
        // disposed when the iteration ends (normally or abruptly).
        let head_using_kind = match &f.left {
            ForStatementLeft::VariableDeclaration(d) => match d.kind {
                VariableDeclarationKind::Using => Some(false),
                VariableDeclarationKind::AwaitUsing => Some(true),
                _ => None,
            },
            _ => None,
        };
        if let Some(is_await) = head_using_kind {
            self.compile_with_dispose_scope(is_await, |c| {
                c.emit(Op::TrackDisposable { is_await });
                c.bind_for_target(&f.left)?; // consumes value
                c.compile_stmt(&f.body)
            })?;
        } else {
            self.bind_for_target(&f.left)?; // consumes value
            self.compile_stmt(&f.body)?;
        }
        self.exit_scope();
        self.emit(Op::Jump(top)); // `top` pops the close handler before next()
        self.cur().handler_depth -= 1;
        self.cur().finally_depth -= 1;

        // Normal-exhaustion (done) path: the close handler is not active here
        // (the protocol round runs outside it), so just drop the result.
        let done_label = self.here();
        self.patch_jump(jt, done_label);
        self.emit(Op::Pop); // pop result on done path
        let skip_close = self.emit(Op::Jump(0));

        // Close landing: reached only on abrupt completion (via the handler's
        // `finally`). `EndFinally` then resumes the parked completion.
        let close_ip = self.here();
        self.patch_finally(close_push, close_ip);
        self.emit(Op::LoadCell(iter_cell));
        self.emit(Op::IteratorClose);
        self.emit(Op::EndFinally);

        let end = self.here();
        self.patch_jump(skip_close, end);
        self.pop_loop(end, top);
        self.exit_scope();
        Ok(())
    }

    fn bind_for_target(&mut self, left: &ForStatementLeft) -> R {
        match left {
            ForStatementLeft::VariableDeclaration(d) => {
                let function_scoped = matches!(d.kind, VariableDeclarationKind::Var);
                // `const`/`using` loop bindings are immutable: assignment in
                // the body is a TypeError.
                let is_const = matches!(
                    d.kind,
                    VariableDeclarationKind::Const
                        | VariableDeclarationKind::Using
                        | VariableDeclarationKind::AwaitUsing
                );
                let decl = &d.declarations[0];
                self.bind_pattern_kind(&decl.id, function_scoped, is_const)?;
            }
            _ => {
                // assignment target (existing binding / member)
                let target = left.as_assignment_target().unwrap();
                self.assign_target(target)?;
            }
        }
        Ok(())
    }

    /// Spec `UpdateEmpty(completion, undefined)`: the compound statements
    /// (if/loops/switch/try/with/labelled) produce **undefined** — not the
    /// preceding statement's value — when their own body produces nothing.
    /// Zeroing the completion register on statement entry implements that
    /// (only meaningful where the register exists: script/eval bodies).
    fn zero_completion(&mut self) {
        if self.cur_ref().track_completion {
            self.emit(Op::LoadUndefined);
            self.store_binding("%completion");
        }
    }

    fn compile_try(&mut self, t: &TryStatement) -> R {
        if let Some(finalizer) = &t.finalizer {
            // Wrap try/catch with a finally landing pad.
            self.compile_try_with_finally(t, finalizer)
        } else {
            self.compile_try_catch_only(t)
        }
    }

    /// Set the `finally` target of a previously-emitted `PushTryHandler`.
    fn patch_finally(&mut self, at: usize, target: u32) {
        if let Op::PushTryHandler { finally, .. } = &mut self.cur().code[at] {
            *finally = target;
        } else {
            panic!("patch_finally on non-PushTryHandler op");
        }
    }

    fn compile_try_catch_only(&mut self, t: &TryStatement) -> R {
        let handler = t.handler.as_ref();
        let push = self.emit(Op::PushTryHandler {
            catch: 0,
            finally: u32::MAX,
        });
        self.cur().handler_depth += 1;
        self.compile_block(&t.block.body)?;
        self.emit(Op::PopTryHandler);
        self.cur().handler_depth -= 1;
        let skip = self.emit(Op::Jump(0));
        let catch_start = self.here();
        self.patch_jump(push, catch_start);
        // The try block's partial completion value is discarded when it threw:
        // the catch clause's own completion is what UpdateEmpty sees.
        // (Stack-neutral: the exception stays on top.)
        self.zero_completion();
        // exception is on stack
        self.enter_scope(false);
        if let Some(h) = handler {
            if let Some(param) = &h.param {
                self.bind_pattern(&param.pattern, false)?;
            } else {
                self.emit(Op::Pop);
            }
            self.hoist_funcs(&h.body.body)?;
            for s in &h.body.body {
                self.compile_stmt(s)?;
            }
        } else {
            self.emit(Op::Pop);
        }
        self.exit_scope();
        let end = self.here();
        self.patch_jump(skip, end);
        Ok(())
    }

    /// `try { body } catch (e) { handler } finally { fin }` compiled to a single
    /// finalizer landing pad reached by every completion path:
    /// - normal: `body` falls through and jumps to the finalizer (no parked
    ///   completion → `EndFinally` falls through to the end);
    /// - throw in `body`: the try-body handler's `catch` (or, with no catch, its
    ///   `finally`) is taken by `do_completion`;
    /// - throw/return/break/continue in `body` or `catch`: `do_completion` runs
    ///   the finalizer with the completion parked, and `EndFinally` resumes it.
    /// This single-copy model (vs. duplicating the finalizer per path) is what
    /// makes non-local exits run `finally`.
    fn compile_try_with_finally(&mut self, t: &TryStatement, finalizer: &BlockStatement) -> R {
        let push = self.emit(Op::PushTryHandler {
            catch: u32::MAX,
            finally: u32::MAX,
        });
        self.cur().handler_depth += 1;
        self.cur().finally_depth += 1;
        self.compile_block(&t.block.body)?;
        self.emit(Op::PopTryHandler);
        self.cur().handler_depth -= 1;
        // Normal completion: into the finalizer with no parked completion.
        let normal_to_fin = self.emit(Op::Jump(0));

        let mut inner_push = None;
        let mut catch_normal_to_fin = None;
        if let Some(h) = &t.handler {
            // Throw in the try body lands here (exception on stack).
            let catch_ip = self.here();
            self.patch_jump(push, catch_ip);
            // A finally-only handler so an abrupt completion in the catch body
            // also runs the finalizer.
            let ip = self.emit(Op::PushTryHandler {
                catch: u32::MAX,
                finally: u32::MAX,
            });
            inner_push = Some(ip);
            self.cur().handler_depth += 1;
            // Discard the try block's partial completion value (see
            // compile_try_catch_only).
            self.zero_completion();
            self.enter_scope(false);
            if let Some(param) = &h.param {
                self.bind_pattern(&param.pattern, false)?;
            } else {
                self.emit(Op::Pop);
            }
            self.hoist_funcs(&h.body.body)?;
            for s in &h.body.body {
                self.compile_stmt(s)?;
            }
            self.exit_scope();
            self.emit(Op::PopTryHandler);
            self.cur().handler_depth -= 1;
            catch_normal_to_fin = Some(self.emit(Op::Jump(0)));
        }

        // Finalizer landing pad. Every handler installed above routes its
        // `finally` here; the normal/catch-normal paths jump here too.
        let fin_ip = self.here();
        self.patch_finally(push, fin_ip);
        if let Some(ip) = inner_push {
            self.patch_finally(ip, fin_ip);
        }
        self.patch_jump(normal_to_fin, fin_ip);
        if let Some(j) = catch_normal_to_fin {
            self.patch_jump(j, fin_ip);
        }
        // The finalizer body itself is no longer inside this try's finally region.
        self.cur().finally_depth -= 1;
        // A normally-completing finalizer's value is DISCARDED (spec: "If F is
        // a normal completion, set F to B"): save/restore the completion
        // register around the finalizer body.
        let saved = if self.cur_ref().track_completion {
            let c = self.temp();
            self.load_binding("%completion");
            self.emit(Op::InitCell(c));
            Some(c)
        } else {
            None
        };
        self.compile_block(&finalizer.body)?;
        if let Some(c) = saved {
            self.emit(Op::LoadCell(c));
            self.store_binding("%completion");
        }
        self.emit(Op::EndFinally);
        Ok(())
    }

    fn compile_switch(&mut self, s: &SwitchStatement) -> R {
        self.compile_expr(&s.discriminant)?; // discriminant on stack
        self.enter_scope(false);
        self.push_loop(None, false); // for break (not continue)
        let mut case_jumps: Vec<(usize, usize)> = Vec::new(); // (case_index, jump_addr)
        let mut default_index: Option<usize> = None;
        for (idx, case) in s.cases.iter().enumerate() {
            if let Some(test) = &case.test {
                self.emit(Op::Dup);
                self.compile_expr(test)?;
                self.emit(Op::StrictEq);
                let jt = self.emit(Op::JumpIfTrue(0));
                case_jumps.push((idx, jt));
            } else {
                default_index = Some(idx);
            }
        }
        // No match: jump to default or end.
        let to_default = self.emit(Op::Jump(0));
        // Emit case bodies.
        let mut body_starts: Vec<u32> = Vec::with_capacity(s.cases.len());
        for case in &s.cases {
            let start = self.here();
            body_starts.push(start);
            for st in &case.consequent {
                self.compile_stmt(st)?;
            }
        }
        let end = self.here();
        // Patch case test jumps.
        for (idx, at) in case_jumps {
            self.patch_jump(at, body_starts[idx]);
        }
        match default_index {
            Some(di) => self.patch_jump(to_default, body_starts[di]),
            None => self.patch_jump(to_default, end),
        }
        self.pop_loop(end, end);
        self.emit(Op::Pop); // pop discriminant
        self.exit_scope();
        Ok(())
    }

    fn compile_with(&mut self, w: &WithStatement) -> R {
        // Evaluate the object, ToObject + push as a dynamic scope.
        self.compile_expr(&w.object)?;
        self.emit(Op::PushWithScope);
        // A try-handler landing pad pops the with-scope and rethrows so a throw
        // inside the body cannot leak the environment. (The runtime `unwind`
        // also truncates `with_scope` to this handler's depth, but the explicit
        // pad keeps the bytecode self-describing.)
        let push = self.emit(Op::PushTryHandler {
            catch: 0,
            finally: u32::MAX,
        });
        // Compile the body with identifier resolution routed through the
        // with-scope (the `with_depth` bump makes load/store_binding emit the
        // dynamic Load/StoreName ops).
        self.cur().with_depth += 1;
        let body_res = self.compile_stmt(&w.body);
        self.cur().with_depth -= 1;
        body_res?;
        self.emit(Op::PopTryHandler);
        // Normal completion: pop the with-scope and jump past the landing pad.
        self.emit(Op::PopWithScope);
        let skip = self.emit(Op::Jump(0));
        // Landing pad (pending exception on the stack): drop the with-scope and
        // rethrow.
        let land = self.here();
        self.patch_jump(push, land);
        self.emit(Op::PopWithScope);
        self.emit(Op::Throw);
        let end = self.here();
        self.patch_jump(skip, end);
        Ok(())
    }

    fn compile_labeled(&mut self, l: &LabeledStatement) -> R {
        let label = l.label.name.as_str().to_string();
        // For labeled loops, the loop's own push_loop should carry the label.
        // Simple approach: push a non-loop context capturing breaks to this label,
        // wrapping the inner statement.
        match &l.body {
            Statement::WhileStatement(_)
            | Statement::DoWhileStatement(_)
            | Statement::ForStatement(_)
            | Statement::ForInStatement(_)
            | Statement::ForOfStatement(_) => {
                self.compile_labeled_loop(&label, &l.body)?;
            }
            _ => {
                self.push_loop(Some(label), false);
                self.compile_stmt(&l.body)?;
                let end = self.here();
                self.pop_loop(end, end);
            }
        }
        Ok(())
    }

    // ---- loop/break/continue bookkeeping ----

    fn push_loop(&mut self, label: Option<String>, is_loop: bool) {
        let label = label.or_else(|| self.pending_label.take());
        let with_depth = self.cur().with_depth;
        let handler_depth = self.cur().handler_depth;
        let finally_depth = self.cur().finally_depth;
        self.cur().loops.push(LoopCtx {
            label,
            break_jumps: Vec::new(),
            continue_jumps: Vec::new(),
            is_loop,
            with_depth,
            cont_handler_depth: handler_depth,
            cont_finally_depth: finally_depth,
            brk_handler_depth: handler_depth,
            brk_finally_depth: finally_depth,
        });
    }

    /// The `with` nesting depth at the loop/label a `break`/`continue` targets,
    /// so the jump can pop the with-scopes entered between here and there.
    fn target_with_depth(&self, label: Option<&str>, is_continue: bool) -> Option<u32> {
        let loops = &self.cur_ref().loops;
        let c = match label {
            Some(l) if is_continue => loops
                .iter()
                .rev()
                .find(|c| c.is_loop && c.label.as_deref() == Some(l)),
            Some(l) => loops.iter().rev().find(|c| c.label.as_deref() == Some(l)),
            None if is_continue => loops.iter().rev().find(|c| c.is_loop),
            None => loops.iter().rev().find(|c| c.is_loop || c.label.is_none()),
        };
        c.map(|c| c.with_depth)
    }

    /// The try-handler / try-finally depths at the loop/label a `break`/`continue`
    /// targets — `(handler_depth, finally_depth)`. Mirrors [`target_with_depth`]'s
    /// target selection so the completion machinery unwinds to the right boundary.
    fn target_handler_finally_depth(
        &self,
        label: Option<&str>,
        is_continue: bool,
    ) -> Option<(u32, u32)> {
        let loops = &self.cur_ref().loops;
        let c = match label {
            Some(l) if is_continue => loops
                .iter()
                .rev()
                .find(|c| c.is_loop && c.label.as_deref() == Some(l)),
            Some(l) => loops.iter().rev().find(|c| c.label.as_deref() == Some(l)),
            None if is_continue => loops.iter().rev().find(|c| c.is_loop),
            None => loops.iter().rev().find(|c| c.is_loop || c.label.is_none()),
        };
        c.map(|c| {
            if is_continue {
                (c.cont_handler_depth, c.cont_finally_depth)
            } else {
                (c.brk_handler_depth, c.brk_finally_depth)
            }
        })
    }

    /// Emit `PopWithScope` for each `with` scope between the current depth and
    /// `target` (used before a break/continue/return that leaves them).
    fn emit_pop_with_to(&mut self, target: u32) {
        let cur = self.cur().with_depth;
        for _ in target..cur {
            self.emit(Op::PopWithScope);
        }
    }

    /// Emit the jump for a `break`/`continue`, returning its patch index. When
    /// the statement crosses one or more enclosing `finally` regions, it routes
    /// through `Op::CompletionJump` (which runs those finallys, unwinding the
    /// handler stack to the target loop's depth, before jumping); otherwise it is
    /// a plain `Op::Jump` exactly as before. Both have their target patched by
    /// `register_break`/`register_continue` via `patch_jump`.
    fn emit_break_continue_jump(&mut self, label: Option<&str>, is_continue: bool) -> usize {
        let cur_finally = self.cur_ref().finally_depth;
        if let Some((handler_depth, finally_depth)) =
            self.target_handler_finally_depth(label, is_continue)
        {
            if cur_finally > finally_depth {
                return self.emit(Op::CompletionJump {
                    target: 0,
                    boundary: handler_depth,
                });
            }
        }
        self.emit(Op::Jump(0))
    }

    fn load_num(&mut self, n: f64) {
        let i = self.konst(Const::Number(n));
        self.emit(Op::LoadConst(i));
    }

    fn pop_loop(&mut self, break_target: u32, continue_target: u32) {
        let ctx = self.cur().loops.pop().unwrap();
        for at in ctx.break_jumps {
            self.patch_jump(at, break_target);
        }
        for at in ctx.continue_jumps {
            self.patch_jump(at, continue_target);
        }
    }

    fn register_break(&mut self, at: usize, label: Option<String>) -> R {
        let loops = &mut self.cur().loops;
        let target = match &label {
            Some(l) => loops
                .iter_mut()
                .rev()
                .find(|c| c.label.as_deref() == Some(l)),
            None => loops
                .iter_mut()
                .rev()
                .find(|c| c.is_loop || c.label.is_none()),
        };
        match target {
            Some(c) => {
                c.break_jumps.push(at);
                Ok(())
            }
            None => Err("Illegal break statement".into()),
        }
    }

    fn register_continue(&mut self, at: usize, label: Option<String>) -> R {
        let loops = &mut self.cur().loops;
        let target = match &label {
            Some(l) => loops
                .iter_mut()
                .rev()
                .find(|c| c.is_loop && c.label.as_deref() == Some(l)),
            None => loops.iter_mut().rev().find(|c| c.is_loop),
        };
        match target {
            Some(c) => {
                c.continue_jumps.push(at);
                Ok(())
            }
            None => Err("Illegal continue statement".into()),
        }
    }

    fn compile_labeled_loop(&mut self, label: &str, body: &Statement) -> R {
        // Re-dispatch to the specific loop compiler but with a labeled context.
        // We set a pending label that the next push_loop consumes.
        self.pending_label = Some(label.to_string());
        self.compile_stmt(body)
    }
}

// =========================================================================
// Expressions
// =========================================================================

enum ArgForm {
    Count(u32),
    Spread,
}

impl Compiler {
    fn compile_expr(&mut self, expr: &Expression) -> R {
        match expr {
            Expression::BooleanLiteral(b) => {
                self.emit(if b.value { Op::LoadTrue } else { Op::LoadFalse });
            }
            Expression::NullLiteral(_) => {
                self.emit(Op::LoadNull);
            }
            Expression::NumericLiteral(n) => self.load_num(n.value),
            Expression::BigIntLiteral(b) => {
                // `b.value` is normalized to base-10 with no `n` suffix.
                let idx = self.konst(Const::BigInt(Rc::from(b.value.as_str())));
                self.emit(Op::LoadConst(idx));
            }
            Expression::StringLiteral(s) => self.load_string_literal(s),
            Expression::TemplateLiteral(t) => self.compile_template(t)?,
            Expression::Identifier(id) => {
                // ClassFieldDefinition early error: ContainsArguments — the
                // initializer (and any direct eval / arrow inside it) may not
                // reference `arguments`.
                if self.in_field_initializer && id.name == "arguments" {
                    return Err("'arguments' is not allowed in class field initializer".to_string());
                }
                self.load_binding(id.name.as_str())
            }
            Expression::ThisExpression(_) => self.load_binding("%this"),
            Expression::MetaProperty(m) => {
                if m.meta.name.as_str() == "new" {
                    self.load_binding("%newtarget");
                } else {
                    self.emit(Op::LoadUndefined); // import.meta
                }
            }
            Expression::ArrayExpression(a) => self.compile_array(a)?,
            Expression::ObjectExpression(o) => self.compile_object(o)?,
            Expression::ParenthesizedExpression(p) => self.compile_expr(&p.expression)?,
            Expression::SequenceExpression(s) => {
                for (i, e) in s.expressions.iter().enumerate() {
                    self.compile_expr(e)?;
                    if i + 1 < s.expressions.len() {
                        self.emit(Op::Pop);
                    }
                }
            }
            Expression::AssignmentExpression(a) => self.compile_assignment(a)?,
            Expression::BinaryExpression(b) => self.compile_binary(b)?,
            Expression::LogicalExpression(l) => self.compile_logical(l)?,
            Expression::UnaryExpression(u) => self.compile_unary(u)?,
            Expression::UpdateExpression(u) => self.compile_update(u)?,
            Expression::ConditionalExpression(c) => {
                self.compile_expr(&c.test)?;
                let jf = self.emit(Op::JumpIfFalse(0));
                self.compile_expr(&c.consequent)?;
                let je = self.emit(Op::Jump(0));
                let alt = self.here();
                self.patch_jump(jf, alt);
                self.compile_expr(&c.alternate)?;
                let end = self.here();
                self.patch_jump(je, end);
            }
            Expression::CallExpression(c) => self.compile_call(c)?,
            Expression::NewExpression(n) => self.compile_new(n)?,
            Expression::ChainExpression(c) => self.compile_chain(&c.expression)?,
            Expression::PrivateInExpression(p) => {
                // `#x in obj` — PrivateElementFind on the RHS's own
                // [[PrivateElements]] (field, method, or accessor alike).
                self.compile_expr(&p.right)?;
                let key = self.private_storage_key(p.left.name.as_str())?;
                let k = self.str_const(&key);
                self.emit(Op::PrivateHasOwn(k));
            }
            Expression::StaticMemberExpression(m) => {
                if matches!(m.object, Expression::Super(_)) {
                    // `super.x`: this-binding (TDZ-checked), GetSuperBase, then
                    // Get(base, "x") with `this` as receiver.
                    self.emit_super_ref()?;
                    let k = self.str_const(m.property.name.as_str());
                    self.emit(Op::SuperGet(k));
                } else {
                    self.compile_expr(&m.object)?;
                    if m.optional {
                        let j = self.emit(Op::JumpIfNullish(0));
                        self.chain_jumps.push(j);
                    }
                    let k = self.str_const(m.property.name.as_str());
                    self.emit(Op::GetProp(k));
                }
            }
            Expression::ComputedMemberExpression(m) => {
                if matches!(m.object, Expression::Super(_)) {
                    // `super[expr]`: this-binding (TDZ), the key expression,
                    // THEN GetSuperBase (MakeSuperPropertyReference order) —
                    // ToPropertyKey runs inside SuperGetDynamic, after the
                    // base fetch.
                    if !self.super_prop_allowed() {
                        return Err(
                            "'super' keyword is only valid inside a class or method".to_string()
                        );
                    }
                    self.load_binding("%this");
                    self.compile_expr(&m.expression)?;
                    self.emit(Op::GetSuperBase);
                    self.emit(Op::Swap); // [this, base, key]
                    self.emit(Op::SuperGetDynamic);
                } else {
                    self.compile_expr(&m.object)?;
                    if m.optional {
                        let j = self.emit(Op::JumpIfNullish(0));
                        self.chain_jumps.push(j);
                    }
                    self.compile_expr(&m.expression)?;
                    self.emit(Op::GetPropDynamic);
                }
            }
            Expression::PrivateFieldExpression(m) => {
                // Private names resolve lexically to their declaring class
                // (suffixed storage keys + per-instance brand checks); see
                // `resolve_private`/`emit_private_get_op`.
                self.compile_expr(&m.object)?;
                self.emit_private_get_op(m.field.name.as_str())?;
            }
            Expression::FunctionExpression(f) => {
                let name = f.id.as_ref().map(|i| i.name.as_str().to_string());
                if let Some(n) = &name {
                    // A named function expression binds its own name in a
                    // dedicated scope around the closure: visible inside the
                    // body, immutable (strict write TypeError, sloppy ignored).
                    self.enter_scope(false);
                    let cell = self.declare_fn_name(n);
                    self.compile_function(f, Some(n))?;
                    // StoreCell (not InitCell): mutate the cell in place so the
                    // body's captured self-reference observes the closure.
                    self.emit(Op::Dup);
                    self.emit(Op::StoreCell(cell));
                    self.exit_scope();
                } else {
                    self.compile_function(f, None)?;
                }
            }
            Expression::ArrowFunctionExpression(a) => self.compile_arrow(a, None)?,
            Expression::ClassExpression(c) => {
                let name = c.id.as_ref().map(|i| i.name.as_str().to_string());
                self.compile_class(c, name.as_deref())?;
            }
            Expression::AwaitExpression(a) => {
                self.compile_expr(&a.argument)?;
                self.emit(Op::Await);
            }
            Expression::YieldExpression(y) => self.compile_yield(y)?,
            Expression::RegExpLiteral(r) => {
                let pat_text = r.regex.pattern.text.as_str();
                let flags_text = r.regex.flags.to_string();
                // Validate the literal at compile time so an invalid pattern is a
                // parse-phase SyntaxError (before any code runs), matching the many
                // `negative: { phase: parse }` regex tests. Runtime `new RegExp(str)`
                // still validates separately when the source is dynamic.
                crate::regexp::regex_is_valid(pat_text, &flags_text)?;
                let pat = self.str_const(pat_text);
                let flags = self.str_const(flags_text.as_str());
                self.emit(Op::NewRegExp {
                    pattern: pat,
                    flags,
                });
            }
            Expression::TaggedTemplateExpression(t) => self.compile_tagged_template(t)?,
            // TS wrappers: types are stripped — compile the inner expression.
            Expression::TSAsExpression(e) => self.compile_expr(&e.expression)?,
            Expression::TSSatisfiesExpression(e) => self.compile_expr(&e.expression)?,
            Expression::TSNonNullExpression(e) => self.compile_expr(&e.expression)?,
            Expression::TSTypeAssertion(e) => self.compile_expr(&e.expression)?,
            Expression::TSInstantiationExpression(e) => self.compile_expr(&e.expression)?,
            Expression::Super(_) => {
                return Err("super is not supported in this position".into());
            }
            Expression::ImportExpression(e) => {
                // Evaluate the specifier (sync; its evaluation may throw), then
                // produce a Promise. Module loading is unsupported, so the Promise
                // rejects — but `import(x).then(...)`/`.catch(...)` now work.
                self.compile_expr(&e.source)?;
                self.emit(Op::DynamicImport);
            }
            _ => {
                return Err(format!("unsupported expression: {:?}", expr_kind(expr)));
            }
        }
        Ok(())
    }

    fn compile_named_expr(&mut self, expr: &Expression, name: &str) -> R {
        match expr {
            // Named evaluation reaches "through" the parenthesized-expression cover
            // grammar: `x = (function(){})` names the function, but `x = (0, f)` (a
            // sequence) does not — only a sole parenthesized operand is unwrapped.
            Expression::ParenthesizedExpression(p) => self.compile_named_expr(&p.expression, name),
            Expression::FunctionExpression(f) if f.id.is_none() => {
                self.compile_function(f, Some(name))
            }
            Expression::ArrowFunctionExpression(a) => self.compile_arrow(a, Some(name)),
            Expression::ClassExpression(c) if c.id.is_none() => self.compile_class(c, Some(name)),
            _ => self.compile_expr(expr),
        }
    }

    fn compile_array(&mut self, a: &ArrayExpression) -> R {
        let has_spread = a
            .elements
            .iter()
            .any(|e| matches!(e, ArrayExpressionElement::SpreadElement(_)));
        if !has_spread {
            for el in &a.elements {
                match el {
                    ArrayExpressionElement::Elision(_) => {
                        self.emit(Op::LoadHole);
                    }
                    other => {
                        let e = other.as_expression().unwrap();
                        self.compile_expr(e)?;
                    }
                }
            }
            self.emit(Op::NewArray(a.elements.len() as u32));
        } else {
            self.emit(Op::NewArray(0));
            for el in &a.elements {
                match el {
                    ArrayExpressionElement::SpreadElement(s) => {
                        self.compile_expr(&s.argument)?; // [arr, iterable]
                        self.emit(Op::ArraySpread); // [arr]
                    }
                    ArrayExpressionElement::Elision(_) => {
                        self.array_push_value(|c| {
                            c.emit(Op::LoadHole);
                            Ok(())
                        })?;
                    }
                    other => {
                        let e = other.as_expression().unwrap();
                        self.array_push_value(|c| c.compile_expr(e))?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Append one value to the array on top of stack (spread-mode array literal).
    /// Keeps the array on the stack.
    fn array_push_value(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        // [arr]
        self.emit(Op::Dup); // [arr, arr]
        self.emit(Op::Dup); // [arr, arr, arr]
        let push = self.str_const("push");
        self.emit(Op::GetProp(push)); // [arr, arr, pushfn]
        self.emit(Op::Swap); // [arr, pushfn, arr]
        f(self)?; // [arr, pushfn, arr, value]
        self.emit(Op::Call(1)); // [arr, len]
        self.emit(Op::Pop); // [arr]
        Ok(())
    }

    fn compile_object(&mut self, o: &ObjectExpression) -> R {
        self.emit(Op::NewObject);
        // Duplicate plain `__proto__: v` definitions are an early SyntaxError
        // (computed/shorthand/method forms don't count).
        fn is_proto_def(p: &ObjectProperty) -> bool {
            !p.computed
                && !p.shorthand
                && !p.method
                && matches!(p.kind, PropertyKind::Init)
                && property_key_name(&p.key) == "__proto__"
        }
        let proto_defs = o
            .properties
            .iter()
            .filter(|pr| match pr {
                ObjectPropertyKind::ObjectProperty(p) => is_proto_def(p),
                _ => false,
            })
            .count();
        if proto_defs > 1 {
            return Err(
                "SyntaxError: Duplicate __proto__ fields are not allowed in object literals".into(),
            );
        }
        for prop in &o.properties {
            match prop {
                ObjectPropertyKind::ObjectProperty(p) => {
                    // [obj]
                    // `__proto__: v` (plain, non-computed, non-shorthand) is
                    // NOT a property definition: it sets the [[Prototype]].
                    if is_proto_def(p) {
                        self.compile_expr(&p.value)?; // [obj, v]
                        self.emit(Op::SetProtoFromLiteral); // [obj]
                        continue;
                    }
                    let is_accessor = matches!(p.kind, PropertyKind::Get | PropertyKind::Set);
                    if p.computed {
                        self.compile_property_key_expr(&p.key)?; // [obj, key]
                                                                 // ToPropertyKey NOW (spec: ComputedPropertyName
                                                                 // evaluation runs before the value), so key-coercion
                                                                 // side effects precede the value and never re-run.
                        self.emit(Op::ToPropertyKey);
                    } else {
                        let name = property_key_name(&p.key);
                        self.load_str(&name); // [obj, key]
                    }
                    // A concise method (`{ m(){} }`) or accessor carries a
                    // [[HomeObject]] so its `super.prop` resolves against this
                    // object; a plain data property (`{ m: function(){} }`) does
                    // not. Flag the next function so its `super` uses the home.
                    let is_method = p.method || is_accessor;
                    if is_method {
                        self.pending_home_super = true;
                        self.pending_method = true;
                    }
                    // value
                    if !p.computed {
                        // An accessor's function name is prefixed (`get x`/`set x`)
                        // per SetFunctionName with a `prefix`.
                        let nm = match p.kind {
                            PropertyKind::Get => format!("get {}", property_key_name(&p.key)),
                            PropertyKind::Set => format!("set {}", property_key_name(&p.key)),
                            PropertyKind::Init => property_key_name(&p.key),
                        };
                        self.compile_named_expr(&p.value, &nm)?;
                    } else {
                        self.compile_expr(&p.value)?;
                        // Computed key + anonymous value: NamedEvaluation takes
                        // the runtime key (with get/set prefix for accessors).
                        if Self::is_anonymous_fn_expr(&p.value) {
                            let prefix = self.str_const(match p.kind {
                                PropertyKind::Get => "get",
                                PropertyKind::Set => "set",
                                PropertyKind::Init => "",
                            });
                            self.emit(Op::SetFunctionNameFromKey(prefix));
                        }
                    }
                    self.pending_home_super = false;
                    self.pending_method = false;
                    if is_method {
                        // [obj, key, value] — stamp value.[[HomeObject]] = obj.
                        self.emit(Op::SetHomeObject);
                    }
                    match p.kind {
                        PropertyKind::Init => {
                            self.emit(Op::DefineField);
                        }
                        PropertyKind::Get => {
                            self.emit(Op::DefineGetter);
                        }
                        PropertyKind::Set => {
                            self.emit(Op::DefineSetter);
                        }
                    }
                }
                ObjectPropertyKind::SpreadProperty(s) => {
                    self.compile_expr(&s.argument)?; // [obj, src]
                    self.emit(Op::ObjectSpread); // [obj]
                }
            }
        }
        Ok(())
    }

    /// Whether `e` is an ANONYMOUS function/arrow/class expression — the forms
    /// NamedEvaluation gives a name from the assignment target or property key.
    fn is_anonymous_fn_expr(e: &Expression) -> bool {
        match e {
            Expression::FunctionExpression(f) => f.id.is_none(),
            Expression::ArrowFunctionExpression(_) => true,
            Expression::ClassExpression(c) => c.id.is_none(),
            _ => false,
        }
    }

    fn compile_property_key_expr(&mut self, key: &PropertyKey) -> R {
        if let Some(e) = key.as_expression() {
            self.compile_expr(e)
        } else {
            let name = property_key_name(key);
            self.load_str(&name);
            Ok(())
        }
    }

    fn compile_template(&mut self, t: &TemplateLiteral) -> R {
        // Interleave quasis and expressions, ToString each part, concat.
        let mut parts = 0u32;
        for (i, quasi) in t.quasis.iter().enumerate() {
            let cooked = quasi
                .value
                .cooked
                .as_ref()
                .map(|s| s.as_str())
                .unwrap_or("");
            if quasi.lone_surrogates {
                // oxc FFFD-encodes a cooked quasi containing lone surrogates
                // (same scheme as string literals); decode to real code units.
                let idx = self.intern_str(decode_lone_surrogates(cooked));
                self.emit(Op::LoadConst(idx));
            } else {
                self.load_str(cooked);
            }
            parts += 1;
            if i < t.expressions.len() {
                self.compile_expr(&t.expressions[i])?;
                self.emit(Op::ToStringOp);
                parts += 1;
            }
        }
        self.emit(Op::ConcatStrings(parts));
        Ok(())
    }

    fn compile_tagged_template(&mut self, t: &TaggedTemplateExpression) -> R {
        // tag(templateObject, ...exprs). The template object is cached per
        // source position and frozen (GetTemplateObject builds it once); the
        // tag callee and the substitution expressions evaluate on every call.
        //
        // A member-call tag (`obj.tag\`...\``) keeps `obj` as the `this`
        // receiver, mirroring an ordinary method call.
        let parts = TemplateParts {
            cooked: t
                .quasi
                .quasis
                .iter()
                .map(|q| q.value.cooked.as_ref().map(|s| Rc::from(s.as_str())))
                .collect(),
            raw: t
                .quasi
                .quasis
                .iter()
                .map(|q| Rc::from(q.value.raw.as_str()))
                .collect(),
        };
        let idx = {
            let fc = self.cur();
            fc.templates.push(parts);
            (fc.templates.len() - 1) as u32
        };
        // Evaluate the callee with the correct `this` (the receiver for a
        // member-expression tag, else undefined).
        match &t.tag {
            Expression::StaticMemberExpression(m) if !matches!(m.object, Expression::Super(_)) => {
                self.compile_expr(&m.object)?; // [obj]
                self.emit(Op::Dup); // [obj, obj]
                let k = self.str_const(m.property.name.as_str());
                self.emit(Op::GetProp(k)); // [obj, fn]
                self.emit(Op::Swap); // [fn, obj(this)]
            }
            Expression::ComputedMemberExpression(m)
                if !matches!(m.object, Expression::Super(_)) =>
            {
                self.compile_expr(&m.object)?; // [obj]
                self.emit(Op::Dup); // [obj, obj]
                self.compile_expr(&m.expression)?; // [obj, obj, key]
                self.emit(Op::GetPropDynamic); // [obj, fn]
                self.emit(Op::Swap); // [fn, obj(this)]
            }
            _ => {
                self.compile_expr(&t.tag)?; // [fn]
                self.emit(Op::LoadUndefined); // [fn, this=undefined]
            }
        }
        self.emit(Op::GetTemplateObject(idx)); // [fn, this, templateObject]
        for e in &t.quasi.expressions {
            self.compile_expr(e)?;
        }
        self.emit(Op::Call(1 + t.quasi.expressions.len() as u32));
        Ok(())
    }

    fn compile_binary(&mut self, b: &BinaryExpression) -> R {
        use oxc::syntax::operator::BinaryOperator as B;
        // `in` and `instanceof` have special operand handling.
        match b.operator {
            B::In => {
                self.compile_expr(&b.left)?; // key
                self.compile_expr(&b.right)?; // obj
                self.emit(Op::HasProp);
                return Ok(());
            }
            B::Instanceof => {
                self.compile_expr(&b.left)?;
                self.compile_expr(&b.right)?;
                self.emit(Op::InstanceOf);
                return Ok(());
            }
            _ => {}
        }
        self.compile_expr(&b.left)?;
        self.compile_expr(&b.right)?;
        let op = match b.operator {
            B::Addition => Op::Add,
            B::Subtraction => Op::Sub,
            B::Multiplication => Op::Mul,
            B::Division => Op::Div,
            B::Remainder => Op::Mod,
            B::Exponential => Op::Pow,
            B::Equality => Op::Eq,
            B::Inequality => Op::Ne,
            B::StrictEquality => Op::StrictEq,
            B::StrictInequality => Op::StrictNe,
            B::LessThan => Op::Lt,
            B::LessEqualThan => Op::Le,
            B::GreaterThan => Op::Gt,
            B::GreaterEqualThan => Op::Ge,
            B::BitwiseAnd => Op::BitAnd,
            B::BitwiseOR => Op::BitOr,
            B::BitwiseXOR => Op::BitXor,
            B::ShiftLeft => Op::Shl,
            B::ShiftRight => Op::Shr,
            B::ShiftRightZeroFill => Op::UShr,
            B::In | B::Instanceof => unreachable!(),
        };
        self.emit(op);
        Ok(())
    }

    fn compile_logical(&mut self, l: &LogicalExpression) -> R {
        use oxc::syntax::operator::LogicalOperator as L;
        self.compile_expr(&l.left)?;
        let jump = match l.operator {
            L::And => self.emit(Op::JumpIfFalsyPeek(0)),
            L::Or => self.emit(Op::JumpIfTruthyPeek(0)),
            L::Coalesce => self.emit(Op::JumpIfNullishPeek(0)),
        };
        self.compile_expr(&l.right)?;
        let end = self.here();
        self.patch_jump(jump, end);
        Ok(())
    }

    fn compile_unary(&mut self, u: &UnaryExpression) -> R {
        use oxc::syntax::operator::UnaryOperator as U;
        match u.operator {
            U::Typeof => {
                // typeof of a bare undefined identifier must not throw.
                if let Expression::Identifier(id) = &u.argument {
                    let name = id.name.as_str();
                    if self.in_with(name) {
                        // Inside a `with`, a bare name may live on the with-object;
                        // if not, fall back to its static binding. For an
                        // unresolved (global) name use the typeof-safe,
                        // non-throwing read so `typeof undefinedName` is "undefined".
                        let fallback = match self.resolve(name) {
                            Resolved::Cell(i) => Op::LoadCell(i),
                            Resolved::Upvalue(i) => Op::LoadUpvalue(i),
                            Resolved::Global => {
                                let gt = self.str_const(name);
                                Op::LoadGlobalTypeof(gt)
                            }
                        };
                        let n = self.str_const(name);
                        self.emit(Op::LoadName {
                            name: n,
                            fallback: Box::new(fallback),
                        });
                        self.emit(Op::TypeofExpr);
                        return Ok(());
                    }
                    match self.resolve(name) {
                        Resolved::Global => {
                            let n = self.str_const(name);
                            self.emit(Op::LoadGlobalTypeof(n));
                            self.emit(Op::TypeofExpr);
                            return Ok(());
                        }
                        _ => {}
                    }
                }
                self.compile_expr(&u.argument)?;
                self.emit(Op::TypeofExpr);
            }
            U::Delete => {
                match &u.argument {
                    // `delete super.x` / `delete super[e]`: a Super Reference is
                    // never deletable. Evaluate the reference components in spec
                    // order — GetThisBinding (TDZ-checked), the computed key's
                    // GetValue (but NOT ToPropertyKey), GetSuperBase — then
                    // throw a ReferenceError (spec 13.5.1.2 step 5.b).
                    Expression::StaticMemberExpression(m)
                        if matches!(m.object, Expression::Super(_)) =>
                    {
                        if !self.super_prop_allowed() {
                            return Err(
                                "'super' keyword is only valid inside a class or method".into()
                            );
                        }
                        self.load_binding("%this");
                        self.emit(Op::GetSuperBase);
                        self.emit(Op::Pop);
                        self.emit(Op::Pop);
                        self.emit_throw_error("ReferenceError", "Unsupported reference to 'super'");
                    }
                    Expression::ComputedMemberExpression(m)
                        if matches!(m.object, Expression::Super(_)) =>
                    {
                        if !self.super_prop_allowed() {
                            return Err(
                                "'super' keyword is only valid inside a class or method".into()
                            );
                        }
                        self.load_binding("%this");
                        self.compile_expr(&m.expression)?;
                        self.emit(Op::GetSuperBase);
                        self.emit(Op::Pop);
                        self.emit(Op::Pop);
                        self.emit(Op::Pop);
                        self.emit_throw_error("ReferenceError", "Unsupported reference to 'super'");
                    }
                    Expression::StaticMemberExpression(m) => {
                        self.compile_expr(&m.object)?;
                        let k = self.str_const(m.property.name.as_str());
                        self.emit(Op::DeleteProp(k));
                    }
                    Expression::ComputedMemberExpression(m) => {
                        self.compile_expr(&m.object)?;
                        self.compile_expr(&m.expression)?;
                        self.emit(Op::DeletePropDynamic);
                    }
                    Expression::Identifier(id) => {
                        // Strict-mode `delete identifier` is an early error.
                        if self.cur_ref().strict {
                            return Err(
                                "SyntaxError: Delete of an unqualified identifier in strict mode."
                                    .into(),
                            );
                        }
                        let name = id.name.as_str();
                        if self.in_with(name) {
                            // Inside `with`, delete from the with-object when
                            // the name resolves there (else fall through to the
                            // global / report-success path in the op).
                            let n = self.str_const(name);
                            self.emit(Op::DeleteName(n));
                        } else {
                            match self.resolve(name) {
                                // Declared bindings are not deletable.
                                Resolved::Cell(_) | Resolved::Upvalue(_) => {
                                    self.emit(Op::LoadFalse)
                                }
                                // Globals: delete per configurability.
                                Resolved::Global => {
                                    let n = self.str_const(name);
                                    self.emit(Op::DeleteName(n))
                                }
                            };
                        }
                    }
                    other => {
                        // `delete <non-reference>`: the operand is still
                        // EVALUATED (spec step 1, `delete foo()` calls foo),
                        // then — not being a Reference — `delete` is `true`.
                        self.compile_expr(other)?;
                        self.emit(Op::Pop);
                        self.emit(Op::LoadTrue);
                    }
                }
            }
            U::Void => {
                self.compile_expr(&u.argument)?;
                self.emit(Op::Pop);
                self.emit(Op::LoadUndefined);
            }
            U::UnaryPlus => {
                self.compile_expr(&u.argument)?;
                self.emit(Op::Pos);
            }
            U::UnaryNegation => {
                self.compile_expr(&u.argument)?;
                self.emit(Op::Neg);
            }
            U::LogicalNot => {
                self.compile_expr(&u.argument)?;
                self.emit(Op::Not);
            }
            U::BitwiseNot => {
                self.compile_expr(&u.argument)?;
                self.emit(Op::BitNot);
            }
        }
        Ok(())
    }

    fn compile_update(&mut self, u: &UpdateExpression) -> R {
        use oxc::syntax::operator::UpdateOperator as U;
        let inc = matches!(u.operator, U::Increment);
        match &u.argument {
            SimpleAssignmentTarget::AssignmentTargetIdentifier(id) => {
                let name = id.name.as_str();
                if self.in_with(name) {
                    // Once-resolved Reference: capture the with-aware base before
                    // the read so the write can't be redirected in between.
                    let t_base = self.capture_name_base(name);
                    self.load_via_base(name, t_base);
                    self.emit(Op::ToNumeric);
                    if u.prefix {
                        self.emit(if inc { Op::Inc } else { Op::Dec });
                        self.store_via_base_keep(name, t_base);
                    } else {
                        let t_old = self.temp();
                        self.emit(Op::Dup);
                        self.emit(Op::InitCell(t_old));
                        self.emit(if inc { Op::Inc } else { Op::Dec });
                        self.store_via_base_keep(name, t_base);
                        self.emit(Op::Pop);
                        self.emit(Op::LoadCell(t_old));
                    }
                    return Ok(());
                }
                self.load_binding(name);
                self.emit(Op::ToNumeric); // old value (BigInt-preserving)
                if u.prefix {
                    self.emit(if inc { Op::Inc } else { Op::Dec });
                    self.emit(Op::Dup);
                    self.store_binding(name);
                } else {
                    self.emit(Op::Dup);
                    self.emit(if inc { Op::Inc } else { Op::Dec });
                    self.store_binding(name);
                }
            }
            SimpleAssignmentTarget::StaticMemberExpression(m)
                if matches!(m.object, Expression::Super(_)) =>
            {
                self.super_update(Some(m.property.name.as_str()), None, inc, u.prefix)?;
            }
            SimpleAssignmentTarget::ComputedMemberExpression(m)
                if matches!(m.object, Expression::Super(_)) =>
            {
                self.super_update(None, Some(&m.expression), inc, u.prefix)?;
            }
            SimpleAssignmentTarget::StaticMemberExpression(m) => {
                let t_obj = self.temp();
                self.compile_expr(&m.object)?;
                self.emit(Op::InitCell(t_obj));
                self.emit(Op::LoadCell(t_obj));
                let k = self.str_const(m.property.name.as_str());
                self.emit(Op::GetProp(k));
                self.emit(Op::ToNumeric);
                let t_old = self.temp();
                self.emit(Op::InitCell(t_old));
                self.emit(Op::LoadCell(t_obj));
                self.emit(Op::LoadCell(t_old));
                self.emit(if inc { Op::Inc } else { Op::Dec });
                self.emit(Op::SetProp(k));
                self.emit(Op::Pop);
                if u.prefix {
                    self.emit(Op::LoadCell(t_old));
                    self.emit(if inc { Op::Inc } else { Op::Dec });
                } else {
                    self.emit(Op::LoadCell(t_old));
                }
            }
            SimpleAssignmentTarget::ComputedMemberExpression(m) => {
                let t_obj = self.temp();
                let t_key = self.temp();
                self.compile_expr(&m.object)?;
                self.emit(Op::InitCell(t_obj));
                self.compile_expr(&m.expression)?;
                self.emit(Op::InitCell(t_key));
                // Coerce the key once (after the base coercibility check); the
                // write below reuses the coerced key with no re-`toString`.
                self.emit(Op::LoadCell(t_obj));
                self.emit(Op::RequireCoercible);
                self.emit(Op::LoadCell(t_key));
                self.emit(Op::ToPropertyKey);
                self.emit(Op::InitCell(t_key));
                self.emit(Op::LoadCell(t_obj));
                self.emit(Op::LoadCell(t_key));
                self.emit(Op::GetPropDynamic);
                self.emit(Op::ToNumeric);
                let t_old = self.temp();
                self.emit(Op::InitCell(t_old));
                self.emit(Op::LoadCell(t_obj));
                self.emit(Op::LoadCell(t_key));
                self.emit(Op::LoadCell(t_old));
                self.emit(if inc { Op::Inc } else { Op::Dec });
                self.emit(Op::SetPropDynamic);
                self.emit(Op::Pop);
                if u.prefix {
                    self.emit(Op::LoadCell(t_old));
                    self.emit(if inc { Op::Inc } else { Op::Dec });
                } else {
                    self.emit(Op::LoadCell(t_old));
                }
            }
            SimpleAssignmentTarget::PrivateFieldExpression(m) => {
                let name = m.field.name.as_str().to_string();
                let t_obj = self.temp();
                self.compile_expr(&m.object)?;
                self.emit(Op::InitCell(t_obj));
                self.emit(Op::LoadCell(t_obj));
                self.emit_private_get_op(&name)?;
                self.emit(Op::ToNumeric);
                let t_old = self.temp();
                self.emit(Op::InitCell(t_old));
                self.emit(Op::LoadCell(t_obj));
                self.emit(Op::LoadCell(t_old));
                self.emit(if inc { Op::Inc } else { Op::Dec });
                self.emit_private_set_op(&name)?;
                self.emit(Op::Pop);
                if u.prefix {
                    self.emit(Op::LoadCell(t_old));
                    self.emit(if inc { Op::Inc } else { Op::Dec });
                } else {
                    self.emit(Op::LoadCell(t_old));
                }
            }
            _ => return Err("invalid update target".into()),
        }
        Ok(())
    }

    fn temp(&mut self) -> u32 {
        self.cur().alloc_cell()
    }

    fn compile_chain(&mut self, e: &ChainElement) -> R {
        let saved = std::mem::take(&mut self.chain_jumps);
        self.chain_jumps = Vec::new();
        self.compile_chain_element(e)?;
        let end = self.here();
        let jumps = std::mem::replace(&mut self.chain_jumps, saved);
        for j in jumps {
            self.patch_jump(j, end);
        }
        Ok(())
    }

    fn compile_chain_element(&mut self, e: &ChainElement) -> R {
        match e {
            ChainElement::CallExpression(c) => self.compile_call(c),
            ChainElement::TSNonNullExpression(t) => self.compile_expr(&t.expression),
            ChainElement::StaticMemberExpression(m) => {
                self.compile_expr(&m.object)?;
                if m.optional {
                    let j = self.emit(Op::JumpIfNullish(0));
                    self.chain_jumps.push(j);
                }
                let k = self.str_const(m.property.name.as_str());
                self.emit(Op::GetProp(k));
                Ok(())
            }
            ChainElement::ComputedMemberExpression(m) => {
                self.compile_expr(&m.object)?;
                if m.optional {
                    let j = self.emit(Op::JumpIfNullish(0));
                    self.chain_jumps.push(j);
                }
                self.compile_expr(&m.expression)?;
                self.emit(Op::GetPropDynamic);
                Ok(())
            }
            ChainElement::PrivateFieldExpression(m) => {
                // `obj?.#x`: short-circuit on a nullish base, then the usual
                // brand-checked private read.
                self.compile_expr(&m.object)?;
                if m.optional {
                    let j = self.emit(Op::JumpIfNullish(0));
                    self.chain_jumps.push(j);
                }
                self.emit_private_get_op(m.field.name.as_str())?;
                Ok(())
            }
        }
    }

    fn compile_call(&mut self, c: &CallExpression) -> R {
        // Direct `eval(...)`: snapshot the visible scope and emit DirectEval.
        // The runtime falls back to an ordinary call when the callee value
        // isn't the %eval% intrinsic (shadowed/reassigned `eval`). Spread and
        // optional calls keep the ordinary (indirect-semantics) path.
        if let Expression::Identifier(id) = &c.callee {
            if id.name.as_str() == "eval"
                && !c.optional
                && !c
                    .arguments
                    .iter()
                    .any(|a| matches!(a, Argument::SpreadElement(_)))
            {
                let desc = self.collect_eval_scope();
                self.load_binding("eval"); // [callee]
                for a in &c.arguments {
                    let e = a.as_expression().unwrap();
                    self.compile_expr(e)?;
                }
                let scope_idx = {
                    let fc = self.cur();
                    fc.eval_scopes.push(desc);
                    (fc.eval_scopes.len() - 1) as u32
                };
                self.emit(Op::DirectEval {
                    argc: c.arguments.len() as u32,
                    scope: scope_idx,
                });
                return Ok(());
            }
        }
        // super(...) — SuperCall (13.3.7.1): evaluate the args, then
        // Construct(parent, args, new.target) so the PARENT allocates `this`
        // (giving builtin subclasses real exotic instances), bind the result
        // as `this` (once), and install instance fields/brands on it. The
        // expression's value is the bound `this`.
        if matches!(c.callee, Expression::Super(_)) {
            self.load_binding("%superclass"); // [super]
            match self.compile_args(&c.arguments)? {
                ArgForm::Count(n) => {
                    self.load_binding("%newtarget"); // [super, args.., nt]
                    self.emit(Op::ConstructSuper(n));
                }
                ArgForm::Spread => {
                    self.load_binding("%newtarget"); // [super, argsArr, nt]
                    self.emit(Op::ConstructSuperSpread);
                }
            }
            self.emit_super_bind_and_init();
            return Ok(());
        }
        // super.method(...) — Get(base, name) with `this` receiver, call
        // with `this`.
        if let Expression::StaticMemberExpression(m) = &c.callee {
            if matches!(m.object, Expression::Super(_)) {
                self.emit_super_ref()?;
                let k = self.str_const(m.property.name.as_str());
                self.emit(Op::SuperGet(k)); // [method]
                self.load_binding("%this"); // [method, this]
                self.finish_call(c)?;
                return Ok(());
            }
        }
        // super[expr](...) — same, with a computed key.
        if let Expression::ComputedMemberExpression(m) = &c.callee {
            if matches!(m.object, Expression::Super(_)) {
                if !self.super_prop_allowed() {
                    return Err(
                        "'super' keyword is only valid inside a class or method".to_string()
                    );
                }
                self.load_binding("%this");
                self.compile_expr(&m.expression)?;
                self.emit(Op::GetSuperBase);
                self.emit(Op::Swap);
                self.emit(Op::SuperGetDynamic); // [method]
                self.load_binding("%this"); // [method, this]
                self.finish_call(c)?;
                return Ok(());
            }
        }
        // Method call: set `this` to the receiver.
        match &c.callee {
            Expression::StaticMemberExpression(m) => {
                self.compile_expr(&m.object)?; // [obj]
                if m.optional {
                    let j = self.emit(Op::JumpIfNullish(0));
                    self.chain_jumps.push(j);
                }
                self.emit(Op::Dup); // [obj, obj]
                let k = self.str_const(m.property.name.as_str());
                self.emit(Op::GetProp(k)); // [obj, func]
                self.emit(Op::Swap); // [func, obj]
                if c.optional {
                    let j = self.emit(Op::JumpIfNullish(0));
                    self.chain_jumps.push(j);
                }
                self.finish_call(c)?;
            }
            Expression::ComputedMemberExpression(m) => {
                self.compile_expr(&m.object)?;
                if m.optional {
                    let j = self.emit(Op::JumpIfNullish(0));
                    self.chain_jumps.push(j);
                }
                self.emit(Op::Dup);
                self.compile_expr(&m.expression)?;
                self.emit(Op::GetPropDynamic);
                self.emit(Op::Swap);
                self.finish_call(c)?;
            }
            Expression::PrivateFieldExpression(m) => {
                self.compile_expr(&m.object)?; // [obj]
                self.emit(Op::Dup); // [obj, obj]
                                    // Brand-checking read: calling a private method on an object that
                                    // doesn't have it must throw a TypeError (not silently read undefined).
                self.emit_private_get_op(m.field.name.as_str())?; // [obj, method]
                self.emit(Op::Swap); // [method, obj]
                self.finish_call(c)?;
            }
            _ => {
                self.compile_expr(&c.callee)?; // [func]
                if c.optional {
                    let j = self.emit(Op::JumpIfNullish(0));
                    self.chain_jumps.push(j);
                }
                self.emit(Op::LoadUndefined); // this
                self.finish_call(c)?;
            }
        }
        Ok(())
    }

    /// Compile arguments and emit the appropriate Call op. Stack has [func, this]
    /// already.
    fn finish_call(&mut self, c: &CallExpression) -> R {
        match self.compile_args(&c.arguments)? {
            ArgForm::Count(n) => {
                self.emit(Op::Call(n));
            }
            ArgForm::Spread => {
                self.emit(Op::CallSpread);
            }
        }
        Ok(())
    }

    fn compile_args(&mut self, args: &[Argument]) -> Result<ArgForm, String> {
        let has_spread = args.iter().any(|a| matches!(a, Argument::SpreadElement(_)));
        if !has_spread {
            for a in args {
                let e = a.as_expression().unwrap();
                self.compile_expr(e)?;
            }
            Ok(ArgForm::Count(args.len() as u32))
        } else {
            // Build an arguments array (with spreads), use CallSpread.
            self.emit(Op::NewArray(0));
            for a in args {
                match a {
                    Argument::SpreadElement(s) => {
                        self.compile_expr(&s.argument)?;
                        self.emit(Op::ArraySpread);
                    }
                    other => {
                        let e = other.as_expression().unwrap();
                        self.array_push_value(|c| c.compile_expr(e))?;
                    }
                }
            }
            Ok(ArgForm::Spread)
        }
    }

    fn compile_new(&mut self, n: &NewExpression) -> R {
        self.compile_expr(&n.callee)?; // [ctor]
        let has_spread = n
            .arguments
            .iter()
            .any(|a| matches!(a, Argument::SpreadElement(_)));
        if !has_spread {
            for a in &n.arguments {
                let e = a.as_expression().unwrap();
                self.compile_expr(e)?;
            }
            self.emit(Op::New(n.arguments.len() as u32));
        } else {
            self.emit(Op::NewArray(0));
            for a in &n.arguments {
                match a {
                    Argument::SpreadElement(s) => {
                        self.compile_expr(&s.argument)?;
                        self.emit(Op::ArraySpread);
                    }
                    other => {
                        let e = other.as_expression().unwrap();
                        self.array_push_value(|c| c.compile_expr(e))?;
                    }
                }
            }
            self.emit(Op::NewSpread);
        }
        Ok(())
    }

    fn compile_yield(&mut self, y: &YieldExpression) -> R {
        if y.delegate {
            // yield* expr — desugar to the spec's delegation loop. In an async
            // generator the delegate uses the *async* iterator protocol: the
            // iterator comes from @@asyncIterator (GetAsyncIterator) and each
            // step result is Awaited before its `done`/`value` are read.
            //
            // The loop forwards the value SENT into the outer generator to the
            // inner iterator's `next(sent)` (exactly one argument, through the
            // next method CACHED at GetIterator time per the spec's iterator
            // record), checks each step result is an Object, delegates a
            // `.throw()` resumption to the inner iterator's `throw` method
            // (closing the inner iterator with a TypeError when it has none),
            // and delegates a `.return(v)` resumption to the inner iterator's
            // `return` method (finishing the outer return when it has none).
            let is_async = self.cur().kind.is_async();
            self.compile_expr(y.argument.as_ref().unwrap())?;
            if is_async {
                self.emit(Op::GetAsyncIterator);
            } else {
                self.emit(Op::GetIterator);
            }
            let iter_cell = self.temp();
            self.emit(Op::InitCell(iter_cell));
            let next_k = self.str_const("next");
            let done_k = self.str_const("done");
            let value_k = self.str_const("value");
            // Iterator record: cache the next method once (Get(iter, "next")
            // must be observable exactly once, at GetIterator time).
            let next_cell = self.temp();
            self.emit(Op::LoadCell(iter_cell));
            self.emit(Op::GetProp(next_k));
            self.emit(Op::InitCell(next_cell));
            let sent_cell = self.temp();
            self.emit(Op::LoadUndefined);
            self.emit(Op::InitCell(sent_cell));

            // -- next_call: result = cachedNext.call(inner, sent) --
            let next_call = self.here();
            self.emit(Op::LoadCell(next_cell));
            self.emit(Op::LoadCell(iter_cell)); // [next, iter]
            self.emit(Op::LoadCell(sent_cell)); // [next, iter, sent]
            self.emit(Op::Call(1)); // [result]

            // -- have_result: (async: Await) -> object check -> done? --
            let have_result = self.here();
            if is_async {
                self.emit(Op::Await); // result is a promise of { value, done }
            }
            self.emit(Op::RequireIterResult);
            self.emit(Op::Dup);
            self.emit(Op::GetProp(done_k));
            let jt = self.emit(Op::JumpIfTrue(0)); // [result]
            self.emit(Op::GetProp(value_k)); // [value]
                                             // Yield inside a catch-only region: a `.throw(e)` resumption lands
                                             // in the delegation handler below instead of unwinding, and a
                                             // `.return(v)` resumption lands at the return-delegation block.
            let yield_site = self.here();
            let push_h = self.emit(Op::PushTryHandler {
                catch: u32::MAX,
                finally: u32::MAX,
            });
            let mark = self.emit(Op::MarkDelegationHandler(u32::MAX));
            self.cur().handler_depth += 1;
            self.emit(Op::Yield); // [sent']
            self.emit(Op::StoreCell(sent_cell));
            self.emit(Op::PopTryHandler);
            self.cur().handler_depth -= 1;
            self.emit(Op::Jump(next_call));

            // -- catch: delegate the thrown value to inner.throw(e) --
            let catch_lbl = self.here();
            self.patch_jump(push_h, catch_lbl); // [e]
            let e_cell = self.temp();
            self.emit(Op::InitCell(e_cell));
            let thr_cell = self.temp();
            self.emit(Op::LoadCell(iter_cell));
            let throw_k = self.str_const("throw");
            self.emit(Op::GetProp(throw_k));
            self.emit(Op::InitCell(thr_cell));
            self.emit(Op::LoadCell(thr_cell));
            let jno_throw = self.emit(Op::JumpIfNullish(0)); // [thr] (peek)
            self.emit(Op::Pop);
            self.emit(Op::LoadCell(thr_cell));
            self.emit(Op::LoadCell(iter_cell));
            self.emit(Op::LoadCell(e_cell)); // [thr, iter, e]
            self.emit(Op::Call(1)); // [result]
            self.emit(Op::Jump(have_result));
            // No `throw` method: close the inner iterator, then TypeError.
            let no_throw = self.here();
            self.patch_jump(jno_throw, no_throw);
            self.emit(Op::Pop); // drop the nullish `throw`
            self.emit(Op::LoadCell(iter_cell));
            self.emit(Op::IteratorClose);
            let tk = self.str_const("TypeError");
            self.emit(Op::LoadGlobal(tk));
            let mk = self.str_const("The iterator does not provide a 'throw' method");
            self.emit(Op::LoadConst(mk));
            self.emit(Op::New(1));
            self.emit(Op::Throw);

            // -- return delegation (spec 15.5.5 step 7.c): a `.return(v)`
            // resumption jumps here with [v]. Forward to inner.return(v);
            // when the inner has no `return`, the outer return proceeds. --
            let ret_lbl = self.here();
            self.patch_jump(mark, ret_lbl); // [v]
            let ret_cell = self.temp();
            self.emit(Op::InitCell(ret_cell));
            let retm_cell = self.temp();
            self.emit(Op::LoadCell(iter_cell));
            let return_k = self.str_const("return");
            self.emit(Op::GetProp(return_k));
            self.emit(Op::InitCell(retm_cell));
            self.emit(Op::LoadCell(retm_cell));
            let jno_ret = self.emit(Op::JumpIfNullish(0)); // [retm] (peek)
            self.emit(Op::Pop);
            self.emit(Op::LoadCell(retm_cell));
            self.emit(Op::LoadCell(iter_cell));
            self.emit(Op::LoadCell(ret_cell)); // [retm, iter, v]
            self.emit(Op::Call(1)); // [innerReturnResult]
            if is_async {
                self.emit(Op::Await);
            }
            self.emit(Op::RequireIterResult);
            self.emit(Op::Dup);
            self.emit(Op::GetProp(done_k));
            let jr_done = self.emit(Op::JumpIfTrue(0)); // [result]
                                                        // Not done: keep delegating — yield the inner value and loop.
            self.emit(Op::GetProp(value_k)); // [value]
            self.emit(Op::Jump(yield_site));
            // Done: the outer generator returns IteratorValue(result),
            // running any enclosing finally blocks.
            let r_done = self.here();
            self.patch_jump(jr_done, r_done);
            self.emit(Op::GetProp(value_k)); // [value]
            self.emit(Op::Return);
            // No `return` method: complete the outer return with v
            // ((async) after awaiting it).
            let no_ret = self.here();
            self.patch_jump(jno_ret, no_ret);
            self.emit(Op::Pop); // drop the nullish `return`
            self.emit(Op::LoadCell(ret_cell));
            if is_async {
                self.emit(Op::Await);
            }
            self.emit(Op::Return);

            // -- end: result of yield* = final result.value --
            let end = self.here();
            self.patch_jump(jt, end);
            self.emit(Op::GetProp(value_k)); // [result.value]
        } else {
            if let Some(arg) = &y.argument {
                self.compile_expr(arg)?;
            } else {
                self.emit(Op::LoadUndefined);
            }
            self.emit(Op::Yield);
        }
        Ok(())
    }
}

impl Compiler {
    fn compile_assignment(&mut self, a: &AssignmentExpression) -> R {
        use oxc::syntax::operator::AssignmentOperator as A;
        match &a.left {
            AssignmentTarget::AssignmentTargetIdentifier(id) => {
                let name = id.name.as_str().to_string();
                if self.in_with(&name) {
                    // Inside a `with`, resolve the Reference base once — before
                    // the RHS runs — and write through the captured base.
                    match a.operator {
                        A::Assign => {
                            let t_base = self.capture_name_base(&name);
                            self.compile_named_expr(&a.right, &name)?;
                            self.store_via_base_keep(&name, t_base);
                        }
                        A::LogicalAnd | A::LogicalOr | A::LogicalNullish => {
                            let t_base = self.capture_name_base(&name);
                            self.load_via_base(&name, t_base);
                            let j = match a.operator {
                                A::LogicalAnd => self.emit(Op::JumpIfFalsyPeek(0)),
                                A::LogicalOr => self.emit(Op::JumpIfTruthyPeek(0)),
                                _ => self.emit(Op::JumpIfNullishPeek(0)),
                            };
                            // NamedEvaluation: `x ||= function(){}` names it "x".
                            self.compile_named_expr(&a.right, &name)?;
                            self.store_via_base_keep(&name, t_base);
                            let end = self.here();
                            self.patch_jump(j, end);
                        }
                        other => {
                            let t_base = self.capture_name_base(&name);
                            self.load_via_base(&name, t_base);
                            self.compile_expr(&a.right)?;
                            self.emit(compound_op(other));
                            self.store_via_base_keep(&name, t_base);
                        }
                    }
                    return Ok(());
                }
                match a.operator {
                    A::Assign => {
                        self.compile_named_expr(&a.right, &name)?;
                        self.emit(Op::Dup);
                        self.store_binding_assign(&name);
                    }
                    A::LogicalAnd | A::LogicalOr | A::LogicalNullish => {
                        self.load_binding(&name);
                        let j = match a.operator {
                            A::LogicalAnd => self.emit(Op::JumpIfFalsyPeek(0)),
                            A::LogicalOr => self.emit(Op::JumpIfTruthyPeek(0)),
                            _ => self.emit(Op::JumpIfNullishPeek(0)),
                        };
                        // NamedEvaluation: `x ||= function(){}` names it "x".
                        self.compile_named_expr(&a.right, &name)?;
                        self.emit(Op::Dup);
                        self.store_binding_assign(&name);
                        let end = self.here();
                        self.patch_jump(j, end);
                    }
                    other => {
                        self.load_binding(&name);
                        self.compile_expr(&a.right)?;
                        self.emit(compound_op(other));
                        self.emit(Op::Dup);
                        self.store_binding_assign(&name);
                    }
                }
            }
            AssignmentTarget::StaticMemberExpression(m) => {
                let k = self.str_const(m.property.name.as_str());
                if matches!(m.object, Expression::Super(_)) {
                    self.super_member_assign(Some(k), None, a)?;
                } else {
                    self.member_assign(&m.object, k, a)?;
                }
            }
            AssignmentTarget::PrivateFieldExpression(m) => {
                self.member_assign_private(&m.object, m.field.name.as_str(), a)?;
            }
            AssignmentTarget::ComputedMemberExpression(m)
                if matches!(m.object, Expression::Super(_)) =>
            {
                self.super_member_assign(None, Some(&m.expression), a)?;
            }
            AssignmentTarget::ComputedMemberExpression(m) => match a.operator {
                A::Assign => {
                    self.compile_expr(&m.object)?;
                    self.compile_expr(&m.expression)?;
                    self.compile_expr(&a.right)?;
                    self.emit(Op::SetPropDynamic);
                }
                other => {
                    let t_obj = self.temp();
                    let t_key = self.temp();
                    self.compile_expr(&m.object)?;
                    self.emit(Op::InitCell(t_obj));
                    self.compile_expr(&m.expression)?;
                    self.emit(Op::InitCell(t_key));
                    // GetValue order: RequireObjectCoercible(base) first, then
                    // ToPropertyKey exactly once (its `toString` must not run
                    // again at the write).
                    self.emit(Op::LoadCell(t_obj));
                    self.emit(Op::RequireCoercible);
                    self.emit(Op::LoadCell(t_key));
                    self.emit(Op::ToPropertyKey);
                    self.emit(Op::InitCell(t_key));
                    self.emit(Op::LoadCell(t_obj));
                    self.emit(Op::LoadCell(t_key));
                    self.emit(Op::GetPropDynamic);
                    let logical_jump = match other {
                        A::LogicalAnd => Some(self.emit(Op::JumpIfFalsyPeek(0))),
                        A::LogicalOr => Some(self.emit(Op::JumpIfTruthyPeek(0))),
                        A::LogicalNullish => Some(self.emit(Op::JumpIfNullishPeek(0))),
                        _ => None,
                    };
                    self.compile_expr(&a.right)?;
                    if logical_jump.is_none() {
                        self.emit(compound_op(other));
                    }
                    let t_val = self.temp();
                    self.emit(Op::InitCell(t_val));
                    self.emit(Op::LoadCell(t_obj));
                    self.emit(Op::LoadCell(t_key));
                    self.emit(Op::LoadCell(t_val));
                    self.emit(Op::SetPropDynamic);
                    if let Some(j) = logical_jump {
                        let end = self.here();
                        self.patch_jump(j, end);
                    }
                }
            },
            AssignmentTarget::ArrayAssignmentTarget(_)
            | AssignmentTarget::ObjectAssignmentTarget(_) => {
                self.compile_expr(&a.right)?;
                self.emit(Op::Dup);
                self.assign_target(&a.left)?;
            }
            _ => return Err("unsupported assignment target".into()),
        }
        Ok(())
    }

    /// `super.x <op>= v` / `super[key] <op>= v`: the reference components
    /// (`this`, the raw key for the computed form, then the super base)
    /// evaluate once, before the RHS, per MakeSuperPropertyReference.
    fn super_member_assign(
        &mut self,
        key_const: Option<u32>,
        key_expr: Option<&Expression>,
        a: &AssignmentExpression,
    ) -> R {
        use oxc::syntax::operator::AssignmentOperator as A;
        if !self.super_prop_allowed() {
            return Err("'super' keyword is only valid inside a class or method".to_string());
        }
        let t_this = self.temp();
        let t_base = self.temp();
        let t_key = key_expr.map(|_| self.temp());
        self.load_binding("%this");
        self.emit(Op::InitCell(t_this));
        if let (Some(e), Some(tk)) = (key_expr, t_key) {
            self.compile_expr(e)?;
            self.emit(Op::InitCell(tk));
        }
        self.emit(Op::GetSuperBase);
        self.emit(Op::InitCell(t_base));
        let load_ref = |c: &mut Self| {
            c.emit(Op::LoadCell(t_this));
            c.emit(Op::LoadCell(t_base));
            if let Some(tk) = t_key {
                c.emit(Op::LoadCell(tk));
            }
        };
        let get = |c: &mut Self| {
            match key_const {
                Some(k) => c.emit(Op::SuperGet(k)),
                None => c.emit(Op::SuperGetDynamic),
            };
        };
        let set = |c: &mut Self| {
            match key_const {
                Some(k) => c.emit(Op::SuperSet(k)),
                None => c.emit(Op::SuperSetDynamic),
            };
        };
        match a.operator {
            A::Assign => {
                load_ref(self);
                self.compile_expr(&a.right)?;
                set(self);
            }
            A::LogicalAnd | A::LogicalOr | A::LogicalNullish => {
                load_ref(self);
                get(self);
                let j = match a.operator {
                    A::LogicalAnd => self.emit(Op::JumpIfFalsyPeek(0)),
                    A::LogicalOr => self.emit(Op::JumpIfTruthyPeek(0)),
                    _ => self.emit(Op::JumpIfNullishPeek(0)),
                };
                self.compile_expr(&a.right)?;
                let t_val = self.temp();
                self.emit(Op::InitCell(t_val));
                load_ref(self);
                self.emit(Op::LoadCell(t_val));
                set(self);
                let end = self.here();
                self.patch_jump(j, end);
            }
            other => {
                load_ref(self);
                get(self);
                self.compile_expr(&a.right)?;
                self.emit(compound_op(other));
                let t_val = self.temp();
                self.emit(Op::InitCell(t_val));
                load_ref(self);
                self.emit(Op::LoadCell(t_val));
                set(self);
            }
        }
        Ok(())
    }

    /// `super.x++` / `super[key]--` (prefix and suffix forms): reference
    /// components evaluate once; read, ToNumeric, write, produce old/new.
    fn super_update(
        &mut self,
        key_name: Option<&str>,
        key_expr: Option<&Expression>,
        inc: bool,
        prefix: bool,
    ) -> R {
        if !self.super_prop_allowed() {
            return Err("'super' keyword is only valid inside a class or method".to_string());
        }
        let key_const = key_name.map(|n| self.str_const(n));
        let t_this = self.temp();
        let t_base = self.temp();
        let t_key = key_expr.map(|_| self.temp());
        self.load_binding("%this");
        self.emit(Op::InitCell(t_this));
        if let (Some(e), Some(tk)) = (key_expr, t_key) {
            self.compile_expr(e)?;
            self.emit(Op::InitCell(tk));
        }
        self.emit(Op::GetSuperBase);
        self.emit(Op::InitCell(t_base));
        let load_ref = |c: &mut Self| {
            c.emit(Op::LoadCell(t_this));
            c.emit(Op::LoadCell(t_base));
            if let Some(tk) = t_key {
                c.emit(Op::LoadCell(tk));
            }
        };
        load_ref(self);
        match key_const {
            Some(k) => self.emit(Op::SuperGet(k)),
            None => self.emit(Op::SuperGetDynamic),
        };
        self.emit(Op::ToNumeric);
        let t_old = self.temp();
        self.emit(Op::InitCell(t_old));
        load_ref(self);
        self.emit(Op::LoadCell(t_old));
        self.emit(if inc { Op::Inc } else { Op::Dec });
        match key_const {
            Some(k) => self.emit(Op::SuperSet(k)),
            None => self.emit(Op::SuperSetDynamic),
        };
        self.emit(Op::Pop);
        self.emit(Op::LoadCell(t_old));
        if prefix {
            self.emit(if inc { Op::Inc } else { Op::Dec });
        }
        Ok(())
    }

    /// Assign to `obj.<k>` (static-name key const `k`) with the given operator.
    fn member_assign(&mut self, obj: &Expression, k: u32, a: &AssignmentExpression) -> R {
        self.member_assign_kind(obj, k, a, false)
    }

    /// Private member assignment `obj.#x <op>= v`: same stack discipline as
    /// `member_assign_kind`, with reads/writes routed through the lexically
    /// resolved brand-checking private ops.
    fn member_assign_private(
        &mut self,
        obj: &Expression,
        name: &str,
        a: &AssignmentExpression,
    ) -> R {
        use oxc::syntax::operator::AssignmentOperator as A;
        match a.operator {
            A::Assign => {
                self.compile_expr(obj)?;
                self.compile_expr(&a.right)?;
                self.emit_private_set_op(name)?;
            }
            A::LogicalAnd | A::LogicalOr | A::LogicalNullish => {
                let t_obj = self.temp();
                self.compile_expr(obj)?;
                self.emit(Op::InitCell(t_obj));
                self.emit(Op::LoadCell(t_obj));
                self.emit_private_get_op(name)?;
                let j = match a.operator {
                    A::LogicalAnd => self.emit(Op::JumpIfFalsyPeek(0)),
                    A::LogicalOr => self.emit(Op::JumpIfTruthyPeek(0)),
                    _ => self.emit(Op::JumpIfNullishPeek(0)),
                };
                self.compile_expr(&a.right)?;
                let t_val = self.temp();
                self.emit(Op::InitCell(t_val));
                self.emit(Op::LoadCell(t_obj));
                self.emit(Op::LoadCell(t_val));
                self.emit_private_set_op(name)?;
                let end = self.here();
                self.patch_jump(j, end);
            }
            other => {
                let t_obj = self.temp();
                self.compile_expr(obj)?;
                self.emit(Op::InitCell(t_obj));
                self.emit(Op::LoadCell(t_obj));
                self.emit_private_get_op(name)?;
                self.compile_expr(&a.right)?;
                self.emit(compound_op(other));
                let t_val = self.temp();
                self.emit(Op::InitCell(t_val));
                self.emit(Op::LoadCell(t_obj));
                self.emit(Op::LoadCell(t_val));
                self.emit_private_set_op(name)?;
            }
        }
        Ok(())
    }

    /// Member assignment `obj.x <op>= v`. `is_private` routes the field read/write
    /// through the brand-checking `PrivateGet`/`PrivateSet` ops.
    fn member_assign_kind(
        &mut self,
        obj: &Expression,
        k: u32,
        a: &AssignmentExpression,
        is_private: bool,
    ) -> R {
        use oxc::syntax::operator::AssignmentOperator as A;
        let get = |c: &mut Self| {
            c.emit(if is_private {
                Op::PrivateGet(k)
            } else {
                Op::GetProp(k)
            })
        };
        let set = |c: &mut Self| {
            c.emit(if is_private {
                Op::PrivateSet(k)
            } else {
                Op::SetProp(k)
            })
        };
        match a.operator {
            A::Assign => {
                self.compile_expr(obj)?;
                self.compile_expr(&a.right)?;
                set(self);
            }
            A::LogicalAnd | A::LogicalOr | A::LogicalNullish => {
                let t_obj = self.temp();
                self.compile_expr(obj)?;
                self.emit(Op::InitCell(t_obj));
                self.emit(Op::LoadCell(t_obj));
                get(self);
                let j = match a.operator {
                    A::LogicalAnd => self.emit(Op::JumpIfFalsyPeek(0)),
                    A::LogicalOr => self.emit(Op::JumpIfTruthyPeek(0)),
                    _ => self.emit(Op::JumpIfNullishPeek(0)),
                };
                self.compile_expr(&a.right)?;
                let t_val = self.temp();
                self.emit(Op::InitCell(t_val));
                self.emit(Op::LoadCell(t_obj));
                self.emit(Op::LoadCell(t_val));
                set(self);
                let end = self.here();
                self.patch_jump(j, end);
            }
            other => {
                let t_obj = self.temp();
                self.compile_expr(obj)?;
                self.emit(Op::InitCell(t_obj));
                self.emit(Op::LoadCell(t_obj));
                get(self);
                self.compile_expr(&a.right)?;
                self.emit(compound_op(other));
                let t_val = self.temp();
                self.emit(Op::InitCell(t_val));
                self.emit(Op::LoadCell(t_obj));
                self.emit(Op::LoadCell(t_val));
                set(self);
            }
        }
        Ok(())
    }

    /// Assign the value on top of the stack to `target`, consuming it.
    fn assign_target(&mut self, target: &AssignmentTarget) -> R {
        match target {
            AssignmentTarget::AssignmentTargetIdentifier(id) => {
                // Destructuring-assignment target (e.g. `({x} = obj)`): TDZ-checked
                // like any other assignment expression.
                self.store_binding_assign(id.name.as_str());
            }
            AssignmentTarget::StaticMemberExpression(m) => {
                let k = self.str_const(m.property.name.as_str());
                let t = self.temp();
                self.emit(Op::InitCell(t));
                if matches!(m.object, Expression::Super(_)) {
                    if !self.super_prop_allowed() {
                        return Err(
                            "'super' keyword is only valid inside a class or method".to_string()
                        );
                    }
                    self.load_binding("%this");
                    self.emit(Op::GetSuperBase);
                    self.emit(Op::LoadCell(t));
                    self.emit(Op::SuperSet(k));
                } else {
                    self.compile_expr(&m.object)?;
                    self.emit(Op::LoadCell(t));
                    self.emit(Op::SetProp(k));
                }
                self.emit(Op::Pop);
            }
            // Private field as a destructuring target: `[obj.#x] = [...]`.
            AssignmentTarget::PrivateFieldExpression(m) => {
                let t = self.temp();
                self.emit(Op::InitCell(t));
                self.compile_expr(&m.object)?;
                self.emit(Op::LoadCell(t));
                self.emit_private_set_op(m.field.name.as_str())?;
                self.emit(Op::Pop);
            }
            AssignmentTarget::ComputedMemberExpression(m) => {
                let t = self.temp();
                self.emit(Op::InitCell(t));
                if matches!(m.object, Expression::Super(_)) {
                    if !self.super_prop_allowed() {
                        return Err(
                            "'super' keyword is only valid inside a class or method".to_string()
                        );
                    }
                    self.load_binding("%this");
                    self.compile_expr(&m.expression)?;
                    self.emit(Op::GetSuperBase);
                    self.emit(Op::Swap); // [this, base, key]
                    self.emit(Op::LoadCell(t));
                    self.emit(Op::SuperSetDynamic);
                } else {
                    self.compile_expr(&m.object)?;
                    self.compile_expr(&m.expression)?;
                    self.emit(Op::LoadCell(t));
                    self.emit(Op::SetPropDynamic);
                }
                self.emit(Op::Pop);
            }
            AssignmentTarget::ArrayAssignmentTarget(arr) => {
                // Iterator-protocol destructuring (spec-correct: works for any
                // iterable, not just indexable array-likes), mirroring the
                // declaration-form `ArrayPattern` path — including IteratorClose
                // on abrupt or leftover completion (see that path for the model).
                self.emit(Op::GetIterator); // [iter]
                let itc = self.temp();
                self.emit(Op::InitCell(itc)); // []
                let done_cell = self.temp();
                self.emit(Op::LoadFalse);
                self.emit(Op::InitCell(done_cell));
                let push = self.emit(Op::PushTryHandler {
                    catch: u32::MAX,
                    finally: u32::MAX,
                });
                self.cur().handler_depth += 1;
                self.cur().finally_depth += 1;
                for el in &arr.elements {
                    match el {
                        Some(maybe) => self.assign_element_ordered(maybe, itc, done_cell)?,
                        None => {
                            self.emit_iter_step_tracked(itc, done_cell); // [value]
                            self.emit(Op::Pop); // elision
                        }
                    }
                }
                if let Some(rest) = &arr.rest {
                    // A member-expression rest target's REFERENCE is evaluated
                    // BEFORE the collection loop (spec AssignmentRestElement
                    // step 1), so a throw there closes the iterator with no
                    // further next() calls.
                    enum RestPre {
                        Static(u32, u32),     // (t_obj, key const)
                        Computed(u32, u32),   // (t_obj, t_key)
                        Private(u32, String), // (t_obj, name)
                        Plain,
                    }
                    let pre = match &rest.target {
                        AssignmentTarget::StaticMemberExpression(sm) => {
                            let t_obj = self.temp();
                            self.compile_expr(&sm.object)?;
                            self.emit(Op::InitCell(t_obj));
                            let k = self.str_const(sm.property.name.as_str());
                            RestPre::Static(t_obj, k)
                        }
                        AssignmentTarget::ComputedMemberExpression(cm) => {
                            let t_obj = self.temp();
                            let t_key = self.temp();
                            self.compile_expr(&cm.object)?;
                            self.emit(Op::InitCell(t_obj));
                            self.compile_expr(&cm.expression)?;
                            self.emit(Op::InitCell(t_key));
                            RestPre::Computed(t_obj, t_key)
                        }
                        AssignmentTarget::PrivateFieldExpression(pm) => {
                            let t_obj = self.temp();
                            self.compile_expr(&pm.object)?;
                            self.emit(Op::InitCell(t_obj));
                            RestPre::Private(t_obj, pm.field.name.as_str().to_string())
                        }
                        _ => RestPre::Plain,
                    };
                    self.emit(Op::NewArray(0)); // [arr]
                    let top = self.here();
                    self.emit(Op::LoadCell(done_cell));
                    let jdone_rest = self.emit(Op::JumpIfTrue(0));
                    // Latch done before stepping (abrupt next/done/value sets
                    // [[Done]] — see emit_iter_step_tracked).
                    self.emit(Op::LoadTrue);
                    self.emit(Op::StoreCell(done_cell));
                    self.emit(Op::LoadCell(itc));
                    self.emit(Op::IteratorNext); // [arr, iter, result]
                    self.emit(Op::Swap);
                    self.emit(Op::Pop); // [arr, result]
                    self.emit(Op::Dup);
                    let dk = self.str_const("done");
                    self.emit(Op::GetProp(dk)); // [arr, result, done]
                    let jend = self.emit(Op::JumpIfTrue(0));
                    let vk = self.str_const("value");
                    self.emit(Op::GetProp(vk)); // [arr, value]
                    self.emit(Op::LoadFalse);
                    self.emit(Op::StoreCell(done_cell)); // normal step: un-latch
                    let tv = self.temp();
                    self.emit(Op::InitCell(tv)); // [arr]
                    self.array_push_value(|c| {
                        c.emit(Op::LoadCell(tv));
                        Ok(())
                    })?; // [arr]
                    self.emit(Op::Jump(top));
                    let end = self.here();
                    self.patch_jump(jend, end);
                    self.emit(Op::Pop); // drop result -> [arr] (done stays latched)
                    let jafter = self.emit(Op::Jump(0));
                    let drest = self.here();
                    self.patch_jump(jdone_rest, drest);
                    let after_rest = self.here();
                    self.patch_jump(jafter, after_rest);
                    match pre {
                        RestPre::Static(t_obj, k) => {
                            let t_val = self.temp();
                            self.emit(Op::InitCell(t_val)); // pops arr
                            self.emit(Op::LoadCell(t_obj));
                            self.emit(Op::LoadCell(t_val));
                            self.emit(Op::SetProp(k));
                            self.emit(Op::Pop);
                        }
                        RestPre::Computed(t_obj, t_key) => {
                            let t_val = self.temp();
                            self.emit(Op::InitCell(t_val));
                            self.emit(Op::LoadCell(t_obj));
                            self.emit(Op::LoadCell(t_key));
                            self.emit(Op::LoadCell(t_val));
                            self.emit(Op::SetPropDynamic);
                            self.emit(Op::Pop);
                        }
                        RestPre::Private(t_obj, name) => {
                            let t_val = self.temp();
                            self.emit(Op::InitCell(t_val));
                            self.emit(Op::LoadCell(t_obj));
                            self.emit(Op::LoadCell(t_val));
                            self.emit_private_set_op(&name)?;
                            self.emit(Op::Pop);
                        }
                        RestPre::Plain => {
                            self.assign_target(&rest.target)?; // consumes arr
                        }
                    }
                }
                self.emit(Op::PopTryHandler);
                self.cur().handler_depth -= 1;
                self.cur().finally_depth -= 1;
                let normal_to_close = self.emit(Op::Jump(0));
                let close_ip = self.here();
                self.patch_finally(push, close_ip);
                self.patch_jump(normal_to_close, close_ip);
                self.emit(Op::LoadCell(done_cell));
                let skip_close = self.emit(Op::JumpIfTrue(0));
                self.emit(Op::LoadCell(itc));
                self.emit(Op::IteratorClose);
                let after_close = self.here();
                self.patch_jump(skip_close, after_close);
                self.emit(Op::EndFinally);
                // Iterator consumed; nothing left on the stack to drop.
            }
            AssignmentTarget::ObjectAssignmentTarget(o) => {
                // RequireObjectCoercible before any property access (empty pattern
                // still rejects a nullish source).
                self.emit(Op::RequireObjectCoercible);
                let mut taken: Vec<String> = Vec::new();
                let mut taken_cells: Vec<u32> = Vec::new();
                let has_rest = o.rest.is_some();
                for prop in &o.properties {
                    match prop {
                        AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(pi) => {
                            self.emit(Op::Dup);
                            taken.push(pi.binding.name.as_str().to_string());
                            let k = self.str_const(pi.binding.name.as_str());
                            self.emit(Op::GetProp(k));
                            if let Some(init) = &pi.init {
                                self.emit(Op::Dup);
                                self.emit(Op::LoadUndefined);
                                self.emit(Op::StrictEq);
                                let jf = self.emit(Op::JumpIfFalse(0));
                                self.emit(Op::Pop);
                                // Named evaluation: `{ x = function(){} } = obj`.
                                self.compile_named_expr(init, pi.binding.name.as_str())?;
                                let t = self.here();
                                self.patch_jump(jf, t);
                            }
                            self.store_binding_assign(pi.binding.name.as_str());
                        }
                        AssignmentTargetProperty::AssignmentTargetPropertyProperty(pp) => {
                            self.emit(Op::Dup);
                            if let Some(e) = pp.name.as_expression() {
                                self.compile_expr(e)?;
                                if has_rest {
                                    self.emit(Op::ToPropertyKey);
                                    let t = self.temp();
                                    self.emit(Op::InitCell(t));
                                    self.emit(Op::LoadCell(t));
                                    taken_cells.push(t);
                                }
                                self.emit(Op::GetPropDynamic);
                            } else {
                                let name = property_key_name(&pp.name);
                                taken.push(name.clone());
                                let k = self.str_const(&name);
                                self.emit(Op::GetProp(k));
                            }
                            self.assign_maybe_default(&pp.binding)?;
                        }
                    }
                }
                if let Some(rest) = &o.rest {
                    // `{ ...rest } = obj`: copy own-enumerable keys minus the taken
                    // ones, then assign the resulting object to the rest target.
                    self.emit(Op::Dup); // [src, src]
                    self.compile_object_rest(&taken, &taken_cells)?; // [src, restObj]
                    self.assign_target(&rest.target)?; // [src]
                }
                self.emit(Op::Pop);
            }
            _ => return Err("unsupported destructuring target".into()),
        }
        Ok(())
    }

    /// One array-destructuring-assignment element, in spec order
    /// (IteratorDestructuringAssignmentEvaluation, AssignmentElement): a
    /// member-expression target's REFERENCE (its object/key expressions) is
    /// evaluated BEFORE the iterator is stepped — so a throw there closes the
    /// iterator without `next()` ever running — and the write happens after
    /// the (possibly defaulted) value is computed.
    fn assign_element_ordered(
        &mut self,
        m: &AssignmentTargetMaybeDefault,
        itc: u32,
        done_cell: u32,
    ) -> R {
        let (target, init): (&AssignmentTarget, Option<&Expression>) = match m {
            AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(d) => {
                (&d.binding, Some(&d.init))
            }
            _ => match m.as_assignment_target() {
                Some(t) => (t, None),
                None => return Err("unsupported destructuring element".into()),
            },
        };
        // `[value]` -> `[value-or-default]`.
        let apply_default = |c: &mut Self, init: Option<&Expression>| -> R {
            if let Some(init) = init {
                c.emit(Op::Dup);
                c.emit(Op::LoadUndefined);
                c.emit(Op::StrictEq);
                let jf = c.emit(Op::JumpIfFalse(0));
                c.emit(Op::Pop);
                c.compile_expr(init)?;
                let t = c.here();
                c.patch_jump(jf, t);
            }
            Ok(())
        };
        match target {
            AssignmentTarget::StaticMemberExpression(sm) => {
                let t_obj = self.temp();
                self.compile_expr(&sm.object)?;
                self.emit(Op::InitCell(t_obj));
                self.emit_iter_step_tracked(itc, done_cell); // [value]
                apply_default(self, init)?;
                let t_val = self.temp();
                self.emit(Op::InitCell(t_val));
                self.emit(Op::LoadCell(t_obj));
                self.emit(Op::LoadCell(t_val));
                let k = self.str_const(sm.property.name.as_str());
                self.emit(Op::SetProp(k));
                self.emit(Op::Pop);
            }
            AssignmentTarget::ComputedMemberExpression(cm) => {
                let t_obj = self.temp();
                let t_key = self.temp();
                self.compile_expr(&cm.object)?;
                self.emit(Op::InitCell(t_obj));
                self.compile_expr(&cm.expression)?;
                self.emit(Op::InitCell(t_key));
                self.emit_iter_step_tracked(itc, done_cell); // [value]
                apply_default(self, init)?;
                let t_val = self.temp();
                self.emit(Op::InitCell(t_val));
                self.emit(Op::LoadCell(t_obj));
                self.emit(Op::LoadCell(t_key));
                self.emit(Op::LoadCell(t_val));
                self.emit(Op::SetPropDynamic);
                self.emit(Op::Pop);
            }
            AssignmentTarget::PrivateFieldExpression(pm) => {
                let t_obj = self.temp();
                self.compile_expr(&pm.object)?;
                self.emit(Op::InitCell(t_obj));
                self.emit_iter_step_tracked(itc, done_cell); // [value]
                apply_default(self, init)?;
                let t_val = self.temp();
                self.emit(Op::InitCell(t_val));
                self.emit(Op::LoadCell(t_obj));
                self.emit(Op::LoadCell(t_val));
                self.emit_private_set_op(pm.field.name.as_str())?;
                self.emit(Op::Pop);
            }
            _ => {
                self.emit_iter_step_tracked(itc, done_cell); // [value]
                self.assign_maybe_default(m)?;
            }
        }
        Ok(())
    }

    fn assign_maybe_default(&mut self, m: &AssignmentTargetMaybeDefault) -> R {
        if let AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(d) = m {
            self.emit(Op::Dup);
            self.emit(Op::LoadUndefined);
            self.emit(Op::StrictEq);
            let jf = self.emit(Op::JumpIfFalse(0));
            self.emit(Op::Pop);
            // Named evaluation for a plain-identifier assignment target.
            if let AssignmentTarget::AssignmentTargetIdentifier(id) = &d.binding {
                self.compile_named_expr(&d.init, id.name.as_str())?;
            } else {
                self.compile_expr(&d.init)?;
            }
            let t = self.here();
            self.patch_jump(jf, t);
            self.assign_target(&d.binding)?;
        } else if let Some(t) = m.as_assignment_target() {
            self.assign_target(t)?;
        } else {
            return Err("unsupported destructuring element".into());
        }
        Ok(())
    }
}

fn compound_op(op: oxc::syntax::operator::AssignmentOperator) -> Op {
    use oxc::syntax::operator::AssignmentOperator as A;
    match op {
        A::Addition => Op::Add,
        A::Subtraction => Op::Sub,
        A::Multiplication => Op::Mul,
        A::Division => Op::Div,
        A::Remainder => Op::Mod,
        A::Exponential => Op::Pow,
        A::ShiftLeft => Op::Shl,
        A::ShiftRight => Op::Shr,
        A::ShiftRightZeroFill => Op::UShr,
        A::BitwiseOR => Op::BitOr,
        A::BitwiseXOR => Op::BitXor,
        A::BitwiseAnd => Op::BitAnd,
        _ => Op::Nop,
    }
}

fn property_key_name(key: &PropertyKey) -> String {
    match key {
        PropertyKey::StaticIdentifier(id) => id.name.as_str().to_string(),
        PropertyKey::PrivateIdentifier(id) => format!("#{}", id.name.as_str()),
        _ => {
            if let Some(e) = key.as_expression() {
                match e {
                    Expression::StringLiteral(s) => s.value.as_str().to_string(),
                    Expression::NumericLiteral(n) => crate::vm::number_to_string(n.value),
                    _ => String::new(),
                }
            } else {
                String::new()
            }
        }
    }
}

fn expr_kind(e: &Expression) -> &'static str {
    match e {
        Expression::JSXElement(_) | Expression::JSXFragment(_) => "JSX",
        Expression::V8IntrinsicExpression(_) => "V8Intrinsic",
        Expression::TSInstantiationExpression(_) => "TSInstantiation",
        Expression::Super(_) => "Super",
        _ => "Expression(other)",
    }
}

// =========================================================================
// Functions & classes
// =========================================================================

impl Compiler {
    fn compile_function(&mut self, f: &Function, name: Option<&str>) -> R {
        let method = std::mem::take(&mut self.pending_method);
        let kind = match (f.r#async, f.generator, method) {
            (true, true, true) => FuncKind::AsyncGeneratorMethod,
            (true, true, false) => FuncKind::AsyncGenerator,
            (false, true, true) => FuncKind::GeneratorMethod,
            (false, true, false) => FuncKind::Generator,
            (true, false, true) => FuncKind::AsyncMethod,
            (true, false, false) => FuncKind::Async,
            (false, false, true) => FuncKind::Method,
            (false, false, false) => FuncKind::Normal,
        };
        let body = f.body.as_deref();
        self.emit_function_core(&f.params, body, false, None, kind, name, None)
    }

    fn compile_arrow(&mut self, a: &ArrowFunctionExpression, name: Option<&str>) -> R {
        let kind = if a.r#async {
            FuncKind::AsyncArrow
        } else {
            FuncKind::Arrow
        };
        let ret_expr: Option<&Expression> = if a.expression {
            match a.body.statements.first() {
                Some(Statement::ExpressionStatement(es)) => Some(&es.expression),
                _ => None,
            }
        } else {
            None
        };
        self.emit_function_core(&a.params, Some(&*a.body), true, ret_expr, kind, name, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_function_core(
        &mut self,
        params: &FormalParameters,
        body: Option<&FunctionBody>,
        arrow: bool,
        arrow_ret: Option<&Expression>,
        kind: FuncKind,
        name: Option<&str>,
        ctor_fields: Option<&[&PropertyDefinition]>,
    ) -> R {
        let mut fc = FnCtx::new(name.unwrap_or(""), kind);
        // A function defined inside a `with` block (directly or transitively)
        // resolves free identifiers against the captured with-scope chain.
        fc.enclosed_in_with = self
            .fns
            .last()
            .map(|f| f.with_depth > 0 || f.enclosed_in_with || f.contains_eval)
            .unwrap_or(false);
        {
            let end = body.map(|b| b.span.end).unwrap_or(params.span.end);
            fc.contains_eval = self.region_has_eval(params.span.start, end);
        }
        // `num_params` carries the function's `length` (ExpectedArgumentCount):
        // the count of leading formal parameters before the first one with a
        // default initializer or the rest element. A destructuring param with no
        // default still counts; a rest param is excluded (it is not in `items`).
        fc.num_params = params
            .items
            .iter()
            .take_while(|p| p.initializer.is_none())
            .count() as u32;
        fc.has_rest = params.rest.is_some();
        // Strict mode propagates from the enclosing function, plus a `"use strict"`
        // directive in this body, plus class bodies (always strict).
        let parent_strict = self.fns.last().map(|f| f.strict).unwrap_or(false);
        let own_strict = body
            .map(|b| {
                b.directives
                    .iter()
                    .any(|d| d.directive.as_str() == "use strict")
            })
            .unwrap_or(false);
        let class_strict = kind.is_class_ctor() || self.in_class_body;
        fc.strict = parent_strict || own_strict || class_strict;
        // One-shot: an object-literal concise method/accessor resolves `super`
        // against its [[HomeObject]]. Arrows never consume it (they take the flag
        // false and inherit the class `%superclass` path).
        if !arrow {
            fc.home_super = std::mem::take(&mut self.pending_home_super);
        } else {
            self.pending_home_super = false;
        }
        self.fns.push(fc);
        self.enter_scope(true);
        // A nested non-arrow function owns its own `arguments`/`new.target`:
        // for the field-initializer eval early errors it is no longer
        // "inside" the initializer. Arrows are transparent.
        let saved_in_field_init = self.in_field_initializer;
        if !arrow {
            self.in_field_initializer = false;
        }

        if !arrow {
            let tc = self.declare("%this", false);
            if kind == FuncKind::DerivedCtor {
                // A derived constructor has no `this` until `super()` constructs
                // it: the cell stays in TDZ (reads throw a ReferenceError) and
                // `super()` initializes it in place via BindThis*. The cell is
                // STABLE and recorded on the proto so [[Construct]] can read the
                // final `this` at frame exit (after `finally` blocks have run).
                self.cur().this_cell = Some(tc);
                self.cur().stable_cells.push(tc);
                self.emit(Op::InitCellTdz(tc));
            } else {
                self.emit(Op::LoadThis);
                // Sloppy functions substitute the global object / box a primitive
                // `this` (OrdinaryCallBindThis); strict functions keep it as-is.
                if !self.cur().strict {
                    self.emit(Op::BindThisSloppy);
                }
                self.emit(Op::InitCell(tc));
            }
            let nt = self.declare("%newtarget", false);
            self.emit(Op::LoadNewTarget);
            self.emit(Op::InitCell(nt));
            if kind == FuncKind::DerivedCtor {
                // Instance fields/private brands install when `super()` returns,
                // not at constructor entry. The work lives in a closure so a
                // `super()` reached from a nested arrow (or direct eval) can run
                // it against the freshly bound `this`.
                let fi = self.declare("%fieldinit", false);
                self.emit_field_init_closure(ctor_fields.unwrap_or(&[]))?;
                self.emit(Op::InitCell(fi));
            }
            // The `arguments` object is materialized (an allocation per call) only
            // when the body actually mentions `arguments` — the common case skips
            // it entirely. Scanning the source region for the word never produces
            // a false negative (if `arguments` is used, the word is present).
            // A direct eval may reference `arguments` even when the body text
            // doesn't, so a function containing eval always materializes it.
            let end = body.map(|b| b.span.end).unwrap_or(params.span.end);
            if self.region_has_arguments(params.span.start, end) || self.cur_ref().contains_eval {
                self.cur().uses_arguments = true;
                let ac = self.declare("arguments", false);
                self.emit(Op::LoadArguments);
                self.emit(Op::InitCell(ac));
            }
        }
        // Functions containing direct eval get an eval-vars object (sloppy
        // eval `var`s land there; it rides the with-scope chain so dynamic
        // name ops and nested closures see them).
        if self.cur_ref().contains_eval {
            self.emit(Op::InitEvalVars);
        }

        // Parameters. Each has an optional default (`initializer`) applied when
        // the argument is `undefined`, then the (possibly destructuring) pattern
        // binds the result.
        //
        // Per FunctionDeclarationInstantiation, every parameter binding is
        // *created* (uninitialized) before any initializer runs. A default that
        // references its own parameter — or a later, not-yet-initialized one —
        // therefore hits the Temporal Dead Zone (ReferenceError). We model that
        // only when some parameter actually has a default: simple-identifier
        // params are pre-declared as TDZ cells up front and then initialized
        // *in place* (StoreCell, never a fresh `Rc`) so the single shared
        // parameter environment record is honored — a default arrow capturing a
        // later param sees that param's value once it is initialized. When no
        // parameter has a default there is no observable TDZ, so the original
        // declare-on-bind fast path is kept untouched.
        let has_param_default = params.items.iter().any(|p| p.initializer.is_some());
        let mut param_cells: Vec<Option<u32>> = Vec::with_capacity(params.items.len());
        if has_param_default {
            for p in &params.items {
                if let BindingPattern::BindingIdentifier(id) = &p.pattern {
                    let cell = self.declare(id.name.as_str(), false);
                    self.emit(Op::InitCellTdz(cell));
                    param_cells.push(Some(cell));
                } else {
                    param_cells.push(None);
                }
            }
        }
        {
            let mut all: Vec<String> = Vec::new();
            for p in &params.items {
                if let BindingPattern::BindingIdentifier(id) = &p.pattern {
                    all.push(id.name.as_str().to_string());
                }
            }
            let fc = self.cur();
            fc.all_param_names = all;
            fc.in_params = true;
        }
        for (i, p) in params.items.iter().enumerate() {
            self.emit(Op::LoadArg(i as u32));
            if let Some(init) = &p.initializer {
                self.emit(Op::Dup);
                self.emit(Op::LoadUndefined);
                self.emit(Op::StrictEq);
                let jf = self.emit(Op::JumpIfFalse(0));
                self.emit(Op::Pop); // drop the undefined arg
                if let BindingPattern::BindingIdentifier(id) = &p.pattern {
                    self.compile_named_expr(init, id.name.as_str())?;
                } else {
                    self.compile_expr(init)?;
                }
                let target = self.here();
                self.patch_jump(jf, target);
            }
            match param_cells.get(i).copied().flatten() {
                // Pre-declared simple-identifier param: initialize in place.
                Some(cell) => {
                    self.emit(Op::StoreCell(cell));
                }
                // Destructuring param (or no defaults present): declare on bind.
                None => self.bind_pattern(&p.pattern, false)?,
            }
            if let BindingPattern::BindingIdentifier(id) = &p.pattern {
                self.cur().param_names.push(id.name.as_str().to_string());
            } else {
                self.cur().param_names.push(String::new());
            }
        }
        if let Some(rest) = &params.rest {
            self.emit(Op::LoadRestArgs(params.items.len() as u32));
            self.bind_pattern(&rest.rest.argument, false)?;
        }
        self.cur().in_params = false;

        // A MAPPED `arguments` object (sloppy, simple parameter list) aliases
        // the parameter cells: record each positional parameter's cell index.
        // A name duplicated later in the list maps only its LAST index.
        // Only built when the function can actually materialize `arguments`
        // (`uses_arguments`: the body mentions the word, or contains a direct
        // eval that could). Otherwise the aliasing is dead data that would
        // needlessly pin every parameter as a stable heap cell — blocking the
        // cells→locals localization of the hottest binding class, parameters.
        if self.cur_ref().uses_arguments
            && !self.cur_ref().strict
            && params.rest.is_none()
            && !has_param_default
            && params
                .items
                .iter()
                .all(|p| matches!(&p.pattern, BindingPattern::BindingIdentifier(_)))
        {
            let names: Vec<&str> = params
                .items
                .iter()
                .filter_map(|p| match &p.pattern {
                    BindingPattern::BindingIdentifier(id) => Some(id.name.as_str()),
                    _ => None,
                })
                .collect();
            let mut cells: Vec<Option<u32>> = Vec::with_capacity(names.len());
            for (i, n) in names.iter().enumerate() {
                let shadowed = names[i + 1..].contains(n);
                cells.push(if shadowed {
                    None
                } else {
                    self.current_scope_cell(n)
                });
            }
            if cells.iter().any(|c| c.is_some()) {
                // The aliased cells must be STABLE: the arguments object may
                // capture them before InitCell runs for the parameter, so the
                // init must mutate the Rc in place, never replace it.
                let stable: Vec<u32> = cells.iter().flatten().copied().collect();
                self.cur().stable_cells.extend(stable);
                self.cur().mapped_param_cells = cells;
            }
        }

        // A base-class constructor installs instance fields/brands at entry; a
        // derived one defers them to `super()` (see %fieldinit above).
        if kind != FuncKind::DerivedCtor {
            if let Some(fields) = ctor_fields {
                self.emit_instance_private_stamps();
                self.emit_field_definitions(fields)?;
            }
        }

        // Generators evaluate their parameters at call time, then suspend before
        // the body (which runs lazily on the first `.next()`).
        if kind.is_generator() {
            self.emit(Op::GeneratorStart);
        }

        // Body.
        if let Some(ret) = arrow_ret {
            self.compile_expr(ret)?;
            self.emit(Op::Return);
        } else if let Some(b) = body {
            if Self::stmts_have_using(&b.statements) {
                // A `using` at function-body top level disposes when the BODY
                // exits (return/throw/fall-through all route through the
                // dispose landing pad).
                let pad_async = Self::stmts_have_await_using(&b.statements);
                self.compile_with_dispose_scope(pad_async, |c| {
                    c.hoist_lexical(&b.statements);
                    c.hoist_vars_all(&b.statements);
                    c.hoist_funcs(&b.statements)?;
                    for s in &b.statements {
                        c.compile_stmt(s)?;
                    }
                    c.emit(Op::LoadUndefined);
                    c.emit(Op::Return);
                    Ok(())
                })?;
            } else {
                self.hoist_lexical(&b.statements);
                self.hoist_vars_all(&b.statements);
                self.hoist_funcs(&b.statements)?;
                for s in &b.statements {
                    self.compile_stmt(s)?;
                }
                self.emit(Op::LoadUndefined);
                self.emit(Op::Return);
            }
        } else {
            self.emit(Op::LoadUndefined);
            self.emit(Op::Return);
        }

        self.in_field_initializer = saved_in_field_init;
        self.exit_scope();
        let fc = self.fns.pop().unwrap();
        let proto = self.finish(fc);
        let idx = self.konst(Const::Func(Rc::new(proto)));
        self.emit(Op::Closure(idx));
        Ok(())
    }

    /// Resolve `#name` against the enclosing class bodies (innermost first):
    /// the runtime storage key, the class id, and the element kind. `None`
    /// when no enclosing class (or seeded direct-eval scope) declares it.
    fn resolve_private(&self, name: &str) -> Option<(String, u32, PrivKind)> {
        for ctx in self.class_privs.iter().rev() {
            if let Some(k) = ctx.names.get(name) {
                return Some((format!("#{}@{}", name, ctx.id), ctx.id, *k));
            }
        }
        None
    }

    /// Compile a class field initializer VALUE with the initializer's
    /// function-environment shape: `new.target` is `undefined` (initializers
    /// run as ordinary Calls of synthetic functions), and `arguments` is an
    /// early error — including via direct eval and through arrows.
    /// `[..] -> [.., value]`.
    fn compile_field_initializer_value(
        &mut self,
        init: Option<&Expression>,
        named: Option<&str>,
    ) -> R {
        self.enter_scope(false);
        let nt = self.declare("%newtarget", false);
        self.emit(Op::LoadUndefined);
        self.emit(Op::InitCell(nt));
        let was = std::mem::replace(&mut self.in_field_initializer, true);
        let r = match init {
            Some(e) => match named {
                Some(n) => self.compile_named_expr(e, n),
                None => self.compile_expr(e),
            },
            None => {
                self.emit(Op::LoadUndefined);
                Ok(())
            }
        };
        self.in_field_initializer = was;
        self.exit_scope();
        r
    }

    /// The compile-time storage key for `#name`, or the spec's early
    /// SyntaxError when no enclosing class (or seeded eval scope) declares it.
    fn private_storage_key(&self, name: &str) -> Result<String, String> {
        match self.resolve_private(name) {
            Some((key, _, _)) => Ok(key),
            None => Err(format!(
                "Private field '#{name}' must be declared in an enclosing class"
            )),
        }
    }

    /// `[obj] -> [value]`: private read for `#name`, resolved at runtime
    /// through the frame's PrivateEnvironment chain.
    fn emit_private_get_op(&mut self, name: &str) -> R {
        let key = self.private_storage_key(name)?;
        let k = self.str_const(&key);
        self.emit(Op::PrivateGet(k));
        Ok(())
    }

    /// `[obj, value] -> [value]`: private write for `#name`.
    fn emit_private_set_op(&mut self, name: &str) -> R {
        let key = self.private_storage_key(name)?;
        let k = self.str_const(&key);
        self.emit(Op::PrivateSet(k));
        Ok(())
    }

    fn compile_class(&mut self, class: &Class, name: Option<&str>) -> R {
        // Collect this class's private names (with kinds) and enter its
        // private scope: every `#x` below — in the constructor, methods,
        // initializers, and nested classes — resolves innermost-first.
        {
            let id = self.next_class_id;
            self.next_class_id += 1;
            let mut names: std::collections::HashMap<String, PrivKind> =
                std::collections::HashMap::new();
            let mut order: Vec<String> = Vec::new();
            let mut instance_groups: Vec<(String, PrivKind)> = Vec::new();
            for el in &class.body.body {
                match el {
                    ClassElement::PropertyDefinition(p) => {
                        if let PropertyKey::PrivateIdentifier(pid) = &p.key {
                            let name = pid.name.as_str().to_string();
                            if !names.contains_key(&name) {
                                order.push(name.clone());
                            }
                            names.insert(
                                name,
                                if p.r#static {
                                    PrivKind::StaticField
                                } else {
                                    PrivKind::Field
                                },
                            );
                        }
                    }
                    ClassElement::MethodDefinition(m) => {
                        if let PropertyKey::PrivateIdentifier(pid) = &m.key {
                            let accessor = matches!(
                                m.kind,
                                MethodDefinitionKind::Get | MethodDefinitionKind::Set
                            );
                            let kind = match (m.r#static, accessor) {
                                (false, false) => PrivKind::Method,
                                (false, true) => PrivKind::Accessor,
                                (true, false) => PrivKind::StaticMethod,
                                (true, true) => PrivKind::StaticAccessor,
                            };
                            let name = pid.name.as_str().to_string();
                            if !names.contains_key(&name) {
                                order.push(name.clone());
                                if matches!(kind, PrivKind::Method | PrivKind::Accessor) {
                                    instance_groups.push((name.clone(), kind));
                                }
                            }
                            names.insert(name, kind);
                        }
                    }
                    _ => {}
                }
            }
            self.class_privs.push(ClassPrivCtx {
                id,
                names,
                order,
                instance_groups,
            });
        }
        let was_class_body = std::mem::replace(&mut self.in_class_body, true);
        let result = self.compile_class_inner(class, name);
        self.in_class_body = was_class_body;
        self.class_privs.pop();
        result
    }

    fn compile_class_inner(&mut self, class: &Class, name: Option<&str>) -> R {
        // The class's own lexical scope: holds `%superclass` and (for a NAMED
        // class) the inner self-binding — a `const` visible to every method
        // and initializer, independent of the outer (mutable) declaration.
        self.enter_scope(false);
        let result = self.compile_class_scoped(class, name);
        self.exit_scope();
        result
    }

    fn compile_class_scoped(&mut self, class: &Class, name: Option<&str>) -> R {
        // NewPrivateEnvironment for this class evaluation — before the
        // heritage evaluates (an `extends` expression can reference `#x`) and
        // before any member closure is created (they capture the chain). A
        // fresh runtime name per key per evaluation is what makes brand
        // checks fail across two evaluations of the same class literal.
        let has_priv_env = self.class_privs.last().is_some_and(|c| !c.order.is_empty());
        if has_priv_env {
            let keys: Vec<String> = {
                let ctx = self.class_privs.last().unwrap();
                ctx.order
                    .iter()
                    .map(|n| format!("#{}@{}", n, ctx.id))
                    .collect()
            };
            let keys: Vec<u32> = keys.iter().map(|k| self.str_const(k)).collect();
            self.emit(Op::PushPrivateEnv(keys.into()));
        }
        // A class with a binding identifier binds it INSIDE the class scope as
        // an immutable (const) self-reference: in TDZ while the heritage and
        // computed keys evaluate, then initialized to the constructor; methods
        // and static initializers see the class even if the outer binding is
        // reassigned, and writes to it throw a TypeError (class bodies are
        // strict).
        let self_cell = class.id.as_ref().map(|id| {
            let c = self.declare_kind(id.name.as_str(), false, true);
            self.emit(Op::InitCellTdz(c));
            c
        });
        // Superclass (if any) stored in a captured binding so methods can `super`.
        let has_super = class.super_class.is_some();
        if let Some(sc) = &class.super_class {
            self.compile_expr(sc)?;
            let c = self.declare("%superclass", false);
            self.emit(Op::InitCell(c));
        }

        // Collect instance fields and the constructor.
        let mut instance_fields: Vec<&PropertyDefinition> = Vec::new();
        let mut ctor_method: Option<&MethodDefinition> = None;
        for el in &class.body.body {
            match el {
                ClassElement::PropertyDefinition(p) if !p.r#static => instance_fields.push(p),
                ClassElement::MethodDefinition(m)
                    if matches!(m.kind, MethodDefinitionKind::Constructor) =>
                {
                    ctor_method = Some(m);
                }
                _ => {}
            }
        }

        // Computed instance-field KEYS evaluate once, at class-definition time
        // (spec ClassFieldDefinitionEvaluation), in element order — not per
        // construction. Declare a class-scope cell per computed field now (so
        // the constructor closure captures it) and fill it in the element walk
        // below; the per-instance initializer only evaluates the VALUE.
        for (i, p) in instance_fields.iter().enumerate() {
            if p.computed {
                let c = self.declare(&format!("%fieldkey{i}"), false);
                self.emit(Op::InitCellTdz(c));
            }
        }

        // Class-scope cells for private INSTANCE methods/accessors, declared
        // (TDZ) before the constructor compiles so its construction-time
        // stamp code captures them; the element walk below fills them. The
        // cells mutate in place (StoreCell), so the capture stays live.
        let instance_groups: Vec<(String, PrivKind)> = self
            .class_privs
            .last()
            .map(|c| c.instance_groups.clone())
            .unwrap_or_default();
        for (n, k) in &instance_groups {
            if matches!(k, PrivKind::Method) {
                let c = self.declare(&format!("%privm#{n}"), false);
                self.emit(Op::InitCellTdz(c));
            } else {
                let c = self.declare(&format!("%privg#{n}"), false);
                self.emit(Op::InitCellTdz(c));
                let c = self.declare(&format!("%privs#{n}"), false);
                self.emit(Op::InitCellTdz(c));
            }
        }

        // Build the constructor closure, then stash it in a temp cell so the rest
        // of class building can address it cleanly.
        if let Some(m) = ctor_method {
            self.emit_function_core(
                &m.value.params,
                m.value.body.as_deref(),
                false,
                None,
                if has_super {
                    FuncKind::DerivedCtor
                } else {
                    FuncKind::ClassCtor
                },
                name,
                Some(&instance_fields),
            )?;
        } else {
            self.synthesize_constructor(name, has_super, &instance_fields)?;
        }
        let ctor_cell = self.temp();
        self.emit(Op::InitCell(ctor_cell));

        // Initialize the self-binding (StoreCell mutates the TDZ cell in
        // place, so closures created either side observe the same cell).
        if let Some(c) = self_cell {
            self.emit(Op::LoadCell(ctor_cell));
            self.emit(Op::StoreCell(c));
        }

        // The constructor's [[HomeObject]] is the class prototype, so
        // `super.x` in the constructor body (and in instance field
        // initializers, which run inline in the constructor frame) resolves
        // against the superclass prototype chain.
        self.emit(Op::LoadCell(ctor_cell));
        let proto_k = self.str_const("prototype");
        self.emit(Op::GetProp(proto_k)); // [proto]
        self.emit(Op::LoadCell(ctor_cell)); // [proto, ctor]
        self.emit(Op::SetHomeObjectAt(1));
        self.emit(Op::Pop);
        self.emit(Op::Pop);

        if has_super {
            self.class_link_super(ctor_cell)?;
        }

        // Group private method/accessor elements by name, emitted at the
        // FIRST element of each group: closure creation is unobservable, so
        // pairing a getter with its later setter there is equivalent to the
        // spec's merge of accessor halves into one PrivateElement.
        struct PrivGroup<'a, 'b> {
            is_static: bool,
            method: Option<&'b MethodDefinition<'a>>,
            getter: Option<&'b MethodDefinition<'a>>,
            setter: Option<&'b MethodDefinition<'a>>,
            emitted: bool,
        }
        let mut priv_groups: std::collections::HashMap<String, PrivGroup> =
            std::collections::HashMap::new();
        for el in &class.body.body {
            if let ClassElement::MethodDefinition(m) = el {
                if matches!(m.kind, MethodDefinitionKind::Constructor) {
                    continue;
                }
                if let PropertyKey::PrivateIdentifier(pid) = &m.key {
                    let g = priv_groups
                        .entry(pid.name.as_str().to_string())
                        .or_insert(PrivGroup {
                            is_static: m.r#static,
                            method: None,
                            getter: None,
                            setter: None,
                            emitted: false,
                        });
                    match m.kind {
                        MethodDefinitionKind::Get => g.getter = Some(m),
                        MethodDefinitionKind::Set => g.setter = Some(m),
                        _ => g.method = Some(m),
                    }
                }
            }
        }

        // ClassDefinitionEvaluation phases: the element walk defines all
        // methods and evaluates every computed KEY in element order, but
        // STATIC initializers (field values and `static {}` blocks) are
        // deferred and run AFTER the walk, in element order — so a static
        // initializer can see methods declared after it, and a later
        // element's computed key evaluates before any static value runs.
        enum StaticEl<'a, 'b> {
            Field(&'b PropertyDefinition<'a>, Option<u32>),
            Block(&'b StaticBlock<'a>),
        }
        let mut static_els: Vec<StaticEl> = Vec::new();
        let mut ifield_idx = 0usize;
        for el in &class.body.body {
            match el {
                ClassElement::MethodDefinition(m)
                    if !matches!(m.kind, MethodDefinitionKind::Constructor) =>
                {
                    if let PropertyKey::PrivateIdentifier(pid) = &m.key {
                        let name = pid.name.as_str().to_string();
                        let (is_static, method, getter, setter, emitted) = {
                            let g = priv_groups.get_mut(&name).unwrap();
                            let was = g.emitted;
                            g.emitted = true;
                            (g.is_static, g.method, g.getter, g.setter, was)
                        };
                        if !emitted {
                            self.class_define_private_group(
                                ctor_cell, &name, is_static, method, getter, setter,
                            )?;
                        }
                    } else {
                        self.class_define_method(ctor_cell, m)?;
                    }
                }
                ClassElement::PropertyDefinition(p) if p.r#static => {
                    // Evaluate a computed KEY now (in element order, including
                    // the "prototype" runtime TypeError); the VALUE runs in
                    // the static phase below.
                    let key_cell = if p.computed {
                        self.compile_property_key_expr(&p.key)?;
                        self.emit(Op::ToPropertyKey);
                        self.emit(Op::Dup);
                        self.load_str("prototype");
                        self.emit(Op::StrictEq);
                        let jok = self.emit(Op::JumpIfFalse(0));
                        let tk = self.str_const("TypeError");
                        self.emit(Op::LoadGlobal(tk));
                        let mk = self
                            .str_const("Classes may not have a static property named 'prototype'");
                        self.emit(Op::LoadConst(mk));
                        self.emit(Op::New(1));
                        self.emit(Op::Throw);
                        let ok = self.here();
                        self.patch_jump(jok, ok);
                        let cell = self.temp();
                        self.emit(Op::InitCell(cell));
                        Some(cell)
                    } else {
                        None
                    };
                    static_els.push(StaticEl::Field(p, key_cell));
                }
                ClassElement::PropertyDefinition(p) => {
                    // Non-static field: evaluate a computed KEY now, in element
                    // order, into its class-scope cell.
                    let i = ifield_idx;
                    ifield_idx += 1;
                    if p.computed {
                        self.compile_property_key_expr(&p.key)?;
                        self.emit(Op::ToPropertyKey);
                        match self.resolve(&format!("%fieldkey{i}")) {
                            Resolved::Cell(c) => self.emit(Op::StoreCell(c)),
                            _ => unreachable!("%fieldkey declared in this scope"),
                        };
                    }
                }
                ClassElement::StaticBlock(b) => static_els.push(StaticEl::Block(b)),
                _ => {}
            }
        }

        // Static phase: field initializers and `static {}` blocks, in order.
        for el in static_els {
            match el {
                StaticEl::Field(p, key_cell) => {
                    self.class_define_static_field(ctor_cell, p, key_cell)?;
                }
                StaticEl::Block(b) => self.emit_static_block(ctor_cell, &b.body)?,
            }
        }

        if has_priv_env {
            self.emit(Op::PopPrivateEnv);
        }
        // Leave the class (constructor) value on the stack.
        self.emit(Op::LoadCell(ctor_cell));
        Ok(())
    }

    /// Emit one private method/accessor group. A STATIC group stamps the
    /// constructor's [[PrivateElements]] now (with [[HomeObject]] = ctor so
    /// `super.x` resolves); an INSTANCE group fills the pre-declared class
    /// scope cells that `emit_instance_private_stamps` reads at construction.
    #[allow(clippy::too_many_arguments)]
    fn class_define_private_group(
        &mut self,
        ctor_cell: u32,
        name: &str,
        is_static: bool,
        method: Option<&MethodDefinition>,
        getter: Option<&MethodDefinition>,
        setter: Option<&MethodDefinition>,
    ) -> R {
        let key = match self.resolve_private(name) {
            Some((key, _, _)) => key,
            None => return Err(format!("private name #{name} not declared")),
        };
        let k = self.str_const(&key);
        if is_static {
            self.emit(Op::LoadCell(ctor_cell)); // [ctor]
            if let Some(m) = method {
                self.pending_home_super = true;
                self.pending_method = true;
                self.compile_function(&m.value, Some(&format!("#{name}")))?;
                self.pending_home_super = false;
                self.emit(Op::SetHomeObjectAt(1)); // [ctor, fn]
                self.emit(Op::PrivateMethodAdd(k)); // [ctor]
            } else {
                match getter {
                    Some(g) => {
                        self.pending_home_super = true;
                        self.pending_method = true;
                        self.compile_function(&g.value, Some(&format!("get #{name}")))?;
                        self.pending_home_super = false;
                        self.emit(Op::SetHomeObjectAt(1));
                    }
                    None => {
                        self.emit(Op::LoadUndefined);
                    }
                }
                match setter {
                    Some(st) => {
                        self.pending_home_super = true;
                        self.pending_method = true;
                        self.compile_function(&st.value, Some(&format!("set #{name}")))?;
                        self.pending_home_super = false;
                        self.emit(Op::SetHomeObjectAt(2));
                    }
                    None => {
                        self.emit(Op::LoadUndefined);
                    }
                }
                self.emit(Op::PrivateAccessorAdd(k)); // [ctor]
            }
            self.emit(Op::Pop);
        } else {
            // Instance group: fill the class-scope cells, stamping each
            // closure's [[HomeObject]] = the class prototype so its
            // `super.x` resolves like a public instance method's.
            self.emit(Op::LoadCell(ctor_cell));
            let proto_k = self.str_const("prototype");
            self.emit(Op::GetProp(proto_k)); // [proto]
            if let Some(m) = method {
                self.pending_method = true;
                self.compile_function(&m.value, Some(&format!("#{name}")))?;
                self.emit(Op::SetHomeObjectAt(1)); // [proto, fn]
                self.store_priv_cell(&format!("%privm#{name}"));
            } else {
                match getter {
                    Some(g) => {
                        self.pending_method = true;
                        self.compile_function(&g.value, Some(&format!("get #{name}")))?;
                        self.emit(Op::SetHomeObjectAt(1));
                    }
                    None => {
                        self.emit(Op::LoadUndefined);
                    }
                }
                self.store_priv_cell(&format!("%privg#{name}"));
                match setter {
                    Some(st) => {
                        self.pending_method = true;
                        self.compile_function(&st.value, Some(&format!("set #{name}")))?;
                        self.emit(Op::SetHomeObjectAt(1));
                    }
                    None => {
                        self.emit(Op::LoadUndefined);
                    }
                }
                self.store_priv_cell(&format!("%privs#{name}"));
            }
            self.emit(Op::Pop); // drop proto
        }
        Ok(())
    }

    /// `StoreCell` (pops the value) into a `%priv…` cell declared in the
    /// current class scope.
    fn store_priv_cell(&mut self, name: &str) {
        match self.resolve(name) {
            Resolved::Cell(c) => {
                self.emit(Op::StoreCell(c));
            }
            _ => unreachable!("%priv cell declared in class scope"),
        }
    }

    /// InitializeInstanceElements step 1: install the class's private
    /// instance methods/accessors on `this` (PrivateMethodOrAccessorAdd, the
    /// spec's brand) from the class-scope cells, before any field
    /// initializers run. A duplicate add — the same object initialized twice
    /// via return-override — is the runtime's TypeError.
    fn emit_instance_private_stamps(&mut self) {
        let (class_id, groups) = match self.class_privs.last() {
            Some(ctx) => (ctx.id, ctx.instance_groups.clone()),
            None => return,
        };
        for (name, kind) in groups {
            let k = self.str_const(&format!("#{name}@{class_id}"));
            self.load_binding("%this");
            if matches!(kind, PrivKind::Method) {
                self.load_binding(&format!("%privm#{name}"));
                self.emit(Op::PrivateMethodAdd(k));
            } else {
                self.load_binding(&format!("%privg#{name}"));
                self.load_binding(&format!("%privs#{name}"));
                self.emit(Op::PrivateAccessorAdd(k));
            }
            self.emit(Op::Pop);
        }
    }

    fn synthesize_constructor(
        &mut self,
        name: Option<&str>,
        has_super: bool,
        fields: &[&PropertyDefinition],
    ) -> R {
        let kind = if has_super {
            FuncKind::DerivedCtor
        } else {
            FuncKind::ClassCtor
        };
        let mut fc = FnCtx::new(name.unwrap_or(""), kind);
        fc.enclosed_in_with = self
            .fns
            .last()
            .map(|f| f.with_depth > 0 || f.enclosed_in_with || f.contains_eval)
            .unwrap_or(false);
        fc.has_rest = has_super;
        fc.num_params = 0;
        fc.strict = true; // class bodies are always strict
        self.fns.push(fc);
        self.enter_scope(true);
        let tc = self.declare("%this", false);
        if has_super {
            self.cur().this_cell = Some(tc);
            self.cur().stable_cells.push(tc);
            self.emit(Op::InitCellTdz(tc));
        } else {
            self.emit(Op::LoadThis);
            self.emit(Op::InitCell(tc));
        }
        let nt = self.declare("%newtarget", false);
        self.emit(Op::LoadNewTarget);
        self.emit(Op::InitCell(nt));
        if has_super {
            // The default derived constructor: `constructor(...args) {
            // super(...args) }` — the parent constructs `this`, then instance
            // fields/brands install on it.
            let fi = self.declare("%fieldinit", false);
            self.emit_field_init_closure(fields)?;
            self.emit(Op::InitCell(fi));
            self.load_binding("%superclass");
            self.emit(Op::LoadRestArgs(0));
            self.load_binding("%newtarget");
            self.emit(Op::ConstructSuperSpread);
            self.emit_super_bind_and_init();
            self.emit(Op::Return); // super() evaluates to the bound `this`
        } else {
            self.emit_instance_private_stamps();
            self.emit_field_definitions(fields)?;
            self.emit(Op::LoadUndefined);
            self.emit(Op::Return);
        }
        self.exit_scope();
        let fc = self.fns.pop().unwrap();
        let proto = self.finish(fc);
        let idx = self.konst(Const::Func(Rc::new(proto)));
        self.emit(Op::Closure(idx));
        Ok(())
    }

    /// Define each instance field on `this` (assumes the surrounding scope can
    /// resolve `%this`): evaluates the (possibly computed) key and initializer
    /// and assigns the result.
    fn emit_field_definitions(&mut self, fields: &[&PropertyDefinition]) -> R {
        for (i, field) in fields.iter().enumerate() {
            self.load_binding("%this");
            if let PropertyKey::PrivateIdentifier(pid) = &field.key {
                // Private field: evaluate the initializer (NamedEvaluation
                // uses the source-visible "#x"), then PrivateFieldAdd —
                // straight onto the receiver's [[PrivateElements]], with
                // no traps and no extensibility check.
                let name = pid.name.as_str();
                let key = self.private_storage_key(name)?;
                self.compile_field_initializer_value(
                    field.value.as_ref(),
                    Some(&format!("#{name}")),
                )?;
                let k = self.str_const(&key);
                self.emit(Op::PrivateFieldAdd(k)); // [this]
                self.emit(Op::Pop);
                continue;
            }
            if field.computed {
                // Computed field key: already evaluated (with ToPropertyKey) at
                // class-definition time into the class-scope `%fieldkey{i}` cell.
                self.load_binding(&format!("%fieldkey{i}")); // [this, key]
                self.compile_field_initializer_value(field.value.as_ref(), None)?;
                // Computed key + anonymous value: NamedEvaluation takes the
                // runtime key.
                if let Some(init) = &field.value {
                    if Self::is_anonymous_fn_expr(init) {
                        let prefix = self.str_const("");
                        self.emit(Op::SetFunctionNameFromKey(prefix));
                    }
                }
            } else {
                let key = property_key_name(&field.key);
                self.load_str(&key); // [this, key]
                self.compile_field_initializer_value(
                    field.value.as_ref(),
                    Some(&property_key_name(&field.key)),
                )?;
            }
            // DefineField (CreateDataPropertyOrThrow): a field is an own data
            // property — an inherited setter/read-only slot must not be hit.
            self.emit(Op::DefineField); // [this]
            self.emit(Op::Pop);
        }
        Ok(())
    }

    /// Build the `%fieldinit` closure of a derived constructor: a function
    /// that, called with the freshly constructed `this`, stamps the private
    /// brand and installs the instance fields (InitializeInstanceElements).
    /// It is invoked by every `super()` site — including ones inside nested
    /// arrows or direct eval, which reach it lexically.
    fn emit_field_init_closure(&mut self, fields: &[&PropertyDefinition]) -> R {
        let mut fc = FnCtx::new("", FuncKind::Method);
        // `super.x` in an instance field initializer resolves against the
        // class prototype — the constructor's [[HomeObject]], inherited at
        // closure creation (the closure is created inside the ctor frame).
        fc.inherit_home = true;
        fc.enclosed_in_with = self
            .fns
            .last()
            .map(|f| f.with_depth > 0 || f.enclosed_in_with || f.contains_eval)
            .unwrap_or(false);
        fc.contains_eval = fields
            .iter()
            .any(|f| self.region_has_eval(f.span.start, f.span.end));
        fc.strict = true; // class bodies are always strict
        self.fns.push(fc);
        self.enter_scope(true);
        let tc = self.declare("%this", false);
        self.emit(Op::LoadThis);
        self.emit(Op::InitCell(tc));
        if self.cur_ref().contains_eval {
            self.emit(Op::InitEvalVars);
        }
        self.emit_instance_private_stamps();
        self.emit_field_definitions(fields)?;
        self.emit(Op::LoadUndefined);
        self.emit(Op::Return);
        self.exit_scope();
        let fc = self.fns.pop().unwrap();
        let proto = self.finish(fc);
        let idx = self.konst(Const::Func(Rc::new(proto)));
        self.emit(Op::Closure(idx));
        Ok(())
    }

    /// After `super()` leaves the constructed instance on the stack: bind it
    /// as `this` (BindThisValue — throws if `super()` already ran) and run the
    /// `%fieldinit` closure against it. `[instance] -> [instance]`.
    fn emit_super_bind_and_init(&mut self) {
        match self.resolve("%this") {
            Resolved::Cell(i) => self.emit(Op::BindThisCell(i)),
            Resolved::Upvalue(i) => self.emit(Op::BindThisUpvalue(i)),
            // No reachable `%this` (malformed context): leave the value as-is.
            Resolved::Global => return,
        };
        if !matches!(self.resolve("%fieldinit"), Resolved::Global) {
            self.emit(Op::Dup); // [this, this]
            self.load_binding("%fieldinit"); // [this, this, fi]
            self.emit(Op::Swap); // [this, fi, this]
            self.emit(Op::Call(0)); // [this, undefined]
            self.emit(Op::Pop); // [this]
        }
    }

    /// Wire `ctor.prototype.__proto__ = Super.prototype` and
    /// `ctor.__proto__ = Super`, handling `extends null` and non-constructor
    /// heritage (TypeError) natively.
    fn class_link_super(&mut self, ctor_cell: u32) -> R {
        self.emit(Op::LoadCell(ctor_cell));
        self.load_binding("%superclass");
        self.emit(Op::ClassLinkSuper);
        Ok(())
    }

    fn class_define_method(&mut self, ctor_cell: u32, m: &MethodDefinition) -> R {
        // target = m.static ? ctor : ctor.prototype
        self.emit(Op::LoadCell(ctor_cell)); // [ctor]
        if !m.r#static {
            let prototype = self.str_const("prototype");
            self.emit(Op::GetProp(prototype)); // [proto]
        }
        if m.computed {
            self.compile_property_key_expr(&m.key)?;
            // ToPropertyKey NOW — see compile_object: the later
            // SetFunctionNameFromKey must not re-run coercion side effects.
            self.emit(Op::ToPropertyKey);
        } else {
            let name = property_key_name(&m.key);
            self.load_str(&name);
        }
        // Accessors take a prefixed function name (`get x` / `set x`).
        let fname = match m.kind {
            MethodDefinitionKind::Get => format!("get {}", property_key_name(&m.key)),
            MethodDefinitionKind::Set => format!("set {}", property_key_name(&m.key)),
            _ => property_key_name(&m.key),
        };
        // A STATIC method's `super` resolves against the constructor's prototype
        // (`getPrototypeOf(ctor)` == the superclass constructor), so give it a
        // [[HomeObject]] = ctor and route its `super` through the home path. (An
        // *instance* method keeps the `%superclass.prototype` path untouched.)
        if m.r#static {
            self.pending_home_super = true;
        }
        self.pending_method = true;
        self.compile_function(&m.value, Some(&fname))?;
        // Computed-key method/accessor: the compile-time name above is just
        // "[computed]" — SetFunctionName with the runtime key.
        if m.computed {
            let prefix = self.str_const(match m.kind {
                MethodDefinitionKind::Get => "get",
                MethodDefinitionKind::Set => "set",
                _ => "",
            });
            self.emit(Op::SetFunctionNameFromKey(prefix));
        }
        self.pending_home_super = false;
        // [target, key, value] — stamp value.[[HomeObject]] = target (the
        // ctor for statics, the prototype for instance methods), so super
        // property references resolve through the home path.
        self.emit(Op::SetHomeObject);
        match m.kind {
            MethodDefinitionKind::Get => {
                self.emit(Op::DefineMethodGetter);
            }
            MethodDefinitionKind::Set => {
                self.emit(Op::DefineMethodSetter);
            }
            _ => {
                // Class methods are non-enumerable (unlike object-literal methods).
                self.emit(Op::DefineMethod);
            }
        }
        self.emit(Op::Pop); // drop target
        Ok(())
    }

    /// Run one STATIC field's initializer (static phase): `this` is the
    /// constructor, `new.target` is undefined, and a computed key was already
    /// evaluated into `key_cell` during the element walk.
    fn class_define_static_field(
        &mut self,
        ctor_cell: u32,
        p: &PropertyDefinition,
        key_cell: Option<u32>,
    ) -> R {
        self.emit(Op::LoadCell(ctor_cell)); // [ctor]
        if let PropertyKey::PrivateIdentifier(pid) = &p.key {
            // Private static field: PrivateFieldAdd on the constructor.
            let name = pid.name.as_str();
            let key = self.private_storage_key(name)?;
            self.emit_static_initializer_closure(p.value.as_ref(), Some(&format!("#{name}")))?; // [ctor, fn]
            self.emit(Op::SetHomeObjectAt(1));
            self.emit(Op::LoadCell(ctor_cell)); // [ctor, fn, ctor(this)]
            self.emit(Op::Call(0)); // [ctor, value]
            let k = self.str_const(&key);
            self.emit(Op::PrivateFieldAdd(k)); // [ctor]
            self.emit(Op::Pop);
            return Ok(());
        }
        if let Some(cell) = key_cell {
            self.emit(Op::LoadCell(cell)); // [ctor, key]
            self.emit_static_initializer_closure(p.value.as_ref(), None)?;
        } else {
            let key = property_key_name(&p.key);
            self.load_str(&key); // [ctor, key]
            self.emit_static_initializer_closure(
                p.value.as_ref(),
                Some(&property_key_name(&p.key)),
            )?;
        }
        // [ctor, key, fn] — home = ctor, then call with this = ctor.
        self.emit(Op::SetHomeObject);
        self.emit(Op::LoadCell(ctor_cell)); // [ctor, key, fn, ctor(this)]
        self.emit(Op::Call(0)); // [ctor, key, value]
        if key_cell.is_some() {
            if let Some(init) = &p.value {
                if Self::is_anonymous_fn_expr(init) {
                    let prefix = self.str_const("");
                    self.emit(Op::SetFunctionNameFromKey(prefix));
                }
            }
        }
        self.emit(Op::DefineField); // [ctor]
        self.emit(Op::Pop);
        Ok(())
    }

    /// Build the synthetic method closure for one STATIC field initializer:
    /// called with `this` = the constructor, `new.target` = undefined,
    /// [[HomeObject]] = the constructor (stamped by the caller), `arguments`
    /// an early error. Leaves the closure on the stack.
    fn emit_static_initializer_closure(
        &mut self,
        init: Option<&Expression>,
        named: Option<&str>,
    ) -> R {
        let mut fc = FnCtx::new("", FuncKind::Method);
        fc.enclosed_in_with = self
            .fns
            .last()
            .map(|f| f.with_depth > 0 || f.enclosed_in_with || f.contains_eval)
            .unwrap_or(false);
        // Conservative: runs once per class definition.
        fc.contains_eval = true;
        fc.strict = true; // class bodies are always strict
        fc.home_super = true;
        self.fns.push(fc);
        self.enter_scope(true);
        let tc = self.declare("%this", false);
        self.emit(Op::LoadThis);
        self.emit(Op::InitCell(tc));
        let nt = self.declare("%newtarget", false);
        self.emit(Op::LoadUndefined);
        self.emit(Op::InitCell(nt));
        self.emit(Op::InitEvalVars);
        let was = std::mem::replace(&mut self.in_field_initializer, true);
        let compiled: R = match init {
            Some(e) => match named {
                Some(n) => self.compile_named_expr(e, n),
                None => self.compile_expr(e),
            },
            None => {
                self.emit(Op::LoadUndefined);
                Ok(())
            }
        };
        self.in_field_initializer = was;
        compiled?;
        self.emit(Op::Return);
        self.exit_scope();
        let fc = self.fns.pop().unwrap();
        let proto = self.finish(fc);
        let idx = self.konst(Const::Func(Rc::new(proto)));
        self.emit(Op::Closure(idx));
        Ok(())
    }

    /// `static { … }`: ClassStaticBlockDefinitionEvaluation — the body runs at
    /// class-definition time, in element order with the static field
    /// initializers, as a synthetic method Call with `this` = the
    /// constructor, `new.target` = undefined, [[HomeObject]] = the
    /// constructor, and `arguments` an early error (functions declared inside
    /// get their own).
    fn emit_static_block(&mut self, ctor_cell: u32, body: &[Statement]) -> R {
        self.emit(Op::LoadCell(ctor_cell)); // [ctor]
        let mut fc = FnCtx::new("", FuncKind::Method);
        fc.enclosed_in_with = self
            .fns
            .last()
            .map(|f| f.with_depth > 0 || f.enclosed_in_with || f.contains_eval)
            .unwrap_or(false);
        // Conservative: scanning the statement spans needs the source region;
        // static blocks are rare and run once, so always install eval-vars.
        fc.contains_eval = true;
        fc.strict = true; // class bodies are always strict
        fc.home_super = true; // super.x resolves via [[HomeObject]] = ctor
        self.fns.push(fc);
        self.enter_scope(true);
        let tc = self.declare("%this", false);
        self.emit(Op::LoadThis);
        self.emit(Op::InitCell(tc));
        let nt = self.declare("%newtarget", false);
        self.emit(Op::LoadUndefined);
        self.emit(Op::InitCell(nt));
        self.emit(Op::InitEvalVars);
        let was = std::mem::replace(&mut self.in_field_initializer, true);
        let compiled: R = (|c: &mut Self| {
            c.hoist_lexical(body);
            c.hoist_vars_all(body);
            c.hoist_funcs(body)?;
            for st in body {
                c.compile_stmt(st)?;
            }
            Ok(())
        })(self);
        self.in_field_initializer = was;
        compiled?;
        self.emit(Op::LoadUndefined);
        self.emit(Op::Return);
        self.exit_scope();
        let fc = self.fns.pop().unwrap();
        let proto = self.finish(fc);
        let idx = self.konst(Const::Func(Rc::new(proto)));
        self.emit(Op::Closure(idx)); // [ctor, block]
        self.emit(Op::SetHomeObjectAt(1));
        self.emit(Op::LoadCell(ctor_cell)); // [ctor, block, ctor(this)]
        self.emit(Op::Call(0)); // [ctor, result]
        self.emit(Op::Pop);
        self.emit(Op::Pop);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Module-declaration AST helpers (free functions)
// ---------------------------------------------------------------------------

/// The local binding name introduced by an import specifier.
fn import_local_name<'a>(spec: &'a ImportDeclarationSpecifier) -> &'a str {
    match spec {
        ImportDeclarationSpecifier::ImportSpecifier(s) => s.local.name.as_str(),
        ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => s.local.name.as_str(),
        ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => s.local.name.as_str(),
    }
}

/// The string value of a `ModuleExportName` (identifier or string-literal form).
fn module_export_name(n: &ModuleExportName) -> String {
    match n {
        ModuleExportName::IdentifierName(i) => i.name.as_str().to_string(),
        ModuleExportName::IdentifierReference(i) => i.name.as_str().to_string(),
        ModuleExportName::StringLiteral(s) => s.value.as_str().to_string(),
    }
}

/// The function declaration carried by a statement, looking through an
/// `export function f(){}` wrapper (used by `hoist_funcs` in module mode).
fn stmt_function_decl<'a>(s: &'a Statement) -> Option<&'a Function<'a>> {
    match s {
        Statement::FunctionDeclaration(f) => Some(f),
        Statement::ExportNamedDeclaration(e) => match e.declaration.as_ref() {
            Some(Declaration::FunctionDeclaration(f)) => Some(f),
            _ => None,
        },
        _ => None,
    }
}

/// The bound names declared by `export <decl>` (for recording local exports).
fn declaration_bound_names(decl: &Declaration) -> Vec<String> {
    let mut out = Vec::new();
    match decl {
        Declaration::VariableDeclaration(d) => {
            for dctor in &d.declarations {
                collect_pattern_names(&dctor.id, &mut out);
            }
        }
        Declaration::FunctionDeclaration(f) => {
            if let Some(id) = &f.id {
                out.push(id.name.as_str().to_string());
            }
        }
        Declaration::ClassDeclaration(c) => {
            if let Some(id) = &c.id {
                out.push(id.name.as_str().to_string());
            }
        }
        _ => {}
    }
    out
}

/// Collect the names of `var` bindings declared anywhere in a module top-level
/// statement (including `export var` and nested blocks/loops) — mirrors the
/// `hoist_vars` traversal, used to initialize module `var` cells to `undefined`.
fn collect_module_var_names(stmt: &Statement, out: &mut Vec<String>) {
    let var_decl = |d: &VariableDeclaration, out: &mut Vec<String>| {
        if matches!(d.kind, VariableDeclarationKind::Var) {
            for decl in &d.declarations {
                collect_pattern_names(&decl.id, out);
            }
        }
    };
    match stmt {
        Statement::VariableDeclaration(d) => var_decl(d, out),
        Statement::ExportNamedDeclaration(e) => {
            if let Some(Declaration::VariableDeclaration(d)) = &e.declaration {
                var_decl(d, out);
            }
        }
        Statement::BlockStatement(b) => {
            b.body.iter().for_each(|s| collect_module_var_names(s, out))
        }
        Statement::IfStatement(i) => {
            collect_module_var_names(&i.consequent, out);
            if let Some(a) = &i.alternate {
                collect_module_var_names(a, out);
            }
        }
        Statement::ForStatement(f) => {
            if let Some(ForStatementInit::VariableDeclaration(d)) = &f.init {
                var_decl(d, out);
            }
            collect_module_var_names(&f.body, out);
        }
        Statement::ForInStatement(f) => {
            if let ForStatementLeft::VariableDeclaration(d) = &f.left {
                var_decl(d, out);
            }
            collect_module_var_names(&f.body, out);
        }
        Statement::ForOfStatement(f) => {
            if let ForStatementLeft::VariableDeclaration(d) = &f.left {
                var_decl(d, out);
            }
            collect_module_var_names(&f.body, out);
        }
        Statement::WhileStatement(w) => collect_module_var_names(&w.body, out),
        Statement::DoWhileStatement(w) => collect_module_var_names(&w.body, out),
        Statement::LabeledStatement(l) => collect_module_var_names(&l.body, out),
        Statement::WithStatement(w) => collect_module_var_names(&w.body, out),
        Statement::TryStatement(t) => {
            t.block
                .body
                .iter()
                .for_each(|s| collect_module_var_names(s, out));
            if let Some(h) = &t.handler {
                h.body
                    .body
                    .iter()
                    .for_each(|s| collect_module_var_names(s, out));
            }
            if let Some(f) = &t.finalizer {
                f.body.iter().for_each(|s| collect_module_var_names(s, out));
            }
        }
        Statement::SwitchStatement(s) => {
            for case in &s.cases {
                case.consequent
                    .iter()
                    .for_each(|st| collect_module_var_names(st, out));
            }
        }
        _ => {}
    }
}

/// Collect the identifier names bound by a (possibly destructuring) pattern.
fn collect_pattern_names(pat: &BindingPattern, out: &mut Vec<String>) {
    match pat {
        BindingPattern::BindingIdentifier(id) => out.push(id.name.as_str().to_string()),
        BindingPattern::ObjectPattern(o) => {
            for p in &o.properties {
                collect_pattern_names(&p.value, out);
            }
            if let Some(rest) = &o.rest {
                collect_pattern_names(&rest.argument, out);
            }
        }
        BindingPattern::ArrayPattern(a) => {
            for el in a.elements.iter().flatten() {
                collect_pattern_names(el, out);
            }
            if let Some(rest) = &a.rest {
                collect_pattern_names(&rest.argument, out);
            }
        }
        BindingPattern::AssignmentPattern(a) => collect_pattern_names(&a.left, out),
        _ => {}
    }
}
