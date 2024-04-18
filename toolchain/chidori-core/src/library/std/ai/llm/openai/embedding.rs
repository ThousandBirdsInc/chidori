use async_trait::async_trait;
use openai_api_rs::v1::embedding::{EmbeddingRequest, EmbeddingResponse};
use crate::library::std::ai::llm::{ChatCompletionReq, ChatModelBatch, ChatModelStream, EmbeddingModel, EmbeddingReq, Usage};
use crate::library::std::ai::llm::openai::OpenAIChatModel;

#[async_trait]
impl EmbeddingModel for OpenAIChatModel {
    async fn embed(&self, embedding_request: EmbeddingReq) -> Result<Vec<f32>, String> {
        let model = embedding_request.model;
        if self.api_url == "https://api.openai.com/v1" {
            if !vec![
                "text-embedding-3-small",
                "text-embedding-3-large",
                "text-embedding-ada-002",
            ]
                .contains(&model.as_str())
            {
                return Err(format!("OpenAI model {} is not supported", model));
            }
        }
        if self.api_url == "http://localhost:11434/v1" {
            return Err("Ollama does not yet support the openai embeddings api format".to_string());
        }
        let req = EmbeddingRequest {
            model,
            input: embedding_request.content,
            dimensions: None,
            user: None,
        };
        self.client
            .embedding(req)
            .map(|res| res.data.first().unwrap().embedding.clone())
            .map_err(|e| e.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openai_api_rs::v1::api::Client;
    use std::env;
    use crate::library::std::ai::llm::EmbeddingReq;

    #[tokio::test]
    async fn test_openai_embedding() {
        let api_key = env::var("OPENAI_API_KEY").unwrap().to_string();
        let api_url_v1: &str = "https://api.openai.com/v1";
        let model = OpenAIChatModel::new(api_url_v1.to_string(), api_key);
        let result = model.embed(EmbeddingReq {
            content: "".to_string(),
            model: "text-embedding-3-small".to_string(),
            frequency_penalty: None,
            max_tokens: None,
            presence_penalty: None,
            stop: None,
        }).await;
        assert!(result.is_ok());
        let response = result.unwrap();
    }
}
