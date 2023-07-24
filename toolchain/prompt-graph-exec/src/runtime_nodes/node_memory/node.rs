use std::collections::{HashMap, HashSet};
use base64::{Engine as _};
use anyhow::Result;
use openai_api_rs::v1::api::Client;
use prompt_graph_core::proto2::{ChangeValue, ChangeValueWithCounter, InputProposal, item, ItemCore, MemoryAction, NodeWillExecute, Path, PromptGraphNodeMemory, SupportedEmebddingModel, SupportedVectorDatabase};
use prompt_graph_core::templates::render_template_prompt;
use std::env;
use openai_api_rs::v1::embedding::EmbeddingRequest;
use futures::executor;
use http_body_util::BodyExt;
use prost::Message;
use qdrant_client::prelude::*;
use qdrant_client::qdrant::vectors_config::Config;
use qdrant_client::qdrant::{
    Condition, CreateCollection, Filter, SearchPoints, VectorParams, VectorsConfig,
};
use prompt_graph_core::create_change_value;
use prompt_graph_core::proto2::prompt_graph_node_memory::{EmbeddingModel, VectorDbProvider};
use prompt_graph_core::proto2::serialized_value::Val;
use crate::executor::NodeExecutionContext;


// TODO: we will want to give the user the ability to retry a set of values with a different distance model
// TODO: needs an identifier for the db
// TODO: configure store and retrieve
// TODO: n.template; <- used to build the embedding content
// TODO: this needs to get values into a particular shape
// TODO: we need some selectors into the query?
// let metadata_selector = vec![String:from("metadata")];
// let embedding_selector = vec![String:from("embedding")];
// let vector_query = vec![String:from("query")];
// TODO: db name as a parameter and table - we want to be able to reuse
// TODO: choose the query function


#[tracing::instrument]
pub async fn execute_node_memory(ctx: &NodeExecutionContext<'_>) -> Result<Vec<ChangeValue>> {
    let &NodeExecutionContext {
        node_will_execute_on_branch,
        item: item::Item::NodeMemory(n),
        item_core,
        namespaces,
        ..
    } = ctx else {
        panic!("execute_node_memory: expected NodeExecutionContext with NodeMemory item");
    };

    let mut filled_values = vec![];
    let mut change_set: Vec<ChangeValue> = node_will_execute_on_branch.node.as_ref().unwrap()
        .change_values_used_in_execution.iter().filter_map(|x| x.change_value.clone()).collect();

    // This excludes partials, there is no use case we currently know of for partials in embedding templates.
    let content_to_embed = render_template_prompt(&n.template, &change_set.clone(), &HashMap::new()).unwrap();
    let collection_name = &n.collection_name;

    let embedding_vec = if let Some(EmbeddingModel::Model(enum_)) = n.embedding_model {
        match SupportedEmebddingModel::from_i32(enum_).unwrap() {
            SupportedEmebddingModel::TextEmbeddingAda002 => {
                // Getting embedding value
                let client = Client::new(env::var("OPENAI_API_KEY").unwrap().to_string());
                let req = EmbeddingRequest {
                    model: "text-embedding-ada-002".to_string(),
                    input: content_to_embed.clone(),
                    user: Option::None,
                };
                client.embedding(req).await?.data.first().unwrap().embedding.clone()
            }
            SupportedEmebddingModel::TextSearchAdaDoc001 => {
                unimplemented!("TEXT_SEARCH_ADA_DOC_001 embedding is not implemented")
            }
        }
    } else {
        panic!("No model specified for memory node");
    };


    if let Some(VectorDbProvider::Db(enum_)) = n.vector_db_provider {
        match SupportedVectorDatabase::from_i32(enum_).unwrap() {
            SupportedVectorDatabase::InMemory => { unimplemented!(); }
            SupportedVectorDatabase::Chroma => { unimplemented!(); }
            SupportedVectorDatabase::Pineconedb => { unimplemented!(); }
            SupportedVectorDatabase::Qdrant => {
                let config = QdrantClientConfig::from_url("http://localhost:6334");
                let client = QdrantClient::new(Some(config))?;
                if let Some(x) = MemoryAction::from_i32(n.action) {
                    match x {
                        MemoryAction::Read => {
                            let search_result = client
                                .search_points(&SearchPoints {
                                    collection_name: collection_name.into(),
                                    vector: embedding_vec,
                                    // filter: Some(Filter::all([Condition::matches("bar", 12)])),
                                    filter: None,
                                    limit: 10,
                                    with_payload: Some(true.into()),
                                    ..Default::default()
                                })
                                .await?;
                            let found_point = search_result.result.into_iter().next().unwrap();
                            let mut payload = found_point.payload;

                            if let Some(query) = payload.get("query") {
                                let s = query.as_str().unwrap();
                                let v = base64::engine::general_purpose::STANDARD.decode(s)?;
                                let node_will_execute = NodeWillExecute::decode(v.as_slice())?;
                                for change in node_will_execute.change_values_used_in_execution {
                                    if let Some(change_value) = change.change_value {
                                        for output_table in &item_core.output_tables {
                                            let mut address = vec![output_table.clone(), "query".to_string()];
                                            address.extend(change_value.path.clone().unwrap().address);
                                            filled_values.push(
                                                ChangeValue{
                                                    path: Some(Path {
                                                        address,
                                                    }),
                                                    value: change_value.value.clone(),
                                                    branch: 0,
                                                });
                                        }
                                    }
                                }
                            }
                            for namespace in namespaces.iter() {
                                filled_values.push(create_change_value(
                                    vec![namespace.clone(), "key".to_string()],
                                    payload.get("key").map(|x| Val::String(x.as_str().unwrap().to_string())),
                                    0));
                            }
                            // TODO: read should fetch the values into an associated output
                        }
                        MemoryAction::Write => {
                            let mut payload: HashMap<&str, Value> = HashMap::new();
                            payload.insert("key", Value::from(content_to_embed));

                            // As far as I can tell, qdrant doesn't support just shoving binary in here so we store as a base64 string
                            let changes_as_str = base64::engine::general_purpose::STANDARD.encode(&node_will_execute_on_branch.encode_to_vec());
                            payload.insert("query", Value::from(changes_as_str));
                            let points = vec![PointStruct::new(0, embedding_vec, payload.into())];
                            client
                                .upsert_points_blocking(collection_name, points, None)
                                .await?;
                        }
                        MemoryAction::Delete => {
                            unimplemented!("Memory Node DELETE is not implemented")
                        }
                    }
                }
            }
        }
    }

    // store this into the vector db
    // self.memory_vector_db.insert(result.data.iter().map(|x| (x.embedding, x.id))));
    // TODO: query vector database (in memory?)
    Ok(filled_values)
}


pub async fn initialize_node_memory_init(n: &PromptGraphNodeMemory, core: &ItemCore, branch: u64, counter: u64) -> Result<(Vec<ChangeValueWithCounter>, Vec<InputProposal>)> {
    let collection_name = &n.collection_name;

    let embedding_length = if let Some(EmbeddingModel::Model(enum_)) = n.embedding_model {
        match SupportedEmebddingModel::from_i32(enum_).unwrap() {
            SupportedEmebddingModel::TextEmbeddingAda002 => 1536,
            SupportedEmebddingModel::TextSearchAdaDoc001 => 768,
        }
    } else {
        0
    };

    if let Some(VectorDbProvider::Db(enum_)) = n.vector_db_provider {
        match SupportedVectorDatabase::from_i32(enum_).unwrap() {
            SupportedVectorDatabase::InMemory => { unimplemented!(); }
            SupportedVectorDatabase::Chroma => { unimplemented!(); }
            SupportedVectorDatabase::Pineconedb => { unimplemented!(); }
            SupportedVectorDatabase::Qdrant => {
                let config = QdrantClientConfig::from_url("http://localhost:6334");
                let client = QdrantClient::new(Some(config))?;

                // TODO: check if the collection already exists first
                // TODO: at initialization if the collection does not exist, create it
                client
                    .create_collection(&CreateCollection {
                        collection_name: collection_name.into(),
                        vectors_config: Some(qdrant_client::qdrant::VectorsConfig {
                            config: Some(Config::Params(VectorParams {
                                size: embedding_length,
                                distance: Distance::Cosine.into(),
                                ..Default::default()
                            })),
                        }),
                        ..Default::default()
                    })
                    .await?;
            }
        }
    }

    Ok((vec![], vec![]))
}


#[cfg(test)]
mod tests {
    use prompt_graph_core::graph_definition::create_vector_memory_node;
    use prompt_graph_core::proto2::item;
    use anyhow::Result;
    use super::*;

    // TODO: implement conditionally running this with a qdrant feature
    #[cfg(feature = "qdrant")]
    #[tokio::test]
    async fn test_exec_memory_node_qdrant() {
        // TODO: this test will require a running qdrant instance

        // docker run -p 6333:6333 -p 6334:6334 \
        // -e QDRANT__SERVICE__GRPC_PORT="6334" \
        // qdrant/qdrant

        let config = QdrantClientConfig::from_url("http://localhost:6334");
        let client = QdrantClient::new(Some(config)).unwrap();
        let _ = client.delete_collection("test_exec_memory_node_qdrant").await;

        let collection_name = "test_exec_memory_node_qdrant".to_string();

        let write = create_vector_memory_node(
            "".to_string(),
            vec![None],
            "".to_string(),
            "WRITE".to_string(),
            "TEXT_EMBEDDING_ADA_002".to_string(),
            "example embedding".to_string(),
            "QDRANT".to_string(),
            collection_name.clone(),
            vec![]
        ).unwrap();

        let nwe = NodeWillExecute {
            source_node: "".to_string(),
            change_values_used_in_execution: vec![],
            matched_query_index: 0
        };

        if let (core, item::Item::NodeMemory(n)) = (write.core.unwrap(), write.item.unwrap()) {
            initialize_node_memory_init(
                &n,
                &core,
                0,
                0).await.unwrap();

            let ctx = NodeExecutionContext {
                node_will_execute: &nwe,
                item_core: &core,
                item: &item::Item::NodeMemory(n),
                namespaces: &HashSet::from(["".to_string()]),
                template_partials: &HashMap::new()
            };
            execute_node_memory(&ctx).await.unwrap();
        } else {
            assert!(false);
        }


        let nwe = NodeWillExecute {
            source_node: "".to_string(),
            change_values_used_in_execution: vec![],
            matched_query_index: 0
        };

        let read = create_vector_memory_node(
            "".to_string(),
            vec![None],
            "".to_string(),
            "READ".to_string(),
            "TEXT_EMBEDDING_ADA_002".to_string(),
            "example".to_string(),
            "QDRANT".to_string(),
            collection_name.clone(),
            vec![]
        ).unwrap();

        if let (core, item::Item::NodeMemory(n)) = (read.core.unwrap(), read.item.unwrap()) {
            let ctx = NodeExecutionContext {
                node_will_execute: &nwe,
                item_core: &core,
                item: &item::Item::NodeMemory(n),
                namespaces: &HashSet::from(["".to_string()]),
                template_partials: &HashMap::new()
            };
            let recollection = execute_node_memory( &ctx ).await.unwrap();
            assert_eq!(recollection[0].path, Some(Path { address: vec![ "".to_string(), "key".to_string() ] }));
        } else {
            assert!(false);
        }
    }
}
