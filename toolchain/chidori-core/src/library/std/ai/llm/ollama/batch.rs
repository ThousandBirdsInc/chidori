use ollama_rs::generation::completion::request::GenerationRequest;
use ollama_rs::generation::options::GenerationOptions;
use ollama_rs::Ollama;

async fn batch() {
    // By default it will connect to localhost:11434
    let ollama = Ollama::default();

// // For custom values:
//     let ollama = Ollama::new("http://localhost".to_string(), 11434);

    let model = "llama2:latest".to_string();
    let prompt = "Why is the sky blue?".to_string();

    let options = GenerationOptions::default()
        .temperature(0.2)
        .repeat_penalty(1.5)
        .top_k(25)
        .top_p(0.25);

    let res = ollama.generate(GenerationRequest::new(model, prompt).options(options)).await;

    if let Ok(res) = res {
        println!("{}", res.response);
    }

}