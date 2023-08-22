use anyhow::anyhow;
use prost::Message;
use serde::{Deserialize, Serialize};

use crate::proto2 as dsl;
use crate::proto2::{ItemCore, Query};
use crate::proto2::prompt_graph_node_loader::LoadFrom;

/// Maps a string to a supported vector database type
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

/// Maps a string to a supported embedding model type
fn map_string_to_embedding_model(encoding: &str) -> anyhow::Result<dsl::SupportedEmebddingModel> {
    match encoding {
        "TEXT_EMBEDDING_ADA_002" => Ok(dsl::SupportedEmebddingModel::TextEmbeddingAda002),
        "TEXT_SEARCH_ADA_DOC_001" => Ok(dsl::SupportedEmebddingModel::TextSearchAdaDoc001),
        _ => {
            Err(anyhow!("Unknown embedding model: {}", encoding))
        },
    }
}

/// Maps a string to a supported chat model type
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

/// Maps a string to a supported source language type
fn map_string_to_supported_source_langauge(encoding: &str) -> anyhow::Result<dsl::SupportedSourceCodeLanguages> {
    match encoding {
        "DENO" => Ok(dsl::SupportedSourceCodeLanguages::Deno),
        "STARLARK" => Ok(dsl::SupportedSourceCodeLanguages::Starlark),
        _ => {
            Err(anyhow!("Unknown source language: {}", encoding))
        },
    }
}

/// Converts a string representing a query definition to a Query type
fn create_query(query_def: Option<String>) -> dsl::Query {
     dsl::Query {
        query: query_def.map(|d|d),
    }
}

/// Converts a string representing an output definition to an OutputType type
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


/// A graph definition or DefinitionGraph defines a graph of executable nodes connected by edges or 'triggers'.
/// The graph is defined in a DSL (domain specific language) that is compiled into a binary formatted File that can be
/// executed by the prompt-graph-core runtime.
impl DefinitionGraph {

    /// Returns the File object representing this graph definition
    pub fn get_file(&self) -> &dsl::File {
        &self.internal
    }

    /// Returns an empty graph definition
    pub fn zero() -> Self {
        Self {
            internal: dsl::File::default()
        }
    }

    /// Sets this graph definition to read from & write to the given File object
    pub fn from_file(file: dsl::File) -> Self {
        Self {
            internal: file
        }
    }

    /// Store the given bytes (representing protobuf graph definition) as a
    /// new File object and associate this graph definition with it
    pub fn new(bytes: &[u8]) -> Self {
        Self {
            internal: dsl::File::decode(bytes).unwrap()
        }
    }

    /// Read and return the nodes from internal File object
    pub(crate) fn get_nodes(&self) -> &Vec<dsl::Item> {
        &self.internal.nodes
    }

    /// Read and return a mutable collection of nodes from internal File object
    pub(crate) fn get_nodes_mut(&mut self) -> &Vec<dsl::Item> {
        &self.internal.nodes
    }

    /// Serialize the internal File object to bytes and return them
    pub(crate) fn serialize(&self) -> Vec<u8> {
        let mut buffer = Vec::new();
        self.internal.encode(&mut buffer).unwrap();
        buffer
    }

    /// Push a given node (defined as Item type) to the internal graph definition
    pub fn register_node(&mut self, item: dsl::Item) {
        self.internal.nodes.push(item);
    }

    /// Push a given node (defined as bytes) to the internal graph definition
    pub fn register_node_bytes(&mut self, item: &[u8]) {
        let item = dsl::Item::decode(item).unwrap();
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
            triggers: vec![query_element],
            output: Default::default(),
            output_tables: vec![],
        }),
        item: None,
    }
}

/// Takes in common node parameters and returns a fulfilled node type (a dsl::Item type)
pub fn create_node_parameter(
    name: String,
    output_def: String
) -> dsl::Item {
    dsl::Item {
        core: Some(ItemCore {
            name: name.to_string(),
            output: create_output(&output_def),
            triggers: vec![Query { query: None }],
            output_tables: vec![],
        }),
        item: Some(dsl::item::Item::NodeParameter(dsl::PromptGraphParameterNode {
        })),
    }
}

/// Returns a Map type node, which maps a Path (key) to a given String (value)
pub fn create_op_map(
    name: String,
    query_defs: Vec<Option<String>>,
    path: String,
    output_tables: Vec<String>
) -> dsl::Item {
    dsl::Item {
        core: Some(ItemCore {
            name: name.to_string(),
            triggers: query_defs.into_iter().map(create_query).collect(),
            // TODO: needs to have the type of the input
            output: create_output(r#"
                {
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
/// Takes in executable code and returns a node that executes said code when triggered
/// This executable code can take the format of:
/// - a raw string of code in a supported language
/// - a path to an S3 bucket containing code in a supported language
/// - a zip file containing code in a supported language
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
            triggers: query_defs.into_iter().map(create_query).collect(),
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
/// Returns a custom node that executes a given function
/// When registering a custom node in the SDK, you provide an in-language function and
/// tell chidori to register that function under the given "type_name".
/// This function executed is then executed in the graph
/// when referenced by this "type_name" parameter
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
            triggers: query_defs.into_iter().map(create_query).collect(),
            output: create_output(&output_def),
            output_tables
        }),
        item: Some(dsl::item::Item::NodeCustom(dsl::PromptGraphNodeCustom{
            type_name,
        })),
    }
}

/// Returns a node that, when triggered, echoes back its input for easier querying
pub fn create_observation_node(
    name: String,
    query_defs: Vec<Option<String>>,
    output_def: String,
    output_tables: Vec<String>
) -> dsl::Item {
    dsl::Item {
        core: Some(ItemCore {
            name: name.to_string(),
            triggers: query_defs.into_iter().map(create_query).collect(),
            output: create_output(&output_def),
            output_tables
        }),
        item: Some(dsl::item::Item::NodeObservation(dsl::PromptGraphNodeObservation{
            integration: "".to_string(),
        })),
    }
}

/// Returns a node that can perform some READ/WRITE/DELETE operation on
/// a specified Vector database, using the specified configuration options
/// (options like the embedding_model to use and collection_name namespace to query within)
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
            triggers: query_defs.into_iter().map(create_query).collect(),
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

/// Returns a node that can implement logic from another graph definition
/// This is useful for reusing logic across multiple graphs
/// The graph definition to transclude is specified by either
/// - a path to an S3 bucket containing a graph definition
/// - raw bytes of a graph definition
/// - a File object containing a graph definition
pub fn create_component_node(
    name: String,
    query_defs: Vec<Option<String>>,
    output_def: String,
    output_tables: Vec<String>,
) -> dsl::Item {
    dsl::Item {
        core: Some(ItemCore {
            name: name.to_string(),
            triggers: query_defs.into_iter().map(create_query).collect(),
            output: create_output(&output_def),
            output_tables
        }),
        item: Some(dsl::item::Item::NodeComponent(dsl::PromptGraphNodeComponent {
            transclusion: None,
        })),
    }
}

/// Returns a node that can read bytes from a given source
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
            triggers: query_defs.into_iter().map(create_query).collect(),
            output: create_output(&output_def),
            output_tables
        }),
        item: Some(dsl::item::Item::NodeLoader(dsl::PromptGraphNodeLoader {
            load_from: Some(load_from),
        })),
    }
}

/// Returns a node that, when triggered, performs an API call to a given language model endpoint,
/// using the template parameter as the prompt input to the language model, and returns the result
/// to the graph as a String type labeled "promptResult"
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
            triggers: query_defs.into_iter().map(create_query).collect(),
            output: create_output(r#"
              {
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
