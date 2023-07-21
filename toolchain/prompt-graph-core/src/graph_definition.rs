use anyhow::anyhow;
use prost::Message;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::proto2 as dsl;
use crate::proto2::{ItemCore, Query};
use crate::proto2::prompt_graph_node_loader::LoadFrom;
use crate::utils::wasm_error::CoreError;

fn map_string_to_vector_database(encoding: &str) -> anyhow::Result<dsl::SupportedVectorDatabase> {
    match encoding {
        "IN_MEMORY" => Ok(dsl::SupportedVectorDatabase::InMemory),
        "CHROMA" => Ok(dsl::SupportedVectorDatabase::Chroma),
        "PINECONEDB" => Ok(dsl::SupportedVectorDatabase::Pineconedb),
        "QDRANT" => Ok(dsl::SupportedVectorDatabase::Qdrant),
        _ => {
            Err(anyhow!("Unknown vector database: {}", encoding))
        },
    }
}

fn map_string_to_embedding_model(encoding: &str) -> anyhow::Result<dsl::SupportedEmebddingModel> {
    match encoding {
        "TEXT_EMBEDDING_ADA_002" => Ok(dsl::SupportedEmebddingModel::TextEmbeddingAda002),
        "TEXT_SEARCH_ADA_DOC_001" => Ok(dsl::SupportedEmebddingModel::TextSearchAdaDoc001),
        _ => {
            Err(anyhow!("Unknown embedding model: {}", encoding))
        },
    }
}

fn map_string_to_chat_model(encoding: &str) -> anyhow::Result<dsl::SupportedChatModel> {
    match encoding {
        "GPT_4" => Ok(dsl::SupportedChatModel::Gpt4),
        "GPT_4_0314" => Ok(dsl::SupportedChatModel::Gpt40314),
        "GPT_4_32K" => Ok(dsl::SupportedChatModel::Gpt432k),
        "GPT_4_32K_0314" => Ok(dsl::SupportedChatModel::Gpt432k0314),
        "GPT_3_5_TURBO" => Ok(dsl::SupportedChatModel::Gpt35Turbo),
        "GPT_3_5_TURBO_0301" => Ok(dsl::SupportedChatModel::Gpt35Turbo0301),
        _ => {
            Err(anyhow!("Unknown chat model: {}", encoding))
        },
    }
}

fn map_string_to_supported_source_langauge(encoding: &str) -> anyhow::Result<dsl::SupportedSourceCodeLanguages> {
    match encoding {
        "DENO" => Ok(dsl::SupportedSourceCodeLanguages::Deno),
        "STARLARK" => Ok(dsl::SupportedSourceCodeLanguages::Starlark),
        _ => {
            Err(anyhow!("Unknown source language: {}", encoding))
        },
    }
}

fn create_query(query_def: Option<String>) -> dsl::Query {
     dsl::Query {
        query: query_def.map(|d|d),
    }
}

fn create_output(output_def: &str) -> Option<dsl::OutputType> {
    Some(dsl::OutputType {
        output: output_def.to_string(),
    })
}

#[derive(Debug, Serialize, Deserialize)]
pub enum SourceNodeType {
    Code(String, String, bool),
    S3(String),
    Zipfile(Vec<u8>),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DefinitionGraph {
    internal: dsl::File,
}

impl DefinitionGraph {

    pub fn get_file(&self) -> &dsl::File {
        &self.internal
    }

    pub fn zero() -> Self {
        Self {
            internal: dsl::File::default()
        }
    }

    pub fn from_file(file: dsl::File) -> Self {
        Self {
            internal: file
        }
    }

    pub fn new(bytes: &[u8]) -> Self {
        Self {
            internal: dsl::File::decode(bytes).unwrap()
        }
    }

    pub(crate) fn get_nodes(&self) -> &Vec<dsl::Item> {
        &self.internal.nodes
    }

    pub(crate) fn get_nodes_mut(&mut self) -> &Vec<dsl::Item> {
        &self.internal.nodes
    }

    pub(crate) fn serialize(&self) -> Vec<u8> {
        let mut buffer = Vec::new();
        self.internal.encode(&mut buffer).unwrap();
        buffer
    }

    pub fn register_node(&mut self, item: dsl::Item) {
        self.internal.nodes.push(item);
    }
}


#[deprecated(since="0.1.0", note="do not use")]
pub fn create_entrypoint_query(
    query_def: Option<String>
) -> dsl::Item {
    let query_element = dsl::Query {
        query: query_def.map(|x| x.to_string()),
    };
    let _node = dsl::PromptGraphNodeCode {
        source: None,
    };
    dsl::Item {
        core: Some(ItemCore {
            name: "RegistrationCodeNode".to_string(),
            queries: vec![query_element],
            output: Default::default(),
            output_tables: vec![],
        }),
        item: None,
    }
}

pub fn create_node_parameter(
    name: String,
    output_def: String
) -> dsl::Item {
    dsl::Item {
        core: Some(ItemCore {
            name: name.to_string(),
            output: create_output(&output_def),
            queries: vec![Query { query: None }],
            output_tables: vec![],
        }),
        item: Some(dsl::item::Item::NodeParameter(dsl::PromptGraphParameterNode {
        })),
    }
}

pub fn create_op_map(
    name: String,
    query_defs: Vec<Option<String>>,
    path: String,
    output_tables: Vec<String>
) -> dsl::Item {
    dsl::Item {
        core: Some(ItemCore {
            name: name.to_string(),
            queries: query_defs.into_iter().map(create_query).collect(),
            // TODO: needs to have the type of the input
            output: create_output(r#"
                type O {
                    result: String
                }
            "#),
            output_tables,
        }),
        item: Some(dsl::item::Item::Map(dsl::PromptGraphMap {
            path: path.to_string(),
        })),
    }
}

// TODO: automatically wire these into prompt nodes that support function calling
// TODO: https://platform.openai.com/docs/guides/gpt/function-calling
pub fn create_code_node(
    name: String,
    query_defs: Vec<Option<String>>,
    output_def: String,
    source_type: SourceNodeType,
    output_tables: Vec<String>,
) -> dsl::Item {
    let source = match source_type {
        SourceNodeType::Code(language, code, template) => {
            // https://github.com/denoland/deno/discussions/17345
            // https://github.com/a-poor/js-in-rs/blob/main/src/main.rs
            dsl::prompt_graph_node_code::Source::SourceCode( dsl::PromptGraphNodeCodeSourceCode{
                template,
                language: map_string_to_supported_source_langauge(&language).unwrap() as i32,
                source_code: code.to_string(),
            })
        }
        SourceNodeType::S3(path) => {
            dsl::prompt_graph_node_code::Source::S3Path(path)
        }
        SourceNodeType::Zipfile(file) => {
            dsl::prompt_graph_node_code::Source::Zipfile(file)
        }
    };

    dsl::Item {
        core: Some(ItemCore {
            name: name.to_string(),
            queries: query_defs.into_iter().map(create_query).collect(),
            output: create_output(&output_def),
            output_tables
        }),
        item: Some(dsl::item::Item::NodeCode(dsl::PromptGraphNodeCode{
            source: Some(source),
        })),
    }
}



// TODO: automatically wire these into prompt nodes that support function calling
// TODO: https://platform.openai.com/docs/guides/gpt/function-calling
pub fn create_custom_node(
    name: String,
    query_defs: Vec<Option<String>>,
    output_def: String,
    type_name: String,
    output_tables: Vec<String>
) -> dsl::Item {
    dsl::Item {
        core: Some(ItemCore {
            name: name.to_string(),
            queries: query_defs.into_iter().map(create_query).collect(),
            output: create_output(&output_def),
            output_tables
        }),
        item: Some(dsl::item::Item::NodeCustom(dsl::PromptGraphNodeCustom{
            type_name,
        })),
    }
}


pub fn create_observation_node(
    name: String,
    query_defs: Vec<Option<String>>,
    output_def: String,
    output_tables: Vec<String>
) -> dsl::Item {
    dsl::Item {
        core: Some(ItemCore {
            name: name.to_string(),
            queries: query_defs.into_iter().map(create_query).collect(),
            output: create_output(&output_def),
            output_tables
        }),
        item: Some(dsl::item::Item::NodeObservation(dsl::PromptGraphNodeObservation{
            integration: "".to_string(),
        })),
    }
}

pub fn create_vector_memory_node(
    name: String,
    query_defs: Vec<Option<String>>,
    output_def: String,
    action: String,
    embedding_model: String,
    template: String,
    db_vendor: String,
    collection_name: String,
    output_tables: Vec<String>
) -> anyhow::Result<dsl::Item> {
    let model = dsl::prompt_graph_node_memory::EmbeddingModel::Model(map_string_to_embedding_model(&embedding_model)? as i32);
    let vector_db = dsl::prompt_graph_node_memory::VectorDbProvider::Db(map_string_to_vector_database(&db_vendor)? as i32);

    let action = match action.as_str() {
        "READ" => {
            dsl::MemoryAction::Read as i32
        },
        "WRITE" => {
            dsl::MemoryAction::Write as i32
        },
        "DELETE" => {
            dsl::MemoryAction::Delete as i32
        }
        _ => { unreachable!("Invalid action") }
    };

    Ok(dsl::Item {
        core: Some(ItemCore {
            name: name.to_string(),
            queries: query_defs.into_iter().map(create_query).collect(),
            output: create_output(&output_def),
            output_tables
        }),
        item: Some(dsl::item::Item::NodeMemory(dsl::PromptGraphNodeMemory{
            collection_name: collection_name,
            action,
            embedding_model: Some(model),
            template: template,
            vector_db_provider: Some(vector_db),
        })),
    })
}

pub fn create_component_node(
    name: String,
    query_defs: Vec<Option<String>>,
    output_def: String,
    output_tables: Vec<String>,
) -> dsl::Item {
    dsl::Item {
        core: Some(ItemCore {
            name: name.to_string(),
            queries: query_defs.into_iter().map(create_query).collect(),
            output: create_output(&output_def),
            output_tables
        }),
        item: Some(dsl::item::Item::NodeComponent(dsl::PromptGraphNodeComponent {
            transclusion: None,
        })),
    }
}

pub fn create_loader_node(
    name: String,
    query_defs: Vec<Option<String>>,
    output_def: String,
    load_from: LoadFrom,
    output_tables: Vec<String>,
) -> dsl::Item {
    dsl::Item {
        core: Some(ItemCore {
            name: name.to_string(),
            queries: query_defs.into_iter().map(create_query).collect(),
            output: create_output(&output_def),
            output_tables
        }),
        item: Some(dsl::item::Item::NodeLoader(dsl::PromptGraphNodeLoader {
            load_from: Some(load_from),
        })),
    }
}

pub fn create_prompt_node(
    name: String,
    query_defs: Vec<Option<String>>,
    template: String,
    model: String,
    output_tables: Vec<String>,
) -> anyhow::Result<dsl::Item> {
    let chat_model = map_string_to_chat_model(&model)?;
    let model = dsl::prompt_graph_node_prompt::Model::ChatModel(chat_model as i32);
    // TODO: use handlebars Template object in order to inspect the contents of and validate the template against the query
    // https://github.com/sunng87/handlebars-rust/blob/23ca8d76bee783bf72f627b4c4995d1d11008d17/src/template.rs#L963
    // self.handlebars.register_template_string(name, template).unwrap();
    // println!("{:?}", Template::compile(&template).unwrap());
    Ok(dsl::Item {
        core: Some(ItemCore {
            name: name.to_string(),
            queries: query_defs.into_iter().map(create_query).collect(),
            output: create_output(r#"
              type O {
                  promptResult: String
              }
            "#),
            output_tables
        }),
        item: Some(dsl::item::Item::NodePrompt(dsl::PromptGraphNodePrompt{
            template: template.to_string(),
            model: Some(model),
            // TODO: add output but set it to some sane defaults
            temperature: 1.0,
            top_p: 1.0,
            max_tokens: 100,
            presence_penalty: 0.0,
            frequency_penalty: 0.0,
            stop: vec![],
        })),
    })
}

impl DefinitionGraph {

    pub fn register_node_bytes(&mut self, item: &[u8]) {
        let item = dsl::Item::decode(item).unwrap();
        self.internal.nodes.push(item);
    }

}
