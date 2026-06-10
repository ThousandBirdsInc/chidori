// The recorder/runtime host bindings double as the pure-Rust engine's effect
// dispatcher (see `rust_engine::run_module`), so compile them whenever the
// `rust-engine` feature is on, not just under test.
#[cfg(any(test, feature = "rust-engine"))]
pub mod bindings;
pub mod check;
pub mod builtins;
pub mod engine;
pub mod resolver;
pub mod snapshot;
pub mod tools;
pub mod transpile;
