extern crate swc_ecma_parser;
use swc_common::sync::Lrc;
use swc_common::{
    errors::{ColorConfig, Handler},
    FileName, FilePathMapping, SourceMap,
};
use swc_ecma_ast as ast;
use swc_ecma_ast::{Decl, Expr, FnDecl, ModuleDecl, ModuleItem, Pat, PatOrExpr, Stmt};
use swc_ecma_parser::{lexer::Lexer, Parser, StringInput, Syntax};

fn traverse_module(module: ModuleItem) {
    match module {
        ModuleItem::ModuleDecl(mod_decl) => match mod_decl {
            ModuleDecl::Import(_) => {}
            ModuleDecl::ExportDecl(_) => {}
            ModuleDecl::ExportNamed(_) => {}
            ModuleDecl::ExportDefaultDecl(_) => {}
            ModuleDecl::ExportDefaultExpr(_) => {}
            ModuleDecl::ExportAll(_) => {}
            ModuleDecl::TsImportEquals(_) => {}
            ModuleDecl::TsExportAssignment(_) => {}
            ModuleDecl::TsNamespaceExport(_) => {}
        },
        ModuleItem::Stmt(_) => {}
    }
}

fn traverse_expr(expr: ast::Expr) {
    match expr {
        Expr::This(ast::ThisExpr { .. }) => {}
        Expr::Array(ast::ArrayLit { .. }) => {}
        Expr::Object(ast::ObjectLit { .. }) => {}
        Expr::Fn(ast::FnExpr { .. }) => {}
        Expr::Unary(ast::UnaryExpr { .. }) => {}
        Expr::Update(ast::UpdateExpr { .. }) => {}
        Expr::Bin(ast::BinExpr { .. }) => {}
        Expr::Assign(ast::AssignExpr { .. }) => {}
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
        Expr::JSXElement(el) => {
            let ast::JSXElement { .. } = &*el;
        }
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

fn traverse_stmts(stmts: &[Stmt]) {
    for stmt in stmts {
        match stmt {
            Stmt::Expr(expr_stmt) => {
                if let Expr::Assign(assign_expr) = &*expr_stmt.expr {
                    if let PatOrExpr::Pat(pat) = &assign_expr.left {
                        if let Pat::Ident(binding_ident) = &**pat {
                            println!("Assignment to variable: {}", binding_ident.id.sym);
                        }
                    }
                }
            }
            Stmt::Block(block_stmt) => {
                traverse_stmts(&block_stmt.stmts);
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
                Decl::Fn(_) => {}
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
                            traverse_stmts(&body.stmts);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}
