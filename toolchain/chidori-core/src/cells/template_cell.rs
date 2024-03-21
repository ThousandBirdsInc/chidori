use std::collections::HashMap;
use crate::cells::TemplateCell;
use crate::execution::primitives::operation::{InputItemConfiguation, InputSignature, InputType, OperationNode, OutputItemConfiguation, OutputSignature};
use crate::execution::primitives::serialized_value::{RkyvSerializedValue as RKV, serialized_value_to_json_value};

/// Template cells leverage the same tooling as LLM Prompt Cells, but are used for more general templating.
#[tracing::instrument]
pub fn template_cell(cell: &TemplateCell) -> OperationNode {
    let schema =
        chidori_prompt_format::templating::templates::analyze_referenced_partials(&cell.body);

    let mut input_signature = InputSignature::new();
    for (key, value) in &schema.items {
        // input_signature.kwargs.insert(
        //     key.clone(),
        //     InputItemConfiguation {
        //         ty: Some(InputType::String),
        //         default: None,
        //     },
        // );
        input_signature.globals.insert(
            key.clone(),
            InputItemConfiguation {
                ty: Some(InputType::String),
                default: None,
            },
        );
    }


    let mut output_signature = OutputSignature::new();
    if let Some(name) = &cell.name {
        output_signature.functions.insert(
            name.clone(),
            OutputItemConfiguation {
                ty: Some(InputType::Function),
            },
        );
    }

    let body = cell.body.clone();
    OperationNode::new(
        cell.name.clone(),
        input_signature,
        output_signature,
        Box::new(move |x, _| {
            let data = if let RKV::Object(m) = x {
                if let Some(m) = m.get("globals") {
                    serialized_value_to_json_value( m )
                } else {
                    serialized_value_to_json_value(&RKV::Null)
                }
            } else {
                serialized_value_to_json_value(&x)
            };
            let rendered = chidori_prompt_format::templating::templates::render_template_prompt(&body, &data, &HashMap::new()).unwrap();
            RKV::String(rendered)
        }),
    )

}
