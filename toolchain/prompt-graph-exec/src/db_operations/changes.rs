use log::debug;
use std::collections;
use crate::db_operations;
use crate::db_operations::{CHANGE_PENDING_PREFIX, CHANGE_RESOLVED_PREFIX};
use prost::Message;
use prompt_graph_core::proto2::{ChangeValue, ChangeValueWithCounter};


// =================
// Operations around changes - moving them between pending and resolved


/// Stores changes that are pending evaluation, "progress" picks up changes here
fn change_prefix_pending(branch: u64, counter: u64) -> Vec<u8> {
    db_operations::encode_into_slice((CHANGE_PENDING_PREFIX, branch, counter)).unwrap()
}

fn change_prefix_pending_only_branch(branch: u64) -> Vec<u8> {
    db_operations::encode_into_slice((CHANGE_PENDING_PREFIX, branch)).unwrap()
}

fn change_prefix_pending_raw() -> Vec<u8> {
    db_operations::encode_into_slice((CHANGE_PENDING_PREFIX)).unwrap()
}

fn decode_change_prefix_pending(src: &[u8]) -> (u64, u64) {
    let (_, branch, counter): (u16, u64, u64) = db_operations::borrow_decode_from_slice(src).unwrap();
    (branch, counter)
}

/// Stores changes that have been resolved, "progress" moves changes here when node executions are defined
fn change_prefix_resolved(branch: u64, counter: u64) -> Vec<u8> {
    db_operations::encode_into_slice((CHANGE_RESOLVED_PREFIX, branch, counter)).unwrap()
}

fn change_prefix_resolved_raw() -> Vec<u8> {
    db_operations::encode_into_slice((CHANGE_RESOLVED_PREFIX)).unwrap()
}

fn decode_change_prefix_resolved(src: &[u8]) -> (u64, u64) {
    let (_, branch, counter): (u16, u64, u64) = db_operations::borrow_decode_from_slice(src).unwrap();
    (branch, counter)
}

/// Push a change with counter into the database
pub fn insert_new_change_value_with_counter(tree: &sled::Tree, change: ChangeValueWithCounter) {
    debug!("Inserting new change: {} {}", change.branch, change.monotonic_counter);
    // debug_assert!(change.filled_values.len() > 0);
    let counter = change.monotonic_counter;
    let branch = change.branch;
    // TODO: update the head counter (for the fact this change has been executed)
    let _ = tree.insert(change_prefix_pending(branch, counter), change.encode_to_vec());
}

pub fn get_next_pending_change_on_branch(tree: &sled::Tree, branch: u64) -> Option<ChangeValueWithCounter> {
    tree.range((collections::Bound::Included(change_prefix_pending(branch, 0)), collections::Bound::Included(change_prefix_pending(branch, u64::MAX))))
        .next()
        .transpose()
        .unwrap()
        .map(|c| ChangeValueWithCounter::decode(c.1.as_ref()).unwrap())
}

/// This change has now transitioned to being resolved, meaning that its state is now stored
/// based on its path
pub fn resolve_pending_change(tree: &sled::Tree, branch: u64, counter: u64) {
    let change = tree.remove(change_prefix_pending(branch, counter)).unwrap().expect("Changes must exist to be resolved");
    tree.insert(change_prefix_resolved(branch, counter), change).unwrap();
}

pub fn scan_all_pending_changes(tree: &sled::Tree) -> impl Iterator<Item = ChangeValueWithCounter> {
    tree.scan_prefix(change_prefix_pending_raw())
        .map(|c| ChangeValueWithCounter::decode(c.unwrap().1.as_ref()).unwrap())
}

pub fn scan_all_resolved_changes(tree: &sled::Tree) -> impl Iterator<Item = ChangeValueWithCounter> {
    tree.scan_prefix(change_prefix_resolved_raw())
        .map(|c| ChangeValueWithCounter::decode(c.unwrap().1.as_ref()).unwrap())
}

pub fn subscribe_to_pending_change_events(tree: &sled::Tree) -> sled::Subscriber {
    tree.watch_prefix(change_prefix_pending_raw())
}

#[cfg(test)]
mod tests {
    use sled::Config;

    fn test_() {
        let db = Config::new().temporary(true).flush_every_ms(None).open().unwrap();
        let tree = db.open_tree("test").unwrap();
    }
}
