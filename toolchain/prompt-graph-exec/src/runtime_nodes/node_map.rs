use std::collections::HashSet;
use prompt_graph_core::proto2::{ChangeValue, item, NodeWillExecute, PromptGraphMap};
use prompt_graph_core::proto2::serialized_value::Val;
use crate::executor::NodeExecutionContext;


#[tracing::instrument]
pub fn execute_node_map(ctx: &NodeExecutionContext) -> Vec<ChangeValue> {
    let &NodeExecutionContext {
        node_will_execute_on_branch,
        item: item::Item::Map(n),
        ..
    } = ctx else {
        panic!("execute_node_map: expected NodeExecutionContext with Map item");
    };

    let mut change_set: Vec<ChangeValue> = node_will_execute_on_branch.node.as_ref().unwrap()
        .change_values_used_in_execution.iter().filter_map(|x| x.change_value.clone()).collect();
    let mut filled_values = vec![];
    if let Some(change) = change_set.iter().find(|change| change.path.as_ref().unwrap().address.join(".") == n.path) {
        if let Val::Array(vec) = change.value.as_ref().unwrap().val.clone().unwrap() {
            for (i, item) in vec.values.iter().enumerate() {
                filled_values.push(prompt_graph_core::create_change_value(
                    vec![i.to_string()],
                    item.val.clone(),
                0));
            }
        }
    }
    filled_values
}
