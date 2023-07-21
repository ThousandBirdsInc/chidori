use std::path::PathBuf;
use arrow::datatypes::{DataType, Field, Schema};
use prompt_graph_core::proto2::{Branch, ChangeValueWithCounter, File, InputProposal, RequestInputProposalResponse, NodeWillExecuteOnBranch};
use std::sync::Arc;
use arrow::array::{BinaryBuilder, ListBuilder, StringBuilder, UInt64Builder};
use arrow::record_batch::RecordBatch;
use std::fs;
use parquet::file::properties::WriterProperties;
use parquet::arrow::ArrowWriter;
use prost::Message;
use crate::db_operations;
use crate::db_operations::{branches, changes, executing_nodes, graph_mutations, input_proposals_and_responses};

pub const MAX_GRAPHQL_QUERY_SIZE_BYTES: usize = 8000;



// TODO: what queries do we want to run?
//       * change in any individual output value, across time and dimensions
//       * filter to a particular horizon of history across multiple branches
//       * determine how a particular output value has changed across time and dimensions

/// Serialization result includes keys for each table exported and the resulting path locations
/// they have been created at.
pub struct SerializationResult {
    branches_path: PathBuf,
    mutations_path: PathBuf,
    changes_path: PathBuf,
    node_will_executes_path: PathBuf,
    input_proposals_path: PathBuf,
    input_responses_path: PathBuf,
}

pub fn serialize_branches_to_parquet(tree: &sled::Tree, path: PathBuf) -> anyhow::Result<PathBuf> {
    let branches: Vec<_> = tree.scan_prefix(branches::branch_prefix_raw())
        .filter_map(|c| c.ok())
        .map(|(k, v)|
            Branch::decode(v.as_ref()).unwrap()
        ).collect();

    let branch_schema = {
        let field_id = Field::new("id", DataType::UInt64, false);
        let field_source_branch_id = Field::new("item", DataType::UInt64, true);
        let field_source_branch_ids = Field::new("source_branch_ids", DataType::List(Arc::new(field_source_branch_id)), false);
        let field_divergent_branch_id = Field::new("item", DataType::UInt64, true);
        let field_divergent_branch_ids = Field::new("divergent_branch_ids", DataType::List(Arc::new(field_divergent_branch_id)), false);
        let diverges_at_counter = Field::new("diverges_at_counter", DataType::UInt64, false);
        Arc::new(Schema::new(vec![
            field_id,
            field_source_branch_ids,
            field_divergent_branch_ids,
            diverges_at_counter
        ]))
    };

    let branch_record_batch = {
        let mut array_id = UInt64Builder::with_capacity(branches.len());
        let mut array_source_branch_ids = ListBuilder::with_capacity(UInt64Builder::new(), branches.len());
        let mut array_divergent_branch_ids = ListBuilder::with_capacity(UInt64Builder::new(), branches.len());
        let mut array_diverges_at_counter = UInt64Builder::with_capacity(branches.len());
        for branch in branches.into_iter() {
            array_id.append_value(branch.id);
            array_source_branch_ids.append_value(branch.source_branch_ids.into_iter().map(|x| Some(x)));
            array_divergent_branch_ids.append_value(branch.divergent_branches.iter().map(|divergent_branch| Some(divergent_branch.branch)));
            array_diverges_at_counter.append_value(branch.diverges_at_counter);
        }

        RecordBatch::try_new(
            branch_schema.clone(),
            vec![
                Arc::new(array_id.finish()),
                Arc::new(array_source_branch_ids.finish()),
                Arc::new(array_divergent_branch_ids.finish()),
                Arc::new(array_diverges_at_counter.finish())
            ],
        )?
    };

    let mut local_path = path;
    local_path.push("branches.parquet");
    let file = fs::File::create(local_path.clone())?;
    let props = WriterProperties::new();
    let mut writer = ArrowWriter::try_new(file, branch_schema, Some(props))?;
    writer.write(&branch_record_batch)?;
    writer.close()?;
    Ok(local_path)
}


pub fn serialize_mutations_to_parquet(tree: &sled::Tree, path: PathBuf) -> anyhow::Result<PathBuf> {
    let pending_mutations: Vec<_> = tree.scan_prefix(graph_mutations::graph_mutation_prefix_pending_raw())
        .filter_map(|c| c.ok())
        .map(|(k, v)| File::decode(v.as_ref()).unwrap())
        .collect();

    let resolved_mutations: Vec<_> = tree.scan_prefix(graph_mutations::graph_mutation_prefix_resolved_raw())
        .filter_map(|c| c.ok())
        .map(|(k, v)| File::decode(v.as_ref()).unwrap()).collect();

    let mutation_schema = {
        let field_name = Field::new("name", DataType::Utf8, false);

        let field_queries_part = Field::new("item", DataType::Utf8, true);
        let field_queries = Field::new("queries", DataType::List(Arc::new(field_queries_part)), false);


        let field_output = Field::new("output", DataType::Utf8, true);
        let field_status = Field::new("status", DataType::Utf8, false);
        Arc::new(Schema::new(vec![
            field_name,
            field_queries,
            field_output,
            field_status
        ]))
    };

    let mutation_record_batch = {
        let mut array_name = StringBuilder::with_capacity(pending_mutations.len() + resolved_mutations.len(), MAX_GRAPHQL_QUERY_SIZE_BYTES);
        let mut array_queries = ListBuilder::with_capacity(StringBuilder::new(), pending_mutations.len() + resolved_mutations.len());
        let mut array_output = StringBuilder::with_capacity(pending_mutations.len() + resolved_mutations.len(), MAX_GRAPHQL_QUERY_SIZE_BYTES);
        let mut array_status = StringBuilder::with_capacity(pending_mutations.len() + resolved_mutations.len(), MAX_GRAPHQL_QUERY_SIZE_BYTES);
        for mutation in pending_mutations.into_iter().map(|x| x.nodes).flatten() {
            let core = mutation.core.unwrap();
            array_name.append_value(core.name);
            array_queries.append_value(core.queries.into_iter().map(|x| x.query));
            if let Some(output) = core.output {
                array_output.append_value(output.output);
            } else {
                array_output.append_null();
            }
            array_status.append_value(String::from("pending"));
        }
        for mutation in resolved_mutations.into_iter().map(|x| x.nodes).flatten() {
            let core = mutation.core.unwrap();
            array_name.append_value(core.name);
            array_queries.append_value(core.queries.into_iter().map(|x| x.query));
            if let Some(output) = core.output {
                array_output.append_value(output.output);
            } else {
                array_output.append_null();
            }
            array_status.append_value(String::from("resolved"));
        }

        RecordBatch::try_new(
            mutation_schema.clone(),
            vec![
                Arc::new(array_name.finish()),
                Arc::new(array_queries.finish()),
                Arc::new(array_output.finish()),
                Arc::new(array_status.finish()),
            ],
        )?
    };

    let mut local_path = path;
    local_path.push("mutations.parquet");
    let file = fs::File::create(local_path.clone())?;
    let props = WriterProperties::new();
    let mut writer = ArrowWriter::try_new(file, mutation_schema, Some(props))?;
    writer.write(&mutation_record_batch)?;
    writer.close()?;
    Ok(local_path)
}

pub fn serialize_changes_to_parquet(tree: &sled::Tree, path: PathBuf) -> anyhow::Result<PathBuf> {
    let pending_changes: Vec<_> = tree.scan_prefix(changes::change_prefix_pending_raw())
        .filter_map(|c| c.ok())
        .map(|(k, v)| ChangeValueWithCounter::decode(v.as_ref()).unwrap())
        .collect();

    let resolved_changes: Vec<_> = tree.scan_prefix(changes::change_prefix_resolved_raw())
        .filter_map(|c| c.ok())
        .map(|(k, v)| ChangeValueWithCounter::decode(v.as_ref()).unwrap())
        .collect();

    let change_schema = {
        let field_monotonic_counter = Field::new("monotonic_counter", DataType::UInt64, false);
        let field_branch = Field::new("branch", DataType::UInt64, false);
        let field_path_part = Field::new("item", DataType::Utf8, true);
        let field_path = Field::new("path", DataType::List(Arc::new(field_path_part)), false);
        let field_value = Field::new("value", DataType::Binary, true);
        let field_parent_counter = Field::new("item", DataType::UInt64, true);
        let field_parent_counters = Field::new("parent_counters", DataType::List(Arc::new(field_parent_counter)), false);
        let field_source_node = Field::new("source_node", DataType::Utf8, false);
        let field_status = Field::new("status", DataType::Utf8, false);
        Arc::new(Schema::new(vec![
            field_monotonic_counter,
            field_branch,
            field_path,
            field_value,
            field_parent_counters,
            field_source_node,
            field_status
        ]))
    };

    let change_record_batch = {
        let mut array_monotonic_counter = UInt64Builder::with_capacity(pending_changes.len() + resolved_changes.len());
        let mut array_branch = UInt64Builder::with_capacity(pending_changes.len() + resolved_changes.len());
        let mut array_path = ListBuilder::with_capacity(StringBuilder::new(), pending_changes.len() + resolved_changes.len());
        let mut array_value = BinaryBuilder::with_capacity(pending_changes.len() + resolved_changes.len(), MAX_GRAPHQL_QUERY_SIZE_BYTES);
        let mut array_parent_counters = ListBuilder::with_capacity(UInt64Builder::new(), pending_changes.len() + resolved_changes.len());
        let mut array_source_node = StringBuilder::with_capacity(pending_changes.len() + resolved_changes.len(), MAX_GRAPHQL_QUERY_SIZE_BYTES);
        let mut array_status = StringBuilder::with_capacity(pending_changes.len() + resolved_changes.len(), MAX_GRAPHQL_QUERY_SIZE_BYTES);

        for change in pending_changes {
            for value in change.filled_values {
                array_monotonic_counter.append_value(change.monotonic_counter);
                array_branch.append_value(value.branch);
                array_path.append_value(value.path.unwrap().address.into_iter().map(|x| Some(x)));
                array_value.append_value(value.value.unwrap().encode_to_vec());
                array_parent_counters.append_value(change.parent_monotonic_counters.clone().into_iter().map(|x| Some(x)));
                array_source_node.append_value(change.source_node.clone());
                array_status.append_value(String::from("pending"));
            }
        }
        for change in resolved_changes {
            for value in change.filled_values {
                array_monotonic_counter.append_value(change.monotonic_counter);
                array_branch.append_value(value.branch);
                array_path.append_value(value.path.unwrap().address.into_iter().map(|x| Some(x)));
                if let Some(value) = value.value {
                    array_value.append_value(value.encode_to_vec());
                } else {
                    array_value.append_null();
                }
                array_parent_counters.append_value(change.parent_monotonic_counters.clone().into_iter().map(|x| Some(x)));
                array_source_node.append_value(change.source_node.clone());
                array_status.append_value(String::from("resolved"));
            }
        }

        RecordBatch::try_new(
            change_schema.clone(),
            vec![
                Arc::new(array_monotonic_counter.finish()),
                Arc::new(array_branch.finish()),
                Arc::new(array_path.finish()),
                Arc::new(array_value.finish()),
                Arc::new(array_parent_counters.finish()),
                Arc::new(array_source_node.finish()),
                Arc::new(array_status.finish()),
            ],
        )?
    };

    let mut local_path = path;
    local_path.push("changes.parquet");
    let file = fs::File::create(local_path.clone())?;
    let props = WriterProperties::new();
    let mut writer = ArrowWriter::try_new(file, change_schema, Some(props))?;
    writer.write(&change_record_batch)?;
    writer.close()?;
    Ok(local_path)
}


pub fn serialize_node_will_executes_to_parquet(tree: &sled::Tree, path: PathBuf) -> anyhow::Result<PathBuf> {
    let will_exec_events: Vec<_> = tree.scan_prefix(executing_nodes::will_exec_pending_prefix_raw())
        .filter_map(|c| c.ok())
        .map(|(k, v)| NodeWillExecuteOnBranch::decode(v.as_ref()).unwrap())
        .collect();

    let will_exec_events_schema = {
        let field_node_name = Field::new("node_name", DataType::Utf8, false);
        let field_branch = Field::new("branch", DataType::UInt64, false);
        let field_counter = Field::new("counter", DataType::UInt64, false);
        Arc::new(Schema::new(vec![
            field_node_name,
            field_branch,
            field_counter,
        ]))
    };

    let will_exec_event_record_batch = {
        let mut array_node_name = StringBuilder::with_capacity(will_exec_events.len(), 8);
        let mut array_branch = UInt64Builder::with_capacity(will_exec_events.len());
        let mut array_counter = UInt64Builder::with_capacity(will_exec_events.len());

        for will_exec_event in will_exec_events {
            array_node_name.append_value(will_exec_event.node.unwrap().source_node);
            array_branch.append_value(will_exec_event.branch);
            array_counter.append_value(will_exec_event.counter);
        }

        RecordBatch::try_new(
            will_exec_events_schema.clone(),
            vec![
                Arc::new(array_node_name.finish()),
                Arc::new(array_branch.finish()),
                Arc::new(array_counter.finish()),
            ],
        )?
    };

    let mut local_path = path;
    local_path.push("will_exec_events.parquet");
    let file = fs::File::create(local_path.clone())?;
    let props = WriterProperties::new();
    let mut writer = ArrowWriter::try_new(file, will_exec_events_schema, Some(props))?;
    writer.write(&will_exec_event_record_batch)?;
    writer.close()?;
    Ok(local_path)
}

pub fn serialize_input_proposals_to_parquet(tree: &sled::Tree, path: PathBuf) -> anyhow::Result<PathBuf> {
    let input_proposals: Vec<_> = tree.scan_prefix(input_proposals_and_responses::input_proposal_prefix_raw())
        .filter_map(|c| c.ok())
        .map(|(k, v)| InputProposal::decode(v.as_ref()).unwrap())
        .collect();

    let input_proposals_schema = {
        let field_name = Field::new("name", DataType::Utf8, false);
        let field_output = Field::new("output", DataType::Utf8, true);
        let field_counter = Field::new("counter", DataType::UInt64, false);
        let field_branch = Field::new("branch", DataType::UInt64, false);
        Arc::new(Schema::new(vec![
            field_name,
            field_output,
            field_counter,
            field_branch,
        ]))
    };

    let input_proposals_record_batch = {
        let mut array_name = StringBuilder::with_capacity(input_proposals.len(), 8);
        let mut array_output = StringBuilder::with_capacity(input_proposals.len(), 8);
        let mut array_counter = UInt64Builder::with_capacity(input_proposals.len());
        let mut array_branch = UInt64Builder::with_capacity(input_proposals.len());

        for input_proposal in input_proposals {
            array_name.append_value(input_proposal.name);
            if let Some(x) = input_proposal.output {
                array_output.append_value(&x.output[..]);
            } else {
                array_output.append_null();
            }
            array_counter.append_value(input_proposal.counter);
            array_branch.append_value(input_proposal.branch);
        }

        RecordBatch::try_new(
            input_proposals_schema.clone(),
            vec![
                Arc::new(array_name.finish()),
                Arc::new(array_output.finish()),
                Arc::new(array_counter.finish()),
                Arc::new(array_branch.finish()),
            ],
        )?
    };

    let mut local_path = path;
    local_path.push("input_proposals.parquet");
    let file = fs::File::create(local_path.clone())?;
    let props = WriterProperties::new();
    let mut writer = ArrowWriter::try_new(file, input_proposals_schema, Some(props))?;
    writer.write(&input_proposals_record_batch)?;
    writer.close()?;
    Ok(local_path)
}

pub fn serialize_input_responses_to_parquet(tree: &sled::Tree, path: PathBuf) -> anyhow::Result<PathBuf> {
    let input_responses: Vec<_> = tree.scan_prefix(input_proposals_and_responses::input_response_prefix_raw())
        .filter_map(|c| c.ok())
        .map(|(k, v)| RequestInputProposalResponse::decode(v.as_ref()).unwrap())
        .collect();

    let input_responses_schema = {
        let field_id = Field::new("id", DataType::Utf8, false);
        let field_proposal_id = Field::new("proposal_id", DataType::UInt64, false);
        let field_branch = Field::new("branch", DataType::UInt64, false);
        let field_path_part = Field::new("item", DataType::Utf8, true);
        let field_path = Field::new("path", DataType::List(Arc::new(field_path_part)), false);
        let field_value = Field::new("value", DataType::Binary, false);
        Arc::new(Schema::new(vec![
            field_id,
            field_proposal_id,
            field_branch,
            field_path,
            field_value
        ]))
    };

    let input_responses_record_batch = {
        let mut array_id = StringBuilder::with_capacity(input_responses.len(), MAX_GRAPHQL_QUERY_SIZE_BYTES);
        let mut array_proposal_counter = UInt64Builder::with_capacity(input_responses.len());
        let mut array_branch = UInt64Builder::with_capacity(input_responses.len());
        let mut array_path = ListBuilder::with_capacity(StringBuilder::new(), input_responses.len());
        let mut array_value = BinaryBuilder::with_capacity(input_responses.len(), MAX_GRAPHQL_QUERY_SIZE_BYTES);

        for input_response in input_responses {
            for c in input_response.changes {
                array_id.append_value(input_response.id.clone());
                array_proposal_counter.append_value(input_response.proposal_counter);
                array_branch.append_value(input_response.branch);
                array_path.append_value(c.path.unwrap().address.into_iter().map(|x| Some(x)));
                array_value.append_value(c.value.unwrap().encode_to_vec());
            }
        }

        RecordBatch::try_new(
            input_responses_schema.clone(),
            vec![
                Arc::new(array_id.finish()),
                Arc::new(array_proposal_counter.finish()),
                Arc::new(array_branch.finish()),
                Arc::new(array_path.finish()),
                Arc::new(array_value.finish()),
            ],
        )?
    };

    let mut local_path = path;
    local_path.push("input_responses.parquet");
    let file = fs::File::create(local_path.clone())?;
    let props = WriterProperties::new();
    let mut writer = ArrowWriter::try_new(file, input_responses_schema, Some(props))?;
    writer.write(&input_responses_record_batch)?;
    writer.close()?;
    Ok(local_path)
}


// TODO: update all of these to return bytes
pub fn serialize_to_parquet(tree: &sled::Tree, relative_path_directory: PathBuf) -> anyhow::Result<SerializationResult> {
    // We do not serialize, these are necessary for execution but not analysis.
    //     * counters
    //     * playback state
    let mut path = PathBuf::new();
    path.push(relative_path_directory);

    //     * branches
    let branches_path = serialize_branches_to_parquet(tree, path.clone())?;
    //     * mutations
    let mutations_path = serialize_mutations_to_parquet(tree, path.clone())?;
    //     * changes
    let changes_path = serialize_changes_to_parquet(tree, path.clone())?;
    //     * node will execute
    let node_will_executes_path = serialize_node_will_executes_to_parquet(tree, path.clone())?;
    //     * input proposals
    let input_proposals_path = serialize_input_proposals_to_parquet(tree, path.clone())?;
    //     * input responses
    let input_responses_path = serialize_input_responses_to_parquet(tree, path.clone())?;

    Ok(SerializationResult {
        branches_path,
        mutations_path,
        changes_path,
        node_will_executes_path,
        input_proposals_path,
        input_responses_path,
    })
}

#[cfg(test)]
mod tests {
    use std::process::Output;
    use sled::Config;
    use prompt_graph_core::proto2::{ChangeValue, ChangeValueWithCounter, File, InputProposal, Item, item, ItemCore, NodeWillExecute, NodeWillExecuteOnBranch, OutputType, Path, Query, RequestInputProposalResponse, serialized_value, SerializedValue};
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use crate::db_operations::update_change_counter_for_branch;
    use crate::db_operations::branches::{create_branch, create_root_branch};
    use crate::db_operations::changes::{insert_new_change_value_with_counter, resolve_pending_change};
    use crate::db_operations::executing_nodes::insert_will_execute;
    use crate::db_operations::graph_mutations::{insert_pending_graph_mutation, resolve_pending_graph_mutation};
    use crate::db_operations::input_proposals_and_responses::{insert_input_proposal, insert_input_response};
    use crate::db_operations::parquet_serialization::{serialize_branches_to_parquet, serialize_changes_to_parquet, serialize_input_proposals_to_parquet, serialize_input_responses_to_parquet, serialize_mutations_to_parquet, serialize_node_will_executes_to_parquet, serialize_to_parquet};


    #[tokio::test]
    async fn test_writing_branches_to_parquet() {
        let db = Config::new().temporary(true).flush_every_ms(None).open().unwrap();
        let tree = db.open_tree("test").unwrap();
        let path = std::env::temp_dir();
        update_change_counter_for_branch(&tree, 0);
        create_root_branch(&tree);
        create_branch(&tree, 0, 0);
        let branches_path = serialize_branches_to_parquet(&tree, path.clone()).unwrap();
        let file = std::fs::File::open(branches_path).unwrap();
        let reader = SerializedFileReader::new(file).unwrap();
        let mut iter = reader.get_row_iter(None).unwrap();
        let root_branch = iter.next().unwrap();
        assert_eq!(root_branch.to_string(), "{id: 0, source_branch_ids: [], divergent_branch_ids: [1], diverges_at_counter: 0}");
        let created_branch = iter.next().unwrap();
        assert_eq!(created_branch.to_string(), "{id: 1, source_branch_ids: [0], divergent_branch_ids: [], diverges_at_counter: 0}");
    }

    #[tokio::test]
    async fn test_writing_mutations_to_parquet() {
        let db = Config::new().temporary(true).flush_every_ms(None).open().unwrap();
        let tree = db.open_tree("test").unwrap();
        let path = std::env::temp_dir();
        update_change_counter_for_branch(&tree, 0);
        insert_pending_graph_mutation(&tree, 0, File {
            id: "".to_string(),
            nodes: vec![
                Item {
                    core: Some(ItemCore {
                        name: "".to_string(),
                        queries: vec![Query {
                            query: Some("q".to_string()),
                        }],
                        output_tables: vec![],
                        output: Some(OutputType {
                            output: "o".to_string(),
                        }),
                        
                    }),
                    item: Some(item::Item::NodeEcho(prompt_graph_core::proto2::PromptGraphNodeEcho {
                    })),
                }

            ],
        });
        resolve_pending_graph_mutation(&tree, 0, 0);
        let mutations_path = serialize_mutations_to_parquet(&tree, path.clone()).unwrap();
        let file = std::fs::File::open(mutations_path).unwrap();
        let reader = SerializedFileReader::new(file).unwrap();
        let mut iter = reader.get_row_iter(None).unwrap();
        let node = iter.next().unwrap();
        assert_eq!(node.to_string(), r#"{name: "", query: "q", output: "o", status: "resolved"}"#);
    }

    #[tokio::test]
    async fn test_writing_changes_to_parquet() {
        let db = Config::new().temporary(true).flush_every_ms(None).open().unwrap();
        let tree = db.open_tree("test").unwrap();
        let path = std::env::temp_dir();
        update_change_counter_for_branch(&tree, 0);
        // TODO: must have filled values
        insert_new_change_value_with_counter(
            &tree,
            ChangeValueWithCounter {
                filled_values: vec![ChangeValue {
                    path: Some(Path {
                        address: vec![String::from("")],
                    }),
                    value: None,
                    branch: 0,
                }],
                parent_monotonic_counters: vec![],
                monotonic_counter: 0,
                branch: 0,
                source_node: "".to_string(),
            });
        resolve_pending_change(&tree, 0 ,0);
        let changes_path = serialize_changes_to_parquet(&tree, path.clone()).unwrap();
        let file = std::fs::File::open(changes_path).unwrap();
        let reader = SerializedFileReader::new(file).unwrap();
        let mut iter = reader.get_row_iter(None).unwrap();
        let change = iter.next().unwrap();
        assert_eq!(change.to_string(), r#"{monotonic_counter: 0, branch: 0, path: [""], value: null, parent_counters: [], source_node: "", status: "resolved"}"#);
    }

    #[tokio::test]
    async fn test_writing_node_will_change_to_parquet() {
        let db = Config::new().temporary(true).flush_every_ms(None).open().unwrap();
        let tree = db.open_tree("test").unwrap();
        let path = std::env::temp_dir();
        update_change_counter_for_branch(&tree, 0);
        insert_will_execute(&tree, NodeWillExecuteOnBranch {
            counter: 0,
            custom_node_type_name: None,
            node: Some(NodeWillExecute {
                source_node: "".to_string(),
                change_values_used_in_execution: vec![],
                matched_query_index: 0,
            }),
            branch: 0,
        });
        let node_will_executes_path = serialize_node_will_executes_to_parquet(&tree, path.clone()).unwrap();
        let file = std::fs::File::open(node_will_executes_path).unwrap();
        let reader = SerializedFileReader::new(file).unwrap();
        let mut iter = reader.get_row_iter(None).unwrap();
        let will_exec = iter.next().unwrap();
        assert_eq!(will_exec.to_string(), r#"{node_name: "", branch: 0, counter: 0}"#);
    }

    #[tokio::test]
    async fn test_writing_input_proposal_to_parquet() {
        let db = Config::new().temporary(true).flush_every_ms(None).open().unwrap();
        let tree = db.open_tree("test").unwrap();
        let path = std::env::temp_dir();
        update_change_counter_for_branch(&tree, 0);
        insert_input_proposal(&tree, InputProposal {
            name: "".to_string(),
            output: None,
            counter: 0,
            branch: 0,
        });
        let input_proposals_path = serialize_input_proposals_to_parquet(&tree, path.clone()).unwrap();
        let file = std::fs::File::open(input_proposals_path).unwrap();
        let reader = SerializedFileReader::new(file).unwrap();
        let mut iter = reader.get_row_iter(None).unwrap();
        let input_proposal = iter.next().unwrap();
        assert_eq!(input_proposal.to_string(), r#"{name: "", output: null, counter: 0, branch: 0}"#);
    }

    #[tokio::test]
    async fn test_writing_input_response_to_parquet() {
        let db = Config::new().temporary(true).flush_every_ms(None).open().unwrap();
        let tree = db.open_tree("test").unwrap();
        let path = std::env::temp_dir();
        update_change_counter_for_branch(&tree, 0);
        insert_input_response(&tree, RequestInputProposalResponse {
            id: String::from("graph_name"),
            proposal_counter: 0,
            changes: vec![ChangeValue {
                path: Some(Path {
                    address: vec![String::from("")],
                }),
                value: Some(SerializedValue {
                    val: Some(serialized_value::Val::Number(1)),
                }),
                branch: 0,
            }],
            branch: 0,
        });
        let input_responses_path = serialize_input_responses_to_parquet(&tree, path.clone()).unwrap();
        let file = std::fs::File::open(input_responses_path).unwrap();
        let reader = SerializedFileReader::new(file).unwrap();
        let mut iter = reader.get_row_iter(None).unwrap();
        let input_response = iter.next().unwrap();
        assert_eq!(input_response.to_string(), r#"{id: "graph_name", proposal_id: 0, branch: 0, path: [""], value: [24, 1]}"#);
    }
}
