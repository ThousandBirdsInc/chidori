use tokio::time::sleep;

use prompt_graph_core::prompt_composition::templates::render_template_prompt;
use prompt_graph_core::proto::serialized_value::Val;
use prompt_graph_core::proto::{item, ChangeValue, SupportedChatModel};
use std::time::Duration;

use crate::executor::NodeExecutionContext;
use crate::integrations::openai::batch::chat_completion;
use prompt_graph_core::proto::prompt_graph_node_prompt::Model;

trait ChatModel {}

trait CompletionModel {}
