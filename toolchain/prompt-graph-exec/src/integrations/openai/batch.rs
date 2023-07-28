use std::env;
use futures::executor;
use openai_api_rs::v1::api::Client;
use openai_api_rs::v1::chat_completion::{ChatCompletionRequest, ChatCompletionResponse};
use openai_api_rs::v1::chat_completion;
use openai_api_rs::v1::chat_completion::{
    GPT3_5_TURBO,
    GPT3_5_TURBO_0301,
    GPT4,
    GPT4_0314,
    GPT4_32K,
    GPT4_32K_0314
};
use openai_api_rs::v1::error::APIError;
use prompt_graph_core::templates::render_template_prompt;
use prompt_graph_core::proto2::{PromptGraphNodePrompt, SupportedChatModel};


pub async fn chat_completion(n: &PromptGraphNodePrompt, openai_model: SupportedChatModel, templated_string: String) -> Result<ChatCompletionResponse, APIError> {
    let client = Client::new(env::var("OPENAI_API_KEY").unwrap().to_string());

    let model = match openai_model {
        SupportedChatModel::Gpt4 => GPT4,
        SupportedChatModel::Gpt40314 => GPT4_0314,
        SupportedChatModel::Gpt432k => GPT4_32K,
        SupportedChatModel::Gpt432k0314 => GPT4_32K_0314,
        SupportedChatModel::Gpt35Turbo => GPT3_5_TURBO,
        SupportedChatModel::Gpt35Turbo0301 => GPT3_5_TURBO_0301,
    }.to_string();

    let req = ChatCompletionRequest {
        model,
        messages: vec![chat_completion::ChatCompletionMessage {
            role: chat_completion::MessageRole::user,
            content: templated_string,
            name: None,
            function_call: None,
        }],
        functions: None,
        function_call: None,
        temperature: None,
        top_p: None,
        n: None,
        stream: None,
        stop: None,
        max_tokens: None,
        presence_penalty: None,
        frequency_penalty: None,
        logit_bias: None,
        user: None,
    };

    client.chat_completion(req).await
}
