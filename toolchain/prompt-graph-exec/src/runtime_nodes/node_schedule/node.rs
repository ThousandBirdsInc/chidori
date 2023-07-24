use std::collections::HashSet;
use tokio::time::sleep;
use std::env;
use std::time::Duration;
use prompt_graph_core::proto2::{ChangeValue, item, ItemCore, NodeWillExecute, PromptGraphNodePrompt, SupportedChatModel};
use prompt_graph_core::proto2::serialized_value::Val;
use prompt_graph_core::templates::render_template_prompt;
use futures::executor;
use prompt_graph_core::proto2::prompt_graph_node_prompt::Model;
use crate::executor::NodeExecutionContext;
use crate::integrations::openai::batch::chat_completion;


#[tracing::instrument]
pub async fn execute_node_schedule(ctx: &NodeExecutionContext<'_>) -> Vec<ChangeValue> {
    let &NodeExecutionContext {
        node_will_execute_on_branch,
        item: item::Item::NodeSchedule(n),
        item_core,
        namespaces,
        template_partials,
        ..
    } = ctx else {
        panic!("execute_node_schedule: expected NodeExecutionContext with NodePrompt item");
    };

    // TODO: check if this should re-invoke itself
    // n.policy.unwrap().policy_type;

    // Delay proxying this value until the configured point in time
    // tokio::time::delay_until(tokio::time::Instant::now() + Duration::from_secs(5)).await;

    let mut change_set: Vec<ChangeValue> = node_will_execute_on_branch.node.as_ref().unwrap()
        .change_values_used_in_execution.iter().filter_map(|x| x.change_value.clone()).collect();
    let mut filled_values = vec![];

    filled_values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exec_node_schedule() {
    }
}
