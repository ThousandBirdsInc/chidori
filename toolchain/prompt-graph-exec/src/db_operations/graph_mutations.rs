use std::collections;
use log::debug;
use prost::Message;
use prompt_graph_core::proto2::File;

use crate::db_operations;
use crate::db_operations::{GRAPH_MUTATION_PENDING_PREFIX, GRAPH_MUTATION_RESOLVED_PREFIX};

// =================
// Operations around file changes


// TODO: these should use a separate counter space

fn graph_mutation_prefix_pending(branch: u64, counter: u64) -> Vec<u8> {
    db_operations::encode_into_slice((GRAPH_MUTATION_PENDING_PREFIX, branch, counter)).unwrap()
}

fn graph_mutation_prefix_pending_branch_only(branch: u64) -> Vec<u8> {
    db_operations::encode_into_slice((GRAPH_MUTATION_PENDING_PREFIX, branch)).unwrap()
}

fn graph_mutation_prefix_pending_raw() -> Vec<u8> {
    db_operations::encode_into_slice((GRAPH_MUTATION_PENDING_PREFIX)).unwrap()
}

fn decode_graph_mutation_prefix_pending(src: &[u8]) -> (u64, u64) {
    let (_, branch, counter): (u16, u64, u64) = db_operations::borrow_decode_from_slice(src).unwrap();
    (branch, counter)
}

fn graph_mutation_prefix_resolved(branch: u64, counter: u64) -> Vec<u8> {
    db_operations::encode_into_slice((GRAPH_MUTATION_RESOLVED_PREFIX, branch, counter)).unwrap()
}

fn graph_mutation_prefix_resolved_branch_only(branch: u64) -> Vec<u8> {
    db_operations::encode_into_slice((GRAPH_MUTATION_RESOLVED_PREFIX, branch)).unwrap()
}

fn decode_graph_mutation_prefix_resolved(src: &[u8]) -> (u64, u64) {
    let (_, branch, counter): (u16, u64, u64) = db_operations::borrow_decode_from_slice(src).unwrap();
    (branch, counter)
}

pub fn insert_pending_graph_mutation(tree: &sled::Tree, branch: u64, file: File) {
    debug_assert!(file.nodes.len() > 0);
    // Merges always occur at the head of the current branch - users cannot insert
    // merges at arbitrary points in the execution graph
    let counter = db_operations::update_change_counter_for_branch(tree, branch).unwrap();
    debug!("Inserting pending graph mutation: {} {}", branch, counter);
    tree.insert(graph_mutation_prefix_pending(branch, counter), file.encode_to_vec()).unwrap();
}

pub fn get_next_pending_graph_mutation_on_branch(tree: &sled::Tree, branch: u64) -> Option<((u64, u64), File)> {
    tree.range((collections::Bound::Included(graph_mutation_prefix_pending(branch, 0)), collections::Bound::Included(graph_mutation_prefix_pending(branch, u64::MAX))))
        .next()
        .transpose()
        .unwrap()
        .map(|c|
            (
                decode_graph_mutation_prefix_pending(&c.0.as_ref().to_vec()),
                File::decode(c.1.as_ref()).unwrap())
        )
}

pub fn resolve_pending_graph_mutation(tree: &sled::Tree, branch: u64, counter: u64) {
    debug!("Resolving pending graph mutation: {} {}", branch, counter);
    let change = tree.remove(graph_mutation_prefix_pending(branch, counter)).unwrap().unwrap();
    tree.insert(graph_mutation_prefix_resolved(branch, counter), change).unwrap();
}

pub fn scan_all_file_mutations_on_branch(tree: &sled::Tree, branch: u64) -> impl Iterator<Item = (bool, (u64, u64), File)> {
    let iter_pending = tree.scan_prefix(graph_mutation_prefix_pending_branch_only(branch))
        .map(|c| {
            let c = c.unwrap();
            (false, decode_graph_mutation_prefix_resolved(&c.0.as_ref().to_vec()),
             File::decode(c.1.as_ref()).unwrap())
        });

    let iter_resolved = tree.scan_prefix(graph_mutation_prefix_resolved_branch_only(branch))
        .map(|c| {
            let c = c.unwrap();
            (true, decode_graph_mutation_prefix_resolved(&c.0.as_ref().to_vec()),
            File::decode(c.1.as_ref()).unwrap())
        });

    iter_pending.chain(iter_resolved)
}

pub fn subscribe_to_pending_graph_mutations(tree: &sled::Tree) -> sled::Subscriber {
    tree.watch_prefix(graph_mutation_prefix_pending_raw())
}

#[cfg(test)]
mod tests {
    use sled::Config;

    fn test_() {
        let db = Config::new().temporary(true).flush_every_ms(None).open().unwrap();
        let tree = db.open_tree("test").unwrap();
    }
}
