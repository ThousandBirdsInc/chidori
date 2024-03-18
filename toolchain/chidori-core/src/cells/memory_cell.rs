use crate::cells::{MemoryCell, SupportedMemoryProviders};
use crate::execution::primitives::operation::{InputSignature, InputType, OperationNode, OutputItemConfiguation, OutputSignature};

/// Memory cells when first executed have no inputs. They initialize the connection to the memory store.
/// Once initialized they become communicated with over the functions that they provide to the workspace.
#[tracing::instrument]
pub fn memory_cell(cell: &MemoryCell) -> OperationNode {
    match cell.provider {
        SupportedMemoryProviders::InMemory => {
            let mut input_signature = InputSignature::new();
            // input_signature.globals.insert(
            //     key.clone(),
            //     InputItemConfiguation {
            //         ty: Some(InputType::String),
            //         default: None,
            //     },
            // );

            let triggerable_functions = vec![
                ("run", "run",)
            ];

            let mut output_signature = OutputSignature::new();

            for (key, value) in &triggerable_functions {
                output_signature.functions.insert(
                    key.to_string(),
                    OutputItemConfiguation {
                        ty: Some(InputType::Function),
                    },
                );
            }

            let cell = cell.clone();
            OperationNode::new(
                input_signature,
                output_signature,
                Box::new(move |x, _| {
                    // TODO: this needs to handle stdout and errors
                    x
                }),
            )
        }
    }
}