use crate::library::std::ai::llm;
use crate::library::std::ai::llm::openai::OpenAIChatModel;
use crate::library::std::ai::llm::{ChatCompletionReq, ChatModelStream, LLMStream, Usage};
use async_trait::async_trait;
use deno_core::serde_json;
use futures_util::stream::Stream;
use openai_api_rs::v1::chat_completion::ChatCompletionMessage;
use openai_api_rs::v1::chat_completion::ChatCompletionRequest;
use openai_api_rs::v1::chat_completion::FunctionCall;
use openai_api_rs::v1::chat_completion::MessageRole;
use reqwest::{Client, Response};
use serde_json::Value;
use std::pin::Pin;
use std::task::{Context, Poll};

#[async_trait]
impl ChatModelStream for OpenAIChatModel {
    async fn stream(&self, chat_completion_req: ChatCompletionReq) -> Result<LLMStream, String> {
        let api_url = &self.api_url;
        let client = Client::new();
        let response: Response = match client
            .post(api_url)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&ChatCompletionRequest {
                model: chat_completion_req.model,
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
                stream: Some(true),
                stop: None,
                max_tokens: chat_completion_req.max_tokens,
                presence_penalty: chat_completion_req.presence_penalty,
                frequency_penalty: chat_completion_req.frequency_penalty,
                logit_bias: chat_completion_req.logit_bias,
                user: chat_completion_req.user,
                seed: chat_completion_req.seed,
            })
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => return Err(format!("API request error: {}", error)),
        };

        if response.status().is_success() {
            Ok(LLMStream {
                response: Box::pin(response.bytes_stream()),
                buffer: String::new(),
                first_chunk: true,
                usage: Usage::default(),
            })
        } else {
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| String::from("Unknown error"));
            Err(format!("API request error: {}", error_text))
        }
    }
}

impl Stream for LLMStream {
    type Item = String;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match self.response.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    let mut utf8_str = String::from_utf8_lossy(&chunk).to_string();

                    if self.first_chunk {
                        let lines: Vec<&str> = utf8_str.lines().collect();
                        utf8_str = if lines.len() >= 2 {
                            lines[lines.len() - 2].to_string()
                        } else {
                            utf8_str.clone()
                        };
                        self.first_chunk = false;
                    }

                    let trimmed_str = utf8_str.trim_start_matches("data: ");

                    let json_result: Result<Value, _> = serde_json::from_str(trimmed_str);

                    match json_result {
                        Ok(json) => {
                            if let Some(choices) = json.get("choices") {
                                if let Some(choice) = choices.get(0) {
                                    if let Some(content) =
                                        choice.get("delta").and_then(|delta| delta.get("content"))
                                    {
                                        if let Some(content_str) = content.as_str() {
                                            self.buffer.push_str(content_str);
                                            let output = self.buffer.replace("\\n", "\n");
                                            return Poll::Ready(Some(output));
                                        }
                                    }
                                }
                            }
                        }
                        Err(_) => {}
                    }
                }
                Poll::Ready(Some(Err(error))) => {
                    eprintln!("Error in stream: {:?}", error);
                    return Poll::Ready(None);
                }
                Poll::Ready(None) => {
                    return Poll::Ready(None);
                }
                Poll::Pending => {
                    return Poll::Pending;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dotenv;
    use futures_util::stream::StreamExt;
    use openai_api_rs::v1::chat_completion::{ChatCompletionMessage, MessageRole};
    use std::env;

    #[tokio::test]
    async fn test_gpt_stream_raw_line() {
        dotenv::dotenv().ok();
        let api_key = env::var("OPENAI_API_KEY").unwrap().to_string();
        let api_url_v1: &str = "https://api.openai.com/v1";
        let model = OpenAIChatModel::new(api_url_v1.to_string(), api_key);
        let stream = model.stream(Default::default()).await.unwrap();
        let mut stream = Box::pin(stream);
        while let Some(value) = stream.next().await {
            println!("{}", value);
        }
    }
}
