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

pub fn compile_script(src: &str) -> Result<FuncProto, String> {
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
}

struct Scope {
    bindings: Vec<Binding>,
    is_function_scope: bool,
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
            uses_arguments: false,
            this_cell: None,
            new_target_cell: None,
            arguments_cell: None,
            script_global: false,
            loops: Vec::new(),
            track_completion: false,
            with_depth: 0,
            handler_depth: 0,
            finally_depth: 0,
            strict: false,
            stable_cells: Vec::new(),
            home_super: false,
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

struct Compiler {
    fns: Vec<FnCtx>,
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
}

impl Compiler {
    fn new() -> Compiler {
        Compiler {
            fns: Vec::new(),
            pending_label: None,
            pending_home_super: false,
            chain_jumps: Vec::new(),
            module_imports: Vec::new(),
            module_exports: Vec::new(),
            module_requested: Vec::new(),
            is_module: false,
            source: String::new(),
        }
    }

    /// Whether the source region `[start, end)` mentions `arguments` (the word).
    /// Conservative: an unreadable span returns `true` (materialize, to be safe).
    fn region_has_arguments(&self, start: u32, end: u32) -> bool {
        self.source
            .get(start as usize..end as usize)
            .map_or(true, |s| s.contains("arguments"))
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
            Op::CompletionJump { target: t, .. } => *t = target,
            _ => panic!("patch_jump on non-jump op"),
        }
    }

    fn konst(&mut self, k: Const) -> u32 {
        let c = self.cur();
        c.consts.push(k);
        (c.consts.len() - 1) as u32
    }

    fn str_const(&mut self, s: &str) -> u32 {
        // Dedup string constants.
        let c = self.cur();
        for (i, k) in c.consts.iter().enumerate() {
            if let Const::String(existing) = k {
                if existing.as_ref() == s {
                    return i as u32;
                }
            }
        }
        c.consts.push(Const::String(Rc::from(s)));
        (c.consts.len() - 1) as u32
    }

    fn load_str(&mut self, s: &str) {
        let i = self.str_const(s);
        self.emit(Op::LoadConst(i));
    }

    // ---- scopes & bindings ----

    fn enter_scope(&mut self, is_function: bool) {
        self.cur().scopes.push(Scope {
            bindings: Vec::new(),
            is_function_scope: is_function,
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
        match pat {
            BindingPattern::BindingIdentifier(id) if self.in_global_scope() => {
                let n = self.str_const(id.name.as_str());
                self.emit(Op::DeclareGlobal(n));
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

    /// True when the current identifier reference is textually inside a `with`
    /// block (in this function) and could shadow against the with-object. We
    /// skip synthetic compiler-internal names (`%this`, `%completion`, ...)
    /// which can never be shadowed by a real object property.
    fn in_with(&self, name: &str) -> bool {
        self.fns.last().unwrap().with_depth > 0 && !name.starts_with('%')
    }

    fn store_binding(&mut self, name: &str) {
        // Assignment to a `const` binding is a runtime TypeError. Inside a
        // `with` the const cell is still the fallback target, so the dynamic op
        // carries the const-assign throw as its fallback.
        let fallback = if self.binding_is_const(name) {
            Op::ThrowConstAssign
        } else {
            match self.resolve(name) {
                Resolved::Cell(i) => Op::StoreCell(i),
                Resolved::Upvalue(i) => Op::StoreUpvalue(i),
                Resolved::Global => {
                    let n = self.str_const(name);
                    Op::StoreGlobal(n)
                }
            }
        };
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
        let fallback = if self.binding_is_const(name) {
            Op::ThrowConstAssign
        } else {
            match self.resolve(name) {
                Resolved::Cell(i) => Op::StoreCellChecked(i),
                Resolved::Upvalue(i) => Op::StoreUpvalueChecked(i),
                Resolved::Global => {
                    let n = self.str_const(name);
                    Op::StoreGlobal(n)
                }
            }
        };
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
        let fallback = match self.resolve(name) {
            Resolved::Cell(i) => Op::LoadCell(i),
            Resolved::Upvalue(i) => Op::LoadUpvalue(i),
            Resolved::Global => {
                let n = self.str_const(name);
                Op::LoadGlobal(n)
            }
        };
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

    // ---- top level ----

    fn compile_toplevel(&mut self, program: &Program) -> Result<FuncProto, String> {
        let mut fc = FnCtx::new("<script>", FuncKind::Normal);
        fc.track_completion = true;
        fc.script_global = true;
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
        self.fns.push(fc);
        self.enter_scope(true);
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
                // An anonymous `export default function(){}` gets the name "default".
                self.compile_function(
                    f,
                    Some(f.id.as_ref().map_or("default", |i| i.name.as_str())),
                )?;
                // A named `export default function f(){}` also binds `f` locally.
                if let Some(id) = &f.id {
                    let c = self.declare(id.name.as_str(), false);
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
                // `export default <AssignmentExpression>`.
                let expr: &Expression = other.as_expression().unwrap();
                self.compile_expr(expr)?;
                self.emit(Op::InitCell(star));
            }
        }
        self.module_exports.push(ExportEntry {
            export_name: Some("default".to_string()),
            kind: ExportKind::Local {
                local_name: "*default*".to_string(),
            },
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
        FuncProto {
            name: fc.name,
            code: fc.code,
            consts: fc.consts,
            num_locals: 0,
            num_cells: fc.num_cells,
            num_params: fc.num_params,
            has_rest: fc.has_rest,
            upvalues: fc.upvalues,
            kind: fc.kind,
            source_start: 0,
            uses_arguments: fc.uses_arguments,
            param_names: fc.param_names,
            is_strict: fc.strict,
            stable_cells: fc.stable_cells.clone(),
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
    fn hoist_funcs(&mut self, stmts: &[Statement]) -> R {
        // Top-level function declarations become global-object properties. Their
        // bodies reference each other via `LoadGlobal` (resolved at call time), so
        // a single definition pass suffices.
        if self.in_global_scope() {
            for s in stmts {
                if let Statement::FunctionDeclaration(f) = s {
                    if let Some(id) = &f.id {
                        self.compile_function(f, Some(id.name.as_str()))?;
                        let n = self.str_const(id.name.as_str());
                        // A function declaration *creates* the global binding
                        // (CreateGlobalFunctionBinding) — it is not an assignment
                        // to an existing one, so establish the property first.
                        // Otherwise the strict-mode unresolvable-reference check on
                        // `StoreGlobal` would reject the install itself.
                        self.emit(Op::DeclareGlobal(n));
                        self.emit(Op::StoreGlobal(n));
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
            Statement::IfStatement(i) => self.compile_if(i)?,
            Statement::WhileStatement(w) => self.compile_while(w)?,
            Statement::DoWhileStatement(w) => self.compile_do_while(w)?,
            Statement::ForStatement(f) => self.compile_for(f)?,
            Statement::ForInStatement(f) => self.compile_for_in(f)?,
            Statement::ForOfStatement(f) => self.compile_for_of(f)?,
            Statement::ReturnStatement(r) => {
                if let Some(arg) = &r.argument {
                    self.compile_expr(arg)?;
                } else {
                    self.emit(Op::LoadUndefined);
                }
                // Leave every active `with` environment before returning.
                self.emit_pop_with_to(0);
                self.emit(Op::Return);
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
            Statement::TryStatement(t) => self.compile_try(t)?,
            Statement::SwitchStatement(s) => self.compile_switch(s)?,
            Statement::WithStatement(w) => self.compile_with(w)?,
            Statement::LabeledStatement(l) => self.compile_labeled(l)?,
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
        self.hoist_lexical(body);
        self.hoist_funcs(body)?;
        for s in body {
            self.compile_stmt(s)?;
        }
        self.exit_scope();
        Ok(())
    }

    fn compile_var_decl(&mut self, d: &VariableDeclaration) -> R {
        let function_scoped = matches!(d.kind, VariableDeclarationKind::Var);
        let is_const = matches!(d.kind, VariableDeclarationKind::Const);
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
        let jhave = self.emit(Op::Jump(0));
        // result.done: latch done_cell, drop result, push undefined.
        let donelbl = self.here();
        self.patch_jump(jdone, donelbl);
        self.emit(Op::Pop);
        self.emit(Op::LoadTrue);
        self.emit(Op::StoreCell(done_cell));
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
                for p in &o.properties {
                    self.emit(Op::Dup);
                    if p.computed {
                        self.compile_property_key_expr(&p.key)?;
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
                    // rest object: shallow copy excluding taken keys (approx: copy all).
                    self.emit(Op::Dup);
                    self.compile_object_rest(&taken)?;
                    self.bind_pattern_kind(&rest.argument, function_scoped, is_const)?;
                }
                self.emit(Op::Pop); // drop source
            }
        }
        Ok(())
    }

    fn compile_object_rest(&mut self, taken: &[String]) -> R {
        // Object rest `{ ...rest }`: copy the source's own enumerable properties,
        // then remove the keys already bound by preceding pattern properties
        // (CopyDataProperties with `excludedNames`). stack: src -> restObj
        self.emit(Op::NewObject);
        self.emit(Op::Swap);
        self.emit(Op::ObjectSpread); // [restObj]  (all own-enumerable keys)
        for k in taken {
            self.emit(Op::Dup);
            let kc = self.str_const(k);
            self.emit(Op::DeleteProp(kc)); // [restObj, bool]
            self.emit(Op::Pop); // [restObj]
        }
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

        // Install a finally-style handler over the loop so an *abrupt* completion
        // (break / return / throw / continue to an outer loop) runs IteratorClose
        // on the iterator. Normal exhaustion (`done: true`) and a `continue` to
        // this loop must NOT close it, so the close handler is popped on the done
        // path, and `continue`'s unwind boundary stays inside it (see below).
        let outer_hd = self.cur().handler_depth;
        let outer_fd = self.cur().finally_depth;
        let close_push = self.emit(Op::PushTryHandler {
            catch: u32::MAX,
            finally: u32::MAX,
        });
        self.cur().handler_depth += 1;
        self.cur().finally_depth += 1;
        self.push_loop(None, true);
        // `break` unwinds to *outside* the close handler (so it closes); `continue`
        // stays inside (so it re-iterates without closing).
        if let Some(lp) = self.cur().loops.last_mut() {
            lp.brk_handler_depth = outer_hd;
            lp.brk_finally_depth = outer_fd;
        }

        let top = self.here();
        self.emit(Op::LoadCell(iter_cell));
        self.emit(Op::IteratorNext); // [iter, result] -> result is pushed; iter stays
                                     // IteratorNext leaves [iter, result]; drop the iter copy underneath.
        self.emit(Op::Swap);
        self.emit(Op::Pop); // [result]
        if f.r#await {
            // for-await: the iterator's next() returns a promise of the result;
            // await it before reading done/value (await of a non-promise is a
            // no-op, so this also works for sync iterables of plain values).
            self.emit(Op::Await);
        }
        self.emit(Op::Dup);
        let done_k = self.str_const("done");
        self.emit(Op::GetProp(done_k)); // [result, done]
        let jt = self.emit(Op::JumpIfTrue(0)); // consumes done; [result]
        let value_k = self.str_const("value");
        self.emit(Op::GetProp(value_k)); // [value]
        self.enter_scope(false);
        self.bind_for_target(&f.left)?; // consumes value
        self.compile_stmt(&f.body)?;
        self.exit_scope();
        self.emit(Op::Jump(top));

        // Normal-exhaustion (done) path: remove the close handler and skip the
        // close landing entirely (the iterator closed itself by returning done).
        let done_label = self.here();
        self.patch_jump(jt, done_label);
        self.emit(Op::PopTryHandler);
        self.cur().handler_depth -= 1;
        self.cur().finally_depth -= 1;
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
                let decl = &d.declarations[0];
                self.bind_pattern(&decl.id, function_scoped)?;
            }
            _ => {
                // assignment target (existing binding / member)
                let target = left.as_assignment_target().unwrap();
                self.assign_target(target)?;
            }
        }
        Ok(())
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
        self.compile_block(&finalizer.body)?;
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
            Expression::StringLiteral(s) => self.load_str(s.value.as_str()),
            Expression::TemplateLiteral(t) => self.compile_template(t)?,
            Expression::Identifier(id) => self.load_binding(id.name.as_str()),
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
                // `#x in obj` — private field check; approximate via key presence.
                self.load_str(&format!("#{}", p.left.name.as_str()));
                self.compile_expr(&p.right)?;
                self.emit(Op::HasProp);
            }
            Expression::StaticMemberExpression(m) => {
                if matches!(m.object, Expression::Super(_)) {
                    if self.cur().home_super {
                        // Object-method super: getPrototypeOf([[HomeObject]])[name].
                        let k = self.str_const(m.property.name.as_str());
                        self.emit(Op::GetSuperProp(k));
                    } else {
                        self.load_binding("%superclass");
                        let proto = self.str_const("prototype");
                        self.emit(Op::GetProp(proto));
                        let k = self.str_const(m.property.name.as_str());
                        self.emit(Op::GetProp(k));
                    }
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
                    if self.cur().home_super {
                        self.compile_expr(&m.expression)?;
                        self.emit(Op::GetSuperPropDynamic);
                    } else {
                        // `super[expr]`: read from the superclass prototype
                        // (mirrors the `super.prop` static-member case above).
                        self.load_binding("%superclass");
                        let proto = self.str_const("prototype");
                        self.emit(Op::GetProp(proto));
                        self.compile_expr(&m.expression)?;
                        self.emit(Op::GetPropDynamic);
                    }
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
                // Private names modeled as non-enumerable string keys "#name", with
                // a brand check (`PrivateGet`) so foreign access throws a TypeError.
                self.compile_expr(&m.object)?;
                let k = self.str_const(&format!("#{}", m.field.name.as_str()));
                self.emit(Op::PrivateGet(k));
            }
            Expression::FunctionExpression(f) => {
                let name = f.id.as_ref().map(|i| i.name.as_str().to_string());
                self.compile_function(f, name.as_deref())?;
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
        for prop in &o.properties {
            match prop {
                ObjectPropertyKind::ObjectProperty(p) => {
                    // [obj]
                    let is_accessor = matches!(p.kind, PropertyKind::Get | PropertyKind::Set);
                    if p.computed {
                        self.compile_property_key_expr(&p.key)?; // [obj, key]
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
                    }
                    self.pending_home_super = false;
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
            self.load_str(cooked);
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
        // tag(strings, ...exprs) where strings has a `raw` property.
        // Build the strings array.
        self.compile_expr(&t.tag)?; // [tag]
        self.emit(Op::LoadUndefined); // this
                                      // strings array
        for q in &t.quasi.quasis {
            let cooked = q.value.cooked.as_ref().map(|s| s.as_str()).unwrap_or("");
            self.load_str(cooked);
        }
        self.emit(Op::NewArray(t.quasi.quasis.len() as u32)); // [tag, this, strings]
                                                              // raw property = same strings (approx)
        self.emit(Op::Dup);
        self.emit(Op::Dup);
        let raw = self.str_const("raw");
        self.emit(Op::SetProp(raw));
        self.emit(Op::Pop);
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
                    Expression::Identifier(id) if self.in_with(id.name.as_str()) => {
                        // `delete name` inside a `with` deletes from the
                        // with-object when the name resolves there; otherwise a
                        // bare-name delete is a no-op reporting success.
                        let n = self.str_const(id.name.as_str());
                        self.emit(Op::DeleteName(n));
                    }
                    _ => {
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
            ChainElement::PrivateFieldExpression(_) => {
                Err("private field access is not supported".into())
            }
        }
    }

    fn compile_call(&mut self, c: &CallExpression) -> R {
        // super(...) — call the parent constructor with the current `this`.
        if matches!(c.callee, Expression::Super(_)) {
            self.load_binding("%superclass"); // [super]
            self.load_binding("%this"); // [super, this]
            self.finish_call(c)?;
            return Ok(());
        }
        // super.method(...) — look up on Super.prototype, call with `this`.
        if let Expression::StaticMemberExpression(m) = &c.callee {
            if matches!(m.object, Expression::Super(_)) {
                if self.cur().home_super {
                    let k = self.str_const(m.property.name.as_str());
                    self.emit(Op::GetSuperProp(k)); // [method]
                } else {
                    self.load_binding("%superclass");
                    let proto = self.str_const("prototype");
                    self.emit(Op::GetProp(proto));
                    let k = self.str_const(m.property.name.as_str());
                    self.emit(Op::GetProp(k)); // [method]
                }
                self.load_binding("%this"); // [method, this]
                self.finish_call(c)?;
                return Ok(());
            }
        }
        // super[expr](...) — same, with a computed key.
        if let Expression::ComputedMemberExpression(m) = &c.callee {
            if matches!(m.object, Expression::Super(_)) {
                if self.cur().home_super {
                    self.compile_expr(&m.expression)?;
                    self.emit(Op::GetSuperPropDynamic); // [method]
                } else {
                    self.load_binding("%superclass");
                    let proto = self.str_const("prototype");
                    self.emit(Op::GetProp(proto));
                    self.compile_expr(&m.expression)?;
                    self.emit(Op::GetPropDynamic); // [method]
                }
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
                let k = self.str_const(&format!("#{}", m.field.name.as_str()));
                // Brand-checking read: calling a private method on an object that
                // doesn't have it must throw a TypeError (not silently read undefined).
                self.emit(Op::PrivateGet(k)); // [obj, method]
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
            // yield* expr  — desugar to a loop yielding each value. In an async
            // generator the delegate uses the *async* iterator protocol: the
            // iterator comes from @@asyncIterator (GetAsyncIterator) and each
            // `next()` result is Awaited before its `done`/`value` are read.
            let is_async = self.cur().kind.is_async();
            self.compile_expr(y.argument.as_ref().unwrap())?;
            if is_async {
                self.emit(Op::GetAsyncIterator);
            } else {
                self.emit(Op::GetIterator);
            }
            let iter_cell = self.temp();
            self.emit(Op::InitCell(iter_cell));
            let top = self.here();
            self.emit(Op::LoadCell(iter_cell));
            self.emit(Op::IteratorNext);
            self.emit(Op::Swap);
            self.emit(Op::Pop); // [result]
            if is_async {
                self.emit(Op::Await); // result is a promise of { value, done }
            }
            self.emit(Op::Dup);
            let done_k = self.str_const("done");
            self.emit(Op::GetProp(done_k));
            let jt = self.emit(Op::JumpIfTrue(0)); // [result]
            let value_k = self.str_const("value");
            self.emit(Op::GetProp(value_k));
            self.emit(Op::Yield);
            self.emit(Op::Pop); // discard sent value
            self.emit(Op::Jump(top));
            let end = self.here();
            self.patch_jump(jt, end);
            let value_k2 = self.str_const("value");
            self.emit(Op::GetProp(value_k2)); // result of yield* = final value
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
                        self.compile_expr(&a.right)?;
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
                self.member_assign(&m.object, k, a)?;
            }
            AssignmentTarget::PrivateFieldExpression(m) => {
                let k = self.str_const(&format!("#{}", m.field.name.as_str()));
                self.member_assign_kind(&m.object, k, a, true)?;
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

    /// Assign to `obj.<k>` (static-name key const `k`) with the given operator.
    fn member_assign(&mut self, obj: &Expression, k: u32, a: &AssignmentExpression) -> R {
        self.member_assign_kind(obj, k, a, false)
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
                self.compile_expr(&m.object)?;
                self.emit(Op::LoadCell(t));
                self.emit(Op::SetProp(k));
                self.emit(Op::Pop);
            }
            // Private field as a destructuring target: `[obj.#x] = […]`.
            AssignmentTarget::PrivateFieldExpression(m) => {
                let k = self.str_const(&format!("#{}", m.field.name.as_str()));
                let t = self.temp();
                self.emit(Op::InitCell(t));
                self.compile_expr(&m.object)?;
                self.emit(Op::LoadCell(t));
                self.emit(Op::SetProp(k));
                self.emit(Op::Pop);
            }
            AssignmentTarget::ComputedMemberExpression(m) => {
                let t = self.temp();
                self.emit(Op::InitCell(t));
                self.compile_expr(&m.object)?;
                self.compile_expr(&m.expression)?;
                self.emit(Op::LoadCell(t));
                self.emit(Op::SetPropDynamic);
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
                    self.emit_iter_step_tracked(itc, done_cell); // [value]
                    match el {
                        Some(maybe) => self.assign_maybe_default(maybe)?, // consumes value
                        None => {
                            self.emit(Op::Pop); // elision
                        }
                    }
                }
                if let Some(rest) = &arr.rest {
                    self.emit(Op::NewArray(0)); // [arr]
                    let top = self.here();
                    self.emit(Op::LoadCell(done_cell));
                    let jdone_rest = self.emit(Op::JumpIfTrue(0));
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
                    let end = self.here();
                    self.patch_jump(jend, end);
                    self.emit(Op::Pop); // drop result -> [arr]
                    self.emit(Op::LoadTrue);
                    self.emit(Op::StoreCell(done_cell));
                    let jafter = self.emit(Op::Jump(0));
                    let drest = self.here();
                    self.patch_jump(jdone_rest, drest);
                    let after_rest = self.here();
                    self.patch_jump(jafter, after_rest);
                    self.assign_target(&rest.target)?; // consumes arr
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
                    self.compile_object_rest(&taken)?; // [src, restObj]
                    self.assign_target(&rest.target)?; // [src]
                }
                self.emit(Op::Pop);
            }
            _ => return Err("unsupported destructuring target".into()),
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
        let kind = if f.r#async && f.generator {
            FuncKind::AsyncGenerator
        } else if f.generator {
            FuncKind::Generator
        } else if f.r#async {
            FuncKind::Async
        } else {
            FuncKind::Normal
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
        let class_strict = matches!(kind, FuncKind::ClassCtor);
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

        if !arrow {
            let tc = self.declare("%this", false);
            self.emit(Op::LoadThis);
            // Sloppy functions substitute the global object / box a primitive
            // `this` (OrdinaryCallBindThis); strict functions keep it as-is.
            if !self.cur().strict {
                self.emit(Op::BindThisSloppy);
            }
            self.emit(Op::InitCell(tc));
            let nt = self.declare("%newtarget", false);
            self.emit(Op::LoadNewTarget);
            self.emit(Op::InitCell(nt));
            // The `arguments` object is materialized (an allocation per call) only
            // when the body actually mentions `arguments` — the common case skips
            // it entirely. Scanning the source region for the word never produces
            // a false negative (if `arguments` is used, the word is present).
            let end = body.map(|b| b.span.end).unwrap_or(params.span.end);
            if self.region_has_arguments(params.span.start, end) {
                let ac = self.declare("arguments", false);
                self.emit(Op::LoadArguments);
                self.emit(Op::InitCell(ac));
            }
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

        // Class instance-field initializers run at the top of the constructor.
        if let Some(fields) = ctor_fields {
            for field in fields {
                self.load_binding("%this");
                if field.computed {
                    self.compile_property_key_expr(&field.key)?;
                    if let Some(init) = &field.value {
                        self.compile_expr(init)?;
                    } else {
                        self.emit(Op::LoadUndefined);
                    }
                    self.emit(Op::SetPropDynamic);
                } else {
                    let key = property_key_name(&field.key);
                    if let Some(init) = &field.value {
                        self.compile_named_expr(init, &key)?;
                    } else {
                        self.emit(Op::LoadUndefined);
                    }
                    let k = self.str_const(&key);
                    self.emit(Op::SetProp(k));
                }
                self.emit(Op::Pop);
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
            self.hoist_lexical(&b.statements);
            self.hoist_vars_all(&b.statements);
            self.hoist_funcs(&b.statements)?;
            for s in &b.statements {
                self.compile_stmt(s)?;
            }
            self.emit(Op::LoadUndefined);
            self.emit(Op::Return);
        } else {
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

    fn compile_class(&mut self, class: &Class, name: Option<&str>) -> R {
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

        // Build the constructor closure, then stash it in a temp cell so the rest
        // of class building can address it cleanly.
        if let Some(m) = ctor_method {
            self.emit_function_core(
                &m.value.params,
                m.value.body.as_deref(),
                false,
                None,
                FuncKind::ClassCtor,
                name,
                Some(&instance_fields),
            )?;
        } else {
            self.synthesize_constructor(name, has_super, &instance_fields)?;
        }
        let ctor_cell = self.temp();
        self.emit(Op::InitCell(ctor_cell));

        if has_super {
            self.class_link_super(ctor_cell)?;
        }

        for el in &class.body.body {
            match el {
                ClassElement::MethodDefinition(m)
                    if !matches!(m.kind, MethodDefinitionKind::Constructor) =>
                {
                    self.class_define_method(ctor_cell, m)?;
                }
                ClassElement::PropertyDefinition(p) if p.r#static => {
                    self.class_define_static_field(ctor_cell, p)?;
                }
                _ => {}
            }
        }

        // Leave the class (constructor) value on the stack.
        self.emit(Op::LoadCell(ctor_cell));
        Ok(())
    }

    fn synthesize_constructor(
        &mut self,
        name: Option<&str>,
        has_super: bool,
        fields: &[&PropertyDefinition],
    ) -> R {
        let mut fc = FnCtx::new(name.unwrap_or(""), FuncKind::ClassCtor);
        fc.has_rest = has_super;
        fc.num_params = 0;
        self.fns.push(fc);
        self.enter_scope(true);
        let tc = self.declare("%this", false);
        self.emit(Op::LoadThis);
        self.emit(Op::InitCell(tc));
        let nt = self.declare("%newtarget", false);
        self.emit(Op::LoadNewTarget);
        self.emit(Op::InitCell(nt));
        let ac = self.declare("arguments", false);
        self.emit(Op::LoadArguments);
        self.emit(Op::InitCell(ac));
        if has_super {
            // super(...arguments)
            self.load_binding("%superclass");
            self.load_binding("%this");
            self.load_binding("arguments");
            self.emit(Op::CallSpread);
            self.emit(Op::Pop);
        }
        for field in fields {
            self.load_binding("%this");
            if field.computed {
                // Computed field key (`[expr] = …`): evaluate + ToPropertyKey so a
                // number becomes its string form and a symbol stays a symbol.
                self.compile_property_key_expr(&field.key)?;
                if let Some(init) = &field.value {
                    self.compile_expr(init)?;
                } else {
                    self.emit(Op::LoadUndefined);
                }
                self.emit(Op::SetPropDynamic);
            } else {
                let key = property_key_name(&field.key);
                if let Some(init) = &field.value {
                    self.compile_named_expr(init, &key)?;
                } else {
                    self.emit(Op::LoadUndefined);
                }
                let k = self.str_const(&key);
                self.emit(Op::SetProp(k));
            }
            self.emit(Op::Pop);
        }
        self.emit(Op::LoadUndefined);
        self.emit(Op::Return);
        self.exit_scope();
        let fc = self.fns.pop().unwrap();
        let proto = self.finish(fc);
        let idx = self.konst(Const::Func(Rc::new(proto)));
        self.emit(Op::Closure(idx));
        Ok(())
    }

    /// Link `ctor.prototype.__proto__ = Super.prototype` and
    /// `ctor.__proto__ = Super` via `Object.setPrototypeOf`.
    fn class_link_super(&mut self, ctor_cell: u32) -> R {
        let object = self.str_const("Object");
        let set_proto = self.str_const("setPrototypeOf");
        let prototype = self.str_const("prototype");

        // Object.setPrototypeOf(ctor.prototype, Super.prototype)
        self.emit(Op::LoadGlobal(object));
        self.emit(Op::GetProp(set_proto)); // [setFn]
        self.emit(Op::LoadUndefined); // this
        self.emit(Op::LoadCell(ctor_cell));
        self.emit(Op::GetProp(prototype)); // ctor.prototype
        self.load_binding("%superclass");
        self.emit(Op::GetProp(prototype)); // super.prototype
        self.emit(Op::Call(2));
        self.emit(Op::Pop);

        // Object.setPrototypeOf(ctor, Super)
        self.emit(Op::LoadGlobal(object));
        self.emit(Op::GetProp(set_proto));
        self.emit(Op::LoadUndefined);
        self.emit(Op::LoadCell(ctor_cell));
        self.load_binding("%superclass");
        self.emit(Op::Call(2));
        self.emit(Op::Pop);
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
        self.compile_function(&m.value, Some(&fname))?;
        self.pending_home_super = false;
        if m.r#static {
            // [ctor, key, value] — stamp value.[[HomeObject]] = ctor.
            self.emit(Op::SetHomeObject);
        }
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

    fn class_define_static_field(&mut self, ctor_cell: u32, p: &PropertyDefinition) -> R {
        self.emit(Op::LoadCell(ctor_cell)); // [ctor]
        if p.computed {
            self.compile_property_key_expr(&p.key)?; // [ctor, key]
            if let Some(init) = &p.value {
                self.compile_expr(init)?;
            } else {
                self.emit(Op::LoadUndefined);
            }
        } else {
            let key = property_key_name(&p.key);
            self.load_str(&key); // [ctor, key]
            if let Some(init) = &p.value {
                self.compile_named_expr(init, &key)?;
            } else {
                self.emit(Op::LoadUndefined);
            }
        }
        self.emit(Op::DefineField); // [ctor]
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
