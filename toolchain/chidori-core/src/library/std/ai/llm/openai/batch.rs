use async_trait::async_trait;

use crate::library::std::ai::llm;
use crate::library::std::ai::llm::openai::OpenAIChatModel;
use crate::library::std::ai::llm::{ChatCompletionReq, ChatCompletionRes, ChatModelBatch};

use openai_api_rs::v1::chat_completion::{
    ChatCompletionMessage, ChatCompletionRequest, FunctionCall, MessageRole,
};

pub enum SupportedChatModel {
    Gpt4,
    Gpt40314,
    Gpt432k,
    Gpt432k0314,
    Gpt35Turbo,
    Gpt35Turbo0301,
}

#[async_trait]
impl ChatModelBatch for OpenAIChatModel {
    async fn batch(
        &self,
        chat_completion_req: ChatCompletionReq,
    ) -> Result<ChatCompletionRes, String> {
        let model = chat_completion_req.model;
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
        let req = ChatCompletionRequest {
            model,
            messages: chat_completion_req
                .template_messages
                .into_iter()
                .map(|m| ChatCompletionMessage {
                    role: match m.role {
                        llm::MessageRole::User => MessageRole::user,
                        llm::MessageRole::System => MessageRole::system,
                        llm::MessageRole::Assistant => MessageRole::assistant,
                        llm::MessageRole::Function => MessageRole::function,
                    },
                    content: m.content,
                    name: m.name,
                    function_call: m.function_call.map(|f| FunctionCall {
                        name: f.name,
                        arguments: f.arguments,
                    }),
                })
                .collect(),
            tool_choice: None,
            tools: None,
            functions: None,
            function_call: None,
            temperature: chat_completion_req.temperature,
            top_p: chat_completion_req.top_p,
            n: None,
            response_format: chat_completion_req.response_format,
            stream: None,
            stop: None,
            max_tokens: chat_completion_req.max_tokens,
            presence_penalty: chat_completion_req.presence_penalty,
            frequency_penalty: chat_completion_req.frequency_penalty,
            logit_bias: chat_completion_req.logit_bias,
            user: chat_completion_req.user,
            seed: chat_completion_req.seed,
        };
        self.client
            .chat_completion(req)
            .map(|res| ChatCompletionRes {
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
                    })
                    .collect(),
                usage: llm::Usage {
                    prompt_tokens: res.usage.prompt_tokens,
                    completion_tokens: res.usage.completion_tokens,
                    total_tokens: res.usage.total_tokens,
                },
            })
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
        let model = OpenAIChatModel::new(api_key);
        let chat_completion_req = ChatCompletionReq {
            model: "".to_string(),
            ..ChatCompletionReq::default()
        };
        let result = model.batch(chat_completion_req).await;
        assert!(result.is_ok());
        let response = result.unwrap();
    }
}
