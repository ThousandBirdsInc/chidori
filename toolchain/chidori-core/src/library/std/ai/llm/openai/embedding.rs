use async_trait::async_trait;
use openai_api_rs::v1::embedding::{EmbeddingRequest, EmbeddingResponse};
use crate::library::std::ai::llm::{ChatCompletionReq, ChatModelBatch, ChatModelStream, EmbeddingModel, EmbeddingReq, Usage};
use crate::library::std::ai::llm::openai::OpenAIChatModel;

#[async_trait]
impl EmbeddingModel for OpenAIChatModel {
    async fn embed(&self, embedding_request: EmbeddingReq) -> Result<Vec<f32>, String> {
        let req = EmbeddingRequest {
            model: embedding_request.model,
            input: embedding_request.content,
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
        // TODO: Ollama doesnt support embeddings api
        let api_key = env::var("OPENAI_API_KEY").unwrap().to_string();
        let API_URL_V1: &str = "https://api.openai.com/v1";
        let model = OpenAIChatModel::new(API_URL_V1.to_string(), api_key);
        let embedding_req = EmbeddingReq {
            content: "".to_string(),
            model: "".to_string(),
            frequency_penalty: None,
            max_tokens: None,
            presence_penalty: None,
            stop: None,
        };
        let result = model.embed(embedding_req).await;
        assert!(result.is_ok());
        let response = result.unwrap();
    }
}
