//! Opt-in local content-addressed prompt cache (Phase 3 of
//! `docs/context-management.md`).
//!
//! Keyed on the versioned request digest (`host_core::prompt_request_digest`)
//! over the fully assembled request, so an exact repeat of a prompt — same
//! model, system, tools, messages, and cache layout — can be served locally
//! without calling the provider at all. The cache lives entirely on the live
//! path: replay short-circuits to the call log before the cache is ever
//! consulted, and a cache hit is recorded as a normal `CallRecord`, so two
//! runs issuing an identical prompt record identical results whether or not
//! the second paid the provider.
//!
//! Enabled by setting `CHIDORI_PROMPT_CACHE_DIR` to a directory path; one
//! JSON file per digest (the `llm_response_to_json` form, shared by the text
//! and structured prompt paths). Disabled (the default) this module is inert.

use serde_json::Value;
use std::path::{Path, PathBuf};

fn cache_dir() -> Option<PathBuf> {
    let dir = std::env::var("CHIDORI_PROMPT_CACHE_DIR").ok()?;
    let dir = dir.trim();
    if dir.is_empty() {
        return None;
    }
    Some(PathBuf::from(dir))
}

/// Whether the local prompt cache is enabled for this process.
pub fn enabled() -> bool {
    cache_dir().is_some()
}

/// Look up a previously stored response JSON by request digest. Any read or
/// parse failure is a miss — the cache can only ever skip a provider call,
/// never fail one.
pub fn lookup(digest: &str) -> Option<Value> {
    lookup_in(&cache_dir()?, digest)
}

/// Store a successful response JSON under its request digest.
pub fn store(digest: &str, response: &Value) {
    if let Some(dir) = cache_dir() {
        store_in(&dir, digest, response);
    }
}

fn lookup_in(dir: &Path, digest: &str) -> Option<Value> {
    let raw = std::fs::read_to_string(dir.join(format!("{digest}.json"))).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Best-effort store: the write is atomic (temp file + rename) so concurrent
/// runs never observe a torn entry, and any I/O failure is silently ignored.
fn store_in(dir: &Path, digest: &str, response: &Value) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let tmp = dir.join(format!(".{digest}.{}.tmp", std::process::id()));
    let path = dir.join(format!("{digest}.json"));
    if std::fs::write(&tmp, response.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn store_then_lookup_roundtrips_and_misses_are_none() {
        let dir =
            std::env::temp_dir().join(format!("chidori-prompt-cache-{}", uuid::Uuid::new_v4()));
        let response = json!({ "content": "answer", "blocks": [], "tool_calls": [] });
        assert_eq!(lookup_in(&dir, "deadbeef"), None);
        store_in(&dir, "deadbeef", &response);
        assert_eq!(lookup_in(&dir, "deadbeef"), Some(response));
        assert_eq!(lookup_in(&dir, "0000beef"), None);
        // A torn/corrupt entry is a miss, never an error.
        std::fs::write(dir.join("0bad.json"), "{not json").unwrap();
        assert_eq!(lookup_in(&dir, "0bad"), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn disabled_without_env_flag() {
        // The suite never sets CHIDORI_PROMPT_CACHE_DIR globally, so the
        // default posture is observable here: inert lookup and store.
        if std::env::var("CHIDORI_PROMPT_CACHE_DIR").is_err() {
            assert!(!enabled());
            assert_eq!(lookup("deadbeef"), None);
            store("deadbeef", &json!({}));
        }
    }
}
