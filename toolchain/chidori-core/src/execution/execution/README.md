# Execution Graph

The `ExecutionGraph` struct manages and executes a graph of operations. Key methods include:

1. **`new()`**:
    - Initializes a new `ExecutionGraph` with default input/output nodes
    - Sets up a background task for async operations and state updates

2. **`shutdown()`**:
    - Allows graceful shutdown of the background task

3. **`get_execution_graph_elements()`**:
    - Returns the edges of the execution graph

4. **`render_execution_graph_to_graphviz()`**:
    - Visualizes the execution graph using Graphviz

5. **`get_state_at_id()`**:
    - Retrieves the state of a specific node in the graph

6. **`get_merged_state_history()`**:
    - Computes merged state history for a given endpoint in the graph

7. **`progress_graph()`**:
    - Updates the graph with a new state
    - Creates a new node and edge

8. **`step_execution_with_previous_state()`**:
    - Executes a single step in the graph
    - Updates state and returns outputs

9. **`mutate_graph()`**:
    - Modifies graph structure by adding/updating operations

10. **`external_step_execution()`**:
    - Interface for external callers to step through graph execution

This system is designed for managing complex, potentially asynchronous operations in a graph structure. It supports stepwise execution, state management, and graph mutations. The use of UUIDs for node identification and graph visualization capabilities suggest it's intended for complex, branching computational workflows.

# Execution State

# ExecutionState and Related Structures Summary

This code defines several structures and enums related to managing the execution of operations in a graph-like structure. Here's a high-level overview:

## Key Structures and Enums

1. **OperationExecutionStatus**:
    - Enum representing the status of an operation's execution.

2. **DependencyGraphMutation**:
    - Enum for creating or deleting dependencies in the graph.

3. **FutureExecutionState**:
    - Struct representing a future state of execution.

4. **ExecutionStateEvaluation**:
    - Enum representing either a complete or executing state.

5. **ExecutionState**:
    - Main struct managing the execution state of operations.

## ExecutionState Methods

1. **`new()`**:
    - Initializes a new ExecutionState.

2. **`with_graph_sender()`**:
    - Creates an ExecutionState with a graph sender for updates.

3. **`clone_with_new_id()`**:
    - Creates a clone of the current state with a new ID.

4. **`state_get()`, `state_get_value()`, `state_insert()`, `state_consume_marked()`**:
    - Methods for managing state data.

5. **`render_dependency_graph()`**:
    - Visualizes the dependency graph.

6. **`get_dependency_graph()`, `get_dependency_graph_flattened()`**:
    - Methods for retrieving the dependency graph structure.

7. **`update_op()`**:
    - Updates an operation in the graph.

8. **`upsert_operation()`**:
    - Inserts or updates an operation in the state.

9. **`apply_dependency_graph_mutations()`**:
    - Applies mutations to the dependency graph.

10. **`dispatch()`**:
    - Invokes a function made available by the execution state.

11. **`step_execution()`**:
    - Executes a single step in the graph, updating the state.

## Key Features

- Manages a graph of operations with dependencies.
- Supports asynchronous and long-running operations.
- Provides methods for updating and querying the execution state.
- Allows for visualization of the dependency graph.
- Handles function dispatching within the execution context.

This system appears designed for managing complex, potentially asynchronous operations in a graph structure, allowing for stepwise execution, state management, and dynamic updates to the operation graph.