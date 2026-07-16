pub mod app_data;
pub mod call_log;
pub mod capability;
pub mod context;
pub mod cost;
pub mod crypto;
pub mod engine;
/// Typed error taxonomy: the pause interrupt and run-failure classification.
pub mod errors;
pub mod host_actor;
/// Detached, durable, addressable agent processes (`chidori.agents.*`).
pub mod host_agent;
pub mod host_branch;
pub mod host_core;
/// OS-level isolation: run an agent in a sandboxed child process and broker its
/// host effects back over a pipe (see `docs/os-isolation-plan.md`).
pub mod isolate;
pub mod memory;
pub mod native;
pub mod otel;
pub mod prompt_cache;
/// Pure-Rust JS engine integration — the only JavaScript engine.
pub mod rust_engine;
pub mod secret_env;
pub mod snapshot;
/// SSRF guard for the guest-facing `http`/`fetch` host effect.
pub mod ssrf;
/// Pluggable persistence for the durable run artifact (journal + blobs).
pub mod store;
/// S3-compatible blob backend for the run store (S3 / R2 / GCS / MinIO).
pub mod store_blob;
pub mod template;
pub mod typescript;
pub mod vfs;
pub mod workspace;
