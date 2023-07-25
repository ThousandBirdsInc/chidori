use prompt_graph_core::proto2::{ChangeValue, item, NodeWillExecuteOnBranch, Path};
use crate::db_operations::custom_node_execution::get_custom_node_execution;
use crate::db_operations::executing_nodes::get_complete_custom_node_will_exec;
use crate::executor::NodeExecutionContext;

#[tracing::instrument]
pub async fn execute_node_custom(ctx: &NodeExecutionContext<'_>) -> anyhow::Result<Vec<ChangeValue>> {
    let &NodeExecutionContext {
        node_will_execute_on_branch,
        item: item::Item::NodeCustom(n),
        namespaces,
        tree,
        ..
    } = ctx else {
        panic!("execute_node_custom: expected NodeExecutionContext with Custom item");
    };

    let NodeWillExecuteOnBranch { branch, counter, ..} = node_will_execute_on_branch;

    loop {
        if let Some(change) = get_custom_node_execution(&tree, *branch, *counter) {
            let mut result_filled_values = vec![];
            for filled_value in change.filled_values {
                for output_table in namespaces.iter() {
                    let mut address = vec![output_table.clone()];
                    address.extend(filled_value.path.as_ref().unwrap().address.clone());
                    result_filled_values.push(
                    ChangeValue {
                        path: Some(Path {
                            address,
                        }),
                        value: filled_value.value.clone(),
                        branch: 0,
                    });
                }

            }
            return Ok(result_filled_values);
        }
    }
}
