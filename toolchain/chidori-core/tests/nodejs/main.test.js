const {
    std_ai_llm_openai_batch,
    std_code_rustpython_source_code_run_python
} = require("../..");

async function delay(ms) {
    // Returns a promise that resolves after "ms" milliseconds
    return new Promise(resolve => setTimeout(resolve, ms));
}

test('std_code_rustpython_source_code_run_python', () => {
    expect(std_code_rustpython_source_code_run_python("x = 2+2")).toEqual({"x": 4});
});



test('std_ai_llm_openai_batch', async () => {
    const result = await std_ai_llm_openai_batch(
        "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",
        {
            frequency_penalty: null,
            logit_bias: null,
            max_tokens: null,
            presence_penalty: null,
            response_format: null,
            seed: null,
            stop: null,
            temperature: null,
            top_p: null,
            user: null,
            model: "gpt-3.5-turbo",
            template_messages: [
                {
                    role: "User",
                    content: "what is the capital of Japan",
                    name: null,
                    function_call: null,
                },
            ],
        }
    );
    result["id"] = "-";
    result["created"] = 0;
    result["usage"] = {};
    result["choices"][0]["text"] = "Hello";
    expect(result).toEqual({
        "choices": [
            {
                "finish_reason": "",
                "index": 0,
                "logprobs": null,
                "text": "Hello",
            }
        ],
        "created": 0,
        "id": "-",
        "model": "gpt-3.5-turbo-0613",
        "object": "chat.completion",
        "usage": {}
    });
});


