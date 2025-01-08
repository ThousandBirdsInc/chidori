use ruff_python_ast::{Expr, Operator, Stmt, StmtClassDef, StmtFunctionDef};
use ruff_python_ast::{str::Quote, Mod, ModModule};
use ruff_python_parser::{self, parse_module, Mode, ParseError};
use ruff_source_file::LineEnding;
use crate::ruff_python_codegen::{Generator, Stylist};

/// This slices python code into executable units that are useful to handle state checkpointing against:
/// - The body of while and for loops are split out into function calls
/// - If statements are split into individual branches and the evaluation of their condition

/// This also injects statements that keep track of our execution stack during evaluation and for jumping to
/// the appropriate point in execution when we restore from a stored execution state


fn handle_expr(expr: Expr) {
    match expr {
        Expr::BoolOp(_) => {}
        Expr::Named(_) => {}
        Expr::BinOp(_) => {}
        Expr::UnaryOp(_) => {}
        Expr::Lambda(_) => {}
        Expr::If(_) => {}
        Expr::Dict(_) => {}
        Expr::Set(_) => {}
        Expr::ListComp(_) => {}
        Expr::SetComp(_) => {}
        Expr::DictComp(_) => {}
        Expr::Generator(_) => {}
        Expr::Await(_) => {}
        Expr::Yield(_) => {}
        Expr::YieldFrom(_) => {}
        Expr::Compare(_) => {}
        Expr::Call(_) => {}
        Expr::FString(_) => {}
        Expr::StringLiteral(_) => {}
        Expr::BytesLiteral(_) => {}
        Expr::NumberLiteral(_) => {}
        Expr::BooleanLiteral(_) => {}
        Expr::NoneLiteral(_) => {}
        Expr::EllipsisLiteral(_) => {}
        Expr::Attribute(_) => {}
        Expr::Subscript(_) => {}
        Expr::Starred(_) => {}
        Expr::Name(_) => {}
        Expr::List(_) => {}
        Expr::Tuple(_) => {}
        Expr::Slice(_) => {}
        Expr::IpyEscapeCommand(_) => {}
    }
}

fn handle_stmt(stmt: Stmt) {
    match stmt {
        Stmt::FunctionDef(x) => {
            let ruff_python_ast::StmtFunctionDef {
                range, is_async, decorator_list, name, type_params, parameters, returns, body
            } = x;
        }
        Stmt::ClassDef(x) => {
            let ruff_python_ast::StmtClassDef {
                range, decorator_list, name, type_params, arguments, body
            } = x;
        }
        Stmt::Return(x) => {
            let ruff_python_ast::StmtReturn {
                range, value
            } = x;
        }
        Stmt::Delete(x) => {
            let ruff_python_ast::StmtDelete {
                range, targets
            } = x;
        }
        Stmt::Assign(x) => {
            let ruff_python_ast::StmtAssign {
                range, targets, value
            } = x;
        }
        Stmt::AugAssign(x) => {
            let ruff_python_ast::StmtAugAssign {
                range, target, op, value
            } = x;
        }
        Stmt::AnnAssign(_) => {}
        Stmt::TypeAlias(_) => {}
        Stmt::For(x) => {
            let ruff_python_ast::StmtFor {
                range, is_async, target, iter, body, orelse
            } = x;
        }
        Stmt::While(x) => {
            let ruff_python_ast::StmtWhile {
                range, test, body, orelse
            } = x;
        }
        Stmt::If(x) => {
            let ruff_python_ast::StmtIf {
                range, test, body, elif_else_clauses
            } = x;
        }
        Stmt::With(x) => {
            let ruff_python_ast::StmtWith {
                range, is_async, items, body
            } = x;
        }
        Stmt::Match(x) => {
            let ruff_python_ast::StmtMatch {
                range, subject, cases
            } = x;
        }
        Stmt::Raise(x) => {
            let ruff_python_ast::StmtRaise {
                range, exc, cause
            } = x;
        }
        Stmt::Try(x) => {
            let ruff_python_ast::StmtTry {
                range, body, handlers, orelse, finalbody, is_star
            } = x;
        }
        Stmt::Import(x) => {
            let ruff_python_ast::StmtImport {
                range, names
            } = x;
        }
        Stmt::Global(x) => {
            let ruff_python_ast::StmtGlobal {
                range, names
            } = x;
        }
        Stmt::Nonlocal(x) => {
            let ruff_python_ast::StmtNonlocal {
                range, names
            } = x;
        }
        Stmt::Expr(x) => {
            let ruff_python_ast::StmtExpr {
                range, value
            } = x;
        }
        Stmt::Pass(x) => {
            let ruff_python_ast::StmtPass {
                range
            } = x;
        }
        Stmt::Break(x) => {
            let ruff_python_ast::StmtBreak {
                range
            } = x;
        }
        Stmt::Continue(x) => {
            let ruff_python_ast::StmtContinue {
                range
            } = x;
        }
        Stmt::Assert(x) => {
            let ruff_python_ast::StmtAssert {
                range, test, msg
            } = x;
        }
        Stmt::ImportFrom(x) => {
            let ruff_python_ast::StmtImportFrom {
                range, module, names, level
            } = x;
        }
        Stmt::IpyEscapeCommand(x) => {
            let ruff_python_ast::StmtIpyEscapeCommand {
                range, kind, value
            } = x;
        }
    }
}

fn handle_stmt_class_def(s: StmtClassDef) {
    for stmt in s.body {
        handle_stmt(stmt);
    }
}

fn handle_stmt_function_def(s: StmtFunctionDef) {
    for stmt in s.body {
        handle_stmt(stmt);
    }
}

fn handle_mod_module(module: ModModule) {
    for stmt in module.body {
        handle_stmt(stmt);
    }
}

pub fn round_trip(code: &str) -> Result<String, ParseError> {
    let parsed = parse_module(code)?;


    // let stylist = Stylist::from_tokens(parsed.tokens(), code);
    // let mut generator: Generator = (&stylist).into();
    // generator.unparse_suite(parsed.suite());
    // Ok(generator.generate())
    Ok(String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_example() {
        assert_eq!(
            round_trip(r"def test(a, b, /, c, *, d, **kwargs):
    pass"
            ).unwrap(),
            r"def test(a, b, /, c, *, d, **kwargs):
    pass"
                .replace('\n', LineEnding::default().as_str())
        );
    }
}