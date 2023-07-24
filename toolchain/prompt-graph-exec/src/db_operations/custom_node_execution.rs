use log::debug;
use crate::db_operations;
use crate::db_operations::{CUSTOM_NODE_EXECUTION_PREFIX};
use prost::Message;
use sled::{Event, Subscriber};
use prompt_graph_core::proto2::ChangeValueWithCounter;

fn custom_node_execution_prefix(branch: u64, counter: u64) -> Vec<u8> {
    db_operations::encode_into_slice((CUSTOM_NODE_EXECUTION_PREFIX, branch, counter)).unwrap()
}

pub fn insert_custom_node_execution(tree: &sled::Tree, change: ChangeValueWithCounter) {
    debug!("Inserting new change: {} {}", change.branch, change.monotonic_counter);
    // debug_assert!(change.filled_values.len() > 0);
    let counter = change.monotonic_counter;
    let branch = change.branch;
    // TODO: update the head counter (for the fact this change has been executed)
    let _ = tree.insert(custom_node_execution_prefix(branch, counter), change.encode_to_vec());
}

pub fn get_custom_node_execution(tree: &sled::Tree, branch: u64, counter: u64) -> Option<ChangeValueWithCounter> {
    tree.get(custom_node_execution_prefix(branch, counter)).unwrap()
        .map(|c| ChangeValueWithCounter::decode(c.as_ref()).unwrap())
}