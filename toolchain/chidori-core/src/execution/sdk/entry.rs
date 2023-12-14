/// This is an SDK for building execution graphs. It is designed to be used iteratively.

/// Start a new execution graph.
fn create() {}

/// Add a node to the execution graph.
fn add_node() {}

/// Add a relationship between two nodes in the execution graph.
fn add_relationship() {}

fn eval() {
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
}
