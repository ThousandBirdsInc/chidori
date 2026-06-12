//! Host-only secret broker.
//!
//! The outer harness (agent-builder grpc server) passes secrets in two parts:
//! the guest-visible `CHIDORI_AGENT_ENV` blob carries an opaque placeholder
//! token per secret (`__CHIDORI_SECRET__<32 hex>__`), while the real values
//! arrive in `CHIDORI_SECRET_ENV` — a JSON map of token → entry that only this
//! host-side module ever reads. Guest JS (and therefore the LLM-authored agent
//! code) never sees a raw secret: `execute_http` substitutes tokens into
//! outbound requests just before they hit the wire, and only when the request
//! host matches the secret's allowlist. A token sent to a non-allowlisted host
//! fails the request rather than leaking the value.
//!
//! `CHIDORI_SECRET_ENV` shape:
//! `{ "<token>": { "key": "OPENAI_API_KEY", "value": "sk-...",
//!                 "allowedHosts": ["api.openai.com", "*.openai.com"],
//!                 "allowAnyHost": false } }`

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::Value;

/// Prefix shared with the agent-builder server's `agent_env.rs` — both sides
/// must agree on the token format.
pub const SECRET_TOKEN_PREFIX: &str = "__CHIDORI_SECRET__";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretEntry {
    /// The env var name the agent knows this secret by (for error messages
    /// and redaction markers — never the value).
    pub key: String,
    pub value: String,
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    #[serde(default)]
    pub allow_any_host: bool,
}

#[derive(Debug, Default)]
pub struct SecretStore {
    by_token: HashMap<String, SecretEntry>,
}

static GLOBAL: OnceLock<SecretStore> = OnceLock::new();

impl SecretStore {
    /// The process-wide store, parsed once from `CHIDORI_SECRET_ENV`. A
    /// missing or malformed env var yields an empty store (no substitution);
    /// malformed JSON is logged because it means secrets the harness intended
    /// to provide will be unusable.
    pub fn global() -> &'static SecretStore {
        GLOBAL.get_or_init(|| match std::env::var("CHIDORI_SECRET_ENV") {
            Ok(raw) if !raw.trim().is_empty() => match Self::from_json(&raw) {
                Ok(store) => store,
                Err(err) => {
                    tracing::error!("invalid CHIDORI_SECRET_ENV, secrets unavailable: {err}");
                    Self::default()
                }
            },
            _ => Self::default(),
        })
    }

    pub fn from_json(raw: &str) -> Result<Self, serde_json::Error> {
        let by_token: HashMap<String, SecretEntry> = serde_json::from_str(raw)?;
        Ok(Self { by_token })
    }

    pub fn is_empty(&self) -> bool {
        self.by_token.is_empty()
    }

    /// True when `text` contains anything that looks like a secret token,
    /// known to this store or not.
    pub fn looks_like_token(text: &str) -> bool {
        text.contains(SECRET_TOKEN_PREFIX)
    }

    /// Replace every known token in `text` with its real value, provided
    /// `host` is allowed for that secret. A token whose secret does not allow
    /// `host` fails the whole request — substituting nothing and returning an
    /// error naming the key and host (never the value).
    pub fn substitute_str(&self, text: &str, host: &str) -> Result<String, String> {
        if !Self::looks_like_token(text) {
            return Ok(text.to_string());
        }
        let mut out = text.to_string();
        for (token, entry) in &self.by_token {
            if !out.contains(token.as_str()) {
                continue;
            }
            if !host_allowed(entry, host) {
                return Err(deny_message(entry, host));
            }
            out = out.replace(token.as_str(), &entry.value);
        }
        Ok(out)
    }

    /// Recursively substitute tokens in every string inside `value`.
    pub fn substitute_value(&self, value: &mut Value, host: &str) -> Result<(), String> {
        match value {
            Value::String(text) => {
                let replaced = self.substitute_str(text, host)?;
                if replaced != *text {
                    *text = replaced;
                }
            }
            Value::Array(items) => {
                for item in items {
                    self.substitute_value(item, host)?;
                }
            }
            Value::Object(map) => {
                for (_, item) in map.iter_mut() {
                    self.substitute_value(item, host)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Strip every secret value this store knows about out of `text`,
    /// replacing it with `[REDACTED:<KEY>]`. Run over response bodies,
    /// response headers, and transport error strings before they are returned
    /// to the guest — those all flow into the durable call log and OTEL.
    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        for entry in self.by_token.values() {
            if entry.value.is_empty() {
                continue;
            }
            if out.contains(entry.value.as_str()) {
                out = out.replace(entry.value.as_str(), &format!("[REDACTED:{}]", entry.key));
            }
        }
        out
    }

    /// Recursively redact secret values from every string inside `value`.
    pub fn redact_value(&self, value: &mut Value) {
        match value {
            Value::String(text) => {
                let redacted = self.redact(text);
                if redacted != *text {
                    *text = redacted;
                }
            }
            Value::Array(items) => {
                for item in items {
                    self.redact_value(item);
                }
            }
            Value::Object(map) => {
                for (_, item) in map.iter_mut() {
                    self.redact_value(item);
                }
            }
            _ => {}
        }
    }

    #[cfg(test)]
    pub fn for_tests(entries: Vec<(String, SecretEntry)>) -> Self {
        Self {
            by_token: entries.into_iter().collect(),
        }
    }
}

fn deny_message(entry: &SecretEntry, host: &str) -> String {
    format!(
        "secret {} is not permitted for host {host} (allowed: {})",
        entry.key,
        if entry.allowed_hosts.is_empty() {
            "none".to_string()
        } else {
            entry.allowed_hosts.join(", ")
        }
    )
}

/// Case-insensitive host match: exact hostname, or `*.suffix` matching any
/// subdomain of `suffix` (but not `suffix` itself — list both to allow both).
fn host_allowed(entry: &SecretEntry, host: &str) -> bool {
    if entry.allow_any_host {
        return true;
    }
    let host = host.to_ascii_lowercase();
    entry.allowed_hosts.iter().any(|pattern| {
        let pattern = pattern.to_ascii_lowercase();
        if let Some(suffix) = pattern.strip_prefix("*.") {
            host.len() > suffix.len() + 1 && host.ends_with(suffix) && {
                let boundary = host.len() - suffix.len() - 1;
                host.as_bytes()[boundary] == b'.'
            }
        } else {
            host == pattern
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn token(id: &str) -> String {
        format!("{SECRET_TOKEN_PREFIX}{id}__")
    }

    fn store() -> SecretStore {
        SecretStore::for_tests(vec![
            (
                token("a1"),
                SecretEntry {
                    key: "OPENAI_API_KEY".into(),
                    value: "sk-real-value".into(),
                    allowed_hosts: vec!["api.openai.com".into()],
                    allow_any_host: false,
                },
            ),
            (
                token("b2"),
                SecretEntry {
                    key: "SLACK_TOKEN".into(),
                    value: "xoxb-slack".into(),
                    allowed_hosts: vec!["*.slack.com".into()],
                    allow_any_host: false,
                },
            ),
            (
                token("c3"),
                SecretEntry {
                    key: "ANY_TOKEN".into(),
                    value: "any-value".into(),
                    allowed_hosts: vec![],
                    allow_any_host: true,
                },
            ),
        ])
    }

    #[test]
    fn parses_camel_case_json() {
        let raw = format!(
            r#"{{"{}": {{"key": "K", "value": "v", "allowedHosts": ["x.com"], "allowAnyHost": false}}}}"#,
            token("d4")
        );
        let store = SecretStore::from_json(&raw).unwrap();
        assert!(!store.is_empty());
    }

    #[test]
    fn substitutes_embedded_token_on_allowed_host() {
        let text = format!("Bearer {}", token("a1"));
        let out = store().substitute_str(&text, "api.openai.com").unwrap();
        assert_eq!(out, "Bearer sk-real-value");
    }

    #[test]
    fn denies_disallowed_host_without_leaking_value() {
        let text = format!("Bearer {}", token("a1"));
        let err = store().substitute_str(&text, "evil.example.com").unwrap_err();
        assert!(err.contains("OPENAI_API_KEY"));
        assert!(err.contains("evil.example.com"));
        assert!(!err.contains("sk-real-value"));
    }

    #[test]
    fn wildcard_matches_subdomain_only() {
        let entry = SecretEntry {
            key: "K".into(),
            value: "v".into(),
            allowed_hosts: vec!["*.slack.com".into()],
            allow_any_host: false,
        };
        assert!(host_allowed(&entry, "api.slack.com"));
        assert!(host_allowed(&entry, "API.SLACK.COM"));
        assert!(!host_allowed(&entry, "slack.com"));
        assert!(!host_allowed(&entry, "notslack.com"));
        assert!(!host_allowed(&entry, "evilslack.com"));
    }

    #[test]
    fn any_host_substitutes_anywhere() {
        let text = token("c3");
        let out = store().substitute_str(&text, "whatever.example").unwrap();
        assert_eq!(out, "any-value");
    }

    #[test]
    fn substitute_value_walks_nested_json() {
        let mut value = json!({
            "headers": { "Authorization": format!("Bearer {}", token("a1")) },
            "list": [token("c3")],
            "n": 7,
        });
        store()
            .substitute_value(&mut value, "api.openai.com")
            .unwrap();
        assert_eq!(value["headers"]["Authorization"], "Bearer sk-real-value");
        assert_eq!(value["list"][0], "any-value");
    }

    #[test]
    fn redacts_values_with_key_names() {
        let out = store().redact("ok sk-real-value and xoxb-slack done");
        assert_eq!(out, "ok [REDACTED:OPENAI_API_KEY] and [REDACTED:SLACK_TOKEN] done");
    }

    #[test]
    fn unknown_token_passes_through() {
        let text = format!("x {} y", token("ffff"));
        let out = store().substitute_str(&text, "api.openai.com").unwrap();
        assert_eq!(out, text);
    }
}
