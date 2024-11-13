use ruff_python_ast::{Expr, Stmt, StmtClassDef, StmtFunctionDef, StmtWhile, StmtIf, StmtTry, StmtFor, ModModule, Arguments, ExprContext, StringLiteralValue, StringLiteral, Suite, ExprNumberLiteral, Int, ExceptHandler};
use std::collections::{HashMap, HashSet};
use ruff_python_ast::name::Name;
use ruff_python_parser::ParseError;
use ruff_text_size::TextRange;
use rustpython_parser::ast::Identifier;
use crate::ruff_python_codegen::{Generator, Stylist};


#[derive(Debug, Clone)]
struct StateVar {
    name: String,
    current_value: u32,
}



#[derive(Debug, Clone)]
struct TransformedFunction {
    name: String,
    parameters: Vec<String>,
    body: Vec<Stmt>,
    source_location: TextRange,
    state: StateVar,
}



#[derive(Debug)]
struct TransformContext {
    function_counter: usize,
    state_counter: u32,
    hoisted_functions: Vec<TransformedFunction>,
    state_variables: HashMap<String, StateVar>,
}



impl TransformContext {
    fn new() -> Self {
        Self {
            function_counter: 0,
            state_counter: 0,
            hoisted_functions: Vec::new(),
            state_variables: HashMap::new(),
        }
    }

    fn next_function_name(&mut self, prefix: &str) -> String {
        let name = format!("{}_{}", prefix, self.function_counter);
        self.function_counter += 1;
        name
    }

    fn create_state_var(&mut self) -> StateVar {
        let name = format!("_state_{}", self.state_counter);
        self.state_counter += 1;
        let state_var = StateVar {
            name,
            current_value: 0,
        };
        self.state_variables.insert(state_var.name.clone(), state_var.clone());
        state_var
    }

    fn next_state(&mut self, state_var: &mut StateVar) -> u32 {
        let next_val = state_var.current_value;
        state_var.current_value += 1;
        next_val
    }
}

#[derive(Debug)]
pub struct CodeTransformer {
    context: TransformContext,
}


impl CodeTransformer {
    pub fn new() -> Self {
        Self {
            context: TransformContext::new(),
        }
    }


    fn create_state_variable(&self, state_var: &StateVar, range: &TextRange) -> Stmt {
        Stmt::Assign(ruff_python_ast::StmtAssign {
            range: *range,
            targets: vec![
                Expr::Name(ruff_python_ast::ExprName {
                    range: *range,
                    id: Name::new(state_var.name.clone()),
                    ctx: ExprContext::Store,
                })
            ],
            value: Box::new(Expr::NumberLiteral(ruff_python_ast::ExprNumberLiteral {
                range: Default::default(),
                value: ruff_python_ast::Number::Int(0u32.into()),
            })),
        })
    }



    fn extract_parameters(&self, func: &StmtFunctionDef) -> Vec<String> {
        let mut params = Vec::new();

        // Handle positional-only parameters
        for arg in &func.parameters.posonlyargs {
            params.push(arg.parameter.name.to_string());
        }

        // Handle regular parameters
        for arg in &func.parameters.args {
            params.push(arg.parameter.name.to_string());
        }

        // Handle variadic positional parameter (*args)
        if let Some(vararg) = &func.parameters.vararg {
            params.push(vararg.name.to_string());
        }

        // Handle keyword-only parameters
        for arg in &func.parameters.kwonlyargs {
            params.push(arg.parameter.name.to_string());
        }

        // Handle variadic keyword parameter (**kwargs)
        if let Some(kwarg) = &func.parameters.kwarg {
            params.push(kwarg.name.to_string());
        }

        params
    }


    fn collect_functions(&mut self, module: &ModModule) {
        for stmt in &module.body {
            match stmt {
                Stmt::FunctionDef(func) => {
                    // Create new state variable for this function
                    let mut func_state = self.context.create_state_var();

                    self.context.hoisted_functions.push(TransformedFunction {
                        name: func.name.to_string(),
                        parameters: self.extract_parameters(func),
                        body: func.body.clone(),
                        source_location: func.range,
                        state: func_state,
                    });
                }
                Stmt::ClassDef(class) => {
                    // Recursively collect functions from class body
                    for stmt in &class.body {
                        if let Stmt::FunctionDef(func) = stmt {
                            // Create new state variable for this method
                            let mut method_state = self.context.create_state_var();

                            self.context.hoisted_functions.push(TransformedFunction {
                                name: format!("{}_{}", class.name, func.name),
                                parameters: {
                                    let mut params = vec!["self".to_string()];
                                    params.extend(self.extract_parameters(func));
                                    params
                                },
                                body: func.body.clone(),
                                source_location: func.range,
                                state: method_state,
                            });
                        }
                    }
                }
                // Handle nested function definitions
                _ => self.collect_nested_functions(stmt),
            }
        }
    }



    fn collect_nested_functions(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::FunctionDef(func) => {
                // Create state variable for nested function
                let mut nested_func_state = self.context.create_state_var();

                // Handle nested function
                let next_function_name = self.context.next_function_name(&func.name.to_string());
                self.context.hoisted_functions.push(TransformedFunction {
                    name: next_function_name,
                    parameters: self.extract_parameters(func),
                    body: func.body.clone(),
                    source_location: func.range,
                    state: nested_func_state,
                });

                // Recursively collect any functions nested within this function
                for stmt in &func.body {
                    self.collect_nested_functions(stmt);
                }
            }
            Stmt::If(if_stmt) => {
                // Check for functions in if/elif/else bodies
                for stmt in &if_stmt.body {
                    self.collect_nested_functions(stmt);
                }
                for clause in &if_stmt.elif_else_clauses {
                    for stmt in &clause.body {
                        self.collect_nested_functions(stmt);
                    }
                }
            }
            Stmt::While(while_stmt) => {
                // Check for functions in while body
                for stmt in &while_stmt.body {
                    self.collect_nested_functions(stmt);
                }
            }
            Stmt::For(for_stmt) => {
                // Check for functions in for loop body
                for stmt in &for_stmt.body {
                    self.collect_nested_functions(stmt);
                }
            }
            Stmt::Try(try_stmt) => {
                // Check for functions in try/except/else/finally blocks
                for stmt in &try_stmt.body {
                    self.collect_nested_functions(stmt);
                }
                for handler in &try_stmt.handlers {
                    let ExceptHandler::ExceptHandler(h) = handler;
                    for stmt in &h.body {
                        self.collect_nested_functions(stmt);
                    }
                }
                for stmt in &try_stmt.orelse {
                    self.collect_nested_functions(stmt);
                }
                for stmt in &try_stmt.finalbody {
                    self.collect_nested_functions(stmt);
                }
            }
            // Add other statement types as needed
            _ => {}
        }
    }




    fn transform_statements(&mut self, statements: &[Stmt], func_state: &mut StateVar) -> Vec<Stmt> {
        let mut result = Vec::new();

        for stmt in statements {
            let transformed = self.transform_statement(stmt, func_state);
            result.extend(transformed);
        }

        result
    }

    fn transform_statement(&mut self, stmt: &Stmt, func_state: &mut StateVar) -> Vec<Stmt> {
        match stmt {
            Stmt::While(while_stmt) => self.transform_while(while_stmt, func_state),
            Stmt::For(for_stmt) => self.transform_for(for_stmt, func_state),
            Stmt::If(if_stmt) => self.transform_if(if_stmt, func_state),
            _ => vec![stmt.clone()],
        }
    }


    fn create_checkpoint_stmt(&self, range: &TextRange) -> Stmt {
        Stmt::Expr(ruff_python_ast::StmtExpr {
            range: *range,
            value: Box::new(Expr::Call(ruff_python_ast::ExprCall {
                range: *range,
                func: Box::new(Expr::Name(ruff_python_ast::ExprName {
                    range: *range,
                    id: Name::new("create_checkpoint".to_string()),
                    ctx: ExprContext::Load,
                })),
                arguments: ruff_python_ast::Arguments {
                    range: Default::default(),
                    args: vec![].into_boxed_slice(),
                    keywords: Box::new([]),
                }
            })),
        })
    }


    fn create_function(
        &mut self,
        name: &str,
        params: &[String],
        body: &[Stmt],
        range: &TextRange,
        state_var: &StateVar,
        is_async: bool,
    ) -> Stmt {
        // Create parameters structure
        let parameters = ruff_python_ast::Parameters {
            range: TextRange::default(),
            posonlyargs: vec![],
            args: params
                .iter()
                .map(|name| {
                    ruff_python_ast::ParameterWithDefault {
                        range: TextRange::default(),
                        parameter: ruff_python_ast::Parameter {
                            range: Default::default(),
                            name: ruff_python_ast::Identifier::new(name.clone(), Default::default()),
                            annotation: None,
                        },
                        default: None,
                    }
                })
                .collect(),
            vararg: None,
            kwonlyargs: vec![],
            kwarg: None,
        };

        // Create closure around state variable
        let mut closure_body = vec![
            // Initialize state variable if it's 0
            Stmt::If(StmtIf {
                range: *range,
                test: Box::new(Expr::Compare(ruff_python_ast::ExprCompare {
                    range: *range,
                    left: Box::new(Expr::Name(ruff_python_ast::ExprName {
                        range: *range,
                        id: Name::new(state_var.name.clone()),
                        ctx: ExprContext::Load,
                    })),
                    ops: vec![ruff_python_ast::CmpOp::Eq].into_boxed_slice(),
                    comparators: vec![
                        Expr::NumberLiteral(ruff_python_ast::ExprNumberLiteral {
                            range: Default::default(),
                            value: ruff_python_ast::Number::Int(0u32.into()),
                        })
                    ].into_boxed_slice(),
                })),
                body: vec![
                    // Initialize any needed variables
                    self.create_state_variable(state_var, range),
                ],
                elif_else_clauses: vec![],
            }),
        ];

        // Transform the body with state management
        let transformed_body = self.transform_statements(body, &mut state_var.clone());
        closure_body.extend(transformed_body);

        // Add return statement if needed
        if !matches!(closure_body.last(), Some(Stmt::Return(_))) {
            closure_body.push(Stmt::Return(ruff_python_ast::StmtReturn {
                range: *range,
                value: None,
            }));
        }

        // Create the function definition
        Stmt::FunctionDef(StmtFunctionDef {
            range: *range,
            is_async,
            decorator_list: vec![],
            name: ruff_python_ast::Identifier::new(name.to_string(), *range),
            type_params: None,
            parameters: Box::new(parameters),
            returns: None,
            body: closure_body,
        })
    }

    fn create_empty_function(
        &self,
        name: &str,
        range: &TextRange,
        state_var: &StateVar,
        is_async: bool,
    ) -> Stmt {
        Stmt::FunctionDef(StmtFunctionDef {
            range: *range,
            is_async,
            decorator_list: vec![],
            name: ruff_python_ast::Identifier::new(name.to_string(), *range),
            type_params: None,
            parameters: Box::new(ruff_python_ast::Parameters {
                range: TextRange::default(),
                posonlyargs: vec![],
                args: vec![],
                vararg: None,
                kwonlyargs: vec![],
                kwarg: None,
            }),
            returns: None,
            body: vec![
                // Initialize state if needed
                self.create_state_variable(state_var, range),
                // Add pass statement
                Stmt::Pass(ruff_python_ast::StmtPass { range: *range }),
                // Add return None
                Stmt::Return(ruff_python_ast::StmtReturn {
                    range: *range,
                    value: None,
                }),
            ],
        })
    }

    // Helper method to update function calls for state management
    fn create_function_call(
        &self,
        func_name: &str,
        args: Vec<Expr>,
        range: &TextRange,
    ) -> Stmt {
        Stmt::Expr(ruff_python_ast::StmtExpr {
            range: *range,
            value: Box::new(Expr::Call(ruff_python_ast::ExprCall {
                range: *range,
                func: Box::new(Expr::Name(ruff_python_ast::ExprName {
                    range: *range,
                    id: Name::new(func_name.to_string()),
                    ctx: ExprContext::Load,
                })),
                arguments: ruff_python_ast::Arguments {
                    range: Default::default(),
                    args: args.into_boxed_slice(),
                    keywords: Box::new([]),
                },
            })),
        })
    }

    // Helper method to wrap a body in a state machine
    fn wrap_function_body(
        &mut self,
        body: Vec<Stmt>,
        state_var: &mut StateVar,
        range: &TextRange,
    ) -> Vec<Stmt> {
        let mut wrapped_body = Vec::new();
        let current_state = self.context.next_state(state_var);

        // Add state initialization if needed
        wrapped_body.push(self.create_state_variable(state_var, range));

        // Add state machine structure
        wrapped_body.push(self.wrap_in_state_check(
            state_var,
            current_state,
            body,
            range,
        ));

        // Add state advancement
        wrapped_body.push(self.create_state_advance(
            state_var,
            current_state,
            range,
        ));

        wrapped_body
    }

    fn create_parameters(&self, params: &[String]) -> ruff_python_ast::Parameters {
        // Create function parameters
        ruff_python_ast::Parameters {
            range: TextRange::default(),
            posonlyargs: vec![],
            args: params
                .iter()
                .map(|name| {
                    ruff_python_ast::ParameterWithDefault {
                        range: TextRange::default(),
                        parameter: ruff_python_ast::Parameter {
                            range: Default::default(),
                            name: ruff_python_ast::Identifier::new(name.clone(), Default::default()),
                            annotation: None,
                        },
                        default: None,
                    }
                })
                .collect(),
            vararg: None,
            kwonlyargs: vec![],
            kwarg: None,
        }
    }

    fn analyze_scope_usage(&self, body: &[Stmt]) -> Vec<String> {
        // Simple scope analysis - collect all variable names
        let mut vars = HashSet::new();
        for stmt in body {
            self.collect_variables(stmt, &mut vars);
        }
        vars.into_iter().collect()
    }

    fn collect_variables(&self, stmt: &Stmt, vars: &mut HashSet<String>) {
        match stmt {
            Stmt::Assign(assign) => {
                // Collect assigned variables
                for target in &assign.targets {
                    if let Expr::Name(name) = target {
                        vars.insert(name.id.clone().parse().unwrap());
                    }
                }
            }
            Stmt::AugAssign(aug_assign) => {
                // Collect augmented assignment variables
                if let Expr::Name(name) = &*aug_assign.target {
                    vars.insert(name.id.clone().parse().unwrap());
                }
            }
            // Add more cases as needed
            _ => {}
        }
    }



    fn wrap_in_state_check(&self, state_var: &StateVar, state_value: u32, body: Vec<Stmt>, range: &TextRange) -> Stmt {
        Stmt::If(StmtIf {
            range: *range,
            test: Box::new(Expr::Compare(ruff_python_ast::ExprCompare {
                range: *range,
                left: Box::new(Expr::Name(ruff_python_ast::ExprName {
                    range: *range,
                    id: Name::new(state_var.name.clone()),
                    ctx: ExprContext::Load,
                })),
                ops: vec![ruff_python_ast::CmpOp::Eq].into_boxed_slice(),
                comparators: vec![
                    Expr::NumberLiteral(ruff_python_ast::ExprNumberLiteral {
                        range: Default::default(),
                        value: ruff_python_ast::Number::Int(state_value.into()),
                    })
                ].into_boxed_slice(),
            })),
            body,
            elif_else_clauses: vec![],
        })
    }


    fn create_state_advance(&self, state_var: &StateVar, next_state: u32, range: &TextRange) -> Stmt {
        Stmt::Assign(ruff_python_ast::StmtAssign {
            range: *range,
            targets: vec![
                Expr::Name(ruff_python_ast::ExprName {
                    range: *range,
                    id: Name::new(state_var.name.clone()),
                    ctx: ExprContext::Store,
                })
            ],
            value: Box::new(Expr::NumberLiteral(ruff_python_ast::ExprNumberLiteral {
                range: Default::default(),
                value: ruff_python_ast::Number::Int((next_state + 1).into()),
            })),
        })
    }

    fn transform_while(&mut self, while_stmt: &StmtWhile, func_state: &mut StateVar) -> Vec<Stmt> {
        let body_func_name = self.context.next_function_name("while_body");
        let current_state = self.context.next_state(func_state);
        let next_state = self.context.next_state(func_state);

        let checkpoint = self.wrap_in_state_check(
            func_state,
            current_state,
            vec![
                self.create_checkpoint_stmt(&while_stmt.range),
                self.create_state_advance(func_state, next_state, &while_stmt.range),
            ],
            &while_stmt.range,
        );

        // Create function for while body with its own state variable
        let mut body_func_state = self.context.create_state_var();
        let body_func = TransformedFunction {
            name: body_func_name.clone(),
            parameters: self.analyze_scope_usage(&while_stmt.body),
            body: while_stmt.body.clone(),
            source_location: while_stmt.range,
            state: body_func_state,
        };
        self.context.hoisted_functions.push(body_func);

        // Create while controller with state check
        let controller = self.wrap_in_state_check(
            func_state,
            next_state,
            vec![
                self.create_while_controller(
                    &while_stmt.test,
                    &body_func_name,
                    &while_stmt.range,
                ),
                self.create_state_advance(func_state, next_state, &while_stmt.range),
            ],
            &while_stmt.range,
        );

        vec![checkpoint, controller]
    }


    fn transform_for(&mut self, for_stmt: &StmtFor, func_state: &mut StateVar) -> Vec<Stmt> {
        let body_func_name = self.context.next_function_name("for_body");
        let current_state = self.context.next_state(func_state);
        let next_state = self.context.next_state(func_state);

        // Create checkpoint with state check
        let checkpoint = self.wrap_in_state_check(
            func_state,
            current_state,
            vec![
                self.create_checkpoint_stmt(&for_stmt.range),
                self.create_state_advance(func_state, next_state, &for_stmt.range),
            ],
            &for_stmt.range,
        );

        // Create function for for-loop body with its own state variable
        let mut body_func_state = self.context.create_state_var();
        let body_func = TransformedFunction {
            name: body_func_name.clone(),
            parameters: self.analyze_scope_usage(&for_stmt.body),
            body: for_stmt.body.clone(),
            source_location: for_stmt.range,
            state: body_func_state,
        };
        self.context.hoisted_functions.push(body_func);

        // Create iterator initialization
        let iter_setup = self.create_iterator_setup(&for_stmt.target, &for_stmt.iter, &for_stmt.range);

        // Create for controller with state check
        let controller = self.wrap_in_state_check(
            func_state,
            next_state,
            vec![
                self.create_for_controller(
                    &for_stmt.target,
                    &body_func_name,
                    &for_stmt.range,
                ),
                self.create_state_advance(func_state, next_state, &for_stmt.range),
            ],
            &for_stmt.range,
        );

        vec![checkpoint, iter_setup, controller]
    }


    fn create_iterator_setup(&self, target: &Expr, iter: &Expr, range: &TextRange) -> Stmt {
        Stmt::Assign(ruff_python_ast::StmtAssign {
            range: *range,
            targets: vec![
                Expr::Name(ruff_python_ast::ExprName {
                    range: *range,
                    id: Name::new("_iterator".to_string()),
                    ctx: ExprContext::Store,
                })
            ],
            value: Box::new(Expr::Call(ruff_python_ast::ExprCall {
                range: *range,
                func: Box::new(Expr::Name(ruff_python_ast::ExprName {
                    range: *range,
                    id: Name::new("iter".to_string()),
                    ctx: ExprContext::Load,
                })),
                arguments: ruff_python_ast::Arguments {
                    range: Default::default(),
                    args: vec![iter.clone()].into_boxed_slice(),
                    keywords: Box::new([]),
                },
            })),
        })
    }


    fn create_for_controller(
        &self,
        target: &Expr,
        body_func_name: &str,
        range: &TextRange,
    ) -> Stmt {
        Stmt::Expr(ruff_python_ast::StmtExpr {
            range: *range,
            value: Box::new(Expr::Call(ruff_python_ast::ExprCall {
                range: *range,
                func: Box::new(Expr::Name(ruff_python_ast::ExprName {
                    range: *range,
                    id: Name::from("for_controller".to_string()),
                    ctx: ExprContext::Load,
                })),
                arguments: ruff_python_ast::Arguments {
                    range: Default::default(),
                    args: vec![
                        Expr::Name(ruff_python_ast::ExprName {
                            range: Default::default(),
                            id: Name::new("_iterator"),
                            ctx: ExprContext::Load,
                        }),
                        target.clone(),
                        Expr::Name(ruff_python_ast::ExprName {
                            range: Default::default(),
                            id: Name::new(body_func_name),
                            ctx: ExprContext::Load,
                        }),
                    ].into_boxed_slice(),
                    keywords: Box::new([]),
                },
            })),
        })
    }


    fn transform_if(&mut self, if_stmt: &StmtIf, func_state: &mut StateVar) -> Vec<Stmt> {
        let mut functions = Vec::new();
        let mut clauses = Vec::new();

        // Create initial checkpoint state
        let current_state = self.context.next_state(func_state);
        let next_state = self.context.next_state(func_state);

        // Create checkpoint with state check
        let checkpoint = self.wrap_in_state_check(
            func_state,
            current_state,
            vec![
                self.create_checkpoint_stmt(&if_stmt.range),
                self.create_state_advance(func_state, next_state, &if_stmt.range),
            ],
            &if_stmt.range,
        );

        // Handle main if branch
        let then_func_name = self.context.next_function_name("if_then");
        let mut then_func_state = self.context.create_state_var();
        let then_func = TransformedFunction {
            name: then_func_name.clone(),
            parameters: self.analyze_scope_usage(&if_stmt.body),
            body: if_stmt.body.clone(),
            source_location: if_stmt.range,
            state: then_func_state,
        };
        functions.push(then_func);
        clauses.push((Box::new(*if_stmt.test.clone()), then_func_name));

        // Handle elif clauses
        for elif_clause in &if_stmt.elif_else_clauses {
            if elif_clause.test.is_some() { // This is an elif clause
                let elif_func_name = self.context.next_function_name("elif_then");
                let mut elif_func_state = self.context.create_state_var();
                let elif_func = TransformedFunction {
                    name: elif_func_name.clone(),
                    parameters: self.analyze_scope_usage(&elif_clause.body),
                    body: elif_clause.body.clone(),
                    source_location: if_stmt.range,
                    state: elif_func_state,
                };
                functions.push(elif_func);
                clauses.push((Box::new(elif_clause.test.clone().unwrap()), elif_func_name));
            }
        }

        // Handle final else clause (if any)
        let else_func_name = self.context.next_function_name("else");
        let mut else_func_state = self.context.create_state_var();
        let else_body = if let Some(last_clause) = if_stmt.elif_else_clauses.last() {
            if last_clause.test.is_none() { // This is an else clause
                last_clause.body.clone()
            } else {
                vec![] // No else clause
            }
        } else {
            vec![] // No else clause
        };

        let else_func = TransformedFunction {
            name: else_func_name.clone(),
            parameters: self.analyze_scope_usage(&else_body),
            body: if else_body.is_empty() {
                vec![Stmt::Pass(ruff_python_ast::StmtPass { range: if_stmt.range })]
            } else {
                else_body
            },
            source_location: if_stmt.range,
            state: else_func_state,
        };
        functions.push(else_func);

        // Add all functions to hoisted functions
        for func in functions {
            self.context.hoisted_functions.push(func);
        }

        // Create chain controller
        let controller = self.wrap_in_state_check(
            func_state,
            next_state,
            vec![
                self.create_if_chain_controller(&clauses, &else_func_name, &if_stmt.range),
                self.create_state_advance(func_state, next_state, &if_stmt.range),
            ],
            &if_stmt.range,
        );

        vec![checkpoint, controller]
    }


    fn create_if_chain_controller(
        &self,
        clauses: &[(Box<Expr>, String)],
        else_func_name: &str,
        range: &TextRange,
    ) -> Stmt {
        Stmt::Expr(ruff_python_ast::StmtExpr {
            range: *range,
            value: Box::new(Expr::Call(ruff_python_ast::ExprCall {
                range: *range,
                func: Box::new(Expr::Name(ruff_python_ast::ExprName {
                    range: *range,
                    id: Name::from("if_chain_controller".to_string()),
                    ctx: ExprContext::Load,
                })),
                arguments: ruff_python_ast::Arguments {
                    range: Default::default(),
                    args: {
                        let mut args = Vec::new();

                        // Add conditions and their corresponding functions
                        for (test, func_name) in clauses {
                            args.push((**test).clone());
                            args.push(Expr::Name(ruff_python_ast::ExprName {
                                range: Default::default(),
                                id: Name::new(func_name.clone()),
                                ctx: ExprContext::Load,
                            }));
                        }

                        // Add else function
                        args.push(Expr::Name(ruff_python_ast::ExprName {
                            range: Default::default(),
                            id: Name::new(else_func_name.to_string()),
                            ctx: ExprContext::Load,
                        }));

                        args.into_boxed_slice()
                    },
                    keywords: Box::new([]),
                }
            })),
        })
    }



    fn transform_module(&mut self, module: &ModModule) -> ModModule {
        // First pass: collect all function definitions
        self.collect_functions(module);

        // Verify no state conflicts exist
        if let Err(e) = self.verify_state_variables() {
            // In a real implementation, you might want to handle this error differently
            panic!("State variable conflict detected: {}", e);
        }

        // Transform the module body while tracking state
        let mut module_state = self.context.create_state_var();
        let transformed_body = self.transform_statements(&module.body, &mut module_state);

        // Generate the final body with all hoisted functions and state management
        let final_body = self.generate_final_body(transformed_body);

        // Create new module with transformed body
        ModModule {
            body: final_body,
            ..module.clone()
        }
    }

}

// Helper functions for creating controller statements
impl CodeTransformer {


    fn create_while_controller(
        &self,
        test: &Expr,
        body_func_name: &str,
        range: &TextRange,
    ) -> Stmt {
        Stmt::Expr(ruff_python_ast::StmtExpr {
            range: *range,
            value: Box::new(Expr::Call(ruff_python_ast::ExprCall {
                range: *range,
                func: Box::new(Expr::Name(ruff_python_ast::ExprName {
                    range: *range,
                    id: Name::from("while_controller".to_string()),
                    ctx: ExprContext::Load,
                })),
                arguments: ruff_python_ast::Arguments {
                    range: Default::default(),
                    args: vec![
                        test.clone(),
                        Expr::Name(ruff_python_ast::ExprName {
                            range: Default::default(),
                            id: Name::new(body_func_name),
                            ctx: ExprContext::Load,
                        }),
                    ].into_boxed_slice(),
                    keywords: Box::new([]),
                },
            })),
        })
    }

    fn create_if_controller(
        &self,
        test: &Expr,
        then_func_name: &str,
        else_func_name: &str,
        range: &TextRange,
    ) -> Stmt {
        // Create a call to if controller
        Stmt::Expr(ruff_python_ast::StmtExpr {
            range: *range,
            value: Box::new(Expr::Call(ruff_python_ast::ExprCall {
                range: *range,
                func: Box::new(Expr::Name(ruff_python_ast::ExprName {
                    range: *range,
                    id: Name::from("if_controller".to_string()),
                    ctx: ExprContext::Load,
                })),
                arguments: ruff_python_ast::Arguments {
                    range: Default::default(),
                    args: vec![
                        test.clone(),
                        Expr::Name(ruff_python_ast::ExprName {
                            range: Default::default(),
                            id: Name::new(then_func_name),
                            ctx: ExprContext::Load,
                        }),
                        Expr::Name(ruff_python_ast::ExprName {
                            range: Default::default(),
                            id: Name::new(else_func_name),
                            ctx: ExprContext::Load,
                        })
                    ].into_boxed_slice(),
                    keywords: Box::new([]),
                }
            })),
        })
    }



    fn generate_final_body(&mut self, transformed_body: Vec<Stmt>) -> Vec<Stmt> {
        let mut final_body = Vec::new();
        let mut processed_functions = HashSet::new();

        // Keep processing until no new functions are added
        while {
            let current_functions: Vec<_> = self.context
                .hoisted_functions
                .iter()
                .filter(|f| !processed_functions.contains(&f.name))
                .cloned()
                .collect();

            !current_functions.is_empty()
        } {
            // Process all currently unprocessed functions
            let functions_to_process: Vec<_> = self.context
                .hoisted_functions
                .iter()
                .filter(|f| !processed_functions.contains(&f.name))
                .cloned()
                .collect();

            for func in functions_to_process {
                // Add state variable initialization
                final_body.push(self.create_state_variable(&func.state, &func.source_location));

                // Create and add the function definition
                final_body.push(self.create_function(
                    &func.name,
                    &func.parameters,
                    &func.body,
                    &func.source_location,
                    &func.state,
                    false, // is_async = false
                ));

                // Mark this function as processed
                processed_functions.insert(func.name);
            }
        }

        // Add transformed body
        final_body.extend(transformed_body);

        final_body
    }


    // Helper method to create any runtime initialization code needed
    fn create_runtime_init(&self) -> Stmt {
        // Create the runtime initialization for state management
        Stmt::Expr(ruff_python_ast::StmtExpr {
            range: TextRange::default(),
            value: Box::new(Expr::Call(ruff_python_ast::ExprCall {
                range: TextRange::default(),
                func: Box::new(Expr::Name(ruff_python_ast::ExprName {
                    range: TextRange::default(),
                    id: Name::new("init_state_runtime".to_string()),
                    ctx: ExprContext::Load,
                })),
                arguments: ruff_python_ast::Arguments {
                    range: Default::default(),
                    args: vec![].into_boxed_slice(),
                    keywords: Box::new([]),
                },
            })),
        })
    }

    // Helper method to create any runtime cleanup code needed
    fn create_runtime_cleanup(&self) -> Stmt {
        // Create the runtime cleanup for state management
        Stmt::Expr(ruff_python_ast::StmtExpr {
            range: TextRange::default(),
            value: Box::new(Expr::Call(ruff_python_ast::ExprCall {
                range: TextRange::default(),
                func: Box::new(Expr::Name(ruff_python_ast::ExprName {
                    range: TextRange::default(),
                    id: Name::new("cleanup_state_runtime".to_string()),
                    ctx: ExprContext::Load,
                })),
                arguments: ruff_python_ast::Arguments {
                    range: Default::default(),
                    args: vec![].into_boxed_slice(),
                    keywords: Box::new([]),
                },
            })),
        })
    }

    // Helper method to verify no state conflicts exist
    fn verify_state_variables(&self) -> Result<(), String> {
        let mut seen_states = HashSet::new();
        for func in &self.context.hoisted_functions {
            if !seen_states.insert(func.state.name.clone()) {
                return Err(format!("Duplicate state variable found: {}", func.state.name));
            }
        }
        Ok(())
    }

}

pub fn transform_code(code: &str) -> Result<String, ParseError> {
    // Parse the input code
    let parse = ruff_python_parser::parse_module(code)?;
    let stylist = Stylist::from_tokens(parse.tokens(), code);
    let mut parsed = parse.into_syntax();

    // Modify the AST
    let mut transformer = CodeTransformer::new();
    let transformed = transformer.transform_module(&parsed);


    // Generate code from modified AST
    // let stylist = Stylist::default();
    let mut generator: Generator = (&stylist).into();
    let suite = Suite::from(transformed.body);
    generator.unparse_suite(&suite);
    Ok(generator.generate())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_machine_transformation() {
        let code = r#"
def example():
    x = 1
    while x < 10:
        x += 1
    if x > 5:
        print("big")
    else:
        print("small")
"#;

        let transformed = transform_code(code).unwrap().replace("\\n", "\r");
        insta::with_settings!({
            description => code,
            omit_expression => true
        }, {
            insta::assert_snapshot!(transformed);
        });
    }
    #[test]
    fn test_state_machine_transformation_elif() {
        let code = r#"
def example():
    x = 1
    while x < 10:
        x += 1
    if x > 5:
        print("big")
    elif x > 3:
        print("middle")
    else:
        print("small")
"#;

        let transformed = transform_code(code).unwrap().replace("\\n", "\r");
        insta::with_settings!({
            description => code,
            omit_expression => true
        }, {
            insta::assert_snapshot!(transformed);
        });
    }


    #[test]
    fn test_state_machine_transformation_for() {
        let code = r#"
def example():
    for i in range(5):
        print(i)
"#;

        let transformed = transform_code(code).unwrap().replace("\\n", "\r");
        insta::with_settings!({
            description => code,
            omit_expression => true
        }, {
            insta::assert_snapshot!(transformed);
        });
    }

    #[test]
    fn test_state_machine_transformation_nested_for() {
        let code = r#"
def example():
    for i in range(3):
        for j in range(2):
            print(i, j)
"#;

        let transformed = transform_code(code).unwrap().replace("\\n", "\r");
        insta::with_settings!({
            description => code,
            omit_expression => true
        }, {
            insta::assert_snapshot!(transformed);
        });
    }
}
