use crate::db_operations;
use crate::db_operations::EXECUTOR_FILES_PREFIX;

fn executor_files_prefix(id: String) -> Vec<u8> {
    db_operations::encode_into_slice((EXECUTOR_FILES_PREFIX, id)).unwrap()
}

fn executor_files_prefix_raw() -> Vec<u8> {
    db_operations::encode_into_slice(EXECUTOR_FILES_PREFIX).unwrap()
}


// =================
// Tracking the existence of files
pub fn insert_executor_file_existence_by_id(tree: &sled::Tree, id: String) {
    tree.insert(executor_files_prefix(id.clone()), db_operations::encode_into_slice(id).unwrap()).unwrap();
}

pub fn scan_all_executor_files(tree: &sled::Tree) -> impl Iterator<Item = String> {
    tree.scan_prefix(executor_files_prefix_raw())
        .map(|c| db_operations::borrow_decode_from_slice(c.unwrap().1.as_ref()).unwrap() )
}
