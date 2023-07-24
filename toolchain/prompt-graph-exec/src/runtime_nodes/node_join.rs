use prompt_graph_core::proto2::{ChangeValue, item, NodeWillExecute, PromptGraphMap};
use prompt_graph_core::proto2::serialized_value::Val;
use crate::executor::NodeExecutionContext;

#[tracing::instrument]
pub fn execute_node_join(ctx: &NodeExecutionContext) -> Vec<ChangeValue> {
    unimplemented!("TODO: implement execute_node_join");
    let NodeExecutionContext {
        node_will_execute_on_branch,
        // TODO: incomplete
        item: item::Item::Map(n),
        item_core,
        ..
    } = ctx else {
        panic!("TODO: implement execute_node_join");
    };

    // TODO: grab the top level paths
    // TODO: use a join policy to combine them
    // TODO: we propagate when _any_ result is ready

    // TODO: join nodes look like _multiple_ nodes in the graph from the executor's perspective

    // TODO: any named subtree is another node instance, that must be met by the dispatch
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
