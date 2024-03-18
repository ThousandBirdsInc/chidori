use crate::cells::{CodeCell, SupportedLanguage};
use crate::execution::primitives::operation::{InputItemConfiguation, InputSignature, InputType, OperationNode, OutputItemConfiguation, OutputSignature};

/// Code cells allow notebooks to evaluate source code in a variety of languages.
#[tracing::instrument]
pub fn code_cell(cell: &CodeCell) -> OperationNode {
    match cell.language {
        SupportedLanguage::PyO3 => {
            // TODO: all callable functions should be exposed as their own OperationNodes
            //       these can be depended on throughout the graph as their own cells.
            let paths =
                chidori_static_analysis::language::python::parse::extract_dependencies_python(
                    &cell.source_code,
                );
            let report = chidori_static_analysis::language::python::parse::build_report(&paths);

            let mut input_signature = InputSignature::new();
            for (key, value) in &report.cell_depended_values {
                input_signature.globals.insert(
                    key.clone(),
                    InputItemConfiguation {
                        ty: Some(InputType::String),
                        default: None,
                    },
                );
            }

            let mut output_signature = OutputSignature::new();
            for (key, value) in &report.cell_exposed_values {
                output_signature.globals.insert(
                    key.clone(),
                    OutputItemConfiguation {
                        ty: Some(InputType::String),
                    },
                );
            }

            for (key, value) in &report.triggerable_functions {
                output_signature.functions.insert(
                    key.clone(),
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
                    crate::library::std::code::runtime_pyo3::source_code_run_python(
                        &cell.source_code,
                        &x,
                        &cell.function_invocation,
                    )
                    .unwrap()
                    .0
                }),
            )
        }
        SupportedLanguage::Starlark => OperationNode::new(
            InputSignature::new(),
            OutputSignature::new(),
            Box::new(|x, _| x),
        ),
        SupportedLanguage::Deno => {
            let paths =
                chidori_static_analysis::language::javascript::parse::extract_dependencies_js(
                    &cell.source_code,
                );
            let report = chidori_static_analysis::language::javascript::parse::build_report(&paths);

            let mut input_signature = InputSignature::new();
            for (key, value) in &report.cell_depended_values {
                input_signature.globals.insert(
                    key.clone(),
                    InputItemConfiguation {
                        ty: Some(InputType::String),
                        default: None,
                    },
                );
            }

            let mut output_signature = OutputSignature::new();
            for (key, value) in &report.cell_exposed_values {
                output_signature.globals.insert(
                    key.clone(),
                    OutputItemConfiguation {
                        ty: Some(InputType::String),
                    },
                );
            }

            for (key, value) in &report.triggerable_functions {
                output_signature.functions.insert(
                    key.clone(),
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
                    crate::library::std::code::runtime_deno::source_code_run_deno(
                        &cell.source_code,
                        &x,
                        &cell.function_invocation,
                    )
                    .unwrap()
                    .0
                }),
            )
        }
    }
}
