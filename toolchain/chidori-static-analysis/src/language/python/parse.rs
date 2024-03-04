use crate::language::{Report, ReportItem, ReportTriggerableFunctions};
use rustpython_parser::ast::{Constant, Expr, Identifier, Stmt};
use rustpython_parser::{ast, Parse};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::collections::HashSet;

// TODO: move to using the Ruff library here to break about functions into independent snippets

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

    fn enter_statement_function(&mut self, name: &Identifier) -> usize {
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
            let idx = machine.enter_call_expression();
            // TODO: call expressions need to extract the identifier of the function being invoked
            let ast::ExprCall {
                func,
                args,
                keywords,
                ..
            } = expr;
            // TODO: needs to be contained within the metadata of the expression
            for arg in args {
                traverse_expression(arg, machine);
            }
            for keyword in keywords {
                traverse_expression(&keyword.value, machine);
            }
            traverse_expression(func, machine);
            machine.pop_until(idx);
        }
        ast::Expr::FormattedValue(ast::ExprFormattedValue { value, .. }) => {
            // TODO: this is a string interpolation and we need to handle internal references
            traverse_expression(value, machine);
        }
        ast::Expr::JoinedStr(ast::ExprJoinedStr { values, .. }) => {
            for value in values {
                traverse_expression(value, machine);
            }
        }
        ast::Expr::Constant(ast::ExprConstant { value, .. }) => {
            machine.encounter_constant(value);
        }
        ast::Expr::Attribute(ast::ExprAttribute { value, attr, .. }) => {
            let x = machine.enter_attr(attr);
            traverse_expression(value, machine);
            machine.pop_until(x);
        }
        ast::Expr::Subscript(ast::ExprSubscript { value, slice, .. }) => {
            traverse_expression(value, machine);
            traverse_expression(slice, machine);
        }
        ast::Expr::Starred(ast::ExprStarred { value, .. }) => {
            traverse_expression(value, machine);
        }
        ast::Expr::Name(ast::ExprName { id, .. }) => {
            machine.encounter_named_reference(id);
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
            // targets has multiple because you can multi assign
            ast::Stmt::Assign(ast::StmtAssign { targets, value, .. }) => {
                let idx = machine.enter_assignment_to_statement();
                for target in targets {
                    traverse_expression(target, machine);
                }
                machine.pop_until(idx);
                let idx = machine.enter_assignment_from_statement();
                traverse_expression(value, machine);
                machine.pop_until(idx);
            }
            ast::Stmt::FunctionDef(ast::StmtFunctionDef {
                body,
                decorator_list,
                name,
                args,
                ..
            }) => {
                machine.globals.insert(name.to_string());
                // TODO: this does include typeparams
                machine.new_local_context();
                let idx = machine.enter_statement_function(name);
                for (i, decorator) in decorator_list.iter().enumerate() {
                    let idx = machine.enter_decorator_expression(&i);
                    traverse_expression(decorator, machine);
                    machine.pop_until(idx);
                }
                // TODO: make this less uniquely handled in the traversal
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
                machine.pop_until(idx);
                machine.pop_local_context();
            }

            ast::Stmt::AsyncFunctionDef(ast::StmtAsyncFunctionDef {
                body,
                decorator_list,
                name,
                args,
                ..
            }) => {
                machine.globals.insert(name.to_string());
                machine.new_local_context();
                let idx = machine.enter_statement_function(name);
                for (i, decorator) in decorator_list.iter().enumerate() {
                    let idx = machine.enter_decorator_expression(&i);
                    traverse_expression(decorator, machine);
                    machine.pop_until(idx);
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
                machine.pop_until(idx);
                machine.pop_local_context();
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
            ast::Stmt::Import(ast::StmtImport { names, .. }) => {
                for name in names {
                    if let Some(name) = &name.asname {
                        machine.globals.insert(name.to_string());
                    } else {
                        machine.globals.insert(name.name.to_string());
                    }
                }
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
    let mut declared_functions = HashMap::new();
    for context_path in context_paths {
        let mut encountered = vec![];
        for (idx, context_path_unit) in context_path.iter().enumerate() {
            encountered.push(context_path_unit);

            // If we've declared a top level function, it is exposed
            if let ContextPath::InFunction(name) = context_path_unit {
                if !triggerable_functions.contains_key(name) {
                    triggerable_functions
                        .entry(name.clone())
                        .or_insert_with(|| ReportTriggerableFunctions {
                            // context_path: context_path.clone(),
                            emit_event: vec![],
                            trigger_on: vec![],
                        });
                }
            }

            // Decorators set the emit event property for a function
            if &ContextPath::IdentifierReferredTo(String::from("ch"), false) == context_path_unit {
                if ContextPath::Attribute(String::from("emit_as")) == context_path[idx - 1] {
                    if let ContextPath::Constant(const_name) = &context_path[idx - 2] {
                        if let ContextPath::InFunctionDecorator(_) = context_path[idx - 4] {
                            if let ContextPath::InFunction(name) = &context_path[idx - 5] {
                                let mut x = triggerable_functions
                                    .entry(name.clone())
                                    .or_insert_with(|| ReportTriggerableFunctions {
                                        emit_event: vec![], // Initialize with an empty string or a default value
                                        trigger_on: vec![],
                                    });
                                x.emit_event.push(const_name.clone());
                            }
                        }
                    }
                }
            }

            // Decorators set the emit event property for a function
            if &ContextPath::IdentifierReferredTo(String::from("ch"), false) == context_path_unit {
                if ContextPath::Attribute(String::from("on_event")) == context_path[idx - 1] {
                    if let ContextPath::Constant(const_name) = &context_path[idx - 2] {
                        if let ContextPath::InFunctionDecorator(_) = context_path[idx - 4] {
                            if let ContextPath::InFunction(name) = &context_path[idx - 5] {
                                let mut x = triggerable_functions
                                    .entry(name.clone())
                                    .or_insert_with(|| ReportTriggerableFunctions {
                                        emit_event: vec![], // Initialize with an empty string or a default value
                                        trigger_on: vec![],
                                    });
                                x.trigger_on.push(const_name.clone());
                            }
                        }
                    }
                }
            }

            if let ContextPath::IdentifierReferredTo(identifier, false) = context_path_unit {
                if identifier != &String::from("ch") {
                    // If this value is not being assigned to, then it is a dependency
                    if !encountered.contains(&&ContextPath::AssignmentToStatement) {
                        depended_values.insert(
                            identifier.clone(),
                            ReportItem {
                                // context_path: context_path.clone(),
                            },
                        );
                    } else {
                        // This is an exposed value if it does not occur inside the scope of a function
                        if encountered
                            .iter()
                            .find(|x| {
                                if let ContextPath::InFunction(_) = x {
                                    true
                                } else {
                                    false
                                }
                            })
                            .is_none()
                        {
                            exposed_values.insert(
                                identifier.clone(),
                                ReportItem {
                                    // context_path: context_path.clone(),
                                },
                            );
                        }
                    }
                }
            }
        }
    }

    Report {
        cell_exposed_values: exposed_values,
        cell_depended_values: depended_values,
        triggerable_functions: triggerable_functions,
        declared_functions: declared_functions,
    }
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
            "#};
        let context_stack_references = extract_dependencies_python(python_source);
        assert_eq!(
            context_stack_references,
            vec![
                vec![
                    ContextPath::InCallExpression,
                    ContextPath::Constant(String::from("default")),
                ],
                vec![
                    ContextPath::InCallExpression,
                    ContextPath::Constant(String::from("default")),
                    ContextPath::InCallExpression,
                    ContextPath::Constant(String::from("openai")),
                ],
                vec![
                    ContextPath::InCallExpression,
                    ContextPath::Constant(String::from("default")),
                    ContextPath::InCallExpression,
                    ContextPath::Constant(String::from("openai")),
                    ContextPath::Attribute(String::from("llm")),
                    ContextPath::IdentifierReferredTo(String::from("ch"), false),
                ],
                vec![
                    ContextPath::InCallExpression,
                    ContextPath::Constant(String::from("default")),
                    ContextPath::Attribute(String::from("configure")),
                    ContextPath::Attribute(String::from("prompt")),
                    ContextPath::IdentifierReferredTo(String::from("ch"), false),
                ],
            ]
        );
    }

    #[test]
    fn test_assignment_to_value() {
        let python_source = indoc! { r#"
            x = 1
            "#};
        let context_stack_references = extract_dependencies_python(python_source);
        // TODO: fix
        // assert_eq!(
        //     context_stack_references,
        //     vec![vec![ContextPath::VariableAssignment(String::from("x")),]]
        // );
    }

    #[test]
    fn test_nothing_extracted_with_no_ch_references() {
        let python_source = indoc! { r#"
            def create_dockerfile():
                return prompt("prompts/create_dockerfile")
            "#};
        let context_stack_references = extract_dependencies_python(python_source);
        assert_eq!(
            context_stack_references,
            vec![
                vec![ContextPath::InFunction(String::from("create_dockerfile")),],
                vec![
                    ContextPath::InFunction(String::from("create_dockerfile")),
                    ContextPath::InCallExpression,
                    ContextPath::Constant(String::from("prompts/create_dockerfile")),
                ],
                vec![
                    ContextPath::InFunction(String::from("create_dockerfile")),
                    ContextPath::InCallExpression,
                    ContextPath::Constant(String::from("prompts/create_dockerfile")),
                    ContextPath::IdentifierReferredTo(String::from("prompt"), false),
                ],
            ]
        );
    }

    #[test]
    fn test_function_decorator_ch_annotation() {
        let python_source = indoc! { r#"
            @ch.register()
            def migration_agent():
                ch.set("bar", 1)
            "#};
        let context_stack_references = extract_dependencies_python(python_source);
        assert_eq!(
            context_stack_references,
            vec![
                vec![ContextPath::InFunction(String::from("migration_agent")),],
                vec![
                    ContextPath::InFunction(String::from("migration_agent")),
                    ContextPath::InFunctionDecorator(0),
                    ContextPath::InCallExpression,
                    ContextPath::Attribute(String::from("register")),
                    ContextPath::IdentifierReferredTo(String::from("ch"), false),
                ],
                vec![
                    ContextPath::InFunction(String::from("migration_agent")),
                    ContextPath::InCallExpression,
                    ContextPath::Constant(String::from("bar")),
                ],
                vec![
                    ContextPath::InFunction(String::from("migration_agent")),
                    ContextPath::InCallExpression,
                    ContextPath::Constant(String::from("bar")),
                    ContextPath::Attribute(String::from("set")),
                    ContextPath::IdentifierReferredTo(String::from("ch"), false),
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
        let context_stack_references = extract_dependencies_python(python_source);
        assert_eq!(
            context_stack_references,
            vec![
                vec![ContextPath::InFunction("dispatch_agent".to_string())],
                vec![
                    ContextPath::InFunction("dispatch_agent".to_string()),
                    ContextPath::InFunctionDecorator(0),
                    ContextPath::InCallExpression,
                    ContextPath::Constant("new_file".to_string())
                ],
                vec![
                    ContextPath::InFunction("dispatch_agent".to_string()),
                    ContextPath::InFunctionDecorator(0),
                    ContextPath::InCallExpression,
                    ContextPath::Constant("new_file".to_string()),
                    ContextPath::Attribute("on_event".to_string()),
                    ContextPath::IdentifierReferredTo("ch".to_string(), false)
                ],
                vec![
                    ContextPath::InFunction("dispatch_agent".to_string()),
                    ContextPath::InFunctionDecorator(1),
                    ContextPath::InCallExpression,
                    ContextPath::Constant("file_created".to_string())
                ],
                vec![
                    ContextPath::InFunction("dispatch_agent".to_string()),
                    ContextPath::InFunctionDecorator(1),
                    ContextPath::InCallExpression,
                    ContextPath::Constant("file_created".to_string()),
                    ContextPath::Attribute("emit_as".to_string()),
                    ContextPath::IdentifierReferredTo("ch".to_string(), false)
                ],
                vec![
                    ContextPath::InFunction("dispatch_agent".to_string()),
                    ContextPath::InCallExpression,
                    ContextPath::Constant("file_path".to_string())
                ],
                vec![
                    ContextPath::InFunction("dispatch_agent".to_string()),
                    ContextPath::InCallExpression,
                    ContextPath::Constant("file_path".to_string()),
                    ContextPath::Attribute("file_path".to_string()),
                    ContextPath::IdentifierReferredTo("ev".to_string(), true)
                ],
                vec![
                    ContextPath::InFunction("dispatch_agent".to_string()),
                    ContextPath::InCallExpression,
                    ContextPath::Constant("file_path".to_string()),
                    ContextPath::Attribute("set".to_string()),
                    ContextPath::IdentifierReferredTo("ch".to_string(), false)
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
        let context_stack_references = extract_dependencies_python(python_source);
        assert_eq!(
            context_stack_references,
            vec![
                vec![ContextPath::InFunction(String::from("evaluate_agent")),],
                vec![
                    ContextPath::InFunction(String::from("evaluate_agent")),
                    ContextPath::InCallExpression,
                    ContextPath::Constant(String::from("file_path")),
                ],
                vec![
                    ContextPath::InFunction(String::from("evaluate_agent")),
                    ContextPath::InCallExpression,
                    ContextPath::Constant(String::from("file_path")),
                    ContextPath::Attribute(String::from("file_path")),
                    ContextPath::IdentifierReferredTo(String::from("ev"), true),
                ],
                vec![
                    ContextPath::InFunction(String::from("evaluate_agent")),
                    ContextPath::InCallExpression,
                    ContextPath::Constant(String::from("file_path")),
                    ContextPath::Attribute(String::from("set")),
                    ContextPath::IdentifierReferredTo(String::from("ch"), false),
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
        let context_stack_references = extract_dependencies_python(python_source);
        assert_eq!(
            context_stack_references,
            vec![
                vec![ContextPath::InFunction(String::from("setup_pipeline")),],
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
                    ContextPath::IdentifierReferredTo(String::from("ch"), false),
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
        let context_stack_references = extract_dependencies_python(python_source);
        assert_eq!(
            context_stack_references,
            vec![
                vec![ContextPath::InFunction(String::from("main")),],
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

    #[test]
    fn test_report_generation() {
        let python_source = indoc! { r#"
        @ch.on_event("new_file")
        @ch.emit_as("file_created")
        def testing():
            x = 2 + y
            return x
            "#};
        let context_stack_references = extract_dependencies_python(python_source);
        let result = build_report(&context_stack_references);
        let report = Report {
            cell_exposed_values: std::collections::HashMap::new(), // No data provided, initializing as empty
            cell_depended_values: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "y".to_string(),
                    ReportItem {
                        // context_path: vec![
                        //     ContextPath::InFunction("testing".to_string()),
                        //     ContextPath::AssignmentFromStatement,
                        //     ContextPath::IdentifierReferredTo("y".to_string(), false),
                        // ],
                    },
                );
                map
            },
            triggerable_functions: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "testing".to_string(),
                    ReportTriggerableFunctions {
                        // context_path: vec![ContextPath::InFunction("testing".to_string())],
                        emit_event: vec!["file_created".to_string()],
                        trigger_on: vec!["new_file".to_string()],
                    },
                );
                map
            },
            declared_functions: std::collections::HashMap::new(), // No data provided, initializing as empty
        };

        assert_eq!(result, report);
    }

    #[test]
    fn test_report_generation_with_import() {
        let python_source = indoc! { r#"
import random

def fun_name():
    w = function_that_doesnt_exist()
    v = 5
    return v

x = random.randint(0, 10)            
"#};
        let context_stack_references = extract_dependencies_python(python_source);
        let result = build_report(&context_stack_references);
        let report = Report {
            cell_exposed_values: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "x".to_string(),
                    ReportItem {
                        // context_path: vec![
                        //     ContextPath::AssignmentToStatement,
                        //     ContextPath::IdentifierReferredTo("x".to_string(), false),
                        // ],
                    },
                );
                map
            },
            cell_depended_values: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "function_that_doesnt_exist".to_string(),
                    ReportItem {
                        // context_path: vec![
                        //     ContextPath::InFunction("fun_name".to_string()),
                        //     ContextPath::AssignmentFromStatement,
                        //     ContextPath::InCallExpression,
                        //     ContextPath::IdentifierReferredTo(
                        //         "function_that_doesnt_exist".to_string(),
                        //         false,
                        //     ),
                        // ],
                    },
                );
                map
            },
            triggerable_functions: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "fun_name".to_string(),
                    ReportTriggerableFunctions {
                        // context_path: vec![ContextPath::InFunction("fun_name".to_string())],
                        emit_event: vec![],
                        trigger_on: vec![],
                    },
                );
                map
            },
            declared_functions: std::collections::HashMap::new(), // No data provided, initializing as empty
        };
        assert_eq!(result, report);
    }
}
