/**
 * Type declarations for TypeScript agents executed inside the Chidori runtime.
 *
 * These are authoring-time types only. The runtime injects the concrete
 * `chidori` host object when it evaluates an agent or tool module.
 */

export type AgentJson =
  | null
  | boolean
  | number
  | string
  | AgentJson[]
  | { [key: string]: AgentJson };

export type JsonObject = { [key: string]: AgentJson };

export interface JsonSchema {
  type?: "object" | "array" | "string" | "number" | "integer" | "boolean" | "null";
  description?: string;
  properties?: Record<string, JsonSchema>;
  items?: JsonSchema;
  required?: string[];
  enum?: AgentJson[];
  default?: AgentJson;
  additionalProperties?: boolean | JsonSchema;
  [keyword: string]: unknown;
}

export interface ToolDefinition {
  name: string;
  description: string;
  parameters: JsonSchema & { type: "object" };
}

export type PromptStreamType = "progress" | "draft" | "subagent" | "final" | (string & {});

export interface PromptOptions {
  type?: PromptStreamType;
  system?: string;
  model?: string;
  maxTokens?: number;
  max_tokens?: number;
  maxTurns?: number;
  max_turns?: number;
  temperature?: number;
  tools?: string[];
  format?: "json" | (string & {});
  stream?: boolean;
}

export interface InputOptions {
  type?: string;
  default?: string;
  choices?: string[];
}

export interface ParallelOptions {
  concurrency?: number;
}

export interface RetryOptions {
  attempts?: number;
  delayMs?: number;
  backoff?: "fixed" | "exponential";
}

export interface TryCallResult<T> {
  ok: boolean;
  value?: T;
  error?: string;
}

export interface HttpRequestOptions {
  method?: "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD";
  headers?: Record<string, string>;
  query?: Record<string, string | number | boolean>;
  body?: AgentJson | string;
  timeoutMs?: number;
}

export interface HttpResponse {
  status: number;
  headers: Record<string, string>;
  body: AgentJson | string | null;
}

export interface TemplateOptions {
  source?: "file" | "inline";
}

export type MemoryAction = "get" | "set" | "delete" | "list" | "clear";

export type WorkspaceFileStatus = "complete" | "writing" | "failed";

export interface WorkspaceEntry {
  path: string;
  status: WorkspaceFileStatus;
  sha256: string;
  bytes: number;
  language?: string | null;
  attempt?: number | null;
  updatedAt?: string | null;
}

export interface WorkspaceListOptions {
  completeOnly?: boolean;
}

export interface WorkspaceWriteOptions {
  language?: string;
}

export interface WorkspaceHost {
  list(options?: WorkspaceListOptions): Promise<WorkspaceEntry[]>;
  read(path: string): Promise<string>;
  write(path: string, content: string, options?: WorkspaceWriteOptions): Promise<WorkspaceEntry>;
  delete(path: string, reason?: string): Promise<void>;
  remove(path: string, reason?: string): Promise<void>;
  manifest(): Promise<AgentJson>;
}

export interface ExecOptions {
  timeoutMs?: number;
  fuel?: number;
  function?: string;
  args?: number[];
  memoryPages?: number;
}

export type TypeScriptImportPolicy = "none" | "relative" | "project";
export type DatePolicy = "disabled" | "fixed" | "host";
export type RandomPolicy = "disabled" | "seeded" | "host";
export type MapSetSnapshotPolicy = "reject" | "serialize";

export interface RuntimePolicyConfig {
  typescript?: {
    imports?: TypeScriptImportPolicy;
  };
  runtime?: {
    date?: DatePolicy;
    random?: RandomPolicy;
  };
  snapshot?: {
    mapsSets?: MapSetSnapshotPolicy;
  };
}

export interface Chidori {
  workspace: WorkspaceHost;
  prompt(text: string, options?: PromptOptions): Promise<string>;
  input(message: string, options?: InputOptions): Promise<string>;
  callAgent<TInput extends AgentJson = JsonObject, TOutput extends AgentJson = AgentJson>(
    path: string,
    input?: TInput,
  ): Promise<TOutput>;
  tool<TArgs extends JsonObject = JsonObject, TResult extends AgentJson = AgentJson>(
    name: string,
    args?: TArgs,
  ): Promise<TResult>;
  parallel<TTasks extends readonly (() => Promise<unknown>)[]>(
    tasks: TTasks,
    options?: ParallelOptions,
  ): Promise<{ [Index in keyof TTasks]: Awaited<ReturnType<TTasks[Index]>> }>;
  retry<T>(fn: () => Promise<T>, options?: RetryOptions): Promise<T>;
  tryCall<T>(fn: () => Promise<T>): Promise<TryCallResult<T>>;
  http(url: string, options?: HttpRequestOptions): Promise<HttpResponse>;
  template(pathOrText: string, vars?: JsonObject, options?: TemplateOptions): Promise<string>;
  log(message: string, fields?: JsonObject): Promise<void>;
  memory<T extends AgentJson = AgentJson>(
    action: MemoryAction,
    key?: string,
    value?: T,
    options?: JsonObject,
  ): Promise<T | AgentJson[] | null>;
  checkpoint(label?: string, data?: AgentJson): Promise<void>;
  execJs<T extends AgentJson = AgentJson>(source: string, options?: ExecOptions): Promise<T>;
  execPython<T extends AgentJson = AgentJson>(source: string, options?: ExecOptions): Promise<T>;
  execWasm<T extends AgentJson = AgentJson>(source: string, options?: ExecOptions): Promise<T>;
}

export type AgentFunction<TInput extends AgentJson = JsonObject, TOutput extends AgentJson = AgentJson> = (
  input: TInput,
  chidori: Chidori,
) => Promise<TOutput>;

export type ToolFunction<TArgs extends JsonObject = JsonObject, TResult extends AgentJson = AgentJson> = (
  args: TArgs,
  chidori: Chidori,
) => Promise<TResult>;
