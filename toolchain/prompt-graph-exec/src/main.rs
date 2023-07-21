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
