use std::collections::HashSet;
use prompt_graph_core::proto2::{ChangeValue, item, ItemCore, NodeWillExecute, PromptGraphNodeCode, PromptGraphNodeCodeSourceCode, SupportedSourceCodeLanguages};
use prompt_graph_core::proto2::prompt_graph_node_code::Source;
use prompt_graph_core::templates::{flatten_value_keys, json_value_to_serialized_value, render_template_prompt};
use crate::runtime_nodes::node_code;
use deno_core::serde_json::Value;
use log::debug;
use crate::executor::NodeExecutionContext;

#[cfg(feature = "starlark")]
pub fn run_starlark(source_code: String, change_set: &Vec<ChangeValue>) -> Option<Value> {
    node_code::starlark::source_code_run_starlark(soure_code, change_set)
}

#[cfg(not(feature = "starlark"))]
pub fn run_starlark(source_code: String, change_set: &Vec<ChangeValue>) -> Option<Value> {
    None
}


#[tracing::instrument]
pub fn execute_node_code(ctx: &NodeExecutionContext) -> Vec<ChangeValue> {
    let &NodeExecutionContext {
        node_will_execute_on_branch,
        item: item::Item::NodeCode(n),
        item_core,
        namespaces,
        template_partials,
        ..
    } = ctx else {
        panic!("execute_node_code: expected NodeExecutionContext with NodeCode item");
    };


    let mut change_set: Vec<ChangeValue> = node_will_execute_on_branch.node.as_ref().unwrap()
        .change_values_used_in_execution.iter().filter_map(|x| x.change_value.clone()).collect();

    debug!("execute_node_code {:?}", &n);
    let mut filled_values = vec![];
    if let Some(source) = &n.source {
        match source {
            Source::SourceCode(c) => {
                let source_code = render_template_prompt(&c.source_code, &change_set.clone(), template_partials).unwrap();
                let result = match SupportedSourceCodeLanguages::from_i32(c.language).unwrap() {
                    SupportedSourceCodeLanguages::Deno => {
                        node_code::deno::source_code_run_deno(source_code, &change_set.clone())
                    },
                    SupportedSourceCodeLanguages::Starlark => {
                        run_starlark(source_code, &change_set.clone())
                    }
                };

                // Resolve outputs from code execution into filled values
                let sresult = result.as_ref().map(json_value_to_serialized_value);
                if let Some(val) = sresult {
                    let flattened = flatten_value_keys(val, vec![]);
                    for (k, v) in flattened {
                        for output_table in namespaces.iter() {
                            let mut address = vec![output_table.clone()];
                            address.extend(k.clone());
                            filled_values.push(prompt_graph_core::create_change_value(
                                address,
                                Some(v.clone()),
                                0)
                            );
                        }
                    }
                }
            }
            Source::Zipfile(_) | Source::S3Path(_) => {
                unimplemented!("invoke docker container is not yet implemented");
            }
            _ => {}
        }
    }
    filled_values
}
