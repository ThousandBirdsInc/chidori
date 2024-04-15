pub mod batch;
pub mod streaming;
mod embedding;

use openai_api_rs::v1::api::Client;
use std::env;

pub struct OpenAIChatModel {
    api_url: String,
    api_key: String,
    client: Client,
}

impl OpenAIChatModel {
    pub fn new(api_url: String, api_key: String) -> Self {
        let client = Client::new_with_endpoint(api_url.clone(), api_key.clone());
        Self { api_url, client, api_key }
    }
}
