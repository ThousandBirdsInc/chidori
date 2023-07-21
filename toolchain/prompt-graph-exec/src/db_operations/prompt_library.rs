use std::collections;
use std::collections::HashMap;
use log::debug;
use crate::db_operations;
use crate::db_operations::{PROMPT_COUNTER_PREFIX, PROMPT_LIBRARY_MUTATION_PREFIX};
use prost::Message;
use prompt_graph_core::proto2::{PromptLibraryRecord, UpsertPromptLibraryRecord};


fn prompt_counter_prefix() -> Vec<u8> {
    db_operations::encode_into_slice((PROMPT_COUNTER_PREFIX)).unwrap()
}

fn prompt_library_mutation_prefix(partial_name: String) -> Vec<u8> {
    db_operations::encode_into_slice((PROMPT_LIBRARY_MUTATION_PREFIX, partial_name)).unwrap()
}

fn prompt_library_mutation_prefix_raw() -> Vec<u8> {
    db_operations::encode_into_slice((PROMPT_LIBRARY_MUTATION_PREFIX)).unwrap()
}

pub fn insert_prompt_library_mutation(tree: &sled::Tree, upsert: &UpsertPromptLibraryRecord) {
    let name = upsert.name.clone();

    // if the previous version of a partial exists and is identical, don't add it again
    if let Some(existing) = tree.get(prompt_library_mutation_prefix(name.clone())).unwrap() {
        let existing = PromptLibraryRecord::decode(existing.as_ref()).unwrap();
        if existing.record.unwrap().encode_to_vec() == upsert.encode_to_vec() {
            return;
        }
    }

    let new_prompt_id = tree.update_and_fetch(prompt_counter_prefix(), db_operations::util_increment_start_1).unwrap().map(db_operations::bytes_to_u64).unwrap();
    let rec = PromptLibraryRecord {
        record: Some(upsert.clone()),
        version_counter: new_prompt_id,
    };
    tree.insert(prompt_library_mutation_prefix(name), rec.encode_to_vec()).unwrap();
}

pub fn resolve_all_partials(tree: &sled::Tree) -> HashMap<String, PromptLibraryRecord> {
    let prompt_library_records = tree.scan_prefix(prompt_library_mutation_prefix_raw())
        .map(|c| PromptLibraryRecord::decode(c.unwrap().1.as_ref()).unwrap());
    let mut prompt_library_records_map: HashMap<String, PromptLibraryRecord> = HashMap::new();
    for prompt_library_record in prompt_library_records {
        let name = prompt_library_record.record.as_ref().unwrap().name.clone();
        let version_counter = prompt_library_record.version_counter;
        let existing_version_counter = prompt_library_records_map.get(&name);
        if existing_version_counter.is_none() || existing_version_counter.unwrap().version_counter < version_counter {
            prompt_library_records_map.insert(name, prompt_library_record);
        }
    }
    prompt_library_records_map
}


#[cfg(test)]
mod tests {
    use sled::Config;

    fn test_() {
        let db = Config::new().temporary(true).flush_every_ms(None).open().unwrap();
        let tree = db.open_tree("test").unwrap();
    }
}
