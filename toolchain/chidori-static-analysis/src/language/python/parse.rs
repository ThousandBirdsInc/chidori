use crate::language::{ChidoriStaticAnalysisError, InternalCallGraph, Report, ReportItem, ReportTriggerableFunctions, TextRange};
use rustpython_parser::ast::{Constant, Expr, Identifier, Stmt};
use rustpython_parser::{ast, Parse};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::collections::HashSet;
use crate::language::ContextPath;



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

    fn enter_statement_class(&mut self, name: &Identifier) -> usize {
        self.context_stack
            .push(ContextPath::InClass(name.to_string()));
        self.context_stack_references
            .push(self.context_stack.clone());
        self.context_stack.len()
    }

    fn enter_statement_function(&mut self, name: &Identifier, text_range: TextRange) -> usize {
        self.context_stack
            .push(ContextPath::InFunction(name.to_string(), text_range));
        self.context_stack_references
            .push(self.context_stack.clone());
        self.context_stack.len()
    }

    fn enter_arguments(&mut self) -> usize {
        self.context_stack
            .push(ContextPath::FunctionArguments);
        self.context_stack.len()
    }

    fn encounter_argument(&mut self, name: &Identifier) {
        self.locals.insert(name.to_string());
        self.context_stack
            .push(ContextPath::FunctionArgument(name.to_string()));
        self.context_stack_references
            .push(self.context_stack.clone());
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
                .push(ContextPath::IdentifierReferredTo{
                    name: name.to_string(),
                    in_scope: true,
                    exposed: false
                });
        } else {
            // TODO: this might not be last due to pattern assignments
            if let Some(ContextPath::AssignmentToStatement) = self.context_stack.last() {
                self.locals.insert(name.to_string());
            }
            if let Some(ContextPath::FunctionArguments) = self.context_stack.last() {
                self.locals.insert(name.to_string());
            }
            self.context_stack
                .push(ContextPath::IdentifierReferredTo{
                    name: name.to_string(),
                    in_scope: false,
                    exposed: false
                });
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

fn extract_python_comments(code: &str) -> Vec<String> {
    let mut comments = Vec::new();
    let mut current_block = Vec::new();
    let lines = code.lines();

    for line in lines {
        let trimmed_line = line.trim();
        if let Some(pos) = trimmed_line.find('#') {
            let is_start_of_line = pos == 0;
            let comment = trimmed_line[pos..].trim_start_matches('#').trim().to_string();

            if !comment.is_empty() {
                if is_start_of_line {
                    current_block.push(comment);
                } else {
                    if !current_block.is_empty() {
                        comments.push(current_block.join("\n"));
                        current_block.clear();
                    }
                    comments.push(comment);
                }
            }
        } else {
            if !current_block.is_empty() {
                comments.push(current_block.join("\n"));
                current_block.clear();
            }
        }
    }

    // Add any remaining comments that may be in a block at the end of the code
    if !current_block.is_empty() {
        comments.push(current_block.join("\n"));
    }

    comments
}


pub fn extract_dependencies_python(source_code: &str) -> Result<Vec<Vec<ContextPath>>, ChidoriStaticAnalysisError> {
    // TODO: extract comments and associate them based on position relative to functions
    let mut comments = extract_python_comments(source_code);
    let ast = ast::Suite::parse(source_code, "<embedded>")
        .map_err(|e| {
            ChidoriStaticAnalysisError::ParseError {
                msg: e.error.to_string(),
                offset: e.offset.to_u32(),
                source_path: e.source_path,
                source_code: source_code.to_string(),
            }
        })?;
    let mut machine = ASTWalkContext::default();
    traverse_statements(&ast, &mut machine);
    Ok(machine.context_stack_references)
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
                range,
                ..
            }) => {
                machine.globals.insert(name.to_string());
                // TODO: this does include typeparams
                machine.new_local_context();
                let idx = machine.enter_statement_function(name, TextRange {
                    start: range.start().to_usize(),
                    end: range.end().to_usize()
                });
                for (i, decorator) in decorator_list.iter().enumerate() {
                    let idx = machine.enter_decorator_expression(&i);
                    traverse_expression(decorator, machine);
                    machine.pop_until(idx);
                }
                let args_idx = machine.enter_arguments();
                for ast::ArgWithDefault {
                    range,
                    def,
                    default,
                    ..
                } in &args.args
                {
                    if let ast::Arg { arg, .. } = def {
                        machine.encounter_named_reference(arg);
                    }
                }
                machine.pop_until(args_idx);
                traverse_statements(body, machine);
                machine.pop_until(idx);
                machine.pop_local_context();
            }

            ast::Stmt::AsyncFunctionDef(ast::StmtAsyncFunctionDef {
                body,
                decorator_list,
                name,
                args,
                range,
                ..
            }) => {
                machine.globals.insert(name.to_string());
                machine.new_local_context();
                let idx = machine.enter_statement_function(name, TextRange {
                    start: range.start().to_usize(),
                    end: range.end().to_usize()
                });
                for (i, decorator) in decorator_list.iter().enumerate() {
                    let idx = machine.enter_decorator_expression(&i);
                    traverse_expression(decorator, machine);
                    machine.pop_until(idx);
                }
                let args_idx = machine.enter_arguments();
                for ast::ArgWithDefault {
                    range,
                    def,
                    default,
                    ..
                } in &args.args
                {
                    if let ast::Arg { arg, .. } = def {
                        machine.encounter_named_reference(arg);
                    }
                }
                machine.pop_until(args_idx);
                traverse_statements(body, machine);
                machine.pop_until(idx);
                machine.pop_local_context();
            }
            ast::Stmt::ClassDef(ast::StmtClassDef { name, body, .. }) => {
                machine.globals.insert(name.to_string());
                let idx = machine.enter_statement_class(name);
                // TODO: this does include typeparams
                traverse_statements(body, machine);
                machine.pop_until(idx);
                machine.pop_local_context();
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
                let idx = machine.enter_assignment_to_statement();
                traverse_expression(target, machine);
                machine.pop_until(idx);
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
                let idx = machine.enter_assignment_to_statement();
                traverse_expression(target, machine);
                machine.pop_until(idx);
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

pub fn build_report(context_paths: &Vec<Vec<ContextPath>>) -> Report {
    // TODO: triggerable functions should note what they are triggered by
    // TODO: for each of these we should store the context path that refers to them
    // TODO: context paths should include spans
    // TODO: for each of these we should store their type if its available
    let mut exposed_values = HashMap::new();
    let mut depended_values = HashMap::new();
    let mut triggerable_functions = HashMap::new();
    for context_path in context_paths {
        let mut encountered = vec![];
        for (idx, context_path_unit) in context_path.iter().enumerate() {
            // encountered is the reversed order of the context path
            encountered.push(context_path_unit);

            // If we've declared a top level function, it is exposed
            if let ContextPath::InFunction(name, _) = context_path_unit {
                if !triggerable_functions.contains_key(name) {
                    triggerable_functions
                        .entry(name.clone())
                        .or_insert_with(|| ReportTriggerableFunctions {
                            
                            // context_path: context_path.clone(),
                            arguments: vec![],
                            emit_event: vec![],
                            trigger_on: vec![],
                        });
                }
            }

            // Function arguments get assigned to the triggerable function
            if let ContextPath::FunctionArgument(name) = context_path_unit {
                // traverse back through path until we hit the InFunction
                let clone_path = context_path.clone();
                for (idx, context_path_unit) in (clone_path.into_iter()).rev().enumerate() {
                    if let ContextPath::InFunction(function_name, _) = context_path_unit {
                        let mut x = triggerable_functions
                            .entry(function_name.clone())
                            .or_insert_with(|| ReportTriggerableFunctions {
                                arguments: vec![],
                                emit_event: vec![], // Initialize with an empty string or a default value
                                trigger_on: vec![],
                            });
                        x.arguments.push(name.clone());
                    }
                }
            }

            // If an identifier is referred to, and it has not been assigned to earlier during our interpreting
            if let ContextPath::IdentifierReferredTo{name: identifier, exposed: false, in_scope: false} = context_path_unit {
                // If we encounter both FunctionArguments and InFunction, then this is a function argument
                if encountered.iter().any(|x| matches!(x, ContextPath::InFunction(_, _)))
                    && encountered.iter().any(|x| matches!(x, ContextPath::FunctionArguments))
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

                // If this value is not being assigned to, then it is a dependency
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


    let py_built_ins: HashSet<&str> = [
        "__name__", "type", "abs", "all", "any", "ascii", "bin", "bool", "breakpoint", "bytearray",
        "bytes", "callable", "chr", "classmethod", "compile", "complex", "delattr", "dict", "dir",
        "divmod", "enumerate", "eval", "exec", "filter", "float", "format", "frozenset", "getattr",
        "globals", "hasattr", "hash", "help", "hex", "id", "input", "int", "isinstance", "issubclass",
        "iter", "len", "list", "locals", "map", "max", "memoryview", "min", "next", "object", "oct",
        "open", "ord", "pow", "print", "property", "range", "repr", "reversed", "round", "set", "setattr",
        "slice", "sorted", "staticmethod", "str", "sum", "super", "tuple", "type", "vars", "zip"
    ].iter().cloned().collect();

    depended_values.retain(|value,_ | !py_built_ins.contains(value.as_str()));

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
    use indoc::indoc;

    #[test]
    fn test_extraction_of_ch_statements() {
        let python_source = indoc! { r#"
            from chidori.core import ch
            
            ch.prompt.configure("default", ch.llm(model="openai"))
            "#};
        let context_stack_references = extract_dependencies_python(python_source).unwrap();
        insta::with_settings!({
            description => python_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(context_stack_references);
        });
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
        let context_stack_references = extract_dependencies_python(python_source).unwrap();
        insta::with_settings!({
            description => python_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(context_stack_references);
        });
    }

    #[test]
    fn test_function_decorator_ch_annotation() {
        let python_source = indoc! { r#"
            @ch.register()
            def migration_agent():
                ch.set("bar", 1)
            "#};
        let context_stack_references = extract_dependencies_python(python_source).unwrap();
        insta::with_settings!({
            description => python_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(context_stack_references);
        });
    }

    #[test]
    fn test_function_decorator_ch_annotation_with_internal_ch_and_emit() {
        let python_source = indoc! { r#"
            @ch.on_event("new_file")
            @ch.emit_as("file_created")
            def dispatch_agent(ev):
                ch.set("file_path", ev.file_path)
            "#};
        let context_stack_references = extract_dependencies_python(python_source).unwrap();
        insta::with_settings!({
            description => python_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(context_stack_references);
        });
    }

    #[test]
    fn test_ch_reference_internal_to_function() {
        let python_source = indoc! { r#"
            def evaluate_agent(ev):
                ch.set("file_path", ev.file_path)
                migration_agent()
            "#};
        let context_stack_references = extract_dependencies_python(python_source).unwrap();
        insta::with_settings!({
            description => python_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(context_stack_references);
        });
    }

    #[test]
    fn test_ch_function_decoration_referring_to_another_function() {
        let python_source = indoc! { r#"
            @ch.p(create_dockerfile)
            def setup_pipeline(x):
                return x
            "#};
        let context_stack_references = extract_dependencies_python(python_source).unwrap();

        insta::assert_yaml_snapshot!(context_stack_references);

    }

    #[test]
    fn test_classes_are_identified() {
        let python_source = indoc! { r#"
            import unittest

            class TestMarshalledValues(unittest.TestCase):
                def test_addTwo(self):
                    self.assertEqual(addTwo(2), 4)

            unittest.TextTestRunner().run(unittest.TestLoader().loadTestsFromTestCase(TestMarshalledValues))

            "#};
        let context_stack_references = extract_dependencies_python(python_source).unwrap();

        insta::with_settings!({
            description => python_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(context_stack_references);
        });

    }



    #[test]
    fn test_for_loop_assignments_are_captured() {
        let python_source = indoc! { r#"
        async def run_prompt(number_of_states):
            out = ""
            for state in (await get_states_first_letters(num=number_of_states)).split('\n'):
                out += await first_letter(state)
            return "demo" + out
            "#};
        let context_stack_references = extract_dependencies_python(python_source).unwrap();
        insta::assert_yaml_snapshot!(context_stack_references);
    }

    #[test]
    fn test_pipe_function_composition() {
        let python_source = indoc! { r#"
            def main():
                bar() | foo() | baz()
            "#};
        let context_stack_references = extract_dependencies_python(python_source).unwrap();
        insta::with_settings!({
            description => python_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(context_stack_references);
        });
    }

    #[test]
    fn test_report_generation() {
        let python_source = indoc! { r#"
        def testing():
            x = 2 + y
            return x
            "#};
        let context_stack_references = extract_dependencies_python(python_source).unwrap();
        insta::with_settings!({
            description => python_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(context_stack_references);
        });
        let result = build_report(&context_stack_references);
        let report = Report {

            internal_call_graph: InternalCallGraph {
                graph: Default::default(),
            },
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

    #[test]
    fn test_report_generation_with_import() -> anyhow::Result<()> {
        let python_source = indoc! { r#"
import random

def fun_name():
    w = function_that_doesnt_exist()
    v = 5
    return v

x = random.randint(0, 10)            
"#};
        let context_stack_references = extract_dependencies_python(python_source).map_err(|e| anyhow::Error::msg(format!("{:?}", e)))?;
        insta::with_settings!({
            description => python_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(context_stack_references);
        });
        let result = build_report(&context_stack_references);
        let report = Report {

            internal_call_graph: InternalCallGraph {
                graph: Default::default(),
            },
            cell_exposed_values: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "x".to_string(),
                    ReportItem {
                    },
                );
                map
            },
            cell_depended_values: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "function_that_doesnt_exist".to_string(),
                    ReportItem {
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
                        arguments: vec![],
                        emit_event: vec![],
                        trigger_on: vec![],
                    },
                );
                map
            },
        };
        assert_eq!(result, report);
        Ok(())
    }

    #[test]
    fn test_report_generation_function_with_arguments() -> anyhow::Result<()>  {
        let python_source = indoc! { r#"
        async def complex_args_function(a, b, c=2, d=3):
            return a + b + c + d
            "#};
        let context_stack_references = extract_dependencies_python(python_source).map_err(|e| anyhow::Error::msg(format!("{:?}", e)))?;
        insta::with_settings!({
            description => python_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(context_stack_references);
        });
        let result = build_report(&context_stack_references);
        let report = Report {

            internal_call_graph: InternalCallGraph {
                graph: Default::default(),
            },
            cell_exposed_values: std::collections::HashMap::new(), // No data provided, initializing as empty
            cell_depended_values: std::collections::HashMap::new(),
            triggerable_functions: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "complex_args_function".to_string(),
                    ReportTriggerableFunctions {

                        // context_path: vec![ContextPath::InFunction("testing".to_string())],
                        arguments: vec!["a", "b", "c", "d"].into_iter().map(|a| a.to_string()).collect(),
                        emit_event: vec![],
                        trigger_on: vec![],
                    },
                );
                map
            },
        };

        assert_eq!(result, report);
        Ok(())
    }

    #[test]
    fn test_report_generation_for_loop_variable_assignment() -> anyhow::Result<()>  {
        let python_source = indoc! { r#"
        async def run_prompt(number_of_states):
            out = ""
            for state in (await get_states_first_letters(num=number_of_states)).split('\n'):
                out += await first_letter(state)
            return "demo" + out
            "#};
        let context_stack_references = extract_dependencies_python(python_source).map_err(|e| anyhow::Error::msg(format!("{:?}", e)))?;
        insta::with_settings!({
            sort_maps => true,
            description => python_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(context_stack_references);
        });
        let result = build_report(&context_stack_references);

        insta::with_settings!({
            sort_maps => true,
            description => python_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(result);
        });

        Ok(())
    }

    #[test]
    fn test_reference_to_undeclared_function() -> anyhow::Result<()> {
        let python_source = indoc! { r#"
            out = await read_file_and_load_to_memory("./")
            "#};
        let context_stack_references = extract_dependencies_python(python_source).map_err(|e| anyhow::Error::msg(format!("{:?}", e)))?;
        insta::with_settings!({
            description => python_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(context_stack_references);
        });
        let result = build_report(&context_stack_references);
        insta::with_settings!({
            description => python_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(result);
        });
        Ok(())
    }

    #[test]
    fn test_report_generation_with_class() -> anyhow::Result<()>  {
        let python_source = indoc! { r#"
            import unittest

            class TestMarshalledValues(unittest.TestCase):
                def test_addTwo(self):
                    self.assertEqual(addTwo(2), 4)

            unittest.TextTestRunner().run(unittest.TestLoader().loadTestsFromTestCase(TestMarshalledValues))
            "#};
        let context_stack_references = extract_dependencies_python(python_source).map_err(|e| anyhow::Error::msg(format!("{:?}", e)))?;
        insta::with_settings!({
            description => python_source,
            omit_expression => true
        }, {
            insta::assert_yaml_snapshot!(context_stack_references);
        });
        let result = build_report(&context_stack_references);
        let report = Report {

            internal_call_graph: InternalCallGraph {
                graph: Default::default(),
            },
            cell_exposed_values: std::collections::HashMap::new(), // No data provided, initializing as empty
            cell_depended_values: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "addTwo".to_string(),
                    ReportItem {
                    },
                );
                map
            },
            triggerable_functions: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "test_addTwo".to_string(),
                    ReportTriggerableFunctions {
                        arguments: vec!["self".to_string()],
                        emit_event: vec![],
                        trigger_on: vec![],
                    },
                );
                map
            },
        };

        assert_eq!(result, report);
        Ok(())
    }
}
