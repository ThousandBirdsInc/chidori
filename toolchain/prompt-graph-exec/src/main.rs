/// Prompt Graph Exec combines the functionality of prompt-graph-core into a server implementation.
/// This is suitable for use in a web browser, or as a standalone server. It provides a GRPC API
/// that can manipulate the execution runtime.
///
/// Prompt Graph Exec also provides a module-integration system for importing and exporting
/// packaged functionality from other languages.


mod executor;
mod integrations;
mod runtime_nodes;
mod tonic_runtime;
mod db_operations;

#[macro_use]
extern crate lazy_static;

fn main() {
    env_logger::init();
    tonic_runtime::run_server(String::from("127.0.0.1:9800"), Some(":memory:".to_string()));
}
