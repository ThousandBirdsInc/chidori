pub mod batch;
pub mod streaming;
mod embedding;

use std::collections::HashMap;
use openai_api_rs::v1::api::Client;
use std::env;
use openai_api_rs::v1::chat_completion::{ChatCompletionMessage, ChatCompletionRequest, MessageRole};
use crate::cells::LLMPromptCellChatConfiguration;
use crate::library::std::ai::llm;
use crate::library::std::ai::llm::{ChatCompletionReq, JSONSchemaDefine, JSONSchemaType, Tool, ToolChoiceType};

pub struct OpenAIChatModel {
    api_url: String,
    api_key: String,
    client: Client,
}

impl OpenAIChatModel {
    // TODO: remove api_key parameter, expect usage of a proxy
    pub fn new(api_url: String, api_key: String) -> Self {
        let client = Client::new_with_endpoint(api_url.clone(), api_key.clone());
        Self { api_url, client, api_key }
    }

    pub fn chat_completion_req_to_openai_req(chat_completion_req: &ChatCompletionReq) -> ChatCompletionRequest {
        let config = &chat_completion_req.config;
        ChatCompletionRequest {
            model: config.model.clone(),
            messages: chat_completion_req
                .template_messages
                .iter()
                .map(|m| ChatCompletionMessage {
                    role: match m.role {
                        llm::MessageRole::User => MessageRole::user,
                        llm::MessageRole::System => MessageRole::system,
                        llm::MessageRole::Assistant => MessageRole::assistant,
                        llm::MessageRole::Function => MessageRole::function,
                    },
                    content: openai_api_rs::v1::chat_completion::Content::Text(m.content.clone()),
                    name: m.name.clone(),
                })
                .collect(),
            tool_choice: chat_completion_req.tool_choice.clone().map(our_tool_choice_to_openai),
            tools: chat_completion_req.tools.clone().map(|t| t.into_iter().map(our_tool_to_openai_tool).collect()),
            temperature: config.temperature,
            top_p: config.top_p,
            n: None,
            response_format: None,
            stream: None,
            stop: None,
            max_tokens: config.max_tokens,
            presence_penalty: config.presence_penalty,
            frequency_penalty: config.frequency_penalty,
            logit_bias: config.logit_bias.clone(),
            user: config.user.clone(),
            seed: config.seed,
        }
    }

}



fn our_json_schema_type_to_openai(schema_type: JSONSchemaType) -> openai_api_rs::v1::chat_completion::JSONSchemaType {
    match schema_type {
        JSONSchemaType::Object => openai_api_rs::v1::chat_completion::JSONSchemaType::Object,
        JSONSchemaType::Number => openai_api_rs::v1::chat_completion::JSONSchemaType::Number,
        JSONSchemaType::String => openai_api_rs::v1::chat_completion::JSONSchemaType::String,
        JSONSchemaType::Array => openai_api_rs::v1::chat_completion::JSONSchemaType::Array,
        JSONSchemaType::Null => openai_api_rs::v1::chat_completion::JSONSchemaType::Null,
        JSONSchemaType::Boolean => openai_api_rs::v1::chat_completion::JSONSchemaType::Boolean,
    }
}

fn our_json_schema_define_map_to_openai(schema_define: HashMap<String, Box<JSONSchemaDefine>>) -> HashMap<String, Box<openai_api_rs::v1::chat_completion::JSONSchemaDefine>> {
    schema_define.into_iter().map(|(k, v)|
        (k, Box::new(openai_api_rs::v1::chat_completion::JSONSchemaDefine {
            schema_type: v.schema_type.map(our_json_schema_type_to_openai),
            description: v.description,
            enum_values: v.enum_values,
            properties: v.properties.map(our_json_schema_define_map_to_openai),
            required: v.required,
            items: None,
        }))
    ).collect()
}

fn our_tool_to_openai_tool(tool: Tool) -> openai_api_rs::v1::chat_completion::Tool {
    return openai_api_rs::v1::chat_completion::Tool {
        r#type: openai_api_rs::v1::chat_completion::ToolType::Function,
        function: openai_api_rs::v1::chat_completion::Function {
            name: tool.function.name,
            description: tool.function.description,
            parameters: openai_api_rs::v1::chat_completion::FunctionParameters {
                schema_type: our_json_schema_type_to_openai(tool.function.parameters.schema_type),
                properties: tool.function.parameters.properties.map(our_json_schema_define_map_to_openai),
                required: tool.function.parameters.required
            },
        },
    }
}

fn our_tool_choice_to_openai(tool_choice: ToolChoiceType) -> openai_api_rs::v1::chat_completion::ToolChoiceType {
    match tool_choice {
        ToolChoiceType::None => openai_api_rs::v1::chat_completion::ToolChoiceType::None,
        ToolChoiceType::Auto => openai_api_rs::v1::chat_completion::ToolChoiceType::Auto,
        ToolChoiceType::ToolChoice { tool, .. } => openai_api_rs::v1::chat_completion::ToolChoiceType::ToolChoice {
            tool: our_tool_to_openai_tool(tool),
        }
    }
}
