use crate::execution::execution::execution_graph::ExecutionGraph;
use crate::execution::execution::execution_state::ExecutionState;
use crate::execution::execution::DependencyGraphMutation;
use crate::execution::primitives::cells::CellTypes;
use crate::execution::primitives::identifiers::DependencyReference;
use crate::execution::primitives::operation::{InputSignature, OperationNode, OutputSignature};
use crate::execution::primitives::serialized_value::RkyvSerializedValue as RKV;
use chidori_static_analysis::language::python::parse::{
    Report, ReportItem, ReportTriggerableFunctions,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// This is an SDK for building execution graphs. It is designed to be used iteratively.

type Func = fn(RKV) -> RKV;

pub struct Environment {
    db: ExecutionGraph,
    state: ExecutionState,
    state_id: (usize, usize),
    op_counter: usize,
    reported_cells: HashMap<usize, Report>,
}

impl Environment {
    fn new() -> Self {
        let mut db = ExecutionGraph::new();
        let mut state = ExecutionState::new();
        let state_id = (0, 0);
        Environment {
            db,
            state,
            state_id,
            op_counter: 0,
            reported_cells: HashMap::new(),
        }
    }

    /// Scheduled execution of a function in the graph
    fn schedule() {}

    /// Increment the execution graph by one step
    fn step(&mut self) {
        let (state_id, state) = self.db.step_execution(self.state_id, &self.state);
        self.state_id = state_id;
        self.state = state;
    }

    /// Add a cell into the execution graph
    pub fn upsert_cell(&mut self, cell: CellTypes) -> usize {
        self.op_counter += 1;
        let id = self.op_counter;
        let mut op = match &cell {
            CellTypes::Code(c) => crate::cells::code_cell(c),
            CellTypes::Prompt(c) => crate::cells::llm_prompt_cell(c),
        };
        op.attach_cell(cell);

        // self.reported_cells.insert(id, cell_report.clone());

        self.state = self.state.add_operation(id, op);
        // TODO: all declared functions should get their own operations

        // TODO: we collect and throw errors for: naming collisions, missing dependencies, and missing arguments

        // TODO: add a cell report to the execution engine, updating the execution graph
        // TODO: we need a model of dependencies between cells and the number of arguments they require

        self.op_counter
    }

    /// Resolve the set of dependencies currently available, making necessary changes to the operator graph
    pub fn resolve_dependencies_from_input_signature(&mut self) -> Result<&ExecutionState, String> {
        // TODO: when there is a dependency on a function invocation we need to
        //       instantiate a new instance of the function operation node.
        //       It itself is not part of the call graph until it has such a depedendency.

        let mut available_values = HashMap::new();
        let mut available_functions = HashMap::new();

        // For all reported cells, add their exposed values to the available values
        for (id, op) in self.state.operation_by_id.iter() {
            let output_signature = &op.borrow().signature.output_signature;

            // Store values that are available as globals
            for (key, value) in output_signature.globals.iter() {
                // TODO: throw an error if there is a naming collision
                available_values.insert(key.clone(), id);
            }

            for (key, value) in output_signature.functions.iter() {
                // TODO: throw an error if there is a naming collision
                available_functions.insert(key.clone(), id);
            }

            // TODO: Store triggerable functions that may be passed as values as well
        }

        // TODO: we need to report on INVOKED functions - these functions are calls to
        //       functions with the locals assigned in a particular way. But then how do we handle compositions of these?
        //       Well we just need to invoke them in the correct pattern as determined by operations in that context.

        // Anywhere there is a matched value, we create a dependency graph edge
        let mut mutations = vec![];
        for (destination_cell_id, op) in self.state.operation_by_id.iter() {
            let operation = op.borrow();
            let input_signature = &operation.signature.input_signature;
            for (value_name, value) in input_signature.globals.iter() {
                // TODO: we need to handle collisions between the two of these
                if let Some(source_cell_id) = available_functions.get(value_name) {
                    mutations.push(DependencyGraphMutation::Create {
                        operation_id: destination_cell_id.clone(),
                        depends_on: vec![(
                            *source_cell_id.clone(),
                            DependencyReference::FunctionInvocation(value_name.to_string()),
                        )],
                    });
                }

                if let Some(source_cell_id) = available_values.get(value_name) {
                    mutations.push(DependencyGraphMutation::Create {
                        operation_id: destination_cell_id.clone(),
                        depends_on: vec![(
                            *source_cell_id.clone(),
                            DependencyReference::Global(value_name.to_string()),
                        )],
                    });
                }
            }
        }

        self.state = self.state.apply_dependency_graph_mutations(mutations);
        Ok(&self.state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::primitives::cells::{
        CodeCell, LLMPromptCell, SupportedLanguage, SupportedModelProviders,
    };
    use crate::execution::primitives::serialized_value::RkyvObjectBuilder;
    use chidori_static_analysis::language::python::parse::{
        build_report, extract_dependencies_python,
    };
    use indoc::indoc;

    #[test]
    fn test_execute_cells_with_global_dependency() {
        let mut env = Environment::new();
        let id = env.upsert_cell(CellTypes::Code(CodeCell {
            language: SupportedLanguage::Python,
            source_code: String::from(indoc! { r#"
                x = 20
                "#}),
            function_invocation: None,
        }));
        assert_eq!(id, 1);
        let id = env.upsert_cell(CellTypes::Code(CodeCell {
            language: SupportedLanguage::Python,
            source_code: String::from(indoc! { r#"
                y = x + 1
                "#}),
            function_invocation: None,
        }));
        assert_eq!(id, 2);
        env.resolve_dependencies_from_input_signature();
        env.state.render_dependency_graph();
        env.step();
        assert_eq!(
            env.state.state_get(&1),
            Some(&RkyvObjectBuilder::new().insert_number("x", 20).build())
        );
        assert_eq!(env.state.state_get(&2), None);
        env.step();
        assert_eq!(env.state.state_get(&1), None);
        assert_eq!(
            env.state.state_get(&2),
            Some(&RkyvObjectBuilder::new().insert_number("y", 21).build())
        );
    }

    #[ignore]
    #[test]
    fn test_execute_cells_between_code_and_llm() {
        dotenv::dotenv().ok();
        let mut env = Environment::new();
        let id = env.upsert_cell(CellTypes::Code(CodeCell {
            language: SupportedLanguage::Python,
            source_code: String::from(indoc! { r#"
                x = "Here is a sample string"
                "#}),
            function_invocation: None,
        }));
        assert_eq!(id, 1);
        let id = env.upsert_cell(CellTypes::Prompt(LLMPromptCell::Chat {
            path: None,
            provider: SupportedModelProviders::OpenAI,
            req: "\
              Say only a single word. Give no additional explanation.
              What is the first word of the following: {{x}}.
            "
            .to_string(),
        }));
        assert_eq!(id, 2);
        env.resolve_dependencies_from_input_signature();
        env.state.render_dependency_graph();
        env.step();
        assert_eq!(
            env.state.state_get(&1),
            Some(
                &RkyvObjectBuilder::new()
                    .insert_string("x", "Here is a sample string".to_string())
                    .build()
            )
        );
        assert_eq!(env.state.state_get(&2), None);
        env.step();
        assert_eq!(env.state.state_get(&1), None);
        assert_eq!(
            env.state.state_get(&2),
            Some(&RKV::String("Here".to_string()))
        );
    }

    #[test]
    fn test_execute_cells_via_prompt_calling_api() {
        let mut env = Environment::new();
        let id = env.upsert_cell(CellTypes::Code(CodeCell {
            language: SupportedLanguage::Python,
            source_code: String::from(indoc! { r#"
                x = ch.prompt("generate_names", x="John")
                "#}),
            function_invocation: None,
        }));
        assert_eq!(id, 1);
        let id = env.upsert_cell(CellTypes::Prompt(LLMPromptCell::Chat {
            path: Some("generate_names".to_string()),
            provider: SupportedModelProviders::OpenAI,
            req: "\
              Generate names starting with {{x}}
            "
            .to_string(),
        }));
        assert_eq!(id, 2);
        env.resolve_dependencies_from_input_signature();
        env.state.render_dependency_graph();
        env.step();
        assert_eq!(
            env.state.state_get(&1),
            Some(&RkyvObjectBuilder::new().insert_number("x", 20).build())
        );
        assert_eq!(env.state.state_get(&2), None);
        env.step();
        assert_eq!(env.state.state_get(&1), None);
        assert_eq!(
            env.state.state_get(&2),
            Some(&RkyvObjectBuilder::new().insert_number("y", 21).build())
        );
    }

    #[test]
    fn test_execute_cells_invoking_a_function() {
        let mut env = Environment::new();
        let id = env.upsert_cell(CellTypes::Code(CodeCell {
            language: SupportedLanguage::Python,
            source_code: String::from(indoc! { r#"
                def add(x, y):
                    return x + y
                "#}),
            function_invocation: None,
        }));
        assert_eq!(id, 1);
        let id = env.upsert_cell(CellTypes::Code(CodeCell {
            function_invocation: None,
            language: SupportedLanguage::Python,
            source_code: String::from(indoc! { r#"
                y = add(2,3)
                "#}),
        }));
        assert_eq!(id, 2);
        env.resolve_dependencies_from_input_signature();
        env.state.render_dependency_graph();
        env.step();
        assert_eq!(
            env.state.state_get(&1),
            Some(&RkyvObjectBuilder::new().insert_number("x", 20).build())
        );
        assert_eq!(env.state.state_get(&2), None);
        env.step();
        assert_eq!(env.state.state_get(&1), None);
        assert_eq!(
            env.state.state_get(&2),
            Some(&RkyvObjectBuilder::new().insert_number("y", 21).build())
        );
    }
}
