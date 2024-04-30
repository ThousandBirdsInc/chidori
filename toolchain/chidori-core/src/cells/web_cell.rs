use futures_util::FutureExt;
use tokio::runtime::Runtime;
use crate::cells::{TextRange, WebserviceCell};
use crate::execution::primitives::operation::{InputItemConfiguration, InputSignature, InputType, OperationFnOutput, OperationNode, OutputSignature};
use crate::execution::primitives::serialized_value::{RkyvObjectBuilder, RkyvSerializedValue as RKV};

/// Web cells initialize HTTP handlers that can invoke other cells.
#[tracing::instrument]
pub fn web_cell(cell: &WebserviceCell, range: &TextRange) -> anyhow::Result<OperationNode> {
    let endpoints = crate::library::std::webserver::parse_configuration_string(&cell.configuration);

    let mut input_signature = InputSignature::new();
    for endpoint in endpoints {
        input_signature.globals.insert(
            endpoint.depended_function_identity.clone(),
            InputItemConfiguration {
                ty: Some(InputType::String),
                default: None,
            },
        );
    }

    let mut output_signature = OutputSignature::new();

    let cell = cell.clone();
    let mut op_node = OperationNode::new(
        cell.name.clone(),
        input_signature,
        output_signature,
        Box::new(move |_, x, _, _| {
            // TODO: this needs to handle stdout and errors
            let cell = cell.clone();
            async move {
                let (join_handle, port) = crate::library::std::webserver::run_webservice(&cell, &x).await;
                Ok(OperationFnOutput::with_value(
                    RkyvObjectBuilder::new()
                        .insert_number("port", port as i32)
                        .build()
                ))
            }.boxed()
        }),
    );
    op_node.is_long_running_background_thread = true;
    Ok(op_node)
}
