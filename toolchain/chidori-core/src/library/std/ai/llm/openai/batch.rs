use std::collections::HashMap;
use async_trait::async_trait;
use futures_util::TryStreamExt;

use crate::library::std::ai::llm;
use crate::library::std::ai::llm::openai::OpenAIChatModel;
use crate::library::std::ai::llm::{ChatCompletionReq, ChatCompletionRes, ChatModelBatch, JSONSchemaDefine, JSONSchemaType, Tool, ToolChoiceType};

use openai_api_rs::v1::chat_completion::{
    ChatCompletionMessage, ChatCompletionRequest, MessageRole,
};
use crate::cells::LLMPromptCellChatConfiguration;
use crate::execution::primitives::serialized_value::json_value_to_serialized_value;

#[async_trait]
impl ChatModelBatch for OpenAIChatModel {
    async fn batch(
        &self,
        chat_completion_req: ChatCompletionReq,
    ) -> Result<ChatCompletionRes, String> {
        let model = &chat_completion_req.config.model;
        if self.api_url == "https://api.openai.com/v1" {
            if !vec![
                "gpt-4-1106-preview",
                "gpt-4-vision-preview",
                "gpt-4",
                "gpt-4-0314",
                "gpt-4-0613",
                "gpt-4-32k",
                "gpt-4-32k-0314",
                "gpt-4-32k-0613",
                "gpt-3.5-turbo",
                "gpt-3.5-turbo-16k",
                "gpt-3.5-turbo-0301",
                "gpt-3.5-turbo-0613",
                "gpt-3.5-turbo-1106",
                "gpt-3.5-turbo-16k-0613",
            ]
                .contains(&model.as_str())
            {
                return Err(format!("Model {} is not supported", model));
            }
        }

        let req = Self::chat_completion_req_to_openai_req(&chat_completion_req);
        self.client
            .chat_completion(req)
            .map(|res| {
                ChatCompletionRes {
                id: res.id,
                object: res.object,
                created: res.created,
                model: res.model,
                choices: res
                    .choices
                    .iter()
                    .map(|c| llm::ChatCompletionChoice {
                        text: c.message.content.clone(),
                        index: 0,
                        logprobs: None,
                        finish_reason: "".to_string(),
                        tool_calls: c.message.tool_calls.clone().map(|tool_calls| {
                            tool_calls
                                .iter()
                                .map(|tool_call| {
                                    llm::ChatCompletionToolCall {
                                        id: tool_call.id.clone(),
                                        ty: "function".to_string(),
                                        function: llm::ChatCompletionToolCallFunction {
                                            name: tool_call.function.name.clone(),
                                            arguments: tool_call.function.arguments
                                                .as_ref()
                                                .map(|x| {dbg!(&x); x })
                                                .map(|x| x.as_str())
                                                .map(|x| serde_json::from_str(x).unwrap())
                                                .map(|x| json_value_to_serialized_value(&x)),
                                        }
                                    }
                                })
                                .collect()
                        }),
                    })
                    .collect(),
                usage: llm::Usage {
                    prompt_tokens: res.usage.prompt_tokens,
                    completion_tokens: res.usage.completion_tokens,
                    total_tokens: res.usage.total_tokens,
                },
            }})
            .map_err(|e| e.message)
    }
}



#[cfg(test)]
mod tests {
    use super::*;
    use openai_api_rs::v1::api::Client;
    use std::env;

    #[tokio::test]
    async fn test_batch_completion() {
        let api_key = env::var("OPENAI_API_KEY").unwrap().to_string();
        let api_url_v1: &str = "https://api.openai.com/v1";
        let model = OpenAIChatModel::new(api_url_v1.to_string(), api_key);
        let chat_completion_req = ChatCompletionReq {
            ..ChatCompletionReq::default()
        };
        let result = model.batch(chat_completion_req).await;
        assert!(result.is_ok());
        let response = result.unwrap();
    }
}
