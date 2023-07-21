use crate::db_operations;
use crate::db_operations::BRANCH_PREFIX;
use prompt_graph_core::proto2::{Branch, DivergentBranch};
use prost::Message;


// =================
// Handling branch operations


/// Stores metadata about a branch such as its source branch and the counter on that source branch
/// it diverges at
fn branch_prefix(branch: u64) -> Vec<u8> {
    db_operations::encode_into_slice((BRANCH_PREFIX, branch)).unwrap()
}

pub fn branch_prefix_raw() -> Vec<u8> {
    db_operations::encode_into_slice((BRANCH_PREFIX)).unwrap()
}

pub fn create_root_branch(tree: &sled::Tree) {
    let branch = Branch {
        id: 0,
        source_branch_ids: vec![],
        diverges_at_counter: 0,
        divergent_branches: vec![],
    };
    tree.insert(db_operations::change_counter_prefix(0), &(0 as u64).to_be_bytes()).unwrap();
    tree.update_and_fetch(branch_prefix(0), |v| {
        if v.is_none() {
            Some(branch.encode_to_vec())
        } else {
            v.map(|v| v.to_vec())
        }
    }).unwrap();
}

pub fn contains_branch(tree: &sled::Tree, branch: u64) -> bool {
    tree.contains_key(branch_prefix(branch)).unwrap()
}

pub fn list_branches(tree: &sled::Tree) -> impl Iterator<Item = Branch> {
    tree.scan_prefix(branch_prefix_raw())
        .map(|c| Branch::decode(c.unwrap().1.as_ref()).unwrap())
}

pub fn get_branch(tree: &sled::Tree, branch: u64) -> Option<Branch> {
    tree.get(branch_prefix(branch)).unwrap()
        .map(|c| Branch::decode(c.as_ref()).unwrap())
}

pub fn create_branch(tree: &sled::Tree, source_branch_id: u64, diverges_at_counter: u64) -> u64 {
    // 0 as the root branch is implicit
    let source_branch = get_branch(tree, source_branch_id).expect("source branch not found");

    // TODO: do we update the branch with its immediate children and what counter they diverged at?
    // TODO: then in the executor when a change comes, we get its branch, and then we find those diverging branches
    let mut source_branch_ids = source_branch.source_branch_ids.clone();
    source_branch_ids.push(source_branch_id);

    let new_branch_id = tree.update_and_fetch(db_operations::branch_counter_prefix(), db_operations::util_increment_start_1).unwrap().map(db_operations::bytes_to_u64).unwrap();
    // Store the new branch and include a reference to the source branch
    let branch = Branch {
        id: new_branch_id,
        source_branch_ids,
        diverges_at_counter,
        divergent_branches: vec![],
    };
    tree.insert(branch_prefix(new_branch_id), branch.encode_to_vec()).unwrap();

    // We set the head counter for this branch to the counter at which it diverged
    tree.insert(db_operations::change_counter_prefix(new_branch_id), &diverges_at_counter.to_be_bytes()).unwrap();

    // Update the source branch to include the new branch as a divergent branch
    tree.fetch_and_update(branch_prefix(source_branch_id), |v| {
        let mut source_branch = Branch::decode(v.unwrap().as_ref()).unwrap();
        source_branch.divergent_branches.push(DivergentBranch {
            branch: new_branch_id,
            diverges_at_counter,
        });
        Some(source_branch.encode_to_vec())
    }).unwrap();

    new_branch_id
}

#[cfg(test)]
mod tests {
    use sled::Config;

    fn test_() {
        let db = Config::new().temporary(true).flush_every_ms(None).open().unwrap();
        let tree = db.open_tree("test").unwrap();
    }
}
