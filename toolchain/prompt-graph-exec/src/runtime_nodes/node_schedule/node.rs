use prompt_graph_core::proto::{ChangeValue, item};
use crate::executor::NodeExecutionContext;

#[tracing::instrument]
pub async fn execute_node_schedule(ctx: &NodeExecutionContext<'_>) -> Vec<ChangeValue> {
    let &NodeExecutionContext {
        node_will_execute_on_branch,
        item: item::Item::NodeSchedule(_n),
        item_core: _,
        namespaces: _,
        template_partials: _,
        ..
    } = ctx else {
        panic!("execute_node_schedule: expected NodeExecutionContext with NodePrompt item");
    };

    // TODO: check if this should re-invoke itself
    // n.policy.unwrap().policy_type;

    // Delay proxying this value until the configured point in time
    // tokio::time::delay_until(tokio::time::Instant::now() + Duration::from_secs(5)).await;

    let _change_set: Vec<ChangeValue> = node_will_execute_on_branch.node.as_ref().unwrap()
        .change_values_used_in_execution.iter().filter_map(|x| x.change_value.clone()).collect();
    let filled_values = vec![];

    filled_values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exec_node_schedule() {
    }
}
