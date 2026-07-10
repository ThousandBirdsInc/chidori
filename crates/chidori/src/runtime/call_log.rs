use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A single host function call record for tracing and checkpointing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallRecord {
    /// Monotonically increasing sequence number.
    pub seq: u64,
    /// Sequence number of the enclosing call, when this record was produced
    /// inside another host call's execution (today: a sub-agent invoked via
    /// `call_agent`, whose own host calls nest under it). None for top-level
    /// calls. Lets consumers reconstruct the run as a span tree.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub parent_seq: Option<u64>,
    /// Host function name (e.g. "prompt", "tool", "exec").
    pub function: String,
    /// Arguments passed to the function.
    pub args: Value,
    /// Return value from the function.
    pub result: Value,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// Token usage for LLM calls (None for non-LLM calls).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<TokenUsage>,
    /// When the call started.
    pub timestamp: DateTime<Utc>,
    /// Error message if the call failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Fresh (non-cached) input tokens.
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Input tokens written to the provider prompt cache (billed above base
    /// input rate). Omitted when zero so existing logs round-trip unchanged.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cache_creation_tokens: Option<u64>,
    /// Input tokens served from the provider prompt cache (billed at a steep
    /// discount).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cache_read_tokens: Option<u64>,
}

impl TokenUsage {
    /// Build usage from a provider response, recording cache counts only when
    /// the provider reported any.
    pub fn from_response(response: &crate::providers::LlmResponse) -> Self {
        Self {
            input_tokens: response.input_tokens,
            output_tokens: response.output_tokens,
            cache_creation_tokens: (response.cache_creation_tokens > 0)
                .then_some(response.cache_creation_tokens),
            cache_read_tokens: (response.cache_read_tokens > 0)
                .then_some(response.cache_read_tokens),
        }
    }
}

/// Ordered list of call records forming a checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallLog {
    records: Vec<CallRecord>,
}

impl Default for CallLog {
    fn default() -> Self {
        Self::new()
    }
}

impl CallLog {
    pub fn new() -> Self {
        Self {
            records: Vec::new(),
        }
    }

    pub fn push(&mut self, record: CallRecord) {
        self.records.push(record);
    }

    #[allow(dead_code)]
    pub fn records(&self) -> &[CallRecord] {
        &self.records
    }

    pub fn into_records(self) -> Vec<CallRecord> {
        self.records
    }

    pub fn total_tokens(&self) -> (u64, u64) {
        let mut input = 0;
        let mut output = 0;
        for r in &self.records {
            if let Some(ref usage) = r.token_usage {
                // Cache writes/reads are input tokens the model processed —
                // include them so the total reflects real context size.
                input += usage.input_tokens
                    + usage.cache_creation_tokens.unwrap_or(0)
                    + usage.cache_read_tokens.unwrap_or(0);
                output += usage.output_tokens;
            }
        }
        (input, output)
    }

    pub fn total_duration_ms(&self) -> u64 {
        self.records.iter().map(|r| r.duration_ms).sum()
    }

    /// Walk LLM call records and sum an estimated USD cost based on the
    /// model name stored in each record's args.
    pub fn total_cost_usd(&self) -> f64 {
        use crate::runtime::cost::estimate_cost_usd_with_cache;
        let mut total = 0.0;
        for r in &self.records {
            if r.function != "prompt" {
                continue;
            }
            let Some(usage) = r.token_usage.as_ref() else {
                continue;
            };
            let model = r.args.get("model").and_then(|v| v.as_str()).unwrap_or("");
            total += estimate_cost_usd_with_cache(
                model,
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_creation_tokens.unwrap_or(0),
                usage.cache_read_tokens.unwrap_or(0),
            );
        }
        total
    }

    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(&self.records)
    }
}
