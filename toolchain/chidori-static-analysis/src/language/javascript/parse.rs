extern crate swc_ecma_parser;

use crate::language::javascript::parse::ContextPath::Constant;
use crate::language::{InternalCallGraph, python, TextRange};
use crate::language::{Report, ReportItem, ReportTriggerableFunctions};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use swc_common::sync::Lrc;
use swc_common::{
    errors::{ColorConfig, Handler},
    FileName, FilePathMapping, SourceMap,
};
use swc_common::source_map::Pos;
use swc_ecma_ast as ast;
use swc_ecma_ast::{
    BlockStmtOrExpr, Callee, Decl, Expr, FnDecl, ForHead, Ident, ImportSpecifier, Lit, MemberProp,
    ModuleDecl, ModuleItem, Pat, PatOrExpr, Stmt,
};
use swc_ecma_parser::{lexer::Lexer, Parser, StringInput, Syntax};

#[derive(Debug, Clone, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum ContextPath {
    Initialized,
    InFunction(String, TextRange),
    InAnonFunction,
    Params,
    Param(String),
    InFunctionDecorator(usize),
    InCallExpression,
    ChName,
    AssignmentToStatement,
    AssignmentFromStatement,
    // bool = true (is locally defined)
    IdentifierReferredTo(String, bool),
    Attribute(String),
    Constant(String),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct FunctionContext {
    dependencies: Vec<Vec<ContextPath>>,
}

fn remove_hash_and_numbers(input: &str) -> String {
    match input.find('#') {
        Some(index) => input[..index].to_string(),
        None => input.to_string(),
    }
}

/// The ASTWalkContext structure represents the accumulated state during an Abstract Syntax Tree (AST) walk.
///
/// This context provides a way to track and evaluate references within the AST, allowing for filtered subsets of the AST to be built.
///
/// # Properties
///
/// * `context_stack_references`: A vector of vectors that stores the ContextPath values. Each inner vector represents a stack frame in the evaluation process.
/// * `context_stack`: A vector that tracks the current context path.
/// * `locals`: A set of strings representing local variables defined within the AST.
/// * `local_contexts`: A vector of sets, where each set represents a separate local context.
/// * `globals`: A set of strings representing global variables defined within the AST.
#[derive(Default)]
pub struct ASTWalkContext {
    pub context_stack_references: Vec<Vec<ContextPath>>,
    pub context_stack: Vec<ContextPath>,
    pub locals: HashSet<String>,
    pub local_contexts: Vec<HashSet<String>>,
    pub globals: HashSet<String>,
}

impl ASTWalkContext {
    fn new() -> Self {
        Self {
            context_stack_references: vec![],
            context_stack: vec![],
            locals: HashSet::new(),
            local_contexts: vec![],
            globals: HashSet::new(),
        }
    }

    fn var_exists(&self, name: &str) -> bool {
        if self.globals.contains(name) {
            return true;
        }
        if self.locals.contains(name) {
            return true;
        }
        for local_context in self.local_contexts.iter().rev() {
            if local_context.contains(name) {
                return true;
            }
        }
        return false;
    }

    fn new_local_context(&mut self) {
        self.local_contexts.push(self.locals.clone());
    }

    fn insert_local(&mut self, ident: &ast::Ident) {
        let name = remove_hash_and_numbers(&ident.to_string());
        self.locals.insert(name);
    }

    fn pop_local_context(&mut self) {
        self.local_contexts.pop();

        // Clearing the existing locals
        self.locals.clear();

        // Union of all remaining hashsetss in local_contexts
        self.locals.extend(
            self.local_contexts
                .iter()
                .flat_map(|context| context.iter().cloned()),
        );
    }

    fn enter_anonymous_function(&mut self) -> usize {
        self.context_stack.push(ContextPath::InAnonFunction);
        // self.context_stack_references
        //     .push(self.context_stack.clone());
        self.context_stack.len()
    }

    fn enter_statement_function(&mut self, name: &ast::Ident, range: TextRange) -> usize {
        let name = remove_hash_and_numbers(&name.to_string());
        self.context_stack.push(ContextPath::InFunction(name, range));
        self.context_stack.len()
    }

    fn enter_params(&mut self) -> usize {
        self.context_stack.push(ContextPath::Params);
        self.context_stack.len()
    }

    fn enter_decorator_expression(&mut self, idx: &usize) -> usize {
        self.context_stack
            .push(ContextPath::InFunctionDecorator(idx.clone()));
        self.context_stack.len()
    }

    fn enter_call_expression(&mut self) -> usize {
        self.context_stack.push(ContextPath::InCallExpression);
        self.context_stack.len()
    }

    fn encounter_constant(&mut self, lit: &ast::Lit) {
        match lit {
            Lit::Str(ast::Str { value, .. }) => {
                self.context_stack
                    .push(ContextPath::Constant(value.to_string()));
                // self.context_stack_references
                //     .push(self.context_stack.clone());
            }
            Lit::Bool(_) => {}
            Lit::Null(_) => {}
            Lit::Num(_) => {}
            Lit::BigInt(_) => {}
            Lit::Regex(_) => {}
            Lit::JSXText(_) => {}
        }
    }

    fn encounter_named_reference(&mut self, name: &ast::Ident) {
        // TODO: we can't pop named references because they can be used during patterns which apply to attributes
        let name = remove_hash_and_numbers(&name.to_string());
        // TODO: we need to check if this is a local variable or not
        if self.var_exists(&name.to_string()) {
            // true, the var already exists in the local or global scope
            self.context_stack
                .push(ContextPath::IdentifierReferredTo(name.to_string(), true));
        } else {
            if let Some(ContextPath::AssignmentToStatement) = self.context_stack.last() {
                self.locals.insert(name.to_string());
            }
            if self.context_stack.contains(&ContextPath::Params) {
                self.locals.insert(name.to_string());
                self.context_stack
                    .push(ContextPath::IdentifierReferredTo(name.to_string(), false));
                return;
            }
            self.context_stack
                .push(ContextPath::IdentifierReferredTo(name.to_string(), false));
        }
    }

    fn enter_assignment_to_statement(&mut self) -> usize {
        self.context_stack.push(ContextPath::AssignmentToStatement);
        self.context_stack.len()
    }

    fn enter_assignment_from_statement(&mut self) -> usize {
        self.context_stack
            .push(ContextPath::AssignmentFromStatement);
        self.context_stack.len()
    }

    fn enter_attr(&mut self, name: &ast::Ident) -> usize {
        let name = remove_hash_and_numbers(&name.to_string());
        self.context_stack
            .push(ContextPath::Attribute(name.to_string()));
        self.context_stack.len()
    }

    fn pop_until(&mut self, size: usize) {
        self.context_stack_references
            .push(self.context_stack.clone());
        while self.context_stack.len() >= size {
            self.context_stack.pop();
        }
    }
}

fn traverse_module(module: ModuleItem, machine: &mut ASTWalkContext) {
    match module {
        ModuleItem::ModuleDecl(mod_decl) => match mod_decl {
            ModuleDecl::Import(ast::ImportDecl { specifiers, .. }) => {
                for specifier in specifiers {
                    match specifier {
                        ImportSpecifier::Named(ast::ImportNamedSpecifier { local, .. }) => {
                            let name = remove_hash_and_numbers(&local.to_string());
                            machine.globals.insert(name);
                        }
                        ImportSpecifier::Default(ast::ImportDefaultSpecifier { local, .. }) => {
                            let name = remove_hash_and_numbers(&local.to_string());
                            machine.globals.insert(name);
                        }
                        ImportSpecifier::Namespace(ast::ImportStarAsSpecifier {
                            local, ..
                        }) => {
                            let name = remove_hash_and_numbers(&local.to_string());
                            machine.globals.insert(name);
                        }
                    }
                }
            }
            ModuleDecl::ExportDecl(ast::ExportDecl { .. }) => {}
            ModuleDecl::ExportNamed(ast::NamedExport { .. }) => {}
            ModuleDecl::ExportDefaultDecl(ast::ExportDefaultDecl { .. }) => {}
            ModuleDecl::ExportDefaultExpr(ast::ExportDefaultExpr { .. }) => {}
            ModuleDecl::ExportAll(ast::ExportAll { .. }) => {}
            ModuleDecl::TsImportEquals(_) => {}
            ModuleDecl::TsExportAssignment(_) => {}
            ModuleDecl::TsNamespaceExport(_) => {}
        },
        ModuleItem::Stmt(stmt) => {
            let v = vec![stmt];
            traverse_stmts(v.as_slice(), machine);
        }
    }
}

fn traverse_pat(pat: &ast::Pat, machine: &mut ASTWalkContext) {
    match pat {
        Pat::Ident(ast::BindingIdent { id, .. }) => {
            machine.encounter_named_reference(id);
        }
        Pat::Array(ast::ArrayPat { elems, .. }) => {
            for elem in elems {
                if let Some(elem) = elem {
                    traverse_pat(elem, machine);
                }
            }
        }
        Pat::Rest(ast::RestPat { arg, .. }) => {
            traverse_pat(arg, machine);
        }
        Pat::Object(ast::ObjectPat { props, .. }) => {
            for prop in props {
                match prop {
                    ast::ObjectPatProp::KeyValue(ast::KeyValuePatProp { key, value, .. }) => {
                        traverse_pat(value, machine);
                    }
                    ast::ObjectPatProp::Assign(ast::AssignPatProp { key, value, .. }) => {
                        if let Some(value) = value {
                            traverse_expr(value, machine);
                        }
                    }
                    ast::ObjectPatProp::Rest(ast::RestPat { arg, .. }) => {
                        traverse_pat(arg, machine);
                    }
                }
            }
        }
        Pat::Assign(ast::AssignPat { left, right, .. }) => {
            traverse_pat(left, machine);
            traverse_expr(right, machine);
        }
        Pat::Invalid(ast::Invalid { .. }) => {}
        Pat::Expr(expr) => {
            traverse_expr(expr, machine);
        }
    }
}

fn traverse_expr(expr: &ast::Expr, machine: &mut ASTWalkContext) {
    match expr {
        Expr::This(ast::ThisExpr { .. }) => {}
        Expr::Array(ast::ArrayLit { elems, .. }) => {}
        Expr::Object(ast::ObjectLit { props, .. }) => {}
        Expr::Fn(ast::FnExpr { .. }) => {}
        Expr::Unary(ast::UnaryExpr { arg, .. }) => {
            traverse_expr(arg, machine);
        }
        Expr::Update(ast::UpdateExpr { arg, .. }) => {
            traverse_expr(arg, machine);
        }
        Expr::Bin(ast::BinExpr { left, right, .. }) => {
            traverse_expr(left, machine);
            traverse_expr(right, machine);
        }
        Expr::Assign(ast::AssignExpr {
            op, left, right, ..
        }) => {
            let idx = machine.enter_assignment_to_statement();
            match left {
                PatOrExpr::Expr(expr) => {
                    traverse_expr(expr, machine);
                }
                PatOrExpr::Pat(pat) => {
                    traverse_pat(pat, machine);
                }
            }
            machine.pop_until(idx);
            let idx = machine.enter_assignment_from_statement();
            traverse_expr(right, machine);
            machine.pop_until(idx);
        }
        Expr::Member(ast::MemberExpr { obj, prop, .. }) => {
            match prop {
                MemberProp::Ident(id) => machine.enter_attr(id),
                MemberProp::PrivateName(ast::PrivateName { id, .. }) => machine.enter_attr(id),
                MemberProp::Computed(ast::ComputedPropName { expr, .. }) => {
                    // TODO: handle computed prop name
                    unimplemented!("computed prop name");
                }
            };
            traverse_expr(&obj, machine);
            // machine.pop_until(idx);
        }
        Expr::SuperProp(ast::SuperPropExpr { .. }) => {}
        Expr::Cond(ast::CondExpr {
            test, cons, alt, ..
        }) => {
            traverse_expr(&test, machine);
            traverse_expr(&cons, machine);
            traverse_expr(&alt, machine);
        }
        Expr::Call(ast::CallExpr { args, callee, .. }) => {
            let idx = machine.enter_call_expression();
            match callee {
                Callee::Super(_) => {}
                Callee::Import(_) => {}
                Callee::Expr(expr) => {
                    traverse_expr(expr, machine);
                }
            }

            for arg in args {
                traverse_expr(&arg.expr, machine);
            }
            machine.pop_until(idx);
        }
        Expr::New(ast::NewExpr { callee, args, .. }) => {
            let idx = machine.enter_call_expression();
            traverse_expr(&callee, machine);
            if let Some(args) = args {
                for arg in args {
                    traverse_expr(&arg.expr, machine);
                }
            }
            machine.pop_until(idx);
        }
        Expr::Seq(ast::SeqExpr { exprs, .. }) => {
            for expr in exprs {
                traverse_expr(expr, machine);
            }
        }
        Expr::Ident(id) => {
            machine.encounter_named_reference(id);
        }
        Expr::Lit(lit) => {
            machine.encounter_constant(lit);
        }
        Expr::Tpl(ast::Tpl { exprs, .. }) => {
            for expr in exprs {
                traverse_expr(&expr, machine);
            }
        }
        Expr::TaggedTpl(ast::TaggedTpl { tag, .. }) => {
            traverse_expr(&tag, machine);
        }
        Expr::Arrow(ast::ArrowExpr { params, body, .. }) => {
            let idx = machine.enter_anonymous_function();
            let params_idx = machine.enter_params();
            for param in params {
                traverse_pat(param, machine);
            }
            machine.pop_until(params_idx);
            match **body {
                BlockStmtOrExpr::Expr(ref expr) => {
                    traverse_expr(expr, machine);
                }
                BlockStmtOrExpr::BlockStmt(ref block) => {
                    traverse_stmts(&block.stmts, machine);
                }
            }
            machine.pop_until(idx);
        }
        Expr::Class(ast::ClassExpr { class, .. }) => {
            // TODO: parse class
        }
        Expr::Yield(ast::YieldExpr { arg, .. }) => {
            if let Some(arg) = arg {
                traverse_expr(arg, machine);
            }
        }
        Expr::MetaProp(ast::MetaPropExpr { .. }) => {}
        Expr::Await(ast::AwaitExpr { arg, .. }) => {
            traverse_expr(arg, machine);
        }
        Expr::Paren(ast::ParenExpr { expr, .. }) => {
            traverse_expr(&expr, machine);
        }
        Expr::JSXMember(ast::JSXMemberExpr { .. }) => {}
        Expr::JSXNamespacedName(ast::JSXNamespacedName { .. }) => {}
        Expr::JSXEmpty(ast::JSXEmptyExpr { .. }) => {}
        Expr::JSXElement(el) => {}
        Expr::JSXFragment(ast::JSXFragment { .. }) => {}
        Expr::TsTypeAssertion(ast::TsTypeAssertion { .. }) => {}
        Expr::TsConstAssertion(ast::TsConstAssertion { .. }) => {}
        Expr::TsNonNull(ast::TsNonNullExpr { .. }) => {}
        Expr::TsAs(ast::TsAsExpr { .. }) => {}
        Expr::TsInstantiation(ast::TsInstantiation { .. }) => {}
        Expr::TsSatisfies(ast::TsSatisfiesExpr { .. }) => {}
        Expr::PrivateName(ast::PrivateName { .. }) => {}
        Expr::OptChain(ast::OptChainExpr { .. }) => {}
        Expr::Invalid(ast::Invalid { .. }) => {}
    }
}
fn traverse_stmt(stmt: &Stmt, machine: &mut ASTWalkContext) {
    match stmt {
        Stmt::Expr(expr_stmt) => {
            traverse_expr(&*expr_stmt.expr, machine);
        }
        Stmt::Block(block_stmt) => {
            traverse_stmts(&block_stmt.stmts, machine);
        }
        Stmt::Empty(_) => {}
        Stmt::Debugger(ast::DebuggerStmt { .. }) => {}
        Stmt::With(ast::WithStmt { obj, body, .. }) => {
            traverse_expr(&obj, machine);
            traverse_stmt(&body, machine);
        }
        Stmt::Return(ast::ReturnStmt { arg, .. }) => {
            if let Some(arg) = arg {
                traverse_expr(&arg, machine);
            }
        }
        Stmt::Labeled(ast::LabeledStmt { body, .. }) => {
            traverse_stmt(&body, machine);
        }
        Stmt::Break(ast::BreakStmt { .. }) => {}
        Stmt::Continue(ast::ContinueStmt { .. }) => {}
        Stmt::If(ast::IfStmt {
            test, cons, alt, ..
        }) => {
            traverse_expr(&test, machine);
            traverse_stmt(&cons, machine);
            if let Some(alt) = alt {
                traverse_stmt(&alt, machine);
            }
        }
        Stmt::Switch(ast::SwitchStmt {
            discriminant,
            cases,
            ..
        }) => {
            traverse_expr(&discriminant, machine);
            for case in cases {
                if let Some(test) = &case.test {
                    traverse_expr(&test, machine);
                }
                traverse_stmts(&case.cons, machine);
            }
        }
        Stmt::Throw(ast::ThrowStmt { arg, .. }) => {
            traverse_expr(&arg, machine);
        }
        Stmt::Try(x) => {
            let ast::TryStmt {
                block,
                handler,
                finalizer,
                ..
            } = &**x;
            traverse_stmts(&block.stmts, machine);
        }
        Stmt::While(ast::WhileStmt { test, body, .. }) => {
            traverse_expr(&test, machine);
            traverse_stmt(&body, machine);
        }
        Stmt::DoWhile(ast::DoWhileStmt { test, body, .. }) => {
            traverse_expr(&test, machine);
            traverse_stmt(&body, machine);
        }
        Stmt::For(ast::ForStmt {
            init,
            test,
            body,
            update,
            ..
        }) => {
            if let Some(init) = init {
                match init {
                    ast::VarDeclOrExpr::VarDecl(_) => {}
                    ast::VarDeclOrExpr::Expr(expr) => {
                        traverse_expr(&expr, machine);
                    }
                }
            }
            if let Some(test) = test {
                traverse_expr(&test, machine);
            }
            if let Some(update) = update {
                traverse_expr(&update, machine);
            }
            traverse_stmt(&body, machine);
        }
        Stmt::ForIn(ast::ForInStmt {
            left, right, body, ..
        }) => {
            match left {
                ForHead::VarDecl(x) => {
                    let ast::VarDecl { decls, .. } = &**x;
                    for decl in decls {
                        if let Some(init) = &decl.init {
                            traverse_expr(&init, machine);
                        }
                    }
                }
                ForHead::UsingDecl(x) => {
                    let ast::UsingDecl { decls, .. } = &**x;
                    for decl in decls {
                        if let Some(init) = &decl.init {
                            traverse_expr(&init, machine);
                        }
                    }
                }
                ForHead::Pat(x) => {
                    traverse_pat(&x, machine);
                }
            }
            traverse_expr(&right, machine);
            traverse_stmt(&body, machine);
        }
        Stmt::ForOf(ast::ForOfStmt {
            left, right, body, ..
        }) => {
            // TODO: For loop declarations need to be added to locals
            match left {
                ForHead::VarDecl(x) => {
                    let ast::VarDecl { decls, .. } = &**x;
                    for decl in decls {
                        if let Some(init) = &decl.init {
                            traverse_expr(&init, machine);
                        }
                    }
                }
                ForHead::UsingDecl(x) => {
                    let ast::UsingDecl { decls, .. } = &**x;
                    for decl in decls {
                        if let Some(init) = &decl.init {
                            traverse_expr(&init, machine);
                        }
                    }
                }
                ForHead::Pat(x) => {
                    traverse_pat(&x, machine);
                }
            }
            traverse_expr(&right, machine);
            traverse_stmt(&body, machine);
        }
        Stmt::Decl(decl) => match decl {
            Decl::Class(_) => {}
            Decl::Fn(ast::FnDecl {
                ident, function, ..
            }) => {
                machine.insert_local(ident);
                let ast::Function { params, body, span, .. } = &**function;
                let idx = machine.enter_statement_function(ident, TextRange {
                    start: span.lo.to_usize(),
                    end: span.hi.to_usize(),
                });
                let params_idx = machine.enter_params();
                for param in params {
                    traverse_pat(&param.pat, machine);
                }
                machine.pop_until(params_idx);
                if let Some(body) = body {
                    traverse_stmts(&body.stmts, machine);
                }
                machine.pop_until(idx);
            }
            Decl::Var(v) => {
                let ast::VarDecl { decls, .. } = &**v;
                for decl in decls {
                    let idx = machine.enter_assignment_to_statement();
                    traverse_pat(&decl.name, machine);
                    machine.pop_until(idx);
                    if let Some(init) = &decl.init {
                        let idx = machine.enter_assignment_from_statement();
                        traverse_expr(&init, machine);
                        machine.pop_until(idx);
                    }
                }
            }
            Decl::Using(v) => {
                let ast::UsingDecl { decls, .. } = &**v;
                for decl in decls {
                    traverse_pat(&decl.name, machine);
                    if let Some(init) = &decl.init {
                        traverse_expr(&init, machine);
                    }
                }
            }
            Decl::TsInterface(_) => {}
            Decl::TsTypeAlias(_) => {}
            Decl::TsEnum(_) => {}
            Decl::TsModule(_) => {}
        },
    }
}

fn traverse_stmts(stmts: &[Stmt], machine: &mut ASTWalkContext) {
    for stmt in stmts {
        traverse_stmt(stmt, machine);
    }
}

pub fn extract_dependencies_js(source: &str) -> Vec<Vec<ContextPath>> {
    let mut machine = ASTWalkContext::new();
    let cm: Lrc<SourceMap> = Default::default();
    let handler = Handler::with_tty_emitter(ColorConfig::Auto, true, false, Some(cm.clone()));
    let fm = cm.new_source_file(FileName::Custom("test.js".into()), source.to_string());

    let parse_module = |syntax: Syntax| {
        let lexer = Lexer::new(
            syntax,
            Default::default(),
            StringInput::from(&*fm),
            None,
        );

        let mut parser = Parser::new_from(lexer);

        for e in parser.take_errors() {
            e.into_diagnostic(&handler).emit();
        }

        parser.parse_module()
    };

    let module = parse_module(Syntax::Es(Default::default()))
        .or_else(|_| parse_module(Syntax::Typescript(Default::default())))
        .map_err(|mut e| {
            // Unrecoverable fatal error occurred
            e.into_diagnostic(&handler).emit()
        })
        .expect("failed to parse module as either ECMAScript or TypeScript");

    for item in module.body {
        traverse_module(item, &mut machine);
    }
    machine.context_stack_references
}


pub fn build_report(context_paths: &Vec<Vec<ContextPath>>) -> Report {
    // TODO: get all exposed values
    // TODO: get all values referred to but are not available in a given context
    // TODO: get all triggerable functions
    // TODO: get all events that are emitted

    // TODO: triggerable functions should note what they are triggered by
    // TODO: for each of these we should store the context path that refers to them
    // TODO: context paths should include spans
    // TODO: for each of these we should store their type if its available
    let mut exposed_values = HashMap::new();
    let mut depended_values = HashMap::new();
    let mut triggerable_functions = HashMap::new();
    for context_path in context_paths {
        let mut encountered = vec![];

        let mut in_function: Option<&String> = None;
        let mut in_call_expression = false;
        let mut in_ch_call = false;
        let mut attribute_path = vec![];

        for (idx, context_path_unit) in context_path.iter().enumerate() {
            encountered.push(context_path_unit);

            // If we've declared a top level function, it is exposed
            if let ContextPath::InFunction(name, _) = context_path_unit {
                in_function = Some(name);
                if !triggerable_functions.contains_key(name) {
                    triggerable_functions
                        .entry(name.clone())
                        .or_insert_with(|| ReportTriggerableFunctions {
                            arguments: vec![],
                            emit_event: vec![],
                            trigger_on: vec![],
                        });
                }
            }

            if let Some(in_function_name) = in_function {
                if let ContextPath::InCallExpression = context_path_unit {
                    in_call_expression = true;
                }

                if in_call_expression {
                    if let ContextPath::Attribute(attr) = context_path_unit {
                        attribute_path.push(attr);
                    }
                }

                // Decorators set the emit event property for a function
                if &ContextPath::IdentifierReferredTo(String::from("ch"), false)
                    == context_path_unit
                {
                    in_ch_call = true;
                }

                if context_path.len() - 1 == idx {
                    let mut x = triggerable_functions
                        .entry(in_function_name.clone())
                        .or_insert_with(|| ReportTriggerableFunctions {

                            arguments: vec![],
                            emit_event: vec![], // Initialize with an empty string or a default value
                            trigger_on: vec![],
                        });

                    if attribute_path == vec![&"emitAs".to_string()] {
                        if let ContextPath::Constant(const_name) = encountered[idx] {
                            x.emit_event.push(const_name.clone());
                        }
                    }

                    if attribute_path == vec![&"onEvent".to_string()] {
                        if let ContextPath::Constant(const_name) = encountered[idx] {
                            x.trigger_on.push(const_name.clone());
                        }
                    }
                }
            }

            // TODO: this needs to be updated to be smilar to the python implemnetation
            if let ContextPath::IdentifierReferredTo(identifier, false) = context_path_unit {
                if identifier != &String::from("ch") {
                    // If this value is not being assigned to, then it is a dependency

                    if encountered.iter().any(|x| matches!(x, ContextPath::InFunction(_, _)))
                        && encountered.iter().any(|x| matches!(x, ContextPath::Params))
                    {
                        for context_path_unit in &encountered {
                            if let ContextPath::InFunction(function_name, _) = context_path_unit {
                                let mut x = triggerable_functions
                                    .entry(function_name.clone())
                                    .or_insert_with(|| ReportTriggerableFunctions {
                                        arguments: vec![],
                                        emit_event: vec![], // Initialize with an empty string or a default value
                                        trigger_on: vec![],
                                    });
                                x.arguments.push(identifier.clone());
                            }
                        }
                        continue;
                    }

                    // This is an exposed value if it does not occur inside the scope of a function
                    if encountered
                        .iter()
                        .find(|x| matches!(x, ContextPath::InFunction(_, _)))
                        .is_none()
                        &&
                         encountered
                            .iter()
                            .find(|x| matches!(x, ContextPath::InAnonFunction))
                            .is_none()

                    {
                        if encountered.contains(&&ContextPath::AssignmentToStatement) {
                            exposed_values.insert(
                                identifier.clone(),
                                ReportItem {
                                    // context_path: context_path.clone(),
                                },
                            );
                            continue;
                        }
                    }

                    if !encountered.contains(&&ContextPath::AssignmentToStatement) {
                        depended_values.insert(
                            identifier.clone(),
                            ReportItem {
                                // context_path: context_path.clone(),
                            },
                        );
                        continue;
                    }
                }
            }
        }
    }

    let js_built_ins: HashSet<&str> = [
        "Deno", "Chidori",  "Array", "ArrayBuffer", "Boolean", "DataView", "Date", "Error", "EvalError", "Float32Array",
        "Float64Array", "Function", "Generator", "GeneratorFunction", "Infinity", "Int8Array",
        "Int16Array", "Int32Array", "InternalError", "Intl", "JSON", "Map", "Math", "NaN",
        "Number", "Object", "Promise", "Proxy", "RangeError", "ReferenceError", "Reflect",
        "RegExp", "Set", "SharedArrayBuffer", "String", "Symbol", "SyntaxError", "TypeError",
        "URIError", "Uint8Array", "Uint8ClampedArray", "Uint16Array", "Uint32Array", "WeakMap",
        "WeakSet", "decodeURI", "decodeURIComponent", "encodeURI", "encodeURIComponent", "escape",
        "eval", "isFinite", "isNaN", "parseFloat", "parseInt", "unescape", "uneval", "setTimeout", "setInterval", "console"
    ].iter().cloned().collect();

    depended_values.retain(|value,_ | !js_built_ins.contains(value.as_str()));

    Report {
        internal_call_graph: InternalCallGraph {
            graph: Default::default(),
        },
        cell_exposed_values: exposed_values,
        cell_depended_values: depended_values,
        triggerable_functions: triggerable_functions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::python::parse::extract_dependencies_python;
    use crate::language::{Report, ReportItem, ReportTriggerableFunctions};
    use indoc::indoc;

    #[test]
    fn test_extraction_of_ch_statements() {
        let js_source = indoc! { r#"
            import * as ch from "@1kbirds/chidori";

            ch.prompt.configure("default", ch.llm({model: "openai"}))
            "#};
        let context_stack_references = extract_dependencies_js(js_source);
        insta::assert_yaml_snapshot!(context_stack_references);
    }

    #[test]
    fn test_assignment_to_value() {
        let js_source = indoc! { r#"
            const x = 1
            "#};
        let context_stack_references = extract_dependencies_js(js_source);
        insta::with_settings!({
            description => js_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(context_stack_references);
        });
    }

    #[test]
    fn test_nothing_extracted_with_no_ch_references() {
        let js_source = indoc! { r#"
            function createDockerfile() {
                return prompt("prompts/create_dockerfile")
            }
            "#};
        let context_stack_references = extract_dependencies_js(js_source);
        insta::with_settings!({
            description => js_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(context_stack_references);
        });
    }

    #[test]
    fn test_handling_react_hook_style_refrence() {
        let js_source = indoc! { r#"
            function createDockerfile() {
                useHook(() => {
                   ch.prompt("demo");
                }, [otherFunction]);
                return prompt("prompts/create_dockerfile")
            }
            "#};
        let context_stack_references = extract_dependencies_js(js_source);

        insta::with_settings!({
            description => js_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(context_stack_references);
        });

    }

    #[test]
    fn test_function_decorator_ch_annotation() {
        let js_source = indoc! { r#"
            function migrationAgent() {
                ch.register();
                ch.set("bar", 1);
            }
            "#};
        let context_stack_references = extract_dependencies_js(js_source);
        insta::with_settings!({
            description => js_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(context_stack_references);
        });
    }

    #[test]
    fn test_function_decorator_ch_annotation_with_internal_ch_and_emit() {
        let js_source = indoc! { r#"
            function dispatch_agent(ev) {
                ch.onEvent("new_file")
                ch.emitAs("file_created")
                ch.emitAs("file_created", "multiple", "args")
                ch.set("file_path", ev.file_path)
            }
            "#};
        let context_stack_references = extract_dependencies_js(js_source);

        insta::with_settings!({
            description => js_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(context_stack_references);
        });

    }

    #[test]
    fn test_ch_reference_internal_to_function() {
        let js_source = indoc! { r#"
            function evaluate_agent(ev) {
                ch.set("file_path", ev.file_path)
                migration_agent()
            }
            "#};
        let context_stack_references = extract_dependencies_js(js_source);

        insta::with_settings!({
            description => js_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(context_stack_references);
        });

    }

    #[test]
    fn test_ch_function_decoration_referring_to_another_function() {
        let js_source = indoc! { r#"
            function setupPipeline(x) {
                ch.p(create_dockerfile)
                return x
            }
            "#};
        let context_stack_references = extract_dependencies_js(js_source);

        insta::with_settings!({
            description => js_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(context_stack_references);
        });

    }

    #[test]
    fn test_ch_function_with_arguments() {
        let js_source = indoc! { r#"
            function subtract(a, b) {
                return a - b;
            }

            // Example usage
            const v = subtract(x, 5);
            "#};
        let context_stack_references = extract_dependencies_js(js_source);

        insta::with_settings!({
            description => js_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(context_stack_references);
        });

    }

    #[test]
    fn test_pipe_function_composition() {
        let js_source = indoc! { r#"
            function main() {
                bar() | foo() | baz()
            }
            "#};
        let context_stack_references = extract_dependencies_js(js_source);
        insta::with_settings!({
            description => js_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(context_stack_references);
        });
    }

    #[test]
    fn test_report_generation() {
        let js_source = indoc! { r#"
        function testing() {
            ch.onEvent("new_file");
            ch.emitAs("file_created");
            const x = 2 + y;
            return x
        }
            "#};
        let context_stack_references = extract_dependencies_js(js_source);
        insta::with_settings!({
            description => js_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(context_stack_references);
        });
        let result = build_report(&context_stack_references);
        insta::with_settings!({
            description => js_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(result);
        });
    }

    #[test]
    fn test_report_for_simple_function() {
        let js_source = indoc! { r#"
        function testing(x) {
            return x
        }
            "#};
        let context_stack_references = extract_dependencies_js(js_source);
        insta::with_settings!({
            description => js_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(context_stack_references);
        });
        let result = build_report(&context_stack_references);
        insta::with_settings!({
            description => js_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(result);
        });
    }

    #[test]
    fn test_report_generation_deno_test_framework() {
        let js_source = indoc! { r#"
        import { assertEquals } from "https://deno.land/std@0.221.0/assert/mod.ts";

        Deno.test("addition test", async () => {
            const result = await add_two(2);
            console.log(result);
            assertEquals(result, 4);
        });
        "#};
        let context_stack_references = extract_dependencies_js(js_source);
        insta::with_settings!({
            description => js_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(context_stack_references);
        });
        ;
        let result = build_report(&context_stack_references);
        let report = Report {
            internal_call_graph: InternalCallGraph {
                graph: Default::default(),
            },
            cell_exposed_values: {
                let mut map = std::collections::HashMap::new();
                map
            },
            cell_depended_values: {
                let mut map = std::collections::HashMap::new();
                map.insert("add_two".to_string(), ReportItem {});
                map
            },
            triggerable_functions: {
                let mut map = std::collections::HashMap::new();
                map
            },
        };
        assert_eq!(result, report);
    }

    #[test]
    fn test_report_generation_with_import() {
        let js_source = indoc! { r#"
import { random } from "random"

function fun_name() {
    const w = function_that_doesnt_exist()
    const v = 5
    return v
}

x = random.randint(0, 10)
"#};
        let context_stack_references = extract_dependencies_js(js_source);
        insta::with_settings!({
            description => js_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(context_stack_references);
        });
        ;
        let result = build_report(&context_stack_references);
        let report = Report {

            internal_call_graph: InternalCallGraph {
                graph: Default::default(),
            },
            cell_exposed_values: {
                let mut map = std::collections::HashMap::new();
                map.insert("x".to_string(), ReportItem {});
                map
            },
            cell_depended_values: {
                let mut map = std::collections::HashMap::new();
                map.insert("function_that_doesnt_exist".to_string(), ReportItem {});
                map
            },
            triggerable_functions: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "fun_name".to_string(),
                    ReportTriggerableFunctions {

                        arguments: vec![],
                        emit_event: vec![],
                        trigger_on: vec![],
                    },
                );
                map
            },
        };
        assert_eq!(result, report);
    }
}
