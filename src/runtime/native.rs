#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::policy::{Decision, PolicyCache, PolicyConfig};
use crate::providers::{ContentBlock, LlmRequest, Message, ProviderRegistry, ToolCall, ToolSchema};
use crate::runtime::call_log::{CallLog, CallRecord};
use crate::runtime::context::{InputMode, PendingApproval, PendingInput, RuntimeContext};
use crate::runtime::host_core::{execute_native_tool_call_at_seq, execute_prompt_response};
use crate::runtime::snapshot::PendingHostOperationKind;
use crate::tools::ToolRegistry;

pub const NATIVE_AGENT_CHECKPOINT_FILE: &str = "native_checkpoint.json";

/// Refreshed request state pulled at each tool-loop turn boundary. Mirrors Pi's
/// harness "save point": the host can change the model, system prompt, or tool
/// schemas mid-run, and the change takes effect on the *next* provider request
/// within the same tool loop rather than only on the next top-level run. Each
/// field is optional; `None` keeps the value the run started with.
#[derive(Debug, Clone, Default)]
pub struct TurnSavePoint {
    pub model: Option<String>,
    pub system: Option<Option<String>>,
    pub tool_schemas: Option<Vec<ToolSchema>>,
}

/// Host-supplied callback consulted before each provider request in the tool
/// loop. Returning a `TurnSavePoint` with some fields set overrides those fields
/// for the upcoming request. Kept off `NativeAgentRequest` because it is a live
/// runtime handle, not serializable checkpoint state.
pub type SavePointHook = Arc<dyn Fn() -> TurnSavePoint + Send + Sync>;

pub struct NativeAgentRunner {
    providers: Arc<ProviderRegistry>,
    tokio_rt: Arc<tokio::runtime::Runtime>,
    tools: Arc<ToolRegistry>,
    policy: Arc<PolicyConfig>,
    approvals: Vec<(String, Value)>,
    save_point: Option<SavePointHook>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeAgentRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub system: Option<String>,
    pub temperature: f64,
    pub max_tokens: u64,
    pub tool_schemas: Vec<ToolSchema>,
    pub max_turns: usize,
}

pub struct NativeAgentRunResult {
    pub answer: String,
    pub messages: Vec<Message>,
    pub call_log: CallLog,
    pub run_id: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub paused_approval: Option<NativePendingApproval>,
    pub paused_input: Option<NativePendingInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativePendingApproval {
    pub seq: u64,
    pub call: ToolCall,
    pub approval: PendingApproval,
    #[serde(default)]
    pub batch: Option<PendingBatch>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativePendingInput {
    pub seq: u64,
    pub call: ToolCall,
    pub input: PendingInput,
    #[serde(default)]
    pub batch: Option<PendingBatch>,
}

/// State for a batch of parallel tool calls from a single assistant turn.
///
/// Anthropic's API requires every `tool_use` block in an assistant message to be
/// answered by a `tool_result` block in the immediately following user message.
/// When part of the batch needs an approval or `ask_user` pause, we carry the
/// remaining work in `PendingBatch` so the resume path can finish the batch
/// before flushing a single combined `tool_result` user message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingBatch {
    /// All tool calls in the assistant turn, in their original order.
    pub calls: Vec<ToolCall>,
    /// Per-call results gathered so far. `None` means not yet executed.
    /// On pause, `results[pending_index]` is `None` (the resuming call's result
    /// will be filled in by `resume_*`).
    pub results: Vec<Option<Value>>,
    /// Index of the call that triggered the current pause.
    pub pending_index: usize,
}

enum BatchOutcome {
    Completed,
    Paused {
        paused_approval: Option<NativePendingApproval>,
        paused_input: Option<NativePendingInput>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeAgentCheckpoint {
    pub request: NativeAgentRequest,
    pub messages: Vec<Message>,
    pub call_log: Vec<CallRecord>,
    pub run_id: String,
    pub answer: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub paused_approval: Option<NativePendingApproval>,
    pub paused_input: Option<NativePendingInput>,
}

impl NativeAgentCheckpoint {
    pub fn from_result(request: &NativeAgentRequest, result: &NativeAgentRunResult) -> Self {
        Self {
            request: request.clone(),
            messages: result.messages.clone(),
            call_log: result.call_log.clone().into_records(),
            run_id: result.run_id.clone(),
            answer: result.answer.clone(),
            input_tokens: result.input_tokens,
            output_tokens: result.output_tokens,
            paused_approval: result.paused_approval.clone(),
            paused_input: result.paused_input.clone(),
        }
    }

    pub fn write_to_base_dir(&self, base_dir: impl AsRef<Path>) -> Result<PathBuf> {
        let run_dir = base_dir.as_ref().join(&self.run_id);
        self.write_to_run_dir(&run_dir)?;
        Ok(run_dir)
    }

    pub fn write_to_run_dir(&self, run_dir: impl AsRef<Path>) -> Result<()> {
        let run_dir = run_dir.as_ref();
        fs::create_dir_all(run_dir)?;
        fs::write(
            run_dir.join(NATIVE_AGENT_CHECKPOINT_FILE),
            serde_json::to_vec_pretty(self)?,
        )?;
        fs::write(
            run_dir.join("checkpoint.json"),
            serde_json::to_vec_pretty(&self.call_log)?,
        )?;
        Ok(())
    }

    pub fn read_from_run_dir(run_dir: impl AsRef<Path>) -> Result<Self> {
        let bytes = fs::read(run_dir.as_ref().join(NATIVE_AGENT_CHECKPOINT_FILE))?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

impl NativeAgentRunner {
    pub fn new(
        providers: Arc<ProviderRegistry>,
        tokio_rt: Arc<tokio::runtime::Runtime>,
        tools: Arc<ToolRegistry>,
    ) -> Self {
        Self {
            providers,
            tokio_rt,
            tools,
            policy: PolicyConfig::from_env(),
            approvals: Vec::new(),
            save_point: None,
        }
    }

    pub fn with_policy(mut self, policy: Arc<PolicyConfig>) -> Self {
        self.policy = policy;
        self
    }

    /// Install a save-point hook consulted before each tool-loop turn so the
    /// host can refresh the model / system prompt / tool schemas mid-run.
    pub fn with_save_point(mut self, save_point: SavePointHook) -> Self {
        self.save_point = Some(save_point);
        self
    }

    pub fn with_approvals(mut self, approvals: Vec<(String, Value)>) -> Self {
        self.approvals = approvals;
        self
    }

    pub fn run_pausable(&self, request: NativeAgentRequest) -> Result<NativeAgentRunResult> {
        let ctx = RuntimeContext::new();
        ctx.set_input_mode(InputMode::Pause);
        self.run_with_context(request, ctx)
    }

    pub fn run_with_context(
        &self,
        request: NativeAgentRequest,
        ctx: RuntimeContext,
    ) -> Result<NativeAgentRunResult> {
        self.run_with_state(request, ctx, String::new(), 0, 0)
    }

    pub fn resume_approved_tool(
        &self,
        checkpoint: NativeAgentCheckpoint,
    ) -> Result<NativeAgentRunResult> {
        let ctx = RuntimeContext::with_existing_call_log(
            checkpoint.run_id.clone(),
            checkpoint.call_log.clone(),
        );
        self.resume_approved_tool_with_context(checkpoint, ctx)
    }

    pub fn resume_approved_tool_with_context(
        &self,
        checkpoint: NativeAgentCheckpoint,
        ctx: RuntimeContext,
    ) -> Result<NativeAgentRunResult> {
        let pending = checkpoint
            .paused_approval
            .clone()
            .ok_or_else(|| anyhow::anyhow!("native checkpoint has no pending approval"))?;
        let tool_result = execute_native_tool_call_at_seq(
            &ctx,
            pending.seq,
            &self.tools,
            &pending.call.name,
            pending.call.input.clone(),
        )?;
        self.resume_with_tool_result(checkpoint, ctx, pending.call, tool_result, pending.batch)
    }

    pub fn resume_answered_input(
        &self,
        checkpoint: NativeAgentCheckpoint,
        answer: impl Into<String>,
    ) -> Result<NativeAgentRunResult> {
        let ctx = RuntimeContext::with_existing_call_log(
            checkpoint.run_id.clone(),
            checkpoint.call_log.clone(),
        );
        self.resume_answered_input_with_context(checkpoint, answer, ctx)
    }

    pub fn resume_answered_input_with_context(
        &self,
        checkpoint: NativeAgentCheckpoint,
        answer: impl Into<String>,
        ctx: RuntimeContext,
    ) -> Result<NativeAgentRunResult> {
        let pending = checkpoint
            .paused_input
            .clone()
            .ok_or_else(|| anyhow::anyhow!("native checkpoint has no pending input"))?;
        let tool_result = serde_json::json!({ "answer": answer.into() });
        ctx.record_call(CallRecord {
            seq: pending.seq,
            parent_seq: None,
            function: "tool".to_string(),
            args: serde_json::json!({
                "name": pending.call.name,
                "kwargs": pending.call.input,
                "tool_use_id": pending.call.id,
            }),
            result: tool_result.clone(),
            duration_ms: 0,
            token_usage: None,
            timestamp: Utc::now(),
            error: None,
        });
        self.resume_with_tool_result(checkpoint, ctx, pending.call, tool_result, pending.batch)
    }

    fn resume_with_tool_result(
        &self,
        checkpoint: NativeAgentCheckpoint,
        ctx: RuntimeContext,
        call: ToolCall,
        tool_result: Value,
        batch: Option<PendingBatch>,
    ) -> Result<NativeAgentRunResult> {
        let mut request = checkpoint.request.clone();
        let mut messages = checkpoint.messages.clone();
        let answer = checkpoint.answer.clone();
        let input_tokens = checkpoint.input_tokens;
        let output_tokens = checkpoint.output_tokens;

        let policy_cache = Arc::new(StdMutex::new(PolicyCache::default()));
        for (target, args) in &self.approvals {
            policy_cache.lock().unwrap().approve(target, args);
        }

        // Old checkpoints (or single-tool turns) carry no batch — treat as a
        // one-call batch so we still flush a proper user{tool_result} message.
        let mut batch = batch.unwrap_or_else(|| PendingBatch {
            calls: vec![call.clone()],
            results: vec![None],
            pending_index: 0,
        });
        if batch.pending_index >= batch.calls.len() || batch.results.len() != batch.calls.len() {
            anyhow::bail!("resumed batch has inconsistent pending_index/results state");
        }
        batch.results[batch.pending_index] = Some(tool_result);
        batch.pending_index += 1;

        match self.process_batch(&ctx, &mut messages, batch, &policy_cache)? {
            BatchOutcome::Completed => {
                request.messages = messages;
                self.run_with_state(request, ctx, answer, input_tokens, output_tokens)
            }
            BatchOutcome::Paused {
                paused_approval,
                paused_input,
            } => Ok(self.result(
                &ctx,
                answer,
                messages,
                input_tokens,
                output_tokens,
                paused_approval,
                paused_input,
            )),
        }
    }

    fn run_with_state(
        &self,
        request: NativeAgentRequest,
        ctx: RuntimeContext,
        mut answer: String,
        mut input_tokens: u64,
        mut output_tokens: u64,
    ) -> Result<NativeAgentRunResult> {
        let mut messages = request.messages;
        let policy_cache = Arc::new(StdMutex::new(PolicyCache::default()));
        for (target, args) in &self.approvals {
            policy_cache.lock().unwrap().approve(target, args);
        }

        // Request state that the save-point hook may refresh between turns. These
        // start from the run's request and are overridden per-turn when the host
        // changes model / system / tools mid-run (Pi-style save points).
        let mut current_model = request.model.clone();
        let mut current_system = request.system.clone();
        let mut current_tools = request.tool_schemas.clone();

        for turn in 0..request.max_turns {
            // Pull the latest save point before issuing this turn's request, so a
            // mid-run config change takes effect on the next provider request
            // inside the same tool loop, not just the next top-level run.
            if let Some(save_point) = &self.save_point {
                let refreshed = save_point();
                if let Some(model) = refreshed.model {
                    current_model = model;
                }
                if let Some(system) = refreshed.system {
                    current_system = system;
                }
                if let Some(tools) = refreshed.tool_schemas {
                    current_tools = tools;
                }
            }
            let prompt_type = if turn == 0 { "analysis" } else { "final" };
            let llm_request = LlmRequest {
                model: current_model.clone(),
                messages: messages.clone(),
                system: current_system.clone(),
                temperature: request.temperature,
                max_tokens: request.max_tokens,
                tools: current_tools.clone(),
            };
            let response = execute_prompt_response(
                &ctx,
                &self.providers,
                &self.tokio_rt,
                llm_request,
                serde_json::json!({
                    "model": current_model.clone(),
                    "type": prompt_type,
                    "messages": messages.clone(),
                    "tools": current_tools.clone(),
                }),
                Some(prompt_type.to_string()),
            )?;
            input_tokens += response.input_tokens;
            output_tokens += response.output_tokens;
            if !response.content.is_empty() {
                answer.push_str(&response.content);
            } else {
                answer.push_str(&collect_text_blocks(&response.blocks));
            }
            let tool_calls = response.tool_calls.clone();
            messages.push(Message::assistant_blocks(response.blocks));
            if tool_calls.is_empty() {
                return Ok(self.result(
                    &ctx,
                    answer,
                    messages,
                    input_tokens,
                    output_tokens,
                    None,
                    None,
                ));
            }

            let batch = PendingBatch {
                results: vec![None; tool_calls.len()],
                calls: tool_calls,
                pending_index: 0,
            };
            match self.process_batch(&ctx, &mut messages, batch, &policy_cache)? {
                BatchOutcome::Completed => {}
                BatchOutcome::Paused {
                    paused_approval,
                    paused_input,
                } => {
                    return Ok(self.result(
                        &ctx,
                        answer,
                        messages,
                        input_tokens,
                        output_tokens,
                        paused_approval,
                        paused_input,
                    ));
                }
            }
        }

        anyhow::bail!("native agent exceeded maximum tool iterations")
    }

    /// Process the batch of tool calls from one assistant turn. On success,
    /// pushes a single `user` message holding `tool_result` blocks for **every**
    /// `tool_use` in the originating assistant message. On pause, the partial
    /// batch state is returned in `PendingBatch` so the resume path can finish
    /// the remaining calls before flushing.
    fn process_batch(
        &self,
        ctx: &RuntimeContext,
        messages: &mut Vec<Message>,
        mut batch: PendingBatch,
        policy_cache: &Arc<StdMutex<PolicyCache>>,
    ) -> Result<BatchOutcome> {
        while batch.pending_index < batch.calls.len() {
            let i = batch.pending_index;
            let call = batch.calls[i].clone();
            let seq = ctx.next_seq();

            if call.name == "ask_user" {
                let prompt = call
                    .input
                    .get("question")
                    .and_then(Value::as_str)
                    .unwrap_or("The agent needs input to continue.")
                    .to_string();
                let input = PendingInput {
                    seq,
                    prompt: prompt.clone(),
                };
                ctx.begin_host_operation_with_function(
                    seq,
                    PendingHostOperationKind::Input,
                    Some("input".to_string()),
                    serde_json::json!({
                        "prompt": prompt,
                        "tool_use_id": call.id.clone(),
                        "name": call.name.clone(),
                        "input": call.input.clone(),
                    }),
                );
                return Ok(BatchOutcome::Paused {
                    paused_approval: None,
                    paused_input: Some(NativePendingInput {
                        seq,
                        call,
                        input,
                        batch: Some(batch),
                    }),
                });
            }

            let target = format!("tool:{}", call.name);
            let (decision, reason) = self.policy.decide(&target, &call.input);
            match decision {
                Decision::AlwaysAllow => {}
                Decision::NeverAllow => {
                    anyhow::bail!(
                        "policy: `{}` denied{}",
                        target,
                        reason.map(|r| format!(" ({})", r)).unwrap_or_default()
                    );
                }
                Decision::AskBefore => {
                    let approved = policy_cache
                        .lock()
                        .unwrap()
                        .is_approved(&target, &call.input);
                    if !approved {
                        let approval = PendingApproval {
                            target,
                            args: call.input.clone(),
                            reason,
                        };
                        ctx.begin_host_operation_with_function(
                            seq,
                            PendingHostOperationKind::PolicyApproval,
                            Some("approval".to_string()),
                            serde_json::json!({
                                "target": approval.target.clone(),
                                "args": approval.args.clone(),
                                "reason": approval.reason.clone(),
                                "tool_use_id": call.id.clone(),
                                "name": call.name.clone(),
                                "input": call.input.clone(),
                            }),
                        );
                        return Ok(BatchOutcome::Paused {
                            paused_approval: Some(NativePendingApproval {
                                seq,
                                call,
                                approval,
                                batch: Some(batch),
                            }),
                            paused_input: None,
                        });
                    }
                }
            }

            let tool_result = execute_native_tool_call_at_seq(
                ctx,
                seq,
                &self.tools,
                &call.name,
                call.input.clone(),
            )?;
            batch.results[i] = Some(tool_result);
            batch.pending_index = i + 1;
        }

        let mut content = Vec::with_capacity(batch.calls.len());
        for (call, result_opt) in batch.calls.iter().zip(batch.results.iter()) {
            let result = result_opt.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "batch completed without a result for tool_use_id {}",
                    call.id
                )
            })?;
            content.push(ContentBlock::ToolResult {
                tool_use_id: call.id.clone(),
                content: result.to_string(),
                is_error: result.get("error").is_some(),
            });
        }
        messages.push(Message {
            role: "user".to_string(),
            content,
        });
        Ok(BatchOutcome::Completed)
    }

    fn result(
        &self,
        ctx: &RuntimeContext,
        answer: String,
        messages: Vec<Message>,
        input_tokens: u64,
        output_tokens: u64,
        paused_approval: Option<NativePendingApproval>,
        paused_input: Option<NativePendingInput>,
    ) -> NativeAgentRunResult {
        NativeAgentRunResult {
            answer,
            messages,
            call_log: ctx.call_log(),
            run_id: ctx.run_id(),
            input_tokens,
            output_tokens,
            paused_approval,
            paused_input,
        }
    }
}

fn collect_text_blocks(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{LlmProvider, LlmResponse, TokenSink};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ToolThenDoneProvider {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmProvider for ToolThenDoneProvider {
        fn supports_model(&self, _model: &str) -> bool {
            true
        }

        async fn send(&self, _request: &LlmRequest) -> Result<LlmResponse> {
            let call_index = self.calls.fetch_add(1, Ordering::SeqCst);
            if call_index == 0 {
                return Ok(LlmResponse {
                    content: String::new(),
                    blocks: vec![ContentBlock::ToolUse {
                        id: "call_1".to_string(),
                        name: "echo".to_string(),
                        input: serde_json::json!({ "value": 42 }),
                    }],
                    tool_calls: vec![ToolCall {
                        id: "call_1".to_string(),
                        name: "echo".to_string(),
                        input: serde_json::json!({ "value": 42 }),
                    }],
                    stop_reason: "tool_use".to_string(),
                    input_tokens: 1,
                    output_tokens: 2,
                });
            }
            Ok(LlmResponse {
                content: "done".to_string(),
                blocks: vec![ContentBlock::Text {
                    text: "done".to_string(),
                }],
                tool_calls: Vec::new(),
                stop_reason: "end_turn".to_string(),
                input_tokens: 3,
                output_tokens: 4,
            })
        }

        async fn stream(
            &self,
            request: &LlmRequest,
            on_delta: &mut TokenSink,
        ) -> Result<LlmResponse> {
            let response = self.send(request).await?;
            if !response.content.is_empty() {
                on_delta(&response.content);
            }
            Ok(response)
        }
    }

    struct AskThenDoneProvider {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmProvider for AskThenDoneProvider {
        fn supports_model(&self, _model: &str) -> bool {
            true
        }

        async fn send(&self, _request: &LlmRequest) -> Result<LlmResponse> {
            let call_index = self.calls.fetch_add(1, Ordering::SeqCst);
            if call_index == 0 {
                return Ok(LlmResponse {
                    content: String::new(),
                    blocks: vec![ContentBlock::ToolUse {
                        id: "ask_1".to_string(),
                        name: "ask_user".to_string(),
                        input: serde_json::json!({ "question": "Which file?" }),
                    }],
                    tool_calls: vec![ToolCall {
                        id: "ask_1".to_string(),
                        name: "ask_user".to_string(),
                        input: serde_json::json!({ "question": "Which file?" }),
                    }],
                    stop_reason: "tool_use".to_string(),
                    input_tokens: 1,
                    output_tokens: 1,
                });
            }
            Ok(LlmResponse {
                content: "answered".to_string(),
                blocks: vec![ContentBlock::Text {
                    text: "answered".to_string(),
                }],
                tool_calls: Vec::new(),
                stop_reason: "end_turn".to_string(),
                input_tokens: 2,
                output_tokens: 3,
            })
        }
    }

    #[test]
    fn native_agent_runner_executes_tool_loop_with_chidori_call_log() {
        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(ToolThenDoneProvider {
            calls: AtomicUsize::new(0),
        }));
        let mut tools = ToolRegistry::new();
        tools.register_native("echo", "Echo input", Vec::new(), Ok);
        let runner = NativeAgentRunner::new(
            Arc::new(providers),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
            Arc::new(tools),
        );

        let result = runner
            .run_pausable(NativeAgentRequest {
                model: "test-model".to_string(),
                messages: vec![Message::user_text("use a tool")],
                system: None,
                temperature: 0.0,
                max_tokens: 100,
                tool_schemas: Vec::new(),
                max_turns: 4,
            })
            .unwrap();

        assert_eq!(result.answer, "done");
        assert!(result.paused_approval.is_none());
        assert!(result.paused_input.is_none());
        assert_eq!(result.input_tokens, 4);
        assert_eq!(result.output_tokens, 6);
        let records = result.call_log.into_records();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].function, "prompt");
        assert_eq!(records[1].function, "tool");
        assert_eq!(records[1].args["name"], "echo");
        assert_eq!(records[2].function, "prompt");
    }

    /// Records the model of every provider request so a test can assert which
    /// model each tool-loop turn used. First call returns a tool use; second
    /// returns done.
    struct ModelRecordingProvider {
        calls: AtomicUsize,
        models: Arc<StdMutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for ModelRecordingProvider {
        fn supports_model(&self, _model: &str) -> bool {
            true
        }

        async fn send(&self, request: &LlmRequest) -> Result<LlmResponse> {
            self.models.lock().unwrap().push(request.model.clone());
            let call_index = self.calls.fetch_add(1, Ordering::SeqCst);
            if call_index == 0 {
                return Ok(LlmResponse {
                    content: String::new(),
                    blocks: vec![ContentBlock::ToolUse {
                        id: "call_1".to_string(),
                        name: "echo".to_string(),
                        input: serde_json::json!({ "value": 1 }),
                    }],
                    tool_calls: vec![ToolCall {
                        id: "call_1".to_string(),
                        name: "echo".to_string(),
                        input: serde_json::json!({ "value": 1 }),
                    }],
                    stop_reason: "tool_use".to_string(),
                    input_tokens: 1,
                    output_tokens: 1,
                });
            }
            Ok(LlmResponse {
                content: "done".to_string(),
                blocks: vec![ContentBlock::Text {
                    text: "done".to_string(),
                }],
                tool_calls: Vec::new(),
                stop_reason: "end_turn".to_string(),
                input_tokens: 1,
                output_tokens: 1,
            })
        }

        async fn stream(
            &self,
            request: &LlmRequest,
            _on_delta: &mut TokenSink,
        ) -> Result<LlmResponse> {
            self.send(request).await
        }
    }

    #[test]
    fn save_point_refreshes_model_between_tool_loop_turns() {
        let models = Arc::new(StdMutex::new(Vec::new()));
        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(ModelRecordingProvider {
            calls: AtomicUsize::new(0),
            models: Arc::clone(&models),
        }));
        let mut tools = ToolRegistry::new();
        tools.register_native("echo", "Echo input", Vec::new(), Ok);

        // The host flips the model to "model-b" after the first turn. The hook is
        // consulted before each turn, so the second provider request must use it.
        let turn = Arc::new(AtomicUsize::new(0));
        let turn_for_hook = Arc::clone(&turn);
        let save_point: SavePointHook = Arc::new(move || {
            let n = turn_for_hook.fetch_add(1, Ordering::SeqCst);
            TurnSavePoint {
                model: Some(if n == 0 {
                    "model-a".to_string()
                } else {
                    "model-b".to_string()
                }),
                ..TurnSavePoint::default()
            }
        });

        let runner = NativeAgentRunner::new(
            Arc::new(providers),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
            Arc::new(tools),
        )
        .with_save_point(save_point);

        let result = runner
            .run_pausable(NativeAgentRequest {
                model: "original-model".to_string(),
                messages: vec![Message::user_text("use a tool")],
                system: None,
                temperature: 0.0,
                max_tokens: 100,
                tool_schemas: Vec::new(),
                max_turns: 4,
            })
            .unwrap();

        assert_eq!(result.answer, "done");
        let seen = models.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec!["model-a".to_string(), "model-b".to_string()],
            "each tool-loop turn must use the save-point model, not the request model"
        );
    }

    #[test]
    fn native_agent_checkpoint_persists_and_resumes_approved_tool() {
        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(ToolThenDoneProvider {
            calls: AtomicUsize::new(0),
        }));
        let mut tools = ToolRegistry::new();
        tools.register_native("echo", "Echo input", Vec::new(), Ok);
        let runner = NativeAgentRunner::new(
            Arc::new(providers),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
            Arc::new(tools),
        )
        .with_policy(Arc::new(PolicyConfig {
            rules: vec![crate::policy::PolicyRule {
                target: "tool:echo".to_string(),
                decision: Decision::AskBefore,
                match_args: None,
                reason: Some("test approval".to_string()),
            }],
            default: Decision::AlwaysAllow,
            overlay: None,
        }));

        let request = NativeAgentRequest {
            model: "test-model".to_string(),
            messages: vec![Message::user_text("use a tool")],
            system: None,
            temperature: 0.0,
            max_tokens: 100,
            tool_schemas: Vec::new(),
            max_turns: 4,
        };
        let paused = runner.run_pausable(request.clone()).unwrap();
        assert!(paused.paused_approval.is_some());
        let checkpoint = NativeAgentCheckpoint::from_result(&request, &paused);
        let temp = std::env::temp_dir().join(format!(
            "chidori-native-checkpoint-test-{}",
            uuid::Uuid::new_v4()
        ));
        let run_dir = checkpoint.write_to_base_dir(&temp).unwrap();
        let restored = NativeAgentCheckpoint::read_from_run_dir(&run_dir).unwrap();
        std::fs::remove_dir_all(&temp).unwrap();

        let resumed = runner.resume_approved_tool(restored).unwrap();
        assert_eq!(resumed.answer, "done");
        assert!(resumed.paused_approval.is_none());
        let records = resumed.call_log.into_records();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].function, "prompt");
        assert_eq!(records[1].function, "tool");
        assert_eq!(records[1].seq, 2);
        assert_eq!(records[1].args["name"], "echo");
        assert_eq!(records[2].function, "prompt");
        assert_eq!(records[2].seq, 3);
    }

    #[test]
    fn native_agent_checkpoint_resumes_answered_input() {
        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(AskThenDoneProvider {
            calls: AtomicUsize::new(0),
        }));
        let runner = NativeAgentRunner::new(
            Arc::new(providers),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
            Arc::new(ToolRegistry::new()),
        );

        let request = NativeAgentRequest {
            model: "test-model".to_string(),
            messages: vec![Message::user_text("ask me")],
            system: None,
            temperature: 0.0,
            max_tokens: 100,
            tool_schemas: Vec::new(),
            max_turns: 4,
        };
        let paused = runner.run_pausable(request.clone()).unwrap();
        assert!(paused.paused_input.is_some());
        let checkpoint = NativeAgentCheckpoint::from_result(&request, &paused);

        let resumed = runner
            .resume_answered_input(checkpoint, "src/lib.rs")
            .unwrap();
        assert_eq!(resumed.answer, "answered");
        assert!(resumed.paused_input.is_none());
        let records = resumed.call_log.into_records();
        assert_eq!(records.len(), 3);
        assert_eq!(records[1].function, "tool");
        assert_eq!(records[1].args["name"], "ask_user");
        assert_eq!(
            records[1].args["kwargs"],
            serde_json::json!({ "question": "Which file?" })
        );
        assert_eq!(
            records[1].result,
            serde_json::json!({ "answer": "src/lib.rs" })
        );
        assert_eq!(records[2].seq, 3);
    }

    /// A provider whose first response contains three parallel tool_use blocks
    /// (read, edit, read). The middle one (`edit`) is gated by approval. This
    /// reproduces the 400 we used to hit when a mid-batch pause left the
    /// trailing tool_use orphaned.
    struct ThreeParallelToolsProvider {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmProvider for ThreeParallelToolsProvider {
        fn supports_model(&self, _model: &str) -> bool {
            true
        }

        async fn send(&self, request: &LlmRequest) -> Result<LlmResponse> {
            let call_index = self.calls.fetch_add(1, Ordering::SeqCst);
            if call_index == 0 {
                let blocks = vec![
                    ContentBlock::ToolUse {
                        id: "tu_read_a".to_string(),
                        name: "read".to_string(),
                        input: serde_json::json!({ "path": "a.txt" }),
                    },
                    ContentBlock::ToolUse {
                        id: "tu_edit".to_string(),
                        name: "edit".to_string(),
                        input: serde_json::json!({ "path": "b.txt", "content": "x" }),
                    },
                    ContentBlock::ToolUse {
                        id: "tu_read_c".to_string(),
                        name: "read".to_string(),
                        input: serde_json::json!({ "path": "c.txt" }),
                    },
                ];
                let tool_calls = vec![
                    ToolCall {
                        id: "tu_read_a".to_string(),
                        name: "read".to_string(),
                        input: serde_json::json!({ "path": "a.txt" }),
                    },
                    ToolCall {
                        id: "tu_edit".to_string(),
                        name: "edit".to_string(),
                        input: serde_json::json!({ "path": "b.txt", "content": "x" }),
                    },
                    ToolCall {
                        id: "tu_read_c".to_string(),
                        name: "read".to_string(),
                        input: serde_json::json!({ "path": "c.txt" }),
                    },
                ];
                return Ok(LlmResponse {
                    content: String::new(),
                    blocks,
                    tool_calls,
                    stop_reason: "tool_use".to_string(),
                    input_tokens: 1,
                    output_tokens: 1,
                });
            }
            // Validate that on resume, every tool_use in the prior assistant
            // message is matched by a tool_result in the immediately following
            // user message — the exact invariant Anthropic enforces.
            let last_user_with_results = request
                .messages
                .iter()
                .rev()
                .find(|m| {
                    m.role == "user"
                        && m.content
                            .iter()
                            .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
                })
                .expect("resumed request must contain a user message with tool_result blocks");
            let result_ids: std::collections::HashSet<&str> = last_user_with_results
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                    _ => None,
                })
                .collect();
            for expected in ["tu_read_a", "tu_edit", "tu_read_c"] {
                assert!(
                    result_ids.contains(expected),
                    "missing tool_result for {expected}; got {result_ids:?}"
                );
            }
            Ok(LlmResponse {
                content: "all done".to_string(),
                blocks: vec![ContentBlock::Text {
                    text: "all done".to_string(),
                }],
                tool_calls: Vec::new(),
                stop_reason: "end_turn".to_string(),
                input_tokens: 1,
                output_tokens: 1,
            })
        }

        async fn stream(
            &self,
            request: &LlmRequest,
            on_delta: &mut TokenSink,
        ) -> Result<LlmResponse> {
            let response = self.send(request).await?;
            if !response.content.is_empty() {
                on_delta(&response.content);
            }
            Ok(response)
        }
    }

    #[test]
    fn native_agent_pauses_and_resumes_mid_batch_without_orphaned_tool_use() {
        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(ThreeParallelToolsProvider {
            calls: AtomicUsize::new(0),
        }));
        let mut tools = ToolRegistry::new();
        tools.register_native("read", "Read a file", Vec::new(), |args| {
            Ok(serde_json::json!({ "read": args }))
        });
        tools.register_native("edit", "Edit a file", Vec::new(), |args| {
            Ok(serde_json::json!({ "edited": args }))
        });
        let runner = NativeAgentRunner::new(
            Arc::new(providers),
            Arc::new(tokio::runtime::Runtime::new().unwrap()),
            Arc::new(tools),
        )
        .with_policy(Arc::new(PolicyConfig {
            rules: vec![crate::policy::PolicyRule {
                target: "tool:edit".to_string(),
                decision: Decision::AskBefore,
                match_args: None,
                reason: Some("write tool requires approval".to_string()),
            }],
            default: Decision::AlwaysAllow,
            overlay: None,
        }));

        let request = NativeAgentRequest {
            model: "test-model".to_string(),
            messages: vec![Message::user_text("do three things in parallel")],
            system: None,
            temperature: 0.0,
            max_tokens: 100,
            tool_schemas: Vec::new(),
            max_turns: 4,
        };
        let paused = runner.run_pausable(request.clone()).unwrap();
        let pending = paused
            .paused_approval
            .as_ref()
            .expect("must pause on the edit call");
        assert_eq!(pending.call.id, "tu_edit");
        let batch = pending
            .batch
            .as_ref()
            .expect("pause must carry batch state");
        assert_eq!(batch.calls.len(), 3);
        assert_eq!(batch.pending_index, 1);
        // The first read already ran before the pause.
        assert!(batch.results[0].is_some());
        assert!(batch.results[1].is_none());
        assert!(batch.results[2].is_none());

        // At pause time we must NOT have flushed a half-batch user message —
        // otherwise the resumed prompt would carry an orphaned tool_use.
        assert!(
            !paused.messages.iter().any(|m| m.role == "user"
                && m.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolResult { .. }))),
            "no tool_result user message should be pushed mid-batch"
        );

        let checkpoint = NativeAgentCheckpoint::from_result(&request, &paused);
        let resumed = runner.resume_approved_tool(checkpoint).unwrap();
        assert!(resumed.paused_approval.is_none());
        assert!(resumed.paused_input.is_none());
        assert_eq!(resumed.answer, "all done");

        // The resumed message history must contain exactly one user message
        // whose tool_result ids cover all three tool_use ids.
        let result_msg = resumed
            .messages
            .iter()
            .find(|m| {
                m.role == "user"
                    && m.content
                        .iter()
                        .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
            })
            .expect("resumed messages must include batched tool_results");
        let ids: Vec<&str> = result_msg
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["tu_read_a", "tu_edit", "tu_read_c"]);
    }
}
