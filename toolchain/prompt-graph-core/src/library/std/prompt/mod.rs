struct ChatCompletionReq {
    model: SupportedChatModel,
    frequency_penalty: Option<f32>,
    max_tokens: Option<i32>,
    presence_penalty: Option<f32>,
    stop: Option<Vec<String>>,
}
