

pub mod changes;
pub mod branches;
pub mod executing_nodes;
pub mod graph_mutations;
pub mod input_proposals_and_responses;
pub mod state_path_storage;
pub mod playback;
pub mod prompt_library;
mod changes_gluesql_interface;
pub mod custom_node_execution;

#[cfg(feature = "parquet")]
pub mod parquet_serialization;


use std::ops;
use std::convert::TryInto;
use std::fs;
use std::hash::Hasher;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use bincode;
use log::debug;
use prost::Message;
use rand::Fill;
use sled::{IVec, Transactional};

use prompt_graph_core::proto2::{Branch, ChangeValue, ChangeValueWithCounter, DivergentBranch, File, InputProposal, NodeWillExecute, RequestInputProposalResponse, SerializedValue};


// ==========
// Data prefixes. These are numeric and should never be reused or deleted.
// We use u16 to give us 65535 prefixes, which should be more than enough for forever.
pub const CURRENT_EXECUTED_HORIZON_KEY: u16 = 0;
pub const CURRENT_PLAYBACK_FRAME_KEY: u16 = 1;

pub const IS_PLAYING_PREFIX: u16 = 2;

pub const GRAPH_MUTATION_PENDING_PREFIX: u16 = 3;
pub const GRAPH_MUTATION_RESOLVED_PREFIX: u16 = 4;
pub const CHANGE_PENDING_PREFIX: u16 = 5;
pub const CHANGE_RESOLVED_PREFIX: u16 = 6;

pub const INPUT_PROPOSAL_PREFIX: u16 = 7;
pub const INPUT_RESPONSE_PREFIX: u16 = 8;

pub const WILL_EXEC_PENDING_PREFIX: u16 = 9;

pub const BRANCH_PREFIX: u16 = 10;

// BRANCH_COUNTER: used to generate new branch ids
pub const BRANCH_COUNTER: u16 = 11;

// HEAD_COUNTER: used to identify changes that will exist on a given branch
pub const HEAD_COUNTER: u16 = 12;

pub const SEEN_COUNTER: u16 = 13;

pub const STATE_PREFIX: u16 = 14;
pub const STATE_PATH_LOOKUP_PREFIX: u16 = 15;
pub const STATE_EXEC_COUNTER_PREFIX: u16 = 16;
pub const STATE_EXEC_COUNTER_LOOKUP_PREFIX: u16 = 17;

pub const WILL_EXEC_IN_PROGRESS_PREFIX: u16 = 18;
pub const WILL_EXEC_COMPLETE_PREFIX: u16 = 19;

pub const PROMPT_LIBRARY_MUTATION_PREFIX: u16 = 20;
pub const PROMPT_COUNTER_PREFIX: u16 = 21;

pub const CUSTOM_NODE_EXECUTION_PREFIX: u16 = 21;


/// =================
/// These data prefix helpers are effectively our "Tables". Under some circumstances, we
/// use the const prefix values above this section to query across all records in a table,
/// whereas these prefixes are effectively row identifiers.
/// =================

/// Helpers for bincode
pub fn encode_into_slice<E: bincode::enc::Encode>(val: E) -> Result<Vec<u8>, bincode::error::EncodeError>{
    bincode::encode_to_vec(val, bincode::config::standard().with_big_endian())
}

pub fn borrow_decode_from_slice<'a, D: bincode::de::BorrowDecode<'a>>(src: &'a [u8]) -> Result<D, bincode::error::DecodeError>{
    let r = bincode::borrow_decode_from_slice(src, bincode::config::standard().with_big_endian())?;
    Ok(r.0)
}

/// Stores the current executed horizon for a branch.
pub fn change_counter_prefix(branch: u64) -> Vec<u8> {
    encode_into_slice(( HEAD_COUNTER, branch )).unwrap()
}

pub fn branch_counter_prefix() -> Vec<u8> {
    encode_into_slice(( BRANCH_COUNTER, )).unwrap()
}


// =================
// Serialization utils
pub fn bytes_to_u64(bytes: IVec) -> u64 {
    let array: [u8; 8] = bytes.split_at(8).0.try_into().unwrap();;
    u64::from_be_bytes(array)
}

pub fn bytes_to_bool(bytes: IVec) -> bool {
    bytes.to_vec()[0] == 1
}

// =================
// Mutation utils
pub fn util_increment(old: Option<&[u8]>) -> Option<Vec<u8>> {
    let number = match old {
        Some(bytes) => {
            let array: [u8; 8] = bytes.try_into().unwrap();
            let number = u64::from_be_bytes(array);
            number + 1
        }
        None => 0,
    };
    Some(number.to_be_bytes().to_vec())
}


pub fn util_increment_start_1(old: Option<&[u8]>) -> Option<Vec<u8>> {
    let number = match old {
        Some(bytes) => {
            let array: [u8; 8] = bytes.try_into().unwrap();
            let number = u64::from_be_bytes(array);
            number + 1
        }
        None => 1,
    };
    Some(number.to_be_bytes().to_vec())
}


// =================
// Counters for positions in our execution log

pub fn update_change_counter_for_branch(tree: &sled::Tree, branch: u64) -> Option<u64> {
    tree.update_and_fetch(change_counter_prefix(branch), util_increment).unwrap().map(bytes_to_u64)
}

pub fn get_change_counter_for_branch(tree: &sled::Tree, branch: u64) -> u64 {
    tree.get(change_counter_prefix(branch)).unwrap().map(bytes_to_u64)
        .expect("All instances of querying head counter must succeed")
}

// =================
// Tracking the existence of files
pub fn insert_executor_file_existence_by_id(tree: &sled::Tree, id: String) {
    tree.insert(id.into_bytes(), &[1]).unwrap();
}

