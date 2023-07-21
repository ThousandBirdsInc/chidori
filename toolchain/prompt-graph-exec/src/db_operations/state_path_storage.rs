use std::collections;
use std::hash::Hasher;
use crate::db_operations;
use crate::db_operations::{branches, STATE_EXEC_COUNTER_LOOKUP_PREFIX, STATE_EXEC_COUNTER_PREFIX, STATE_PATH_LOOKUP_PREFIX, STATE_PREFIX};
use prost::Message;
use prompt_graph_core::proto2::{ChangeValue};

// =================
// Handling interactions with the internal state of our execution

/// Stores a change value at a particular branch, counter, and path.
fn state_prefix(branch: u64, counter: u64, path: u64) -> Vec<u8> {
    db_operations::encode_into_slice((STATE_PREFIX, branch, path, counter)).unwrap()
}

fn state_path_lookup_prefix(path: u64) -> Vec<u8> {
    db_operations::encode_into_slice((STATE_PATH_LOOKUP_PREFIX, path)).unwrap()
}

pub fn state_prefix_raw_with_branch(branch: u64) -> Vec<u8> {
    db_operations::encode_into_slice((STATE_PREFIX, branch)).unwrap()
}

fn decode_state_prefix(src: &[u8]) -> (u64, u64, u64) {
    let (prefix, branch, path, counter): (u16, u64, u64, u64) = db_operations::borrow_decode_from_slice(src).unwrap();
    (branch, counter, path)
}

/// Stores a change value at a particular branch, counter, and path.
pub fn state_exec_counter_prefix(branch: u64, node_name_hash: u64, counter: u64) -> Vec<u8> {
    db_operations::encode_into_slice((STATE_EXEC_COUNTER_PREFIX, branch, node_name_hash, counter)).unwrap()
}

fn state_exec_counter_prefix_raw(branch: u64) -> Vec<u8> {
    db_operations::encode_into_slice((STATE_EXEC_COUNTER_PREFIX, branch)).unwrap()
}

fn state_exec_counter_lookup_prefix(node_name_hash: u64) -> Vec<u8> {
    db_operations::encode_into_slice((STATE_EXEC_COUNTER_LOOKUP_PREFIX, node_name_hash)).unwrap()
}

fn decode_state_exec_counter_prefix(src: &[u8]) -> (u64, u64, u64) {
    let (prefix, branch, node_name_hash, counter): (u16, u64, u64, u64) = db_operations::borrow_decode_from_slice(src).unwrap();
    (branch, node_name_hash, counter)
}

pub fn debug_scan_all_state_counters(tree: &sled::Tree, branch: u64) {
    let all: Vec<_> = tree.scan_prefix(state_exec_counter_prefix_raw(branch))
        .map(|c| {
            let (k, v) = c.unwrap();
            let (branch, node_name, counter) = decode_state_exec_counter_prefix(&k);
            (branch, node_name, counter, db_operations::bytes_to_u64(v))
        }).collect();
    println!("- START debug_scan_all_state_counters");
    for (branch, node_name, counter, v) in &all {
        let p = tree.get(state_exec_counter_lookup_prefix(*node_name)).unwrap().unwrap();
        println!("- ({} {:?} {}) = {}", branch, String::from_utf8_lossy(&p).to_string(), counter, v);
    }
    println!("- END debug_scan_all_state_counters");
}


pub fn state_insert(tree: &sled::Tree, address: &[u8], counter: u64, branch: u64, value: ChangeValue) {
    // We hash addresses using a fixed see XXHash64
    let mut hash = twox_hash::XxHash64::with_seed(0);
    hash.write(address);
    let address_hash = hash.finish();
    // We store this address in a lookup table, so we can find it later
    tree.insert(state_path_lookup_prefix( address_hash), address).unwrap();
    tree.insert(state_prefix( branch, counter, address_hash), value.encode_to_vec()).unwrap();
}


/// When we fetch the latest state of a value, we also query for all underlying execution branches
/// we grab the the parents from the branch.
pub fn state_get(tree: &sled::Tree, address: &[u8], counter: u64, branch: u64) -> Option<(u64, ChangeValue)> {
    // get the latest value, below the target counter
    let mut hash = twox_hash::XxHash64::with_seed(0);
    hash.write(address);
    let address_hash = hash.finish();

    // get the target branch and search all source branches for state relevant to this address
    let source_branch = branches::get_branch(tree, branch).expect("branch not found");
    let search_branches = source_branch.source_branch_ids.iter().cloned().chain(std::iter::once(branch));
    for search_branch in search_branches {
        let found = tree.range((collections::Bound::Included(state_prefix(search_branch, 0, address_hash)), collections::Bound::Included(state_prefix(search_branch, counter, address_hash))))
            .next_back()
            .transpose()
            .unwrap()
            .map(|(k, v) | {
                let (_, counter, _) = decode_state_prefix(k.as_ref());
                (counter, ChangeValue::decode(v.as_ref()).unwrap())
            });
        if found.is_some() {
            return found;
        }
    }
    None
}

pub fn debug_scan_all_state_branch(tree: &sled::Tree, branch: u64) {
    let all: Vec<_> = tree.scan_prefix(state_prefix_raw_with_branch(branch))
        .map(|c| {
            let (k, v) = c.unwrap();
            let (branch, counter, path) = decode_state_prefix(&k);
            (path, counter, ChangeValue::decode(v.as_ref()).unwrap())
        }).collect();
    println!("- START debug_scan_all_state_branch");
    for (path, counter, value) in &all {
        let p = tree.get(state_path_lookup_prefix(*path)).unwrap().unwrap();
        println!("- {} {} {:?}", String::from_utf8_lossy(&p).to_string(), counter, value);
    }
    println!("- END debug_scan_all_state_branch");
}

/// State counters are queried for the "latest" state of their execution
/// otherwise subsequent changes will be run again because they're on new counters
/// but we do need to preserve the history of this execution. Giving us a horizon of execution.
pub fn state_get_count_node_execution(tree: &sled::Tree, node_name: &[u8], counter: u64, branch: u64) -> Option<u64> {
    let mut hash = twox_hash::XxHash64::with_seed(0);
    hash.write(node_name);
    let node_name_hash = hash.finish();
    tree.range((collections::Bound::Included(state_exec_counter_prefix(branch, node_name_hash, 0)),
                collections::Bound::Included(state_exec_counter_prefix(branch, node_name_hash, counter))))
        .next_back()
        .transpose()
        .unwrap()
        .map(|(k, v) | {
            db_operations::bytes_to_u64(v)
        })
}

pub fn state_inc_counter_node_execution(tree: &sled::Tree, node_name: &[u8], counter: u64, branch: u64) -> u64 {
    let mut hash = twox_hash::XxHash64::with_seed(0);
    hash.write(node_name);
    let node_name_hash = hash.finish();
    tree.insert(state_exec_counter_lookup_prefix( node_name_hash), node_name).unwrap();
    tree.update_and_fetch(state_exec_counter_prefix(branch, node_name_hash, counter), db_operations::util_increment_start_1).unwrap().map(db_operations::bytes_to_u64).unwrap()
}


#[cfg(test)]
mod tests {
    use sled::Config;

    fn test_() {
        let db = Config::new().temporary(true).flush_every_ms(None).open().unwrap();
        let tree = db.open_tree("test").unwrap();
    }
}
