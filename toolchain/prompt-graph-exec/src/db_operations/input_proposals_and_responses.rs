use crate::db_operations;
use crate::db_operations::{changes, INPUT_PROPOSAL_PREFIX, INPUT_RESPONSE_PREFIX};
use prost::Message;
use prompt_graph_core::proto2::{ChangeValueWithCounter, InputProposal, RequestInputProposalResponse};

/// Input proposals are stored here until they are resolved by input responses
fn input_proposal_prefix(branch: u64, s: String, counter: u64) -> Vec<u8> {
    db_operations::encode_into_slice((INPUT_PROPOSAL_PREFIX, branch, s, counter)).unwrap()
}

fn input_proposal_prefix_raw() -> Vec<u8> {
    db_operations::encode_into_slice((INPUT_PROPOSAL_PREFIX)).unwrap()
}

fn input_response_prefix(branch: u64, counter: u64) -> Vec<u8> {
    db_operations::encode_into_slice((INPUT_RESPONSE_PREFIX, branch, counter)).unwrap()
}

fn input_response_prefix_raw() -> Vec<u8> {
    db_operations::encode_into_slice((INPUT_RESPONSE_PREFIX)).unwrap()
}


// =================
// Handling capturing and communicating input proposals and responses to those proposals
pub fn insert_input_proposal(tree: &sled::Tree, input_proposal: InputProposal) {
    tree.insert(input_proposal_prefix(input_proposal.branch, input_proposal.name.clone(), input_proposal.counter), input_proposal.encode_to_vec()).unwrap();
}

pub fn scan_all_input_proposals(tree: &sled::Tree) -> impl Iterator<Item = InputProposal> {
    tree.scan_prefix(input_proposal_prefix_raw())
        .map(|c| InputProposal::decode(c.unwrap().1.as_ref()).unwrap())
}

pub fn insert_input_response(tree: &sled::Tree, input_response: RequestInputProposalResponse) {
    debug_assert!(input_response.changes.len() > 0);
    // TODO: needs to be adapted for branches
    let counter = db_operations::update_change_counter_for_branch(tree, 0).unwrap();
    let change_value_with_counter = ChangeValueWithCounter {
        source_node: "external".to_string(),
        filled_values: input_response.changes.clone(),
        parent_monotonic_counters: vec![],
        monotonic_counter: counter,
        branch: input_response.branch,
    };
    tree.insert(input_response_prefix(input_response.branch, input_response.proposal_counter), input_response.encode_to_vec()).unwrap();
    changes::insert_new_change_value_with_counter(tree, change_value_with_counter);
}

pub fn scan_all_input_responses(tree: &sled::Tree) -> impl Iterator<Item = RequestInputProposalResponse> {
    tree.scan_prefix(input_response_prefix_raw())
        .map(|c| RequestInputProposalResponse::decode(c.unwrap().1.as_ref()).unwrap())
}

pub fn subscribe_to_pending_input_proposals(tree: sled::Tree) -> sled::Subscriber {
    tree.watch_prefix(input_proposal_prefix_raw())
}



#[cfg(test)]
mod tests {
    use sled::Config;

    fn test_() {
        let db = Config::new().temporary(true).flush_every_ms(None).open().unwrap();
        let tree = db.open_tree("test").unwrap();
    }
}
