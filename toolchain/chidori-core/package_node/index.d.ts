import { ChatCompletionReq }  from "./types/ChatCompletionReq"

declare module '@1kbirds/chidori-core' {
  export function std_code_rustpython_source_code_run_python(source: string): any;

  export function std_ai_llm_openai_batch(api_key: string, payload: ChatCompletionReq): Promise<any>;
}
