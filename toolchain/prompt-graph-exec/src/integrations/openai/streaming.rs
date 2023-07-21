use std::pin::Pin;
use std::task::{Context, Poll};
use deno_core::serde_json;
use futures_util::stream::Stream;
use openai_api_rs::v1::chat_completion::ChatCompletionRequest;
use reqwest::{Client, Response};
use serde_json::Value;
use serde::{Serialize, Deserialize};

pub struct GptStream {
    response: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>>,
    buffer: String,
    first_chunk: bool,
}

pub async fn gpt_stream(api_key: String, completion: ChatCompletionRequest) -> Result<GptStream, String> {
    let api_url = "https://api.openai.com/v1/chat/completions";
    let client = Client::new();
    let response: Response = match client
        .post(api_url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&completion)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return Err(format!("API request error: {}", error)),
    };

    if response.status().is_success() {
        Ok(GptStream {
            response: Box::pin(response.bytes_stream()),
            buffer: String::new(),
            first_chunk: true,
        })
    } else {
        let error_text = response.text().await.unwrap_or_else(|_| String::from("Unknown error"));
        Err(format!("API request error: {}", error_text))
    }
}

impl Stream for GptStream {
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
                                    if let Some(content) = choice.get("delta").and_then(|delta| delta.get("content")) {
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
    use std::env;
    use super::*;
    use futures_util::stream::StreamExt;
    use openai_api_rs::v1::chat_completion::{ChatCompletionMessage, MessageRole};

    #[cfg(feature = "integration-tests")]
    #[tokio::test]
    async fn test_gpt_stream_raw_line() {
        let api_key = env::var("OPENAI_API_KEY").unwrap().to_string();
        let stream = gpt_stream(api_key, ChatCompletionRequest {
            model: "gpt-3.5-turbo".to_string(),
            messages: vec![ChatCompletionMessage {
                role: MessageRole::user,
                content: Some("One sentence to describe a simple advanced usage of Rust".to_string()),
                name: None,
                function_call: None,
            }],
            functions: None,
            function_call: None,
            temperature: None,
            top_p: None,
            n: None,
            stream: Some(true),
            stop: None,
            max_tokens: None,
            presence_penalty: None,
            frequency_penalty: None,
            logit_bias: None,
            user: None,
        }).await.unwrap();
        let mut stream = Box::pin(stream);
        while let Some(value) = stream.next().await {
            println!("{}", value);
        }
    }
}