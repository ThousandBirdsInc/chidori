use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::mpsc::Sender;
use crate::cells::{CellTypes, LLMCodeGenCellChatConfiguration, TemplateCell, TextRange};
use crate::execution::primitives::operation::{AsyncRPCCommunication, InputItemConfiguration, InputSignature, InputType, OperationFn, OperationFnOutput, OperationNode, OutputItemConfiguration, OutputSignature};
use crate::execution::primitives::serialized_value::{RkyvSerializedValue as RKV, serialized_value_to_json_value, RkyvSerializedValue};

use futures_util::FutureExt;
use chidori_prompt_format::templating::templates::{ChatModelRoles, TemplateWithSource};
use crate::execution::execution::execution_graph::ExecutionNodeId;
use crate::execution::execution::ExecutionState;

/// Template cells leverage the same tooling as LLM Prompt Cells, but are used for more general templating.
#[tracing::instrument]
pub fn template_cell(execution_state_id: ExecutionNodeId, cell: &TemplateCell, range: &TextRange) -> anyhow::Result<OperationNode> {
    let schema =
        chidori_prompt_format::templating::templates::analyze_referenced_partials(&cell.body);

    let mut input_signature = InputSignature::new();
    for (key, value) in &schema.unwrap().items {
        input_signature.globals.insert(
            key.clone(),
            InputItemConfiguration {
                ty: Some(InputType::String),
                default: None,
            },
        );
    }


    let mut output_signature = OutputSignature::new();
    if let Some(name) = &cell.name {
        output_signature.functions.insert(
            name.clone(),
            OutputItemConfiguration::Function {
                input_signature: InputSignature::new(),
                emit_event: vec![],
                trigger_on: vec![],
            },
        );
    }

    let body = cell.body.clone();
    Ok(OperationNode::new(
        cell.name.clone(),
        execution_state_id,
        input_signature,
        output_signature,
        CellTypes::Template(cell.clone(), Default::default())
    ))
}


pub fn template_cell_exec(body: String) -> Box<OperationFn> {
    Box::new(move |_, x, _, _| {
        let body = body.clone();
        async move {
            let data = if let RKV::Object(m) = x {
                if let Some(m) = m.get("globals") {
                    serialized_value_to_json_value(m)
                } else {
                    serialized_value_to_json_value(&RKV::Null)
                }
            } else {
                serialized_value_to_json_value(&x)
            };
            let rendered = chidori_prompt_format::templating::templates::render_template_prompt(&body, &data, &HashMap::new()).unwrap();
            Ok(OperationFnOutput::with_value(RKV::String(rendered)))
        }.boxed()
    })
}

#[cfg(test)]
mod test {
    use uuid::Uuid;
    use crate::cells::TextRange;
    use crate::execution::execution::ExecutionState;

    #[tokio::test]
    async fn test_template_cell() -> anyhow::Result<()> {
        let cell = crate::cells::TemplateCell {
            backing_file_reference: None,
            name: Some("test".to_string()),
            body: "Hello, {{ name }}!".to_string(),
        };
        let op = crate::cells::template_cell::template_cell(Uuid::nil(), &cell, &TextRange::default())?;
        let input = crate::execution::primitives::serialized_value::RkyvSerializedValue::Object(
            std::collections::HashMap::new()
        );
        let output = op.execute(&ExecutionState::new_with_random_id(), input, None, None).await?;
        assert_eq!(output.output, Ok(crate::execution::primitives::serialized_value::RkyvSerializedValue::String("Hello, !".to_string())));
        Ok(())
    }
}