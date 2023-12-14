use crate::execution::execution::execution_graph::ExecutionGraph;
use crate::execution::execution::execution_state::ExecutionState;
use crate::execution::execution::DependencyGraphMutation;
use crate::execution::language::parser::{BinaryOp, Error, Expr, Func, Program, Spanned, Value};
use crate::execution::primitives::identifiers::OperationId;
use crate::execution::primitives::operation::OperationNode;
use std::collections::HashMap;
use std::collections::HashSet;

use crate::execution::primitives::serialized_value::{
    serialize_to_vec, RkyvSerializedValue as RSV,
};

fn compile_to_graph(program: Program) -> ExecutionGraph {
    let mut db = ExecutionGraph::new();
    let mut state = ExecutionState::new();
    let state_id = (0, 0);

    // TODO: We start by pushing all top level functions and imports into the graph as nodes
    // let (state_id, mut state) = db.upsert_operation(
    //     state_id,
    //     state,
    //     0,
    //     0,
    //     Box::new(|_args| {
    //         let v = RSV::Number(0);
    //         return serialize_to_vec(&v);
    //     }),
    // );

    // TODO: We construct the dependency graph by walking the AST and building up the graph.
    //       This can be done in any order.
    if let Some(main_func) = program.funcs.get("main") {
        let mut stack = vec![];
        let mut set = HashSet::new();
        let mut deps = vec![];
        eval_to_call_graph(
            &main_func.body,
            &program.funcs,
            &mut stack,
            &mut set,
            &mut deps,
        );
    }

    // TODO: We return the final system state
    db
}

/// Tree walking interpreter
fn eval_expr(
    expr: &Spanned<Expr>,
    funcs: &HashMap<String, Func>,
    stack: &mut Vec<(String, Value)>,
) -> Result<Value, Error> {
    Ok(match &expr.0 {
        Expr::Error => unreachable!(), // Error expressions only get created by parser errors, so cannot exist in a valid AST
        Expr::Value(val) => val.clone(),
        Expr::List(items) => Value::List(
            items
                .iter()
                .map(|item| eval_expr(item, funcs, stack))
                .collect::<Result<_, _>>()?,
        ),
        Expr::Local(name) => stack
            .iter()
            .rev()
            .find(|(l, _)| l == name)
            .map(|(_, v)| v.clone())
            .or_else(|| Some(Value::Func(name.clone())).filter(|_| funcs.contains_key(name)))
            .ok_or_else(|| Error {
                span: expr.1.clone(),
                msg: format!("No such variable '{}' in scope", name),
            })?,
        Expr::Let(local, val, body) => {
            let val = eval_expr(val, funcs, stack)?;
            stack.push((local.clone(), val));
            let res = eval_expr(body, funcs, stack)?;
            stack.pop();
            res
        }
        Expr::Then(a, b) => {
            eval_expr(a, funcs, stack)?;
            eval_expr(b, funcs, stack)?
        }
        Expr::Binary(a, BinaryOp::Add, b) => Value::Num(
            eval_expr(a, funcs, stack)?.num(a.1.clone())?
                + eval_expr(b, funcs, stack)?.num(b.1.clone())?,
        ),
        Expr::Binary(a, BinaryOp::Sub, b) => Value::Num(
            eval_expr(a, funcs, stack)?.num(a.1.clone())?
                - eval_expr(b, funcs, stack)?.num(b.1.clone())?,
        ),
        Expr::Binary(a, BinaryOp::Mul, b) => Value::Num(
            eval_expr(a, funcs, stack)?.num(a.1.clone())?
                * eval_expr(b, funcs, stack)?.num(b.1.clone())?,
        ),
        Expr::Binary(a, BinaryOp::Div, b) => Value::Num(
            eval_expr(a, funcs, stack)?.num(a.1.clone())?
                / eval_expr(b, funcs, stack)?.num(b.1.clone())?,
        ),
        Expr::Binary(a, BinaryOp::Eq, b) => {
            Value::Bool(eval_expr(a, funcs, stack)? == eval_expr(b, funcs, stack)?)
        }
        Expr::Binary(a, BinaryOp::NotEq, b) => {
            Value::Bool(eval_expr(a, funcs, stack)? != eval_expr(b, funcs, stack)?)
        }
        Expr::Binary(a, BinaryOp::PipeOp, b) => {
            Value::Bool(eval_expr(a, funcs, stack)? != eval_expr(b, funcs, stack)?)
        }
        Expr::Call(func, args) => {
            let f = eval_expr(func, funcs, stack)?;
            match f {
                Value::Func(name) => {
                    let f = &funcs[&name];
                    let mut stack = if f.args.len() != args.len() {
                        return Err(Error {
                            span: expr.1.clone(),
                            msg: format!("'{}' called with wrong number of arguments (expected {}, found {})", name, f.args.len(), args.len()),
                        });
                    } else {
                        f.args
                            .iter()
                            .zip(args.iter())
                            .map(|(name, arg)| Ok((name.clone(), eval_expr(arg, funcs, stack)?)))
                            .collect::<Result<_, _>>()?
                    };
                    eval_expr(&f.body, funcs, &mut stack)?
                }
                f => {
                    return Err(Error {
                        span: func.1.clone(),
                        msg: format!("'{:?}' is not callable", f),
                    })
                }
            }
        }
        Expr::If(cond, a, b) => {
            let c = eval_expr(cond, funcs, stack)?;
            match c {
                Value::Bool(true) => eval_expr(a, funcs, stack)?,
                Value::Bool(false) => eval_expr(b, funcs, stack)?,
                c => {
                    return Err(Error {
                        span: cond.1.clone(),
                        msg: format!("Conditions must be booleans, found '{:?}'", c),
                    })
                }
            }
        }
        Expr::Print(a) => {
            let val = eval_expr(a, funcs, stack)?;
            println!("{}", val);
            val
        }
    })
}

/// This walks the AST and statically constructs a call graph
fn eval_to_call_graph<'src>(
    expr: &Spanned<Expr>,
    funcs: &HashMap<String, Func>,

    // TODO: stack is instead the set of nodes that can be depended upon by future statements
    stack: &mut Vec<(&'src str, OperationId)>,

    operation_nodes: &mut HashSet<OperationNode>,
    dep_graph_mutations: &mut Vec<DependencyGraphMutation>,
) -> Result<HashSet<OperationId>, Error> {
    match &expr.0 {
        Expr::Error => unreachable!(), // Error expressions only get created by parser errors, so cannot exist in a valid AST
        // NOOP - value just needs to get pushed into the execution context based on its assignment, this just returns the value
        Expr::Value(val) => Ok(HashSet::new()),
        Expr::Print(val) => Ok(HashSet::new()),
        // A list of expressions, we evaluate each one and return the aggregate set of dependent nodes
        Expr::List(items) => {
            let mut dependencies = HashSet::new();
            for item in items {
                dependencies.extend(eval_to_call_graph(
                    item,
                    funcs,
                    stack,
                    operation_nodes,
                    dep_graph_mutations,
                )?);
            }
            Ok(dependencies)
        }
        // Local means that we're refering to a given function, value, etc.
        Expr::Local(name) => {
            // TODO: this is attempting to get whatever this name is referring to
            // TODO: find the operation id of the named variable or function definition
            // stack
            //     .iter()
            //     .rev()
            //     .find(|(l, _)| l == name)
            //     .map(|(_, v)| v.clone())
            //     .or_else(|| {
            //         Some(Value::Func(name.to_string())).filter(|_| funcs.contains_key(name))
            //     })
            //     .ok_or_else(|| Error {
            //         span: expr.1,
            //         msg: format!("No such variable '{}' in scope", name),
            //     })?
            Ok(HashSet::new())
        }
        // Assignment to a local variable, value, and body is the remaining expressions where the local value is assigned
        Expr::Let(local, val, res) => {
            // TODO: let becomes a node in the graph
            // TODO: operation node should have an optional span associated with it
            // TODO: assigns something to a name
            // TODO: push the head operation node onto the stack
            operation_nodes.insert(OperationNode::new(
                1,
                Some(Box::new(|args| {
                    let v = RSV::Number(0);
                    return serialize_to_vec(&v);
                })),
            ));
            Ok(HashSet::new())
        }
        Expr::Then(a, b) => {
            // TODO: this creates a zero argument (causal) dependency across two expressions
            // Statement A must complete before statement B will run
            // TODO: With "await" execution becomes a thunk for the previous line
            eval_to_call_graph(a, funcs, stack, operation_nodes, dep_graph_mutations)?;
            Ok(eval_to_call_graph(
                b,
                funcs,
                stack,
                operation_nodes,
                dep_graph_mutations,
            )?)
        }

        // These operations are ignored during graph construction, both sides are traversed and any references to other operators pushed into the graph
        Expr::Binary(a, BinaryOp::Add, b)
        | Expr::Binary(a, BinaryOp::Sub, b)
        | Expr::Binary(a, BinaryOp::Mul, b)
        | Expr::Binary(a, BinaryOp::Div, b)
        | Expr::Binary(a, BinaryOp::Eq, b)
        | Expr::Binary(a, BinaryOp::NotEq, b) => {
            let mut dependencies = HashSet::new();
            for item in &[a, b] {
                dependencies.extend(eval_to_call_graph(
                    item,
                    funcs,
                    stack,
                    operation_nodes,
                    dep_graph_mutations,
                )?);
            }
            Ok(dependencies)
        }

        // Conditional logic
        Expr::If(cond, cond_true, cond_false) => {
            // We capture if we depend on any operations within any of these expressions.
            let mut dependencies = HashSet::new();
            for item in &[cond, cond_true, cond_false] {
                dependencies.extend(eval_to_call_graph(
                    item,
                    funcs,
                    stack,
                    operation_nodes,
                    dep_graph_mutations,
                )?);
            }
            Ok(dependencies)
        }

        // Composition, this declares new, anonymous functions in the context of this method
        // those anonymous functions are themselves operations in the graph.
        Expr::Binary(a, BinaryOp::PipeOp, b) => {
            // TODO: PipeOp accepts one operator and then makes the second operator dependent on the output of the first
            dep_graph_mutations.push(DependencyGraphMutation::Create {
                operation_id: 0,
                depends_on: vec![],
            });
            Ok(HashSet::new())

            // TODO: the second operator is wrapping the execution of the first operator
            // TODO: attach the first operator as the first argument of the second operator

            // TODO: we instantiate these operators as dependencies in the graph

            // TODO: operators should support themselves being operator graphs
        }
        Expr::Call(func, args) => {
            let mut dependencies = HashSet::new();
            dependencies.extend(eval_to_call_graph(
                func,
                funcs,
                stack,
                operation_nodes,
                dep_graph_mutations,
            )?);
            for item in args {
                dependencies.extend(eval_to_call_graph(
                    item,
                    funcs,
                    stack,
                    operation_nodes,
                    dep_graph_mutations,
                )?);
            }
            Ok(dependencies)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::language::parser::parse;
    use std::collections::HashMap;

    #[test]
    fn test_compiling_simple_program() {
        let program = parse(
            r#"
        fn fn_x() {
            
        }
        
        fn fn_y() {
            
        }
            
        fn main() {
            // Simple arithmetic
            let x = 1;
            let y = 2;
            print(x + y);
            
            // Circular dependency
            fn_z(composed());
            
            // Composition using pipe operator
            let composed = fn_x |>
                fn_y;
                
            // Composition using partial application
            let composed_2 = fn_y(fn_x)
            
            // Execution of a composed function alongside a conditional
            if composed() == 1 {
                print("x is 1");
            } else {
                print("x is not 1");
            }
            
            
        }
    "#
            .to_string(),
        )
        .unwrap()
        .unwrap();

        let db = compile_to_graph(program);
    }
}
