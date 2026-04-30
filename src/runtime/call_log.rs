use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A single host function call record for tracing and checkpointing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallRecord {
    /// Monotonically increasing sequence number.
    pub seq: u64,
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
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Ordered list of call records forming a checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallLog {
    records: Vec<CallRecord>,
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
                input += usage.input_tokens;
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
        use crate::runtime::cost::estimate_cost_usd;
        let mut total = 0.0;
        for r in &self.records {
            if r.function != "prompt" {
                continue;
            }
            let Some(usage) = r.token_usage.as_ref() else {
                continue;
            };
            let model = r
                .args
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            total += estimate_cost_usd(model, usage.input_tokens, usage.output_tokens);
        }
        total
    }

    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(&self.records)
    }
}
