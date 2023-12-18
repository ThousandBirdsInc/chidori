use rustpython_parser::ast::{Expr, Identifier, Stmt};
use rustpython_parser::{ast, Parse};
use std::collections::HashMap;

struct DecoratorExtrator {
    decorators: Vec<ast::Expr>,
}

struct FunctionMachine {}

#[derive(Default)]
struct ASTWalkContext {
    in_function: bool,
    functions: HashMap<Identifier, FunctionMachine>,
    breadcrumbs: Vec<String>,
    stack: Vec<String>,
}

impl ASTWalkContext {
    fn new() -> Self {
        Self {
            in_function: false,
            functions: HashMap::new(),
            breadcrumbs: vec![],
            stack: vec![],
        }
    }

    fn enter_function(&mut self, name: &Identifier) {}

    fn encounter_ch(&mut self) {}

    fn pop(&mut self) {}
}

fn traverse_expression(expr: &ast::Expr, machine: &mut ASTWalkContext) {
    match expr {
        ast::Expr::BoolOp(ast::ExprBoolOp { values, .. }) => {
            for value in values {
                traverse_expression(value, machine);
            }
        }
        ast::Expr::NamedExpr(ast::ExprNamedExpr { value, .. }) => {
            traverse_expression(value, machine);
        }
        ast::Expr::BinOp(expr) => {
            // dbg!(expr);
            let ast::ExprBinOp { left, right, .. } = expr;
            traverse_expression(left, machine);
            traverse_expression(right, machine);
        }
        ast::Expr::UnaryOp(ast::ExprUnaryOp { operand, .. }) => {
            traverse_expression(operand, machine);
        }
        ast::Expr::Lambda(ast::ExprLambda { body, .. }) => {
            traverse_expression(body, machine);
        }
        ast::Expr::IfExp(ast::ExprIfExp {
            test, body, orelse, ..
        }) => {
            traverse_expression(test, machine);
            traverse_expression(body, machine);
            traverse_expression(orelse, machine);
        }
        ast::Expr::Dict(ast::ExprDict { keys, values, .. }) => {
            for key in keys {
                if let Some(key) = key {
                    traverse_expression(key, machine);
                }
            }
            for value in values {
                traverse_expression(value, machine);
            }
        }
        ast::Expr::Set(ast::ExprSet { elts, .. }) => {
            for elt in elts {
                traverse_expression(elt, machine);
            }
        }
        ast::Expr::ListComp(ast::ExprListComp {
            elt, generators, ..
        }) => {
            traverse_expression(elt, machine);
            for generator in generators {
                traverse_comprehension(generator, machine);
            }
        }
        ast::Expr::SetComp(ast::ExprSetComp {
            elt, generators, ..
        }) => {
            traverse_expression(elt, machine);
            for generator in generators {
                traverse_comprehension(generator, machine);
            }
        }
        ast::Expr::DictComp(ast::ExprDictComp {
            key,
            value,
            generators,
            ..
        }) => {
            traverse_expression(key, machine);
            traverse_expression(value, machine);
            for generator in generators {
                traverse_comprehension(generator, machine);
            }
        }
        ast::Expr::GeneratorExp(ast::ExprGeneratorExp {
            elt, generators, ..
        }) => {
            traverse_expression(elt, machine);
            for generator in generators {
                traverse_comprehension(generator, machine);
            }
        }
        ast::Expr::Await(ast::ExprAwait { value, .. }) => {
            traverse_expression(value, machine);
        }
        ast::Expr::Yield(ast::ExprYield { value, .. }) => {
            if let Some(val) = value {
                traverse_expression(val, machine);
            }
        }
        ast::Expr::YieldFrom(ast::ExprYieldFrom { value, .. }) => {
            traverse_expression(value, machine);
        }
        ast::Expr::Compare(ast::ExprCompare {
            left,
            ops,
            comparators,
            ..
        }) => {
            traverse_expression(left, machine);
            for comparator in comparators {
                traverse_expression(comparator, machine);
            }
        }
        ast::Expr::Call(expr) => {
            // dbg!(expr);
            let ast::ExprCall {
                func,
                args,
                keywords,
                ..
            } = expr;
            traverse_expression(func, machine);
            for arg in args {
                traverse_expression(arg, machine);
            }
            for keyword in keywords {
                traverse_expression(&keyword.value, machine);
            }
        }
        ast::Expr::FormattedValue(ast::ExprFormattedValue { value, .. }) => {
            traverse_expression(value, machine);
        }
        ast::Expr::JoinedStr(ast::ExprJoinedStr { values, .. }) => {
            for value in values {
                traverse_expression(value, machine);
            }
        }
        ast::Expr::Constant(_) => {}
        ast::Expr::Attribute(expr) => {
            let ast::ExprAttribute { value, .. } = expr;
            if let ast::Expr::Name(ast::ExprName { id, .. }) = &value {
                if id == "ch" {
                    machine.encounter_ch();
                }
            }
            traverse_expression(value, machine);
        }
        ast::Expr::Subscript(ast::ExprSubscript { value, slice, .. }) => {
            traverse_expression(value, machine);
            traverse_expression(slice, machine);
        }
        ast::Expr::Starred(ast::ExprStarred { value, .. }) => {
            traverse_expression(value, machine);
        }
        ast::Expr::Name(_) => {}
        ast::Expr::List(ast::ExprList { elts, .. }) => {
            for elt in elts {
                traverse_expression(elt, machine);
            }
        }
        ast::Expr::Tuple(ast::ExprTuple { elts, .. }) => {
            for elt in elts {
                traverse_expression(elt, machine);
            }
        }
        ast::Expr::Slice(ast::ExprSlice {
            lower, upper, step, ..
        }) => {
            if let Some(expr) = lower {
                traverse_expression(expr, machine);
            }
            if let Some(expr) = upper {
                traverse_expression(expr, machine);
            }
            if let Some(expr) = step {
                traverse_expression(expr, machine);
            }
        }
    }
}

fn traverse_comprehension(comp: &ast::Comprehension, machine: &mut ASTWalkContext) {
    traverse_expression(&comp.target, machine);
    traverse_expression(&comp.iter, machine);
    for condition in &comp.ifs {
        traverse_expression(condition, machine);
    }
}

fn traverse_statements(statements: &[ast::Stmt], machine: &mut ASTWalkContext) {
    for stmt in statements {
        match stmt {
            ast::Stmt::Assign(ast::StmtAssign { targets, .. }) => {
                for target in targets {
                    if let ast::Expr::Name(ast::ExprName { id, .. }) = &target {
                        println!("Assignment to variable: {:?}", id);
                    }
                }
            }
            ast::Stmt::FunctionDef(ast::StmtFunctionDef {
                body,
                decorator_list,
                name,
                ..
            }) => {
                // TODO: this does include typeparams
                machine.enter_function(name);
                for decorator in decorator_list {
                    traverse_expression(decorator, machine);
                }
                traverse_statements(body, machine);
                machine.pop();
            }

            ast::Stmt::AsyncFunctionDef(ast::StmtAsyncFunctionDef { body, .. }) => {
                traverse_statements(body, machine);
            }
            ast::Stmt::ClassDef(ast::StmtClassDef { body, .. }) => {
                // TODO: this does include typeparams
                traverse_statements(body, machine);
            }
            ast::Stmt::Return(ast::StmtReturn { value, .. }) => {
                if let Some(expr) = value {
                    traverse_expression(expr, machine);
                }
            }
            ast::Stmt::Delete(ast::StmtDelete { targets, .. }) => {
                for target in targets {
                    traverse_expression(target, machine);
                }
            }
            ast::Stmt::TypeAlias(ast::StmtTypeAlias { .. }) => {
                // No recursion needed
            }
            ast::Stmt::AugAssign(ast::StmtAugAssign { target, value, .. }) => {
                traverse_expression(target, machine);
                traverse_expression(value, machine);
            }
            ast::Stmt::AnnAssign(ast::StmtAnnAssign {
                target,
                annotation,
                value,
                ..
            }) => {
                traverse_expression(target, machine);
                traverse_expression(annotation, machine);
                if let Some(expr) = value {
                    traverse_expression(expr, machine);
                }
            }
            ast::Stmt::For(ast::StmtFor {
                target,
                iter,
                body,
                orelse,
                ..
            }) => {
                traverse_expression(target, machine);
                traverse_expression(iter, machine);
                traverse_statements(body, machine);
                traverse_statements(orelse, machine);
            }
            ast::Stmt::AsyncFor(ast::StmtAsyncFor {
                target,
                iter,
                body,
                orelse,
                ..
            }) => {
                traverse_expression(target, machine);
                traverse_expression(iter, machine);
                traverse_statements(body, machine);
                traverse_statements(orelse, machine);
            }
            ast::Stmt::While(ast::StmtWhile {
                test, body, orelse, ..
            }) => {
                traverse_expression(test, machine);
                traverse_statements(body, machine);
                traverse_statements(orelse, machine);
            }
            ast::Stmt::If(ast::StmtIf {
                test, body, orelse, ..
            }) => {
                traverse_expression(test, machine);
                traverse_statements(body, machine);
                traverse_statements(orelse, machine);
            }
            ast::Stmt::With(ast::StmtWith { items, body, .. }) => {
                for item in items {
                    traverse_expression(&item.context_expr, machine);
                    if let Some(expr) = &item.optional_vars {
                        traverse_expression(&expr, machine);
                    }
                }
                traverse_statements(body, machine);
            }
            ast::Stmt::AsyncWith(ast::StmtAsyncWith { items, body, .. }) => {
                for item in items {
                    traverse_expression(&item.context_expr, machine);
                    if let Some(expr) = &item.optional_vars {
                        traverse_expression(&expr, machine);
                    }
                }
                traverse_statements(body, machine);
            }
            ast::Stmt::Match(ast::StmtMatch { subject, cases, .. }) => {
                traverse_expression(subject, machine);
                for case in cases {
                    traverse_statements(&case.body, machine);
                    // Also traverse any pattern matching expressions if applicable
                }
            }
            ast::Stmt::Raise(ast::StmtRaise { exc, cause, .. }) => {
                if let Some(expr) = exc {
                    traverse_expression(expr, machine);
                }
                if let Some(expr) = cause {
                    traverse_expression(expr, machine);
                }
            }
            ast::Stmt::Try(ast::StmtTry {
                body,
                handlers,
                orelse,
                finalbody,
                ..
            }) => {
                traverse_statements(body, machine);
                for handler in handlers {
                    if let ast::ExceptHandler::ExceptHandler(handler) = handler {
                        traverse_statements(&handler.body, machine);
                    }
                }
                traverse_statements(orelse, machine);
                traverse_statements(finalbody, machine);
            }
            ast::Stmt::TryStar(ast::StmtTryStar { .. }) => {
                // Process try* statement with recursion as needed
            }
            ast::Stmt::Assert(ast::StmtAssert { test, msg, .. }) => {
                traverse_expression(test, machine);
                if let Some(expr) = msg {
                    traverse_expression(expr, machine);
                }
            }
            ast::Stmt::Import(ast::StmtImport { .. }) => {
                // TODO: Import
                // No recursion needed
            }
            ast::Stmt::ImportFrom(ast::StmtImportFrom { .. }) => {
                // TODO: Import
                // No recursion needed
            }
            ast::Stmt::Global(ast::StmtGlobal { .. }) => {
                // No recursion needed
            }
            ast::Stmt::Nonlocal(ast::StmtNonlocal { .. }) => {
                // No recursion needed
            }
            ast::Stmt::Expr(ast::StmtExpr { value, .. }) => {
                traverse_expression(value, machine);
            }
            ast::Stmt::Pass(ast::StmtPass { .. }) => {
                // No recursion needed
            }
            ast::Stmt::Break(ast::StmtBreak { .. }) => {
                // No recursion needed
            }
            ast::Stmt::Continue(ast::StmtContinue { .. }) => {
                // No recursion needed
            }
        }
    }
}

fn example() {
    let python_source = r#"
def is_odd(i):
  return bool(i & 1)
"#;
    let ast = ast::Suite::parse(python_source, "<embedded>");

    assert!(ast.is_ok());
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    #[test]
    fn test_extraction_of_ch_statements() {
        let python_source = indoc! { r#"
            from chidori.core import ch
            
            ch.prompt.configure("default", ch.llm(model="openai"))
            
            def create_dockerfile():
               return prompt("prompts/create_dockerfile")
            
            def migration_agent():
               ch.set("bar", 1)
            
            @ch.on_event("new_file")
            @ch.emit_as("file_created")
            def dispatch_agent(ev):
                ch.set("file_path", ev.file_path)
                
            def evaluate_agent(ev):
                ch.set("file_path", ev.file_path)
                
            @ch.generate("")
            def wizard():
                pass
                
            @ch.p(create_dockerfile)
            def setup_pipeline(x):
                return x
                
            def main():
                 bar() | foo() | baz()
            "#};
        let ast = ast::Suite::parse(python_source, "<embedded>").unwrap();
        // dbg!(&ast);

        // TODO: functiondef -> decorator_list -> call

        // TODO: machine should extract statments namespaced to "ch"
        // TODO: the sub expression should be evalutated

        let mut machine = ASTWalkContext::default();

        for item in ast {
            match item {
                ast::Stmt::FunctionDef(ast::StmtFunctionDef { name, body, .. }) => {
                    traverse_statements(&body, &mut machine)
                }
                _ => {}
            }
        }
    }
}
