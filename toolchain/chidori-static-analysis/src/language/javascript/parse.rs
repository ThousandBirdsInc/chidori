extern crate swc_ecma_parser;

use crate::language::python;
use rustpython_parser::ast::{Constant, Identifier};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use swc_common::sync::Lrc;
use swc_common::{
    errors::{ColorConfig, Handler},
    FileName, FilePathMapping, SourceMap,
};
use swc_ecma_ast as ast;
use swc_ecma_ast::{Decl, Expr, FnDecl, ModuleDecl, ModuleItem, Pat, PatOrExpr, Stmt};
use swc_ecma_parser::{lexer::Lexer, Parser, StringInput, Syntax};

#[derive(Debug, Clone, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum ContextPath {
    Initialized,
    InFunction(String),
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

// TODO: we're building a filtered subset of the AST
// TODO: what we can consider is evaluating the ast into the references
// TODO: the context path is the accumulated state, when we pop it we evaluate it
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

    fn enter_statement_function(&mut self, name: &ast::Ident) -> usize {
        self.context_stack
            .push(ContextPath::InFunction(name.to_string()));
        self.context_stack_references
            .push(self.context_stack.clone());
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

    fn encounter_constant(&mut self, name: &Constant) {
        if let Constant::Str(s) = name {
            self.context_stack
                .push(ContextPath::Constant(s.to_string()));
            self.context_stack_references
                .push(self.context_stack.clone());
        }
    }

    fn encounter_named_reference(&mut self, name: &Identifier) {
        // TODO: we need to check if this is a local variable or not
        if self.var_exists(&name.to_string()) {
            // true, the var exists in the local or global scope
            self.context_stack
                .push(ContextPath::IdentifierReferredTo(name.to_string(), true));
        } else {
            if let Some(ContextPath::AssignmentToStatement) = self.context_stack.last() {
                self.locals.insert(name.to_string());
            }
            self.context_stack
                .push(ContextPath::IdentifierReferredTo(name.to_string(), false));
        }
        self.context_stack_references
            .push(self.context_stack.clone());
        self.context_stack.pop();
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

    fn enter_attr(&mut self, name: &Identifier) -> usize {
        self.context_stack
            .push(ContextPath::Attribute(name.to_string()));
        self.context_stack.len()
    }

    fn pop_until(&mut self, size: usize) {
        while self.context_stack.len() >= size {
            self.context_stack.pop();
        }
    }
}

fn traverse_module(module: ModuleItem, machine: &mut ASTWalkContext) {
    match module {
        ModuleItem::ModuleDecl(mod_decl) => match mod_decl {
            ModuleDecl::Import(ast::ImportDecl { .. }) => {}
            ModuleDecl::ExportDecl(_) => {}
            ModuleDecl::ExportNamed(_) => {}
            ModuleDecl::ExportDefaultDecl(_) => {}
            ModuleDecl::ExportDefaultExpr(_) => {}
            ModuleDecl::ExportAll(_) => {}
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

fn traverse_expr(expr: &ast::Expr, machine: &mut ASTWalkContext) {
    match expr {
        Expr::This(ast::ThisExpr { .. }) => {}
        Expr::Array(ast::ArrayLit { .. }) => {}
        Expr::Object(ast::ObjectLit { .. }) => {}
        Expr::Fn(ast::FnExpr { .. }) => {}
        Expr::Unary(ast::UnaryExpr { .. }) => {}
        Expr::Update(ast::UpdateExpr { .. }) => {}
        Expr::Bin(ast::BinExpr { .. }) => {}
        Expr::Assign(ast::AssignExpr {
            op, left, right, ..
        }) => {
            let idx = machine.enter_assignment_to_statement();
            match left {
                PatOrExpr::Expr(expr) => {
                    traverse_expr(expr, machine);
                }
                PatOrExpr::Pat(pat) => {}
            }
            machine.pop_until(idx);
            traverse_expr(right, machine);
            machine.pop_until(idx);
        }
        Expr::Member(ast::MemberExpr { .. }) => {}
        Expr::SuperProp(ast::SuperPropExpr { .. }) => {}
        Expr::Cond(ast::CondExpr { .. }) => {}
        Expr::Call(ast::CallExpr { .. }) => {}
        Expr::New(ast::NewExpr { .. }) => {}
        Expr::Seq(ast::SeqExpr { .. }) => {}
        Expr::Ident(ast::Ident { .. }) => {}
        Expr::Lit(_) => {}
        Expr::Tpl(ast::Tpl { .. }) => {}
        Expr::TaggedTpl(ast::TaggedTpl { .. }) => {}
        Expr::Arrow(ast::ArrowExpr { .. }) => {}
        Expr::Class(ast::ClassExpr { .. }) => {}
        Expr::Yield(ast::YieldExpr { .. }) => {}
        Expr::MetaProp(ast::MetaPropExpr { .. }) => {}
        Expr::Await(ast::AwaitExpr { .. }) => {}
        Expr::Paren(ast::ParenExpr { .. }) => {}
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

fn traverse_stmts(stmts: &[Stmt], machine: &mut ASTWalkContext) {
    for stmt in stmts {
        match stmt {
            Stmt::Expr(expr_stmt) => {
                traverse_expr(&*expr_stmt.expr, machine);
            }
            Stmt::Block(block_stmt) => {
                traverse_stmts(&block_stmt.stmts, machine);
            }
            Stmt::Empty(_) => {}
            Stmt::Debugger(ast::DebuggerStmt { .. }) => {}
            Stmt::With(ast::WithStmt { .. }) => {}
            Stmt::Return(ast::ReturnStmt { .. }) => {}
            Stmt::Labeled(ast::LabeledStmt { .. }) => {}
            Stmt::Break(ast::BreakStmt { .. }) => {}
            Stmt::Continue(ast::ContinueStmt { .. }) => {}
            Stmt::If(ast::IfStmt { .. }) => {}
            Stmt::Switch(ast::SwitchStmt { .. }) => {}
            Stmt::Throw(ast::ThrowStmt { .. }) => {}
            Stmt::Try(t) => {
                let ast::TryStmt { block, .. } = &**t;
            }
            Stmt::While(ast::WhileStmt { .. }) => {}
            Stmt::DoWhile(ast::DoWhileStmt { .. }) => {}
            Stmt::For(ast::ForStmt { .. }) => {}
            Stmt::ForIn(ast::ForInStmt { .. }) => {}
            Stmt::ForOf(ast::ForOfStmt { .. }) => {}
            Stmt::Decl(decl) => match decl {
                Decl::Class(_) => {}
                Decl::Fn(ast::FnDecl {
                    ident, function, ..
                }) => {
                    let idx = machine.enter_statement_function(ident);
                    let ast::Function { body, .. } = &**function;
                    if let Some(body) = body {
                        traverse_stmts(&body.stmts, machine);
                    }
                    machine.pop_until(idx);
                }
                Decl::Var(_) => {}
                Decl::Using(_) => {}
                Decl::TsInterface(_) => {}
                Decl::TsTypeAlias(_) => {}
                Decl::TsEnum(_) => {}
                Decl::TsModule(_) => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_evaluation_single_node() {
        let machine = &mut ASTWalkContext::new();
        let cm: Lrc<SourceMap> = Default::default();
        let handler = Handler::with_tty_emitter(ColorConfig::Auto, true, false, Some(cm.clone()));

        // Real usage
        // let fm = cm
        //     .load_file(Path::new("test.js"))
        //     .expect("failed to load test.js");
        let fm = cm.new_source_file(
            FileName::Custom("test.js".into()),
            r#"
            function bar() {
               console.log("bar");
            }
            
            function x() {
              ch.get("testing")
            }
            
            function x() {
              ch.set("testing", "other")
            }
            
            function v() {
            
            }
            
            function foo() {
                ch.useEffect(() => {
                    let test = prompt.split("\n");
                    console.log("Hello World!");
                }, ["bar"])
            }
            
            "#
            .into(),
        );
        let lexer = Lexer::new(
            // We want to parse ecmascript
            Syntax::Es(Default::default()),
            // EsVersion defaults to es5
            Default::default(),
            StringInput::from(&*fm),
            None,
        );

        let mut parser = Parser::new_from(lexer);

        for e in parser.take_errors() {
            e.into_diagnostic(&handler).emit();
        }

        let module = parser
            .parse_module()
            .map_err(|mut e| {
                // Unrecoverable fatal error occurred
                e.into_diagnostic(&handler).emit()
            })
            .expect("failed to parser module");

        for item in module.body {
            if let ModuleItem::Stmt(stmt) = item {
                match stmt {
                    Stmt::Decl(Decl::Fn(FnDecl {
                        ident, function, ..
                    })) => {
                        println!("Function name: {}", ident.sym);
                        println!("Function body: {:?}", function.body);
                        if let Some(body) = &function.body {
                            traverse_stmts(&body.stmts, machine);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}
