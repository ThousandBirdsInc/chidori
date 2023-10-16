
use tokio::time::sleep;

use std::time::Duration;
use prompt_graph_core::proto::{ChangeValue, item, SupportedChatModel};
use prompt_graph_core::proto::serialized_value::Val;
use prompt_graph_core::prompt_composition::templates::render_template_prompt;

use prompt_graph_core::proto::prompt_graph_node_prompt::Model;
use crate::executor::NodeExecutionContext;
use crate::integrations::openai::batch::chat_completion;


#[tracing::instrument]
pub async fn execute_node_prompt(ctx: &NodeExecutionContext<'_>) -> Vec<ChangeValue> {
    let &NodeExecutionContext {
        node_will_execute_on_branch,
        item: item::Item::NodePrompt(n),
        item_core: _,
        namespaces,
        template_partials,
        ..
    } = ctx else {
        panic!("execute_node_prompt: expected NodeExecutionContext with NodePrompt item");
    };

    let change_set: Vec<ChangeValue> = node_will_execute_on_branch.node.as_ref().unwrap()
        .change_values_used_in_execution.iter().filter_map(|x| x.change_value.clone()).collect();
    let mut filled_values = vec![];
    // n.model;
    // n.frequency_penalty;
    // n.max_tokens;
    // n.presence_penalty;
    // n.stop;



    if let Some(Model::ChatModel(model)) = n.model {
        let m = SupportedChatModel::from_i32(model).unwrap();
        let templated_string = render_template_prompt(&n.template, &change_set.clone(), template_partials).unwrap();

        let mut delay = Duration::from_secs(1);  // Start with 1 second delay
        loop {
            match chat_completion(&n, m, templated_string.clone()).await {
                Ok(result) => {
                    for output_table in namespaces.iter() {
                        filled_values.push(prompt_graph_core::create_change_value(
                            vec![output_table.clone(), String::from("promptResult")],
                            Some(Val::String(result.choices.first().unwrap().message.content.clone().unwrap())),
                            0)
                        );
                    }
                    break;
                },
                Err(e) => {
                    println!("Failed with error: {}. Retrying after {} seconds...", e, delay.as_secs());
                    sleep(delay).await;
                    delay *= 2;  // Double the delay for the next attempt
                }
            }
        }
    }
    filled_values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exec_node_prompt() {
    }
}
