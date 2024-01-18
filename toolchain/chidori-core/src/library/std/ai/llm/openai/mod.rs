pub mod batch;
pub mod streaming;

use openai_api_rs::v1::api::Client;
use std::env;

pub struct OpenAIChatModel {
    api_key: String,
    client: Client,
}

impl OpenAIChatModel {
    pub fn new(api_key: String) -> Self {
        let client = Client::new(api_key.clone());
        Self { client, api_key }
    }
}
