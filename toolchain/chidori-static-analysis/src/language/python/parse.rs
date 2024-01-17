use rustpython_parser::ast::{Expr, Identifier, Stmt};
use rustpython_parser::{ast, Parse};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::collections::HashSet;

// TODO: move to using the Ruff library here to break about functions into independent snippets

struct DecoratorExtrator {
    decorators: Vec<ast::Expr>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ContextPath {
    Initialized,
    InFunction(String),
    InFunctionDecorator(usize),
    InCallExpression,
    ChName,
    VariableAssignment(String),
    IdentifierReferredTo(String, bool),
    Attribute(String),
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
}

impl ASTWalkContext {
    fn new() -> Self {
        Self {
            context_stack_references: vec![],
            context_stack: vec![],
            locals: HashSet::new(),
        }
    }

    fn enter_statement_function(&mut self, name: &Identifier) {
        self.context_stack
            .push(ContextPath::InFunction(name.to_string()));
    }

    fn enter_decorator_expression(&mut self, idx: &usize) {
        self.context_stack
            .push(ContextPath::InFunctionDecorator(idx.clone()));
    }

    fn enter_call_expression(&mut self) {
        self.context_stack.push(ContextPath::InCallExpression);
    }

    fn encounter_ch(&mut self) {
        self.context_stack.push(ContextPath::ChName);
        self.context_stack_references
            .push(self.context_stack.clone());
        self.context_stack.pop();
    }

    fn encounter_named_reference(&mut self, name: &Identifier) {
        // TODO: we need to check if this is a local variable or not
        if self.locals.contains(&name.to_string()) {
            self.context_stack
                .push(ContextPath::IdentifierReferredTo(name.to_string(), true));
        } else {
            self.context_stack
                .push(ContextPath::IdentifierReferredTo(name.to_string(), false));
        }
        self.context_stack_references
            .push(self.context_stack.clone());
        self.context_stack.pop();
    }

    fn encounter_assignment(&mut self, name: &Identifier) {
        self.context_stack
            .push(ContextPath::VariableAssignment(name.to_string()));
        self.context_stack_references
            .push(self.context_stack.clone());
        self.context_stack.pop();
    }

    fn enter_attr(&mut self, name: &Identifier) {
        self.context_stack
            .push(ContextPath::Attribute(name.to_string()));
    }

    fn pop(&mut self) {
        let ctx = self.context_stack.pop();
        self.locals.clear();
    }
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
            machine.enter_call_expression();
            let ast::ExprCall {
                func,
                args,
                keywords,
                ..
            } = expr;
            for arg in args {
                traverse_expression(arg, machine);
            }
            for keyword in keywords {
                traverse_expression(&keyword.value, machine);
            }
            traverse_expression(func, machine);
            machine.pop();
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
        ast::Expr::Attribute(ast::ExprAttribute { value, attr, .. }) => {
            machine.enter_attr(attr);
            traverse_expression(value, machine);
            machine.pop();
        }
        ast::Expr::Subscript(ast::ExprSubscript { value, slice, .. }) => {
            traverse_expression(value, machine);
            traverse_expression(slice, machine);
        }
        ast::Expr::Starred(ast::ExprStarred { value, .. }) => {
            traverse_expression(value, machine);
        }
        ast::Expr::Name(ast::ExprName { id, .. }) => {
            if id == "ch" {
                machine.encounter_ch();
            } else {
                machine.encounter_named_reference(id);
            }
        }
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

pub fn extract_dependencies_python(source_code: &str) -> Vec<Vec<ContextPath>> {
    let ast = ast::Suite::parse(source_code, "<embedded>").unwrap();
    let mut machine = ASTWalkContext::default();
    traverse_statements(&ast, &mut machine);
    machine.context_stack_references
}

fn traverse_comprehension(comp: &ast::Comprehension, machine: &mut ASTWalkContext) {
    traverse_expression(&comp.target, machine);
    traverse_expression(&comp.iter, machine);
    for condition in &comp.ifs {
        traverse_expression(condition, machine);
    }
}

pub fn traverse_statements(statements: &[ast::Stmt], machine: &mut ASTWalkContext) {
    for stmt in statements {
        match stmt {
            ast::Stmt::Assign(ast::StmtAssign { targets, .. }) => {
                for target in targets {
                    if let ast::Expr::Name(ast::ExprName { id, .. }) = &target {
                        machine.encounter_assignment(id);
                    }
                }
            }
            ast::Stmt::FunctionDef(ast::StmtFunctionDef {
                body,
                decorator_list,
                name,
                args,
                ..
            }) => {
                // TODO: this does include typeparams
                machine.enter_statement_function(name);
                for (i, decorator) in decorator_list.iter().enumerate() {
                    machine.enter_decorator_expression(&i);
                    traverse_expression(decorator, machine);
                    machine.pop();
                }
                for ast::ArgWithDefault {
                    range,
                    def,
                    default,
                    ..
                } in &args.args
                {
                    if let ast::Arg { arg, .. } = def {
                        machine.locals.insert(arg.to_string());
                    }
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
                // TODO: here
                dbg!(target, annotation, value);
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

fn build_dependency_graph(context_paths: Vec<ContextPath>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    #[test]
    fn test_extraction_of_ch_statements() {
        let python_source = indoc! { r#"
            from chidori.core import ch
            
            ch.prompt.configure("default", ch.llm(model="openai"))
            "#};
        let ast = ast::Suite::parse(python_source, "<embedded>").unwrap();
        // dbg!(&ast);

        // TODO: functiondef -> decorator_list -> call

        // TODO: for new we can assume that we only care about modules directly provided

        // TODO: decorators
        // TODO: we want to extract the decorator attribute, and the arguments to the decorator

        // TODO: accessors
        // TODO: we want to extract .get or .set and the path and value referred

        let mut machine = ASTWalkContext::default();

        traverse_statements(&ast, &mut machine);
        assert_eq!(
            machine.context_stack_references,
            vec![
                vec![
                    ContextPath::InCallExpression,
                    ContextPath::InCallExpression,
                    ContextPath::Attribute(String::from("llm")),
                    ContextPath::ChName
                ],
                vec![
                    ContextPath::InCallExpression,
                    ContextPath::Attribute(String::from("configure")),
                    ContextPath::Attribute(String::from("prompt")),
                    ContextPath::ChName
                ],
            ]
        );
    }

    #[test]
    fn test_assignment_to_value() {
        let python_source = indoc! { r#"
            x = 1
            "#};
        let ast = ast::Suite::parse(python_source, "<embedded>").unwrap();
        let mut machine = ASTWalkContext::default();

        traverse_statements(&ast, &mut machine);
        assert_eq!(
            machine.context_stack_references,
            vec![vec![ContextPath::VariableAssignment(String::from("x")),]]
        );
    }

    #[test]
    fn test_nothing_extracted_with_no_ch_references() {
        let python_source = indoc! { r#"
            def create_dockerfile():
                return prompt("prompts/create_dockerfile")
            "#};
        let ast = ast::Suite::parse(python_source, "<embedded>").unwrap();
        let mut machine = ASTWalkContext::default();

        traverse_statements(&ast, &mut machine);
        assert_eq!(
            machine.context_stack_references,
            vec![vec![
                ContextPath::InFunction(String::from("create_dockerfile")),
                ContextPath::InCallExpression,
                ContextPath::IdentifierReferredTo(String::from("prompt"), false),
            ],]
        );
    }

    #[test]
    fn test_function_decorator_ch_annotation() {
        let python_source = indoc! { r#"
            @ch.register()
            def migration_agent():
                ch.set("bar", 1)
            "#};
        let ast = ast::Suite::parse(python_source, "<embedded>").unwrap();
        let mut machine = ASTWalkContext::default();

        traverse_statements(&ast, &mut machine);
        assert_eq!(
            machine.context_stack_references,
            vec![
                vec![
                    ContextPath::InFunction(String::from("migration_agent")),
                    ContextPath::InFunctionDecorator(0),
                    ContextPath::InCallExpression,
                    ContextPath::Attribute(String::from("register")),
                    ContextPath::ChName
                ],
                vec![
                    ContextPath::InFunction(String::from("migration_agent")),
                    ContextPath::InCallExpression,
                    ContextPath::Attribute(String::from("set")),
                    ContextPath::ChName
                ]
            ]
        );
    }

    #[test]
    fn test_function_decorator_ch_annotation_with_internal_ch_and_emit() {
        let python_source = indoc! { r#"
            @ch.on_event("new_file")
            @ch.emit_as("file_created")
            def dispatch_agent(ev):
                ch.set("file_path", ev.file_path)
            "#};
        let ast = ast::Suite::parse(python_source, "<embedded>").unwrap();
        let mut machine = ASTWalkContext::default();

        traverse_statements(&ast, &mut machine);
        assert_eq!(
            machine.context_stack_references,
            vec![
                vec![
                    ContextPath::InFunction(String::from("dispatch_agent")),
                    ContextPath::InFunctionDecorator(0),
                    ContextPath::InCallExpression,
                    ContextPath::Attribute(String::from("on_event")),
                    ContextPath::ChName
                ],
                vec![
                    ContextPath::InFunction(String::from("dispatch_agent")),
                    ContextPath::InFunctionDecorator(1),
                    ContextPath::InCallExpression,
                    ContextPath::Attribute(String::from("emit_as")),
                    ContextPath::ChName
                ],
                vec![
                    ContextPath::InFunction(String::from("dispatch_agent")),
                    ContextPath::InCallExpression,
                    ContextPath::Attribute(String::from("file_path")),
                    ContextPath::IdentifierReferredTo(String::from("ev"), true)
                ],
                vec![
                    ContextPath::InFunction(String::from("dispatch_agent")),
                    ContextPath::InCallExpression,
                    ContextPath::Attribute(String::from("set")),
                    ContextPath::ChName
                ],
            ]
        );
    }

    #[test]
    fn test_ch_reference_internal_to_function() {
        let python_source = indoc! { r#"
            def evaluate_agent(ev):
                ch.set("file_path", ev.file_path)
                migration_agent()
            "#};
        let ast = ast::Suite::parse(python_source, "<embedded>").unwrap();
        let mut machine = ASTWalkContext::default();

        traverse_statements(&ast, &mut machine);
        assert_eq!(
            machine.context_stack_references,
            vec![
                vec![
                    ContextPath::InFunction(String::from("evaluate_agent")),
                    ContextPath::InCallExpression,
                    ContextPath::Attribute(String::from("file_path")),
                    ContextPath::IdentifierReferredTo(String::from("ev"), true),
                ],
                vec![
                    ContextPath::InFunction(String::from("evaluate_agent")),
                    ContextPath::InCallExpression,
                    ContextPath::Attribute(String::from("set")),
                    ContextPath::ChName
                ],
                vec![
                    ContextPath::InFunction(String::from("evaluate_agent")),
                    ContextPath::InCallExpression,
                    ContextPath::IdentifierReferredTo(String::from("migration_agent"), false),
                ]
            ]
        );
    }

    #[test]
    fn test_ch_function_decoration_referring_to_another_function() {
        let python_source = indoc! { r#"
            @ch.p(create_dockerfile)
            def setup_pipeline(x):
                return x
            "#};
        let ast = ast::Suite::parse(python_source, "<embedded>").unwrap();
        let mut machine = ASTWalkContext::default();

        traverse_statements(&ast, &mut machine);
        assert_eq!(
            machine.context_stack_references,
            vec![
                vec![
                    ContextPath::InFunction(String::from("setup_pipeline")),
                    ContextPath::InFunctionDecorator(0),
                    ContextPath::InCallExpression,
                    ContextPath::IdentifierReferredTo(String::from("create_dockerfile"), false),
                ],
                vec![
                    ContextPath::InFunction(String::from("setup_pipeline")),
                    ContextPath::InFunctionDecorator(0),
                    ContextPath::InCallExpression,
                    ContextPath::Attribute(String::from("p")),
                    ContextPath::ChName
                ],
                vec![
                    ContextPath::InFunction(String::from("setup_pipeline")),
                    ContextPath::IdentifierReferredTo(String::from("x"), true),
                ]
            ]
        );
    }

    #[test]
    fn test_pipe_function_composition() {
        let python_source = indoc! { r#"
            def main():
                bar() | foo() | baz()
            "#};
        let ast = ast::Suite::parse(python_source, "<embedded>").unwrap();
        let mut machine = ASTWalkContext::default();

        traverse_statements(&ast, &mut machine);
        assert_eq!(
            machine.context_stack_references,
            vec![
                vec![
                    ContextPath::InFunction(String::from("main")),
                    ContextPath::InCallExpression,
                    ContextPath::IdentifierReferredTo(String::from("bar"), false),
                ],
                vec![
                    ContextPath::InFunction(String::from("main")),
                    ContextPath::InCallExpression,
                    ContextPath::IdentifierReferredTo(String::from("foo"), false),
                ],
                vec![
                    ContextPath::InFunction(String::from("main")),
                    ContextPath::InCallExpression,
                    ContextPath::IdentifierReferredTo(String::from("baz"), false),
                ],
            ]
        );
    }
}
