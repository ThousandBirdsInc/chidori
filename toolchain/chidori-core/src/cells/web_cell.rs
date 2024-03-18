use tokio::runtime::Runtime;
use crate::cells::WebserviceCell;
use crate::execution::primitives::operation::{InputItemConfiguation, InputSignature, InputType, OperationNode, OutputSignature};
use crate::execution::primitives::serialized_value::RkyvSerializedValue as RKV;

/// Web cells initialize HTTP handlers that can invoke other cells.
#[tracing::instrument]
pub fn web_cell(cell: &WebserviceCell) -> OperationNode {
    let endpoints = crate::library::std::webserver::parse_configuration_string(&cell.configuration);

    let mut input_signature = InputSignature::new();
    for endpoint in endpoints {
        input_signature.globals.insert(
            endpoint.depended_function_identity.clone(),
            InputItemConfiguation {
                ty: Some(InputType::String),
                default: None,
            },
        );
    }

    let mut output_signature = OutputSignature::new();

    let cell = cell.clone();
    let mut op_node = OperationNode::new(
        input_signature,
        output_signature,
        Box::new(move |x, _| {
            // TODO: this needs to handle stdout and errors
            let runtime = Runtime::new().unwrap();
            runtime.block_on(async {
                crate::library::std::webserver::run_webservice(&cell, &x).await;
            });
            RKV::Null
        }),
    );
    op_node.is_async = true;
    op_node.is_long_running = true;
    op_node
}
