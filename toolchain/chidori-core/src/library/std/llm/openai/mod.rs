pub mod batch;
pub mod streaming;

use openai_api_rs::v1::api::Client;
use std::env;

pub struct OpenAIChatModel {
    api_key: String,
    client: Client,
}

impl OpenAIChatModel {
    fn new() -> Self {
        let api_key = env::var("OPENAI_API_KEY").unwrap().to_string();
        let client = Client::new(api_key.clone());
        Self { client, api_key }
    }
}
