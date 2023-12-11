use crate::execution::execution::execution_graph::ExecutionGraph;
use crate::execution::execution::execution_state::ExecutionState;
/// Logic to convert the rust ast to our scheduler graph
use crate::execution::language::parser::{BinaryOp, Error, Expr, Func, Program, Value};
use std::collections::HashMap;

use crate::execution::primitives::serialized_value::{
    serialize_to_vec, RkyvSerializedValue as RSV,
};

// TODO: how does this actually fetch the functions?
fn compile_to_graph(program: Program) -> ExecutionGraph {
    let mut db = ExecutionGraph::new();
    let mut state = ExecutionState::new();
    let state_id = (0, 0);

    // We start with the number 0 at node 0
    let (state_id, mut state) = db.upsert_operation(
        state_id,
        state,
        0,
        0,
        Box::new(|_args| {
            let v = RSV::Number(0);
            return serialize_to_vec(&v);
        }),
    );

    if let Some(main_func) = program.funcs.get("main") {
        eval_to_graph(&main_func.body.0, &program.funcs, &mut db);
    }
    // ast.body

    // fn eval_expr(
    //     expr: &Spanned<Expr>,
    //     funcs: &HashMap<String, Func>,
    //     stack: &mut Vec<(String, Value)>,
    // ) -> Result<Value, Error> {

    // TODO: look for a "main" function
    // 1. For each function call, create a node
    // 2. For each import, create a node
    // 3. Create connections between nodes when we refer to variables
    // 4. For each function assigned to a variable, create a connection to all function invocations that refer to that variable
    db
}

/// This walks the AST and constructs a graph of operations
fn eval_to_graph(
    expr: &Expr,
    funcs: &HashMap<String, Func>,
    db: &mut ExecutionGraph,
) -> Result<(), Error> {
    // match &expr {
    //     Expr::Error => unreachable!(), // Error expressions only get created by parser errors, so cannot exist in a valid AST
    //     Expr::Value(val) => val.clone(),
    //     Expr::List(items) => Value::List(),
    //     Expr::Local(name) => stack,
    //     Expr::Let(local, val, body) => {}
    //     Expr::Then(a, b) => {
    //         eval_to_graph(a, funcs, stack)?;
    //         eval_to_graph(b, funcs, stack)?
    //     }
    //     Expr::Binary(a, BinaryOp::Add, b) => Value::Num(),
    //     Expr::Binary(a, BinaryOp::Sub, b) => Value::Num(),
    //     Expr::Binary(a, BinaryOp::Mul, b) => Value::Num(),
    //     Expr::Binary(a, BinaryOp::Div, b) => Value::Num(),
    //     Expr::Binary(a, BinaryOp::Eq, b) => {}
    //     Expr::Binary(a, BinaryOp::NotEq, b) => {}
    //     Expr::Binary(a, BinaryOp::PipeOp, b) => {}
    //     Expr::Call(func, args) => {}
    //     Expr::If(cond, a, b) => {}
    //     Expr::Print(a) => {}
    // }
    Ok(())
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
        fn main() {
            let x = 1;
            let y = 2;
            print(x + y);
            x |>
            y |>
            print(x + y);
        }
    "#
            .to_string(),
        )
        .unwrap()
        .unwrap();

        let db = compile_to_graph(program);
    }
}
