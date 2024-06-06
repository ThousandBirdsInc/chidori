pub mod template_cell;
pub mod code_cell;
pub mod web_cell;
pub mod llm_prompt_cell;
pub mod memory_cell;
pub mod embedding_cell;
pub mod code_gen_cell;

use std::cmp::Ordering;
use std::collections::HashMap;
use rkyv::{Archive, Deserialize, Serialize};
use serde_json::Value;
use crate::library::std::ai::llm::ChatModelBatch;

#[derive(
    Archive,
    serde::Serialize,
    serde::Deserialize,
    Serialize,
    Deserialize,
    Debug,
    PartialEq,
    Clone,
)]
#[archive(bound(serialize = "__S: rkyv::ser::ScratchSpace + rkyv::ser::Serializer"))]
#[archive(check_bytes)]
#[archive_attr(check_bytes(
    bound = "__C: rkyv::validation::ArchiveContext, <__C as rkyv::Fallible>::Error: std::error::Error"
))]
#[archive_attr(derive(Debug))]
pub enum SupportedLanguage {
    PyO3,
    Starlark,
    Deno,
}

#[derive(
    Archive,
    serde::Serialize,
    serde::Deserialize,
    Serialize,
    Deserialize,
    Debug,
    PartialEq,
    Clone,
)]
#[archive(bound(serialize = "__S: rkyv::ser::ScratchSpace + rkyv::ser::Serializer"))]
#[archive(check_bytes)]
#[archive_attr(check_bytes(
    bound = "__C: rkyv::validation::ArchiveContext, <__C as rkyv::Fallible>::Error: std::error::Error"
))]
#[archive_attr(derive(Debug))]
pub struct CodeCell {
    pub name: Option<String>,
    pub language: SupportedLanguage,
    pub source_code: String,
    pub function_invocation: Option<String>,
}


#[derive(
Archive,
serde::Serialize,
serde::Deserialize,
Serialize,
Deserialize,
Debug,
PartialEq,
Clone,
)]
#[archive(bound(serialize = "__S: rkyv::ser::ScratchSpace + rkyv::ser::Serializer"))]
#[archive(check_bytes)]
#[archive_attr(check_bytes(
bound = "__C: rkyv::validation::ArchiveContext, <__C as rkyv::Fallible>::Error: std::error::Error"
))]
#[archive_attr(derive(Debug))]
pub enum SupportedMemoryProviders {
    InMemory,
}


#[derive(
Archive,
serde::Serialize,
serde::Deserialize,
Serialize,
Deserialize,
Debug,
PartialEq,
Clone,
)]
#[archive(bound(serialize = "__S: rkyv::ser::ScratchSpace + rkyv::ser::Serializer"))]
#[archive(check_bytes)]
#[archive_attr(check_bytes(
bound = "__C: rkyv::validation::ArchiveContext, <__C as rkyv::Fallible>::Error: std::error::Error"
))]
#[archive_attr(derive(Debug))]
pub struct MemoryCell {
    pub name: Option<String>,
    pub provider: SupportedMemoryProviders,
    pub embedding_function: String,
}


#[derive(
Archive,
serde::Serialize,
serde::Deserialize,
Serialize,
Deserialize,
Debug,
PartialEq,
Clone,
)]
#[archive(bound(serialize = "__S: rkyv::ser::ScratchSpace + rkyv::ser::Serializer"))]
#[archive(check_bytes)]
#[archive_attr(check_bytes(
bound = "__C: rkyv::validation::ArchiveContext, <__C as rkyv::Fallible>::Error: std::error::Error"
))]
#[archive_attr(derive(Debug))]
pub struct TemplateCell {
    pub name: Option<String>,
    pub body: String,
}

#[derive(
Archive,
serde::Serialize,
serde::Deserialize,
Serialize,
Deserialize,
Debug,
PartialEq,
Clone,
)]
#[archive(bound(serialize = "__S: rkyv::ser::ScratchSpace + rkyv::ser::Serializer"))]
#[archive(check_bytes)]
#[archive_attr(check_bytes(
bound = "__C: rkyv::validation::ArchiveContext, <__C as rkyv::Fallible>::Error: std::error::Error"
))]
#[archive_attr(derive(Debug))]
pub struct WebserviceCellEndpoint {
    pub method: String,
    pub route: String,
    pub depended_function_identity: String,
    pub arg_mapping: Vec<(String, String)>,
}

#[derive(
Archive,
serde::Serialize,
serde::Deserialize,
Serialize,
Deserialize,
Debug,
PartialEq,
Clone,
)]
#[archive(bound(serialize = "__S: rkyv::ser::ScratchSpace + rkyv::ser::Serializer"))]
#[archive(check_bytes)]
#[archive_attr(check_bytes(
bound = "__C: rkyv::validation::ArchiveContext, <__C as rkyv::Fallible>::Error: std::error::Error"
))]
#[archive_attr(derive(Debug))]
pub struct WebserviceCell {
    pub name: Option<String>,
    pub configuration: String,
    pub port: u16,
}


#[derive(
Archive,
serde::Serialize,
serde::Deserialize,
Serialize,
Deserialize,
Debug,
PartialEq,
Clone,
)]
#[archive(bound(serialize = "__S: rkyv::ser::ScratchSpace + rkyv::ser::Serializer"))]
#[archive(check_bytes)]
#[archive_attr(check_bytes(
bound = "__C: rkyv::validation::ArchiveContext, <__C as rkyv::Fallible>::Error: std::error::Error"
))]
#[archive_attr(derive(Debug))]
pub struct ScheduleCell {
    pub configuration: String,
}


#[derive(
    Archive,
    serde::Serialize,
    serde::Deserialize,
    Serialize,
    Deserialize,
    Debug,
    PartialEq,
    Clone,
)]
#[archive(bound(serialize = "__S: rkyv::ser::ScratchSpace + rkyv::ser::Serializer"))]
#[archive(check_bytes)]
#[archive_attr(check_bytes(
    bound = "__C: rkyv::validation::ArchiveContext, <__C as rkyv::Fallible>::Error: std::error::Error"
))]
#[archive_attr(derive(Debug))]
pub enum SupportedModelProviders {
    OpenAI,
}


#[derive(
Default,
Archive,
serde::Serialize,
serde::Deserialize,
Serialize,
Deserialize,
Debug,
PartialEq,
Clone,
)]
#[archive(bound(serialize = "__S: rkyv::ser::ScratchSpace + rkyv::ser::Serializer"))]
#[archive(check_bytes)]
#[archive_attr(check_bytes(
bound = "__C: rkyv::validation::ArchiveContext, <__C as rkyv::Fallible>::Error: std::error::Error"
))]
#[archive_attr(derive(Debug))]
pub struct EjectionConfig {
    pub language: String,
    pub mode: String
}




#[derive(
Default,
Archive,
serde::Serialize,
serde::Deserialize,
Serialize,
Deserialize,
Debug,
PartialEq,
Clone,
)]
#[archive(bound(serialize = "__S: rkyv::ser::ScratchSpace + rkyv::ser::Serializer"))]
#[archive(check_bytes)]
#[archive_attr(check_bytes(
bound = "__C: rkyv::validation::ArchiveContext, <__C as rkyv::Fallible>::Error: std::error::Error"
))]
#[archive_attr(derive(Debug))]
pub struct LLMPromptCellChatConfiguration {
    pub(crate) import: Option<Vec<String>>,

    #[serde(rename = "fn")]
    pub(crate) function_name: Option<String>,

    pub model: String,
    pub api_url: Option<String>,
    pub frequency_penalty: Option<f64>,
    pub max_tokens: Option<i64>,
    pub presence_penalty: Option<f64>,
    pub stop: Option<Vec<String>>,
    pub temperature: Option<f64>,
    pub logit_bias: Option<HashMap<String, i32>>,
    pub user: Option<String>,
    pub seed: Option<i64>,
    pub top_p: Option<f64>,
}

#[derive(
    Archive,
    serde::Serialize,
    serde::Deserialize,
    Serialize,
    Deserialize,
    Debug,
    PartialEq,
    Clone,
)]
#[archive(bound(serialize = "__S: rkyv::ser::ScratchSpace + rkyv::ser::Serializer"))]
#[archive(check_bytes)]
#[archive_attr(check_bytes(
    bound = "__C: rkyv::validation::ArchiveContext, <__C as rkyv::Fallible>::Error: std::error::Error"
))]
#[archive_attr(derive(Debug))]
pub enum LLMPromptCell {
    Chat {
        function_invocation: bool,
        configuration: LLMPromptCellChatConfiguration,
        name: Option<String>,
        provider: SupportedModelProviders,
        req: String,
    },
    Completion {
        req: String,
    },
}


#[derive(
Default,
Archive,
serde::Serialize,
serde::Deserialize,
Serialize,
Deserialize,
Debug,
PartialEq,
Clone,
)]
#[archive(bound(serialize = "__S: rkyv::ser::ScratchSpace + rkyv::ser::Serializer"))]
#[archive(check_bytes)]
#[archive_attr(check_bytes(
bound = "__C: rkyv::validation::ArchiveContext, <__C as rkyv::Fallible>::Error: std::error::Error"
))]
#[archive_attr(derive(Debug))]
pub struct LLMCodeGenCellChatConfiguration {
    #[serde(rename = "fn")]
    pub function_name: Option<String>,

    pub api_url: Option<String>,
    pub model: String,
    pub frequency_penalty: Option<f64>,
    pub max_tokens: Option<i64>,
    pub presence_penalty: Option<f64>,
    pub stop: Option<Vec<String>>,
    pub temperature: Option<f64>,
    pub logit_bias: Option<HashMap<String, i32>>,
    pub user: Option<String>,
    pub seed: Option<i64>,
    pub top_p: Option<f64>,

    pub language: Option<String>,
}

#[derive(
Archive,
serde::Serialize,
serde::Deserialize,
Serialize,
Deserialize,
Debug,
PartialEq,
Clone,
)]
#[archive(bound(serialize = "__S: rkyv::ser::ScratchSpace + rkyv::ser::Serializer"))]
#[archive(check_bytes)]
#[archive_attr(check_bytes(
bound = "__C: rkyv::validation::ArchiveContext, <__C as rkyv::Fallible>::Error: std::error::Error"
))]
#[archive_attr(derive(Debug))]
pub struct LLMCodeGenCell {
    pub function_invocation: bool,
    pub configuration: LLMCodeGenCellChatConfiguration,
    pub name: Option<String>,
    pub provider: SupportedModelProviders,
    pub req: String,
}


#[derive(
Archive,
serde::Serialize,
serde::Deserialize,
Serialize,
Deserialize,
Debug,
PartialEq,
Clone,
)]
#[archive(bound(serialize = "__S: rkyv::ser::ScratchSpace + rkyv::ser::Serializer"))]
#[archive(check_bytes)]
#[archive_attr(check_bytes(
bound = "__C: rkyv::validation::ArchiveContext, <__C as rkyv::Fallible>::Error: std::error::Error"
))]
#[archive_attr(derive(Debug))]
pub struct LLMEmbeddingCell {
    pub function_invocation: bool,
    pub configuration: HashMap<String, String>,
    pub name: Option<String>,
    pub req: String,
}


#[derive(
Archive,
serde::Serialize,
serde::Deserialize,
Serialize,
Deserialize,
Debug,
PartialEq,
Default,
Clone,
)]
#[archive(bound(serialize = "__S: rkyv::ser::ScratchSpace + rkyv::ser::Serializer"))]
#[archive(check_bytes)]
#[archive_attr(check_bytes(
bound = "__C: rkyv::validation::ArchiveContext, <__C as rkyv::Fallible>::Error: std::error::Error"
))]
#[archive_attr(derive(Debug))]
pub struct TextRange {
    pub start: usize,
    pub end: usize,
}

#[derive(
    Archive,
    serde::Serialize,
    serde::Deserialize,
    Serialize,
    Deserialize,
    Debug,
    PartialEq,
    Clone,
)]
#[archive(bound(serialize = "__S: rkyv::ser::ScratchSpace + rkyv::ser::Serializer"))]
#[archive(check_bytes)]
#[archive_attr(check_bytes(
    bound = "__C: rkyv::validation::ArchiveContext, <__C as rkyv::Fallible>::Error: std::error::Error"
))]
#[archive_attr(derive(Debug))]
pub enum CellTypes {
    Code(CodeCell, TextRange),
    CodeGen(LLMCodeGenCell, TextRange),
    Prompt(LLMPromptCell, TextRange),
    Embedding(LLMEmbeddingCell, TextRange),
    Web(WebserviceCell, TextRange),
    Template(TemplateCell, TextRange),
    Memory(MemoryCell, TextRange),
}

impl Eq for CellTypes {

}

impl PartialOrd for CellTypes {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(Ordering::Equal)
    }
}

impl Ord for CellTypes {
    fn cmp(&self, other: &Self) -> Ordering {
        self.partial_cmp(other).unwrap_or(Ordering::Equal)
    }
}

pub fn get_cell_name(cell: &CellTypes) -> &Option<String> {
    match &cell {
        CellTypes::Code(c, _) => &c.name,
        CellTypes::Prompt(c, _) => match c {
            LLMPromptCell::Chat { name, .. } => name,
            LLMPromptCell::Completion { .. } => &None,
        },
        CellTypes::Web(c, _) => &c.name,
        CellTypes::Template(c, _) => &c.name,
        CellTypes::Memory(c, _) => &c.name,
        CellTypes::Embedding(c, _) => &c.name,
        CellTypes::CodeGen(c, _) => &c.name
    }
}
