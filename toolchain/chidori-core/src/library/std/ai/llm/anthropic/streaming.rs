use ollama_rs::generation::completion::request::GenerationRequest;
use ollama_rs::Ollama;
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;

async fn stream() {

    let ollama = Ollama::default();
    let model = "llama2:latest".to_string();
    let prompt = "Why is the sky blue?".to_string();

    let mut stream = ollama.generate_stream(GenerationRequest::new(model, prompt)).await.unwrap();

    let mut stdout = tokio::io::stdout();
    while let Some(res) = stream.next().await {
        let res = res.unwrap();
        // stdout.write(res.as_bytes()).await.unwrap();
        stdout.flush().await.unwrap();
    }
}